#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Open noise regression: fresh synthetic source, first PCM and old/new workers.

The zero-packet assertion intentionally remains strict. Current and baseline
workers fail it for this source; do not interpret latency improvements as a
noise-classification fix. No hardware modules or host audio are used.
"""

import argparse
import array
import hashlib
import json
import math
import queue
import random
import subprocess
import threading
import time
from pathlib import Path

from media_live import Pulse
from mumble_client import ReferenceOpus
from unmute_latency import Worker


def first_pcm(pulse, path):
    recorder = subprocess.Popen(
        [
            str(pulse.bin / "parec"),
            f"--server=unix:{pulse.socket}",
            "--device=mic.monitor",
            "--raw",
            "--format=float32le",
            "--rate=48000",
            "--channels=1",
            "--latency-msec=10",
            "--process-time-msec=10",
        ],
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    result = queue.Queue()
    started = time.monotonic()

    def read():
        first = recorder.stdout.read(480 * 4)
        first_ms = (time.monotonic() - started) * 1000
        result.put((first + recorder.stdout.read((2 * 48000 - 480) * 4), first_ms))

    thread = threading.Thread(target=read, daemon=True)
    thread.start()
    try:
        data, first_ms = result.get(timeout=5)
        assert len(data) == 2 * 48000 * 4
        path.write_bytes(data)
        pcm = array.array("f")
        pcm.frombytes(data)
        rms = math.sqrt(sum(v * v for v in pcm) / len(pcm))
        assert 0.001 < rms < 0.011 and max(abs(v) for v in pcm) <= 0.0101, (
            "raw source differs from the synthetic noise marker"
        )
        return {
            "samples": len(pcm),
            "first_callback_samples": 480,
            "first_independent_pcm_ms": first_ms,
            "rms": rms,
            "max": max(abs(v) for v in pcm),
            "sha256": hashlib.sha256(data).hexdigest(),
            "path": str(path),
            "source": "explicit private mic.monitor; first2s beginning with the first10ms from independent libpulse recorder",
        }
    finally:
        recorder.kill()
        recorder.wait(timeout=5)
        thread.join(timeout=2)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("worker", "baseline-worker", "pulse-bin", "opus-library", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True, mode=0o700)
    reference = ReferenceOpus(args.opus_library)
    rng = random.Random(7)
    data = array.array("f", (rng.uniform(-0.01, 0.01) for _ in range(48000))).tobytes()
    report = {
        "synthetic_only": True,
        "host_devices": False,
        "noise_seed": 7,
        "noise_amplitude": 0.01,
        "source_marker_sha256": hashlib.sha256(data).hexdigest(),
        "requirement": "zero transmitted packets for steady noise",
        "producer": "pacat --latency-msec=20 --process-time-msec=10, float32le48k mono",
        "consumer": "parec --latency-msec=10 --process-time-msec=10, float32le48k mono",
    }
    for label, binary in (("baseline", args.baseline_worker), ("current", args.worker)):
        root = args.output / label
        root.mkdir(exist_ok=True)
        pulse = Pulse(args.pulse_bin, root)
        feeder = None
        worker = None
        try:
            feeder = subprocess.Popen(
                [
                    str(pulse.bin / "pacat"),
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

            def feed(feeder=feeder):
                try:
                    while feeder.poll() is None:
                        feeder.stdin.write(data)
                        feeder.stdin.flush()
                except (BrokenPipeError, ValueError):
                    pass

            thread = threading.Thread(target=feed, daemon=True)
            thread.start()
            # There has NEVER been any speech on this private source.
            raw = first_pcm(pulse, root / "first-callback.f32")
            worker = Worker(binary, pulse, root / "worker.stderr")
            worker.send(
                {
                    "kind": "configure",
                    "bitrate": 128000,
                    "packet_loss_percent": 5,
                    "settings": {},
                }
            )
            worker.send(
                {"kind": "set_state", "epoch": 1, "muted": False, "deafened": False}
            )
            worker.wait(
                lambda worker=worker: "first_processed_pcm" in worker.log.read_text()
            )
            time.sleep(2)
            packets = [
                e["opus"]
                for _, e in worker.events
                if e["kind"] == "packet" and not e["end"]
            ]
            pcm = reference.decode(packets) if packets else []
            report[label] = {
                "worker": str(binary),
                "first_pcm": raw,
                "packets": len(packets),
                "decoded_rms": math.sqrt(sum(v * v for v in pcm) / len(pcm))
                if pcm
                else 0,
                "stages": str(worker.log),
            }
        finally:
            if worker:
                worker.stop()
            if feeder:
                feeder.terminate()
                feeder.wait(timeout=5)
                thread.join(timeout=2)
                try:
                    feeder.stdin.close()
                except BrokenPipeError:
                    pass
            pulse.stop()
    report["passed"] = report["current"]["packets"] == 0
    (args.output / "proof.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report), flush=True)
    assert report["current"]["packets"] == 0, "steady synthetic noise opened the gate"


if __name__ == "__main__":
    main()
