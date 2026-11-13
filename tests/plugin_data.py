#!/usr/bin/env python3
"""Plugin data (PluginDataTransmission) through Nuschl's session and a server.

Run against a loopback server we start ourselves:
  with_local_murmur.py <mumble-server> plugin_data.py <binaries> --port PORT
  with_local_nuschld.py <binaries> plugin_data.py <binaries> --port PORT

Checks that the server relays plugin data with the real sender, whose presence
and certificate hash the receiver reports; present-but-empty and maximal data
and IDs; that a receiver that did not opt in gets nothing; local refusals for a
replaced presence and the reserved `nuschl/` namespace; and that a burst plus
paced sends within the local rate (two a second, burst ten) are all delivered
by the server's default bucket, the next one refused with a retry hint.
"""
import argparse
from pathlib import Path
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mumble_client import Client, certificate_pin  # noqa: E402


class Requests:
    def __init__(self):
        self.next = 0

    def __call__(self):
        self.next += 1
        return self.next


def presence(viewer, client):
    """The session and presence instance `viewer` knows `client` by."""
    deadline = time.monotonic() + 15
    while client.session not in viewer.users:
        assert time.monotonic() < deadline, f"{client.session} unknown to {viewer.session}"
        time.sleep(0.1)
    return [client.session, viewer.users[client.session]["instance"]]


def send(client, request, receivers, data_id, data):
    client.send(kind="plugin_data", request=request, receivers=receivers, data_id=data_id, data=list(data))


def admission(client, request):
    return client.wait(lambda e: e["kind"] == "plugin_data_admission" and e["request"] == request)


def received(client, data_id, timeout=15):
    return client.wait(lambda e: e["kind"] == "plugin_data" and e["data_id"] == data_id, timeout=timeout)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    args = parser.parse_args()
    request = Requests()
    with tempfile.TemporaryDirectory(prefix="nuschl-plugin-data-") as temporary:
        root = Path(temporary)
        pin = certificate_pin(args.binaries, args.host, args.port, root / "probe")
        connect = lambda name: Client(args.binaries, args.host, args.port, pin, name, root / name)
        clients = []
        try:
            alice, bob, carol = (connect(name) for name in ("PluginAlice", "PluginBob", "PluginCarol"))
            clients = [alice, bob, carol]
            to_bob, to_carol = presence(alice, bob), presence(alice, carol)
            alice_seen_by_bob = presence(bob, alice)

            # Bob opts in; commands run in order, so a refusal after it proves it applied.
            bob.send(kind="plugin_accept", accept=True)
            marker = request()
            send(bob, marker, [[0, 0]], "marker", b"")
            assert not admission(bob, marker)["admitted"]

            # Delivery: the real sender, its presence and certificate hash as Bob knows them.
            first = request()
            send(alice, first, [to_bob, to_bob], "plugin-test/1", b"\x00\x01\x02\xff")
            assert admission(alice, first)["admitted"]
            event = received(bob, "plugin-test/1")
            assert event["data"] == "000102ff", event
            assert [event["sender"], event["sender_instance"]] == alice_seen_by_bob, event
            assert event["sender_certificate_hash"] == bob.users[alice.session]["certificate_hash"], event
            print("Plugin data relayed with the real sender.", flush=True)

            # Present but empty data and ID, then the largest of both.
            empty = request()
            send(alice, empty, [to_bob], "", b"")
            assert admission(alice, empty)["admitted"]
            event = received(bob, "")
            assert event["data"] == "", event
            largest = request()
            long_id = "i" * 100
            send(alice, largest, [to_bob], long_id, bytes(range(256)) * 3 + bytes(232))
            assert admission(alice, largest)["admitted"]
            event = received(bob, long_id)
            assert len(event["data"]) == 2000 and event["data"].startswith("000102"), len(event["data"])
            print("Present-empty and maximal plugin data relayed.", flush=True)

            # Carol did not opt in: nothing reaches her stdout.
            quiet = request()
            send(alice, quiet, [to_carol], "plugin-test/quiet", b"x")
            assert admission(alice, quiet)["admitted"]
            time.sleep(1.5)
            assert not any(e["kind"] == "plugin_data" for e in carol.events), "Carol did not opt in"

            # Local refusals: a presence that is not the occupant, the reserved namespace.
            stale = request()
            send(alice, stale, [[to_bob[0], to_bob[1] + 1000]], "plugin-test/stale", b"x")
            event = admission(alice, stale)
            assert not event["admitted"] and "receiver" in event["error"], event
            reserved = request()
            send(alice, reserved, [to_bob], "nuschl/rtc", b"x")
            event = admission(alice, reserved)
            assert not event["admitted"] and "reserved" in event["error"], event
            print("Stale receivers and the reserved namespace are refused locally.", flush=True)

            # Let both the local bucket and the server's drain, then a burst of ten,
            # one refused with a hint, and ten paced at the local rate: the server's
            # default bucket must deliver all twenty.
            time.sleep(6)
            for _ in range(10):
                burst = request()
                send(alice, burst, [to_bob], "plugin-test/rate", b"r")
                assert admission(alice, burst)["admitted"]
            refused = request()
            send(alice, refused, [to_bob], "plugin-test/rate", b"r")
            event = admission(alice, refused)
            assert not event["admitted"] and event["retry_after_ms"], event
            for _ in range(10):
                time.sleep(0.52)
                paced = request()
                send(alice, paced, [to_bob], "plugin-test/rate", b"r")
                assert admission(alice, paced)["admitted"]
            delivered = 0
            deadline = time.monotonic() + 10
            while delivered < 20 and time.monotonic() < deadline:
                received(bob, "plugin-test/rate", timeout=max(0.1, deadline - time.monotonic()))
                delivered += 1
            assert delivered == 20, delivered
            time.sleep(1)
            assert sum(1 for e in bob.events if e["kind"] == "plugin_data" and e["data_id"] == "plugin-test/rate") == 20
            print("A burst plus paced plugin data within the local rate were all delivered.", flush=True)
        finally:
            for client in clients:
                client.close()


if __name__ == "__main__":
    main()
