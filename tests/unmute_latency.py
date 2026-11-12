#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Time real processed Opus on an owned virtual PulseAudio, never host devices.

The worker is forced to the explicit private Pulse server; its daemon loads only
virtual/null devices. Independent libopus checks emitted synthetic PCM. Optional
baseline comparison uses the same fixture; it does not claim GUI-click timing.
"""

import argparse
import array
import json
import math
import os
import re
import struct
import subprocess
import threading
import time
from itertools import pairwise
from pathlib import Path

from local_murmur import Murmur, free_port
from media_live import Pulse
from mumble_client import Client, ReferenceOpus, certificate_pin


class Worker:
    def __init__(self, binary, pulse, log, peer=None, environment=None):
        self.start = time.monotonic()
        self.events = []
        self.log = log
        self.peer = peer
        self.first_wire_sequence = None
        self.stderr = log.open("wb")
        self.process = subprocess.Popen(
            [str(binary), "--live"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr,
            env={
                "PULSE_SERVER": f"unix:{pulse.socket}",
                "XDG_RUNTIME_DIR": str(pulse.root),
                "NUSCHL_AUDIO_BACKEND": "pulse",
                "NUSCHL_AUDIO_TIMING": "1",
                "PATH": os.environ.get("PATH", ""),
                **(environment or {}),
            },
        )
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()

    def read(self):
        while True:
            header = self.process.stdout.read(4)
            if len(header) != 4:
                return
            (length,) = struct.unpack(">I", header)
            assert length <= 1024 * 1024
            event = json.loads(self.process.stdout.read(length))["payload"]
            self.events.append((time.monotonic() - self.start, event))
            if self.peer is not None and event["kind"] == "packet":
                if self.first_wire_sequence is None and not event["end"]:
                    self.first_wire_sequence = self.peer.wire_sequence
                self.peer.send(
                    kind="voice",
                    target=31,
                    sequence=self.peer.wire_sequence,
                    opus=event["opus"],
                    end=event["end"],
                )
                self.peer.wire_sequence += 2

    def send(self, payload):
        data = json.dumps({"version": 1, "request_id": 0, "payload": payload}).encode()
        self.process.stdin.write(struct.pack(">I", len(data)) + data)
        self.process.stdin.flush()

    def wait(self, predicate, timeout=8):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            result = predicate()
            if result:
                return result
            time.sleep(0.005)
        raise AssertionError(
            f"Missing event; stages: {self.log.read_text()}; events: {[e['kind'] for _, e in self.events][-8:]}"
        )

    def stop(self):
        if self.process.poll() is None:
            self.process.kill()
        self.process.wait(timeout=5)
        self.reader.join(timeout=2)
        self.stderr.close()


def marker_proof(path, reference):
    fixture = json.loads(path.read_text())
    samples = reference.decode(fixture["packets"])
    assert len(samples) >= 10 * 960 and all(math.isfinite(v) for v in samples)

    def power(values, frequency):
        real = sum(
            v * math.cos(2 * math.pi * frequency * i / 48000)
            for i, v in enumerate(values)
        )
        imag = sum(
            v * math.sin(2 * math.pi * frequency * i / 48000)
            for i, v in enumerate(values)
        )
        return real * real + imag * imag

    # Skip only the codec's first 5 ms; the initial marker must be present in
    # the FIRST packet, followed by the later authorized source frequency.
    beginning = samples[240:960]
    following = samples[6 * 960 : 9 * 960]
    assert power(beginning, 997) > power(beginning, 3000) * 20, (
        "the initial prefix marker was lost"
    )
    assert power(following, 3000) > power(following, 997) * 20, (
        "the following speech marker was lost"
    )
    return {
        "passed": True,
        "samples": len(samples),
        "initial_hz": 997,
        "following_hz": 3000,
        "source": str(path),
        "decoder": "independent libopus",
    }


class TimedPeer(Client):
    def __init__(self, *args, **kwargs):
        self.audio_times = []
        self.wire_sequence = 0
        super().__init__(*args, **kwargs)

    def _read(self):
        try:
            for line in self.process.stdout:
                event = json.loads(line)
                self.events.append(event)
                if event["kind"] == "audio":
                    self.audio_times.append((event["sequence"], time.monotonic()))
                self.queue.put(event)
        finally:
            self.queue.put(None)


def measure(binary, pulse, root, reference, current, peer=None):
    results = []
    for mode in ("continuous", "push_to_talk", "voice_activity"):
        for trial in range(2):
            worker = Worker(binary, pulse, root / f"{mode}-{trial}.stderr", peer)
            try:
                worker.send(
                    {
                        "kind": "configure",
                        "bitrate": 128000,
                        "packet_loss_percent": 5,
                        "settings": {"transmit": mode},
                    }
                )
                worker.send({"kind": "push_to_talk", "active": True})
                worker.send(
                    {"kind": "set_state", "epoch": 1, "muted": False, "deafened": False}
                )
                packets = lambda worker=worker: [
                    (t, e)
                    for t, e in worker.events
                    if e["kind"] == "packet" and not e["end"]
                ]
                worker.wait(lambda packets=packets: len(packets()) >= 6)
                emitted = packets()
                network_ms = None
                if peer:
                    worker.wait(
                        lambda worker=worker: any(
                            sequence == worker.first_wire_sequence
                            for sequence, _ in peer.audio_times
                        )
                    )
                    network_ms = (
                        next(
                            at
                            for sequence, at in peer.audio_times
                            if sequence == worker.first_wire_sequence
                        )
                        - worker.start
                    ) * 1000
                pcm = reference.decode([e["opus"] for _, e in emitted[:6]])
                rms = math.sqrt(sum(v * v for v in pcm) / len(pcm))
                assert len(pcm) == 6 * 960
                levels = [
                    math.sqrt(sum(v * v for v in pcm[i * 960 : (i + 1) * 960]) / 960)
                    for i in range(6)
                ]
                usable = next(
                    (i for i, level in enumerate(levels) if level > 0.001), None
                )
                assert usable is not None, "no usable PCM in the first six packets"
                usable_network_ms = None
                if peer:
                    sequence = worker.first_wire_sequence + 2 * usable
                    worker.wait(
                        lambda sequence=sequence: any(
                            seq == sequence for seq, _ in peer.audio_times
                        )
                    )
                    usable_network_ms = (
                        next(at for seq, at in peer.audio_times if seq == sequence)
                        - worker.start
                    ) * 1000
                assert rms > 0.001 and all(math.isfinite(v) for v in pcm), (
                    "first encoded PCM was not usable"
                )
                stages = {
                    name: int(us) / 1000
                    for name, us in re.findall(
                        r"stage=(\w+) elapsed_us=(\d+)", worker.log.read_text()
                    )
                }
                # Warm sources prove actual packets, not an optimistic status.
                if (
                    current
                    and mode in ("continuous", "push_to_talk")
                    and (mode != "continuous" or trial)
                ):
                    assert emitted[0][0] < 0.09, (mode, emitted[0][0], stages)
                if current:
                    # Newly approved speech may be prefixed but never flooded.
                    intervals = [(b[0] - a[0]) * 1000 for a, b in pairwise(emitted)]
                    assert min(intervals) > 8, intervals
                    worker.send(
                        {
                            "kind": "set_state",
                            "epoch": 2,
                            "muted": True,
                            "deafened": False,
                        }
                    )
                    worker.wait(lambda worker=worker: worker.process.poll() is not None)
                    endings = [
                        e
                        for _, e in worker.events
                        if e["kind"] == "packet" and e["epoch"] == 2
                    ]
                    assert endings and all(
                        e["end"] and e["opus"] == [248, 255, 254] for e in endings
                    ), endings
                results.append(
                    {
                        "mode": mode,
                        "trial": trial,
                        "first_packet_ms": emitted[0][0] * 1000,
                        "first_private_murmur_echo_ms": network_ms,
                        "first_usable_packet_ms": emitted[usable][0] * 1000,
                        "first_usable_private_murmur_echo_ms": usable_network_ms,
                        "first_six_packet_rms": levels,
                        "rms": rms,
                        "stages_ms": stages,
                    }
                )
            finally:
                worker.stop()
            sources = {
                s["index"]: s["name"]
                for s in pulse.ctl("list", "sources", json_output=True)
            }
            assert not any(
                sources.get(s["source"]) == "mic.monitor"
                and s.get("properties", {}).get("application.name") == "Nuschl"
                for s in pulse.ctl("list", "source-outputs", json_output=True)
            ), "microphone retained after mute"
    # Listening while muted must not open a microphone at all.
    worker = Worker(binary, pulse, root / "muted.stderr")
    try:
        worker.send(
            {
                "kind": "configure",
                "bitrate": 128000,
                "packet_loss_percent": 5,
                "settings": {},
            }
        )
        worker.send({"kind": "set_state", "epoch": 3, "muted": True, "deafened": False})
        worker.wait(
            lambda: any(e["kind"] == "status" and e["ready"] for _, e in worker.events)
        )
        time.sleep(0.1)
        assert not any(e["kind"] == "packet" for _, e in worker.events)
        assert "first_capture_callback" not in worker.log.read_text()
    finally:
        worker.stop()
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--baseline-worker", type=Path)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--marker-fixture", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--murmur", type=Path)
    parser.add_argument(
        "--binaries",
        type=Path,
        help="Cached CLI/helpers for the optional owned Murmur wire path",
    )
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True, mode=0o700)
    reference = ReferenceOpus(args.opus_library)
    report = {"synthetic_only": True, "host_audio_devices": False, "GUI_timing": False}
    if args.marker_fixture:
        report["prefix_marker"] = marker_proof(args.marker_fixture, reference)
    peer = None
    murmur = None
    try:
        if args.murmur is not None:
            assert args.binaries is not None, "--murmur requires --binaries"
            server = args.output / "murmur"
            server.mkdir(exist_ok=True)
            murmur = Murmur(args.murmur, server, free_port())
            # Exercise the requested128k codec target; lower-budget behavior has its
            # own SDK/worker tests. No server authentication/ACL policy is weakened.
            murmur.ini.write_text(
                murmur.ini.read_text().replace("bandwidth=72000", "bandwidth=200000")
            )
            murmur.seed()
            murmur.start()
            pin = certificate_pin(
                args.binaries, "127.0.0.1", murmur.port, server / "probe"
            )
            peer = TimedPeer(
                args.binaries,
                "127.0.0.1",
                murmur.port,
                pin,
                "LatencyFixture",
                server / "peer",
            )
            peer.unmute()
            report["private_murmur"] = {
                "binary": str(args.murmur),
                "bandwidth": 200000,
                "host": "127.0.0.1",
            }
        for label, binary in (
            ("baseline", args.baseline_worker),
            ("current", args.worker),
        ):
            if binary is None:
                continue
            root = args.output / label
            root.mkdir(exist_ok=True)
            pulse = Pulse(args.pulse_bin, root)
            feeder = subprocess.Popen(
                [
                    str(args.pulse_bin / "pacat"),
                    f"--server=unix:{pulse.socket}",
                    "--playback",
                    "--device=mic",
                    "--raw",
                    "--format=float32le",
                    "--rate=48000",
                    "--channels=1",
                    "--latency-msec=20",
                    "--process-time-msec=10",
                ],
                stdin=subprocess.PIPE,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
            samples = array.array(
                "f",
                (
                    0.12
                    * (0.5 + 0.5 * math.sin(2 * math.pi * 4 * i / 48000))
                    * sum(
                        math.sin(2 * math.pi * 140 * h * i / 48000) / h
                        for h in range(1, 13)
                    )
                    for i in range(48000)
                ),
            ).tobytes()

            source = {"samples": samples}

            def feed(feeder=feeder, source=source):
                try:
                    while feeder.poll() is None:
                        feeder.stdin.write(source["samples"])
                        feeder.stdin.flush()
                except (BrokenPipeError, ValueError):
                    pass

            thread = threading.Thread(target=feed, daemon=True)
            thread.start()
            try:
                report[label] = measure(
                    binary, pulse, root, reference, label == "current", peer
                )
            finally:
                feeder.terminate()
                feeder.wait(timeout=5)
                thread.join(timeout=2)
                try:
                    feeder.stdin.close()
                except BrokenPipeError:
                    pass
                pulse.stop()
    finally:
        if peer:
            peer.close()
        if murmur:
            murmur.stop()
    report["passed"] = True
    report["limits"] = (
        "Worker-command timings, not GUI clicks; virtual-source cold latency is also present with independent libpulse."
    )
    (args.output / "proof.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))


if __name__ == "__main__":
    main()
