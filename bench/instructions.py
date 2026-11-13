#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""User-space instructions per second of audio of the media worker's capture
chain and Opus encoder (`nuschl-media-worker --bench STAGE`, counted with
`perf stat -e instructions:u`), and the offline echo reduction. Writes
instructions.json and fails when the whole chain costs more than THRESHOLD
over the baseline.

Usage: instructions.py WORKER BASELINE OUTPUT"""
import json
import statistics
import subprocess
import sys
from pathlib import Path

# Repeated runs of one build differ by well under 0.1% (see "spread" in the
# result); 5% leaves room for compiler and toolchain noise, not for real work.
THRESHOLD = 0.05
REPEATS = 3


def count(worker, stage, output):
    report = output / f"{stage}.perf"
    run = subprocess.run(["perf", "stat", "-x,", "-e", "instructions:u", "-o", str(report), "--",
                          worker, "--bench", stage], check=True, capture_output=True, text=True)
    line = next(line for line in report.read_text().splitlines() if "instructions" in line)
    return int(line.split(",")[0]), run.stdout


def main():
    worker, baseline, output = sys.argv[1], json.loads(Path(sys.argv[2]).read_text()), Path(sys.argv[3])
    output.mkdir()
    counts, reports = {}, []
    for stage in ("input", "capture", "encode"):
        runs = [count(worker, stage, output) for _ in range(REPEATS)]
        counts[stage] = [instructions for instructions, _ in runs]
        reports += [json.loads(report) for _, report in runs if report]
    echo = {report["echo_reduction_db"] for report in reports}
    assert len(echo) == 1, f"the echo reduction differs between runs: {echo}"
    seconds = reports[0]["seconds"]
    median = {stage: statistics.median(values) for stage, values in counts.items()}
    per_second = {
        "capture_chain": round((median["capture"] - median["input"]) / seconds),
        "opus_encode": round((median["encode"] - median["capture"]) / seconds),
        "total": round((median["encode"] - median["input"]) / seconds),
    }
    previous = baseline.get("instructions", {}).get("per_audio_second", {})
    result = {
        "counter": "perf stat -e instructions:u",
        "repeats": REPEATS,
        "per_audio_second": per_second,
        "spread": max((max(v) - min(v)) / statistics.median(v) for v in counts.values()),
        "echo_reduction_db": round(echo.pop(), 2),
        "change": {name: round(value / previous[name] - 1, 4) for name, value in per_second.items() if name in previous},
    }
    print(json.dumps(result, indent=2))
    (output / "instructions.json").write_text(json.dumps(result, indent=2) + "\n")
    if result["change"].get("total", 0) > THRESHOLD:
        sys.exit(f"the chain costs {result['change']['total']:.1%} more instructions than the baseline")


if __name__ == "__main__":
    main()
