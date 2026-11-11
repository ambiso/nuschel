#!/usr/bin/env python3
"""File transfers over magic-wormhole through the real transfer worker sandbox.

`python3 tests/transfer.py target/debug --python <python>`, where the interpreter has
magic-wormhole-mailbox-server, magic-wormhole-transit-relay and magic-wormhole
(the reference CLI). A private mailbox server and transit relay listen on 127.0.0.1;
transfers stay on the relay, so no public rendezvous, relay or STUN server is used.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import queue
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from local_murmur import eventually, free_port  # noqa: E402


def frame(payload):
    body = json.dumps({"version": 1, "request_id": 1, "payload": payload}).encode()
    return struct.pack(">I", len(body)) + body


class Wormhole:
    """A private magic-wormhole mailbox server and transit relay on 127.0.0.1."""

    def __init__(self, python, root):
        self.python = python
        self.bin = Path(python).parent
        self.mailbox_port, self.relay_port = free_port(), free_port()
        self.servers = {"rendezvous": f"ws://127.0.0.1:{self.mailbox_port}/v1",
                        "relay": f"tcp://127.0.0.1:{self.relay_port}", "direct": False}
        twist = str(self.bin / "twist")
        self.daemons = [
            subprocess.Popen([twist, "wormhole-mailbox", f"--port=tcp:{self.mailbox_port}:interface=127.0.0.1",
                              f"--channel-db={root / 'mailbox.sqlite'}"],
                             cwd=root, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT),
            subprocess.Popen([twist, "transitrelay", f"--port=tcp:{self.relay_port}:interface=127.0.0.1"],
                             cwd=root, stdout=subprocess.DEVNULL, stderr=subprocess.STDOUT),
        ]

        def listening(port):
            with socket.socket() as probe:
                return probe.connect_ex(("127.0.0.1", port)) == 0
        eventually(lambda: listening(self.mailbox_port) and listening(self.relay_port),
                   "wormhole servers did not start")

    def environment(self):
        """Nuschl's override: these servers, relay only."""
        return {"NUSCHL_WORMHOLE_RENDEZVOUS": self.servers["rendezvous"],
                "NUSCHL_WORMHOLE_RELAY": self.servers["relay"]}

    def cli(self, *arguments):
        """The reference Python client, on these servers."""
        return [str(self.bin / "wormhole"), f"--relay-url={self.servers['rendezvous']}",
                f"--transit-helper=tcp:127.0.0.1:{self.relay_port}", *arguments]

    def stop(self):
        for daemon in self.daemons:
            daemon.terminate()
            daemon.wait()


