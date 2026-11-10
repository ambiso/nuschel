#!/usr/bin/env python3
"""Encrypted UDP voice against a stock Mumble server, with TCP fallback.

Runs against a server we start ourselves (loopback by default, or the VM's
fixture via --host). Checks OCB2 interoperability, UDP relay, forced TCP and a
mixed UDP/TCP pair. With --block-udp (VM only, root) it also drops inbound UDP
mid-call and checks fallback to the TCP tunnel and recovery.
"""
import argparse
import socket
from pathlib import Path
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mumble_client import Client, ReferenceOpus, certificate_pin  # noqa: E402


def wait_transport(client, predicate, message, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if client.transport and predicate(client.transport):
            return client.transport
        try:
            client.wait(lambda event: event["kind"] == "transport", timeout=1.5)
        except AssertionError:
            pass
    raise AssertionError(f"{message}; last transport: {client.transport}")


def talk(sender, receiver, packets, base):
    for index, packet in enumerate(packets):
        sender.send(kind="voice", sequence=base + index * 2, opus=packet, end=index == len(packets) - 1)
        time.sleep(0.02)
    event = receiver.wait(lambda event: event["kind"] == "audio" and event["sender"] == sender.session)
    assert event["opus"] in packets
    return event


def join(client, channel):
    client.send(kind="join", channel=channel)
    client.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == client.session
                and event["user"]["channel"] == channel)


def iptables(*arguments):
    # The server may be reached over IPv4 or IPv6 (the VM network uses both).
    for tool in ("iptables", "ip6tables"):
        subprocess.run([tool, *arguments], check=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--block-udp", action="store_true")
    parser.add_argument("--netem", action="store_true", help="VM only, root: add delay, jitter and loss with tc")
    args = parser.parse_args()
    reference = ReferenceOpus(args.opus_library)
    packets = reference.encode_tone(10)
    with tempfile.TemporaryDirectory(prefix="nuschel-udp-") as temporary:
        root = Path(temporary)
        pin = certificate_pin(args.binaries, args.host, args.port, root / "probe")
        clients = []
        try:
            alice = Client(args.binaries, args.host, args.port, pin, "UdpAlice", root / "alice")
            bob = Client(args.binaries, args.host, args.port, pin, "UdpBob", root / "bob")
            tcp = Client(args.binaries, args.host, args.port, pin, "TcpCarol", root / "carol", ["--force-tcp"])
            clients = [alice, bob, tcp]
            for client in clients:
                join(client, 1)
            for client in (alice, bob):
                stats = wait_transport(client, lambda s: s["udp_active"], f"{client} never activated UDP")
                assert stats["udp"]["packets"] > 0 and not stats["forced_tcp"]
            alice.unmute()
            bob.unmute()
            tcp.unmute()
            before = bob.transport["good"]
            talk(alice, bob, packets, 0)
            stats = wait_transport(bob, lambda s: s["good"] > before + 3, "voice did not arrive over UDP")
            print(f"UDP relay ok: bob decrypted {stats['good'] - before} datagrams, udp ping {stats['udp']['average_ms']:.1f} ms")
            # The forced-TCP client never activates UDP but still talks both ways.
            assert not tcp.transport.get("udp_active") and tcp.transport.get("forced_tcp", True)
            talk(tcp, alice, packets, 0)
            talk(alice, tcp, packets, 40)
            print("Forced TCP and mixed UDP/TCP relay ok")
            if args.netem:
                device = subprocess.run(["ip", "-o", "route", "get", socket.gethostbyname(args.host)],
                                        capture_output=True, text=True, check=True).stdout.split(" dev ")[1].split()[0]
                subprocess.run(["tc", "qdisc", "add", "dev", device, "root", "netem", "delay", "40ms", "20ms",
                                "loss", "5%"], check=True)
                try:
                    before = bob.transport.get("good", 0)
                    many = reference.encode_tone(100)
                    talk(alice, bob, many, 400)
                    stats = wait_transport(bob, lambda s: s["good"] > before + 60, "voice did not survive jitter and loss", timeout=30)
                    assert stats["udp_active"], "UDP was abandoned under moderate loss"
                    print(f"Lossy network ok: {stats['good'] - before} datagrams, {stats['lost']} lost, {stats['late']} late, "
                          f"udp ping {stats['udp']['average_ms']:.0f} ms", flush=True)
                finally:
                    subprocess.run(["tc", "qdisc", "del", "dev", device, "root"], check=False)
            if args.block_udp:
                # Drop inbound UDP for Bob's socket only: replies stop, so Bob must
                # return to the tunnel and tell the server to stop sending UDP.
                iptables("-I", "INPUT", "-p", "udp", "--sport", str(args.port), "-j", "DROP")
                try:
                    wait_transport(bob, lambda s: not s["udp_active"], "UDP never deactivated", timeout=25)
                    time.sleep(2.5)
                    talk(alice, bob, packets, 100)
                    talk(bob, alice, packets, 100)
                    print("Fallback to TCP while inbound UDP is blocked ok")
                finally:
                    iptables("-D", "INPUT", "-p", "udp", "--sport", str(args.port), "-j", "DROP")
                wait_transport(bob, lambda s: s["udp_active"], "UDP did not recover", timeout=40)
                before = bob.transport["good"]
                talk(alice, bob, packets, 200)
                wait_transport(bob, lambda s: s["good"] > before + 3, "voice did not resume over UDP")
                print("UDP recovery ok")
        finally:
            for client in clients:
                client.close()


if __name__ == "__main__":
    main()
