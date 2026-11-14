#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""User-space instructions per second of audio of the media worker's capture
chain and Opus encoder (`nuschl-media-worker --bench STAGE`, counted with
`perf stat -e instructions:u`), and the offline echo reduction. Writes
instructions.json and fails when a stage costs more than THRESHOLD over the
baseline, or when the baseline was measured on another CPU model. To refresh
the baseline, copy `cpu` and `per_audio_second` from the printed result.

Usage: instructions.py WORKER BASELINE OUTPUT"""
import json
import statistics
import subprocess
import sys
from pathlib import Path

# Repeated runs of one build differ by less than 0.0001% ("spread" in the
# result); 5% covers compiler and toolchain variation.
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
    cpu = next(line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
               if line.startswith("model name"))
    previous = baseline["instructions"]
    result = {
        "cpu": cpu,
        "counter": "perf stat -e instructions:u",
        "repeats": REPEATS,
        "per_audio_second": per_second,
        "spread": max((max(v) - min(v)) / statistics.median(v) for v in counts.values()),
        "echo_reduction_db": round(echo.pop(), 2),
        "change": {name: round(value / previous["per_audio_second"][name] - 1, 4)
                   for name, value in per_second.items()},
    }
    print(json.dumps(result, indent=2))
    (output / "instructions.json").write_text(json.dumps(result, indent=2) + "\n")
    if previous["cpu"] != cpu:
        sys.exit(f"the baseline was measured on {previous['cpu']}; this CPU is {cpu}")
    if worse := {name: change for name, change in result["change"].items() if change > THRESHOLD}:
        sys.exit(f"more instructions than the baseline: {worse}")


if __name__ == "__main__":
    main()