class Worker:
    """`nuschl transfer-sandbox`: the transfer worker in the sandbox Nuschl gives it."""

    def __init__(self, binaries, request, file=None):
        command = [str(binaries / "nuschl"), "transfer-sandbox"] + (["--file", str(file)] if file else [])
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=subprocess.DEVNULL)
        self.events = queue.Queue()
        self.backlog = []
        self.data = bytearray()
        self.slow = 0.0
        threading.Thread(target=self._read, daemon=True).start()
        self.send(request)

    def send(self, payload):
        self.process.stdin.write(frame(payload))
        self.process.stdin.flush()

    def _read(self):
        while header := self.process.stdout.read(4):
            size, = struct.unpack(">I", header)
            assert size <= 12 * 1024 * 1024
            envelope = json.loads(self.process.stdout.read(size))
            assert envelope["version"] == 1 and envelope["request_id"] == 0, envelope
            event = envelope["payload"]
            if event["kind"] == "data":
                self.data += base64.b64decode(event["bytes"])
                time.sleep(self.slow)  # a slow disk: the pipe fills and the sender waits
            else:
                self.events.put(event)
        self.events.put({"kind": "exited"})

    def wait(self, kind, timeout=60, **fields):
        """The first event of `kind` (with `fields`), seen earlier or yet to come."""
        def matches(event):
            return event["kind"] == kind and all(event.get(k) == v for k, v in fields.items())
        for index, event in enumerate(self.backlog):
            if matches(event):
                return self.backlog.pop(index)
        deadline = time.monotonic() + timeout
        while True:
            event = self.events.get(timeout=max(0.1, deadline - time.monotonic()))
            if matches(event):
                return event
            if event["kind"] == "exited" or (event["kind"] == "failed" and kind != "failed"):
                raise AssertionError(f"waiting for {kind}, got {event}")
            self.backlog.append(event)

    def close(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--python", type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="nuschl-transfer-") as temporary:
        root = Path(temporary)
        wormhole = Wormhole(args.python, root)
        servers = wormhole.servers
        workers = []

        def worker(request, file=None):
            started = Worker(args.binaries, request, file)
            workers.append(started)
            return started

        try:
            payload = os.urandom(3 * 1024 * 1024 + 17)
            offered = root / "offered" / "report.pdf"
            offered.parent.mkdir()
            offered.write_bytes(payload)
            digest = hashlib.sha256(payload).hexdigest()

            sender = worker({"kind": "offer", "name": "report.pdf", "servers": servers}, offered)
            first = sender.wait("code")["code"]
            print("offered with", first, flush=True)

            # Nuschl to Nuschl, through both sandboxes; the receiver is slow,
            # so the sender's progress is seen part-way.
            receiver = worker({"kind": "receive", "code": first, "max_size": len(payload) + 4096, "servers": servers})
            receiver.slow = 0.05
            started = receiver.wait("started")
            assert started["name"] == "report.pdf" and started["size"] == len(payload), started
            peer = sender.wait("started")["peer"]
            # A fresh code as soon as someone picked the file up, before it arrived.
            second = sender.wait("code")["code"]
            assert second != first
            partway = sender.wait("progress", peer=peer)
            assert partway["done"] < partway["total"], partway
            receiver.wait("done", timeout=120)
            sender.wait("done", peer=peer, timeout=30)
            assert hashlib.sha256(receiver.data).hexdigest() == digest
            print("received through the sandbox, then", second, flush=True)

            # The reference Python CLI, as a stock Mumble user would run it.
            output = root / "cli"
            output.mkdir()
            subprocess.run(wormhole.cli("receive", "--accept-file", "--hide-progress",
                                        "-o", str(output / "report.pdf"), second),
                           check=True, timeout=120, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            assert hashlib.sha256((output / "report.pdf").read_bytes()).hexdigest() == digest
            third = sender.wait("code")["code"]
            sender.wait("done", peer=peer + 1, timeout=30)
            print("received with the reference CLI, then", third, flush=True)

            # A used code fails. Nameplates are reused, so an old code may name
            # the current one: then it counts as a wrong guess and spoils it.
            stale = worker({"kind": "receive", "code": first, "max_size": len(payload), "servers": servers})
            reason = stale.wait("failed")["reason"]
            if "wrong" in reason:
                sender.wait("failed", peer=None)
                third = sender.wait("code", timeout=30)["code"]
            else:
                assert "used" in reason, reason
            small = worker({"kind": "receive", "code": third, "max_size": 1024, "servers": servers})
            assert "larger" in small.wait("failed")["reason"]
            assert small.data == b""
            assert "rejected" in sender.wait("failed", peer=peer + 2)["reason"]
            fourth = sender.wait("code")["code"]

            # A wrong code spoils this one: the sender says so and makes another.
            nameplate = fourth.split("-")[0]
            wrong = worker({"kind": "receive", "code": f"{nameplate}-wrong-guess", "max_size": 10, "servers": servers})
            wrong.wait("failed")
            assert "wrong" in sender.wait("failed", peer=None)["reason"]
            fifth = sender.wait("code", timeout=30)["code"]
            assert fifth != fourth
            print("refused stale, oversized and wrong attempts; now", fifth, flush=True)

            # Receiving from the reference CLI's sender.
            (root / "notes.txt").write_text("from the reference client\n")
            cli = subprocess.Popen(wormhole.cli("send", "--code=900-crossover-clockwork", "--hide-progress",
                                                str(root / "notes.txt")),
                                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            # (A nameplate the server will not hand to the offer above.)
            for _ in range(20):
                incoming = worker({"kind": "receive", "code": "900-crossover-clockwork", "max_size": 100,
                                   "servers": servers})
                event = incoming.events.get(timeout=60)
                while event["kind"] not in ("done", "failed", "exited"):
                    event = incoming.events.get(timeout=60)
                if event["kind"] == "done":
                    break
                # Until the CLI has claimed its nameplate, the code is unknown.
                assert "expired" in event.get("reason", ""), event
                time.sleep(0.5)
            assert event["kind"] == "done", event
            assert incoming.data == b"from the reference client\n"
            cli.wait(timeout=30)

            # An authenticated peer can call its own address a "relay".
            # FORCE_RELAY must advertise no local/direct hints and must not
            # contact that address, even when the configured relay fails.
            with socket.socket() as configured, socket.socket() as canary:
                for listener in (configured, canary):
                    listener.bind(("127.0.0.1", 0))
                    listener.listen()
                    listener.settimeout(10)
                pinned = dict(servers, relay=f"tcp://127.0.0.1:{configured.getsockname()[1]}")
                private = worker({"kind": "offer", "name": "report.pdf", "servers": pinned}, offered)
                code = private.wait("code")["code"]
                result = root / "privacy-hints.json"
                adversary = subprocess.Popen([
                    str(wormhole.python), str(Path(__file__).with_name("transfer_privacy_peer.py")),
                    servers["rendezvous"], code, str(canary.getsockname()[1]), str(result),
                ])
                try:
                    eventually(result.exists, "adversary did not complete the authenticated exchange", timeout=30)
                    connection, _ = configured.accept()
                    with connection:
                        connection.settimeout(5)
                        assert connection.recv(1024).startswith(b"please relay ")
                    # The configured endpoint failed, so no direct or
                    # peer-advertised relay fallback may follow.
                    canary.settimeout(6)
                    try:
                        connection, _ = canary.accept()
                    except TimeoutError:
                        pass
                    else:
                        connection.close()
                        raise AssertionError("relay-only transfer contacted an unconfigured peer address")
                    private.send({"kind": "cancel"})
                    private.process.wait(timeout=5)
                    print("relay-only advertised no peer IP and ignored hostile direct/relay hints after relay failure", flush=True)
                finally:
                    if adversary.poll() is None:
                        adversary.terminate()
                    adversary.wait(timeout=10)

            # Cancelling ends the offer: the worker exits promptly.
            sender.send({"kind": "cancel"})
            assert sender.process.wait(timeout=5) == 0
            print("transfer worker checks passed", flush=True)
        finally:
            for started in workers:
                started.close()
            wormhole.stop()


if __name__ == "__main__":
    main()
