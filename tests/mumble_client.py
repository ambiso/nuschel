#!/usr/bin/env python3
"""Exercise Nuschel's real session implementation against a stock Mumble server."""
import argparse
import collections
import ctypes
import json
import math
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import time

from workers import exchange


class Client:
    def __init__(self, binaries, host, port, pin, username, profile, extra=()):
        self.binaries = Path(binaries)
        self.queue = queue.Queue()
        self.events = collections.deque(maxlen=1000)
        self.users = {}
        self.channels = {}
        self.transport = {}
        self.process = subprocess.Popen([
            str(self.binaries / "nuschel-cli"), "--host", host, "--port", str(port),
            "--username", username, "--certificate-sha256", pin, "--profile-dir", str(profile), *extra,
        ], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()
        self.session = self.wait(lambda event: event["kind"] == "connected")["session"]

    def _read(self):
        try:
            for line in self.process.stdout:
                event = json.loads(line)
                self.events.append(event)
                if event["kind"] == "user":
                    self.users[event["user"]["session"]] = event["user"]
                elif event["kind"] == "user_removed":
                    self.users.pop(event["session"], None)
                elif event["kind"] == "channel":
                    self.channels[event["channel"]["id"]] = event["channel"]
                elif event["kind"] == "channel_removed":
                    self.channels.pop(event["channel"], None)
                elif event["kind"] == "transport":
                    self.transport = event["stats"]
                self.queue.put(event)
        finally:
            self.queue.put(None)

    def send(self, **command):
        self.process.stdin.write(json.dumps(command) + "\n")
        self.process.stdin.flush()

    def wait(self, predicate, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                event = self.queue.get(timeout=max(0.01, deadline - time.monotonic()))
            except queue.Empty:
                break
            if event is None:
                raise AssertionError(f"Client exited: {list(self.events)[-8:]}")
            if predicate(event):
                return event
        raise AssertionError(f"Expected event missing: {list(self.events)[-8:]}")

    def unmute(self):
        # Avoid exhausting the stock server's control-message burst allowance.
        time.sleep(1.1)
        self.send(kind="mute", muted=False)
        self.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == self.session
                  and not event["user"]["self_muted"])
        # The stock server's bandwidth limiter needs a nonzero initial interval.
        time.sleep(0.15)

    def create_channel(self, parent, name):
        self.send(kind="create_channel", parent=parent, name=name)
        event = self.wait(lambda event: event["kind"] == "permission_denied" or
                          (event["kind"] == "channel" and event["channel"]["name"] == name))
        assert event["kind"] == "channel", event
        return event["channel"]["id"]

    def close(self):
        if self.process.poll() is None:
            self.send(kind="disconnect")
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.reader.join(timeout=2)
        assert self.process.returncode == 0, self.process.stderr.read()


def certificate_pin(binaries, host, port, profile):
    command = [str(Path(binaries) / "nuschel-cli"), "--host", host, "--port", str(port),
               "--username", "CertificateProbe", "--profile-dir", str(profile)]
    probe = subprocess.run(command, input="", capture_output=True, text=True, timeout=20)
    events = [json.loads(line) for line in probe.stdout.splitlines()]
    certificate = next(event for event in events if event["kind"] == "certificate_required")
    assert probe.returncode != 0 and not certificate["changed"]
    assert not any(event["kind"] == "connected" for event in events)
    wrong = subprocess.run(command + ["--certificate-sha256", "0" * 64], input="",
                           capture_output=True, text=True, timeout=20)
    changed = [json.loads(line) for line in wrong.stdout.splitlines()]
    assert wrong.returncode != 0
    assert any(event["kind"] == "certificate_required" and event["changed"] for event in changed)
    return certificate["sha256"]


class ReferenceOpus:
    """Reference codec runs only in the test process, never in the client authority."""
    def __init__(self, library):
        self.lib = ctypes.CDLL(str(library))
        ptr = ctypes.c_void_p
        integer = ctypes.c_int
        floating = ctypes.POINTER(ctypes.c_float)
        self.lib.opus_encoder_create.argtypes = [integer, integer, integer, ctypes.POINTER(integer)]
        self.lib.opus_encoder_create.restype = ptr
        self.lib.opus_encoder_destroy.argtypes = [ptr]
        self.lib.opus_encoder_ctl.argtypes = [ptr, integer, integer]
        self.lib.opus_encode_float.argtypes = [ptr, floating, integer, ptr, integer]
        self.lib.opus_encode_float.restype = integer
        self.lib.opus_decoder_create.argtypes = [integer, integer, ctypes.POINTER(integer)]
        self.lib.opus_decoder_create.restype = ptr
        self.lib.opus_decoder_destroy.argtypes = [ptr]
        self.lib.opus_decode_float.argtypes = [ptr, ptr, integer, floating, integer, integer]
        self.lib.opus_decode_float.restype = integer

    def encode_tone(self, frames=50):
        error = ctypes.c_int()
        encoder = self.lib.opus_encoder_create(48000, 1, 2048, ctypes.byref(error))
        assert error.value == 0
        try:
            assert self.lib.opus_encoder_ctl(encoder, 4002, 24000) == 0
            assert self.lib.opus_encoder_ctl(encoder, 4006, 0) == 0
            packets = []
            for frame in range(frames):
                pcm = (ctypes.c_float * 960)(*tone(frame * 960, 960))
                output = ctypes.create_string_buffer(1275)
                count = self.lib.opus_encode_float(encoder, pcm, 960, output, 1275)
                assert count > 0
                packets.append(list(output.raw[:count]))
            return packets
        finally:
            self.lib.opus_encoder_destroy(encoder)

    def decode(self, packets):
        error = ctypes.c_int()
        decoder = self.lib.opus_decoder_create(48000, 1, ctypes.byref(error))
        assert error.value == 0
        try:
            samples = []
            for packet in packets:
                packet = bytes(packet)
                output = (ctypes.c_float * 5760)()
                count = self.lib.opus_decode_float(decoder, packet, len(packet), output, 5760, 0)
                assert count > 0, count
                samples.extend(output[:count])
            return samples
        finally:
            self.lib.opus_decoder_destroy(decoder)


def tone(offset, count):
    return [0.15 * math.sin(2 * math.pi * 440 * (offset + i) / 48000) for i in range(count)]


def rms(samples):
    assert samples and all(math.isfinite(value) for value in samples)
    return math.sqrt(sum(value * value for value in samples) / len(samples))


def check_codec(binaries, reference):
    encoded = exchange(Path(binaries) / "nuschel-media-worker", [
        {"kind": "encode_opus", "samples": tone(frame * 960, 960)} for frame in range(30)
    ])
    assert all(packet["status"] == "packet" for packet in encoded)
    assert 0.02 < rms(reference.decode([packet["value"] for packet in encoded])) < 0.4
    decoded = exchange(Path(binaries) / "nuschel-media-worker", [
        {"kind": "decode_opus", "packet": packet} for packet in reference.encode_tone(30)
    ])
    assert all(packet["status"] == "samples" for packet in decoded)
    assert 0.02 < rms([sample for packet in decoded for sample in packet["value"]]) < 0.4


def check_session(binaries, host, port, reference):
    with tempfile.TemporaryDirectory(prefix="nuschel-mumble-client-") as temporary:
        root = Path(temporary)
        pin = certificate_pin(binaries, host, port, root / "probe")
        alice = bob = None
        try:
            alice = Client(binaries, host, port, pin, "NuschelAlice", root / "alice")
            bob = Client(binaries, host, port, pin, "NuschelBob", root / "bob")
            alice.send(kind="create_channel", parent=0, name="Permission test")
            alice.wait(lambda event: event["kind"] == "permission_denied")
            channel = 2
            assert alice.channels[channel]["parent"] == 1 and alice.channels[1]["parent"] == 0
            alice.send(kind="join", channel=channel)
            alice.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == alice.session
                       and event["user"]["channel"] == channel)
            bob.send(kind="join", channel=channel)
            bob.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == bob.session
                     and event["user"]["channel"] == channel)
            body = "First line\n\n```rust\nfn main() {}\n```\n<literal>&"
            alice.send(kind="text", channel=channel, body=body)
            incoming = bob.wait(lambda event: event["kind"] == "text" and event["author"] == "NuschelAlice")
            assert incoming["html"], "stock servers allow HTML"
            prepared, = exchange(Path(binaries) / "nuschel-content-worker", [{"kind": "legacy_html", "html": incoming["body"]}])
            assert prepared["status"] == "prepared"
            assert prepared["value"]["source"] == body
            assert any(block["kind"] == "code" for block in prepared["value"]["document"]["blocks"])
            packets = reference.encode_tone(6)
            alice.unmute()
            alice.send(kind="voice", target=31, sequence=0, opus=packets[0])
            echoed = alice.wait(lambda event: event["kind"] == "audio")
            assert echoed["sender"] == alice.session and echoed["opus"] == packets[0]
            for index, packet in enumerate(packets):
                alice.send(kind="voice", sequence=2 + index * 2, opus=packet, end=index == len(packets) - 1)
                time.sleep(0.02)
            relayed = bob.wait(lambda event: event["kind"] == "audio")
            assert relayed["sender"] == alice.session and relayed["opus"] in packets
            print("Stock Mumble: certificate checks, login, channel tree/join, permission denial, multiline chat and Opus tunneling passed.")
        finally:
            if alice:
                alice.close()
            if bob:
                bob.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    parser.add_argument("--opus-library", type=Path, required=True)
    args = parser.parse_args()
    reference = ReferenceOpus(args.opus_library)
    check_codec(args.binaries, reference)
    check_session(args.binaries, args.host, args.port, reference)
