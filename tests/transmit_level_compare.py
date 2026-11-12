#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Matched transmit-level comparison of two media-worker binaries.

Each worker captures the same frozen, voice-like source from a private PulseAudio
null microphone (at the capture rate under test), runs its real capture chain in
continuous transmit, and emits Opus packets, which reference libopus decodes.
Workers are interleaved trial by trial on the same private Pulse instance. Every
trial is recorded; nothing is gated. Settling/onset and steady-state windows are
reported separately because automatic gain control adapts over time.

Settings: "defaults" = product defaults with echo cancellation off (automatic
gain and noise suppression on); "unprocessed" = automatic gain, noise
suppression and echo cancellation off. Scope: media worker capture → encode
only; no device, client, network or perceptual claim.
"""
import argparse
import array
import hashlib
import json
import math
import subprocess
import tempfile
import threading
import time
from pathlib import Path

from capture_unmute import retarget_microphone
from media_live import Worker
from mumble_client import ReferenceOpus
from output_quality import PrivatePulse

RATES = [44100, 48000]
LEVELS = {"normal": 0.12, "quiet": 0.03}
SETTINGS = {
    "defaults": {"transmit": "continuous", "echo_cancellation": "off"},
    "unprocessed": {"transmit": "continuous", "echo_cancellation": "off",
                    "noise_suppression": False, "automatic_gain": False},
}
SECONDS = 6.0
FRAME = 960  # 20 ms at 48 kHz
HARMONICS = [140 * h for h in range(1, 13)]


def source(rate, level):
    """One second of the capture tests' voice-like signal: 140 Hz harmonics, 4 Hz envelope."""
    return array.array("f", (level * (.5 + .5 * math.sin(2 * math.pi * 4 * i / rate))
                             * sum(math.sin(2 * math.pi * 140 * h * i / rate) / h for h in range(1, 13))
                             for i in range(rate))).tobytes()


def goertzel_db(samples, frequency, rate=48000):
    coefficient = 2 * math.cos(2 * math.pi * frequency / rate)
    s1 = s2 = 0.0
    for value in samples:
        s1, s2 = value + coefficient * s1 - s2, s1
    power = s1 * s1 + s2 * s2 - coefficient * s1 * s2
    return 10 * math.log10(max(power / (len(samples) ** 2), 1e-30))


def rms(values):
    return math.sqrt(sum(v * v for v in values) / len(values)) if values else 0.0


def capture(binary, pulse, settings, seconds, log):
    worker = Worker(binary, pulse)
    try:
        worker.send({"kind": "configure", "bitrate": 128000, "packet_loss_percent": 0, "settings": settings})
        worker.send({"kind": "set_state", "epoch": 1, "muted": False, "deafened": False})
        deadline = time.monotonic() + seconds + 10
        while time.monotonic() < deadline:
            packets = [e for e in list(worker.events) if e.get("kind") == "packet" and not e.get("end")]
            if len(packets) >= int(seconds * 50):
                break
            time.sleep(0.05)
        events = list(worker.events)
    finally:
        worker.stop()
    log.write_text(json.dumps([e.get("kind") for e in events][-50:]) + "\n")
    return [bytes(e["opus"]) for e in events if e.get("kind") == "packet" and not e.get("end")]


