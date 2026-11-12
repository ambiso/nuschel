#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Matched receive-level comparison of two media-worker binaries.

Both workers get the same frozen Opus packets (hashed before any run), the same
private PulseAudio sink (rate and channel layout), the same fixed gains and the
same capture window, and are interleaved trial by trial. This is a comparative
collector: it records every case and never gates, so a historical worker that
fails a modern quality assertion is still measured. It reuses the receive path
of tests/output_quality.py unchanged. No host devices, servers or profiles.

Scope: the media worker only (decoder, jitter, mixer, playback conversion, device
stream), not the client's gain composition, not a physical device, no
perceptual claim.
"""
import argparse
import hashlib
import json
import math
import tempfile
from pathlib import Path

import output_quality as quality

SINKS = [(48000, 2), (44100, 2), (96000, 2), (48000, 1)]
GAINS = [1.0, 0.25]
ONSET = 0.01  # the fixture is silent for 200 ms, then ~0.18 full scale


def freeze(reference, directory):
    """Encode each source once; later runs reuse the same files and hashes."""
    directory.mkdir(parents=True, exist_ok=True)
    sets = {}
    for name, channels in (("mono", 1), ("stereo", 2)):
        path = directory / f"{name}.json"
        if not path.exists():
            packets = reference.encode(quality.fixture(channels), channels, bitrate=128000)
            path.write_text(json.dumps({"channels": channels, "bitrate": 128000,
                                        "packets": [list(packet) for packet in packets]}) + "\n")
        raw = path.read_bytes()
        data = json.loads(raw)
        sets[name] = {"channels": data["channels"],
                      "packets": [bytes(packet) for packet in data["packets"]],
                      "sha256": hashlib.sha256(raw).hexdigest()}
    return sets


def levels(samples, channels, rate):
    """Per-channel RMS and peak over a fixed 1.5 s window 100 ms after onset."""
    planes = [samples[channel::channels] for channel in range(channels)]
    onset = next((index for index in range(len(planes[0]))
                  if any(abs(plane[index]) > ONSET for plane in planes)), None)
    if onset is None:
        return {"onset": None, "rms": [0.0] * channels, "peak": [0.0] * channels}
    begin = onset + rate // 10
    end = begin + rate * 3 // 2
    if end > len(planes[0]):
        return {"onset": onset, "rms": None, "peak": None, "error": "capture shorter than window"}
    window = [plane[begin:end] for plane in planes]
    return {"onset": onset,
            "rms": [math.sqrt(sum(v * v for v in plane) / len(plane)) for plane in window],
            "peak": [max(map(abs, plane)) for plane in window]}


def db(ratio):
    return 20 * math.log10(ratio) if ratio > 0 else float("-inf")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", action="append", nargs=2, metavar=("LABEL", "PATH"), required=True,
                        help="twice: the baseline first, then the candidate")
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--trials", type=int, default=2)
    args = parser.parse_args()
    if len(args.worker) != 2:
        parser.error("give exactly two --worker LABEL PATH pairs")
    args.output.mkdir(parents=True, exist_ok=True)
    workers = [(label, Path(path)) for label, path in args.worker]
    sets = freeze(quality.Reference(args.opus_library), args.output / "frozen")
    report = {"workers": [{"label": label, "path": str(path),
                           "sha256": hashlib.sha256(path.read_bytes()).hexdigest()} for label, path in workers],
              "packets": {name: {"channels": s["channels"], "count": len(s["packets"]), "sha256": s["sha256"]}
                          for name, s in sets.items()},
              "window": "1.5 s starting 100 ms after the first sample above 0.01",
              "scope": "media worker only; private Pulse null sinks; synthetic tone fixture; no device, perceptual "
                       "or client-gain claim",
              "trials": []}
    for rate, sink_channels in SINKS:
        with tempfile.TemporaryDirectory(prefix=f"pulse-{rate}-{sink_channels}-", dir=args.output) as temporary:
            pulse = quality.PrivatePulse(args.pulse_bin, Path(temporary), rate, channels=sink_channels)
            try:
                for trial in range(args.trials):
                    for source, packets in sets.items():
                        for gain in GAINS:
                            for label, path in workers:
                                specification = []
                                capture = args.output / f"{label}-{rate}-{sink_channels}-{source}-{gain}-{trial}.f32le"
                                samples = quality.playback(path, pulse, packets["packets"], capture, gain,
                                                           monitor_rate=rate, specifications=specification,
                                                           monitor_channels=sink_channels)
                                report["trials"].append({"worker": label, "sink_rate": rate,
                                                         "sink_channels": sink_channels, "source": source,
                                                         "gain": gain, "trial": trial,
                                                         "stream_specification": specification,
                                                         **levels(samples, sink_channels, rate)})
                                capture.unlink()
            finally:
                pulse.stop()
    baseline, candidate = workers[0][0], workers[1][0]
    summary = []
    for rate, sink_channels in SINKS:
        for source in sets:
            for gain in GAINS:
                rows = [t for t in report["trials"] if (t["sink_rate"], t["sink_channels"], t["source"], t["gain"])
                        == (rate, sink_channels, source, gain) and t.get("rms")]
                for channel in range(sink_channels):
                    a = [t["rms"][channel] for t in rows if t["worker"] == baseline]
                    b = [t["rms"][channel] for t in rows if t["worker"] == candidate]
                    if not a or not b:
                        continue
                    summary.append({"sink_rate": rate, "sink_channels": sink_channels, "source": source,
                                    "gain": gain, "channel": channel,
                                    f"{baseline}_rms": a, f"{candidate}_rms": b,
                                    "candidate_vs_baseline_db": db((sum(b) / len(b)) / (sum(a) / len(a))),
                                    "trial_spread_db": max(db(max(a) / min(a)) if min(a) else float("inf"),
                                                           db(max(b) / min(b)) if min(b) else float("inf"))})
    report["summary"] = summary
    (args.output / "proof.json").write_text(json.dumps(report, indent=2) + "\n")
    for row in summary:
        print(f"{row['sink_rate']:>6} Hz {row['sink_channels']}ch {row['source']:6} gain {row['gain']:<4} "
              f"ch{row['channel']}: {row['candidate_vs_baseline_db']:+7.3f} dB (spread {row['trial_spread_db']:.3f} dB)",
              flush=True)


if __name__ == "__main__":
    main()