def analyse(pcm):
    """Onset/settling = the first 1 s of transmitted audio; steady = 3–6 s."""
    frames = [pcm[i:i + FRAME] for i in range(0, len(pcm) - FRAME + 1, FRAME)]
    settling = [rms(frame) for frame in frames[:50]]
    steady = pcm[3 * 48000:6 * 48000]
    return {"packets_decoded": len(frames),
            "settling_rms_per_20ms": settling,
            "settling_rms": rms(pcm[:48000]),
            "steady_rms": rms(steady) if len(steady) == 3 * 48000 else None,
            "steady_peak": max(map(abs, steady)) if len(steady) == 3 * 48000 else None,
            "steady_harmonics_db": [goertzel_db(steady, f) for f in HARMONICS] if len(steady) == 3 * 48000 else None}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", action="append", nargs=2, metavar=("LABEL", "PATH"), required=True)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--trials", type=int, default=2)
    args = parser.parse_args()
    if len(args.worker) != 2:
        parser.error("give exactly two --worker LABEL PATH pairs")
    args.output.mkdir(parents=True, exist_ok=True)
    reference = ReferenceOpus(args.opus_library)
    workers = [(label, Path(path)) for label, path in args.worker]
    frozen = {}
    for rate in RATES:
        for level_name, level in LEVELS.items():
            data = source(rate, level)
            path = args.output / f"source-{rate}-{level_name}.f32le"
            path.write_bytes(data)
            frozen[(rate, level_name)] = data
    report = {"workers": [{"label": l, "path": str(p), "sha256": hashlib.sha256(p.read_bytes()).hexdigest()}
                          for l, p in workers],
              "sources": {f"{r}-{n}": hashlib.sha256(d).hexdigest() for (r, n), d in frozen.items()},
              "settings": SETTINGS, "seconds": SECONDS,
              "windows": {"settling": "first 1 s of decoded transmitted audio (also per 20 ms)",
                          "steady": "3–6 s of decoded transmitted audio"},
              "scope": "media worker capture→encode only; private Pulse null microphone; synthetic voice-like "
                       "source; reference libopus decode; no device, client, network or perceptual claim",
              "trials": []}
    for rate in RATES:
        with tempfile.TemporaryDirectory(prefix=f"pulse-tx-{rate}-", dir=args.output) as temporary:
            pulse = PrivatePulse(args.pulse_bin, Path(temporary), 48000)
            try:
                mic = retarget_microphone(pulse, rate)
                for level_name in LEVELS:
                    data = frozen[(rate, level_name)]
                    feeder = subprocess.Popen(
                        [str(args.pulse_bin / "pacat"), f"--server=unix:{pulse.socket}", "--playback", "--device=mic",
                         "--raw", "--format=float32le", f"--rate={rate}", "--channels=1", "--latency-msec=20",
                         "--process-time-msec=10"], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL)

                    def feed(feeder=feeder, data=data):
                        try:
                            while feeder.poll() is None:
                                feeder.stdin.write(data)
                                feeder.stdin.flush()
                        except (BrokenPipeError, ValueError):
                            pass
                    thread = threading.Thread(target=feed, daemon=True)
                    thread.start()
                    try:
                        time.sleep(0.5)
                        for trial in range(args.trials):
                            for settings_name, settings in SETTINGS.items():
                                for label, binary in workers:
                                    log = args.output / f"{label}-{rate}-{level_name}-{settings_name}-{trial}.events.json"
                                    packets = capture(binary, pulse, settings, SECONDS, log)
                                    pcm = reference.decode(packets) if packets else []
                                    report["trials"].append({"worker": label, "capture_rate": rate,
                                                             "mic_spec": mic["sample_specification"],
                                                             "source_level": level_name, "settings": settings_name,
                                                             "trial": trial, "packets": len(packets),
                                                             **analyse(pcm)})
                    finally:
                        feeder.terminate()
                        feeder.wait(timeout=5)
                        thread.join(timeout=2)
                        try:
                            feeder.stdin.close()
                        except BrokenPipeError:
                            pass
            finally:
                pulse.stop()
    baseline, candidate = workers[0][0], workers[1][0]
    summary = []
    for rate in RATES:
        for level_name in LEVELS:
            for settings_name in SETTINGS:
                rows = [t for t in report["trials"] if (t["capture_rate"], t["source_level"], t["settings"])
                        == (rate, level_name, settings_name)]
                row = {"capture_rate": rate, "source_level": level_name, "settings": settings_name}
                for window in ("settling_rms", "steady_rms", "steady_peak"):
                    a = [t[window] for t in rows if t["worker"] == baseline and t[window]]
                    b = [t[window] for t in rows if t["worker"] == candidate and t[window]]
                    row[f"{baseline}_{window}"] = a
                    row[f"{candidate}_{window}"] = b
                    if a and b:
                        row[f"{window}_candidate_vs_baseline_db"] = 20 * math.log10((sum(b) / len(b)) / (sum(a) / len(a)))
                ha = [t["steady_harmonics_db"] for t in rows if t["worker"] == baseline and t["steady_harmonics_db"]]
                hb = [t["steady_harmonics_db"] for t in rows if t["worker"] == candidate and t["steady_harmonics_db"]]
                if ha and hb:
                    row["harmonic_difference_db"] = [sum(x[i] for x in hb) / len(hb) - sum(x[i] for x in ha) / len(ha)
                                                     for i in range(len(HARMONICS))]
                summary.append(row)
    report["summary"] = summary
    (args.output / "proof.json").write_text(json.dumps(report, indent=2) + "\n")
    for row in summary:
        def show(key):
            value = row.get(key)
            return f"{value:+7.2f}" if value is not None else "    n/a"
        print(f"{row['capture_rate']:>6} Hz {row['source_level']:6} {row['settings']:11} "
              f"settling {show('settling_rms_candidate_vs_baseline_db')} dB  "
              f"steady {show('steady_rms_candidate_vs_baseline_db')} dB  "
              f"peak {show('steady_peak_candidate_vs_baseline_db')} dB", flush=True)


if __name__ == "__main__":
    main()
