# SPDX-License-Identifier: EUPL-1.2
"""The one-core suite's NixOS test driver part: each test `runs` times beside
a busy loop on the guest's only core. Per test: the pass rate, the workers' CPU
seconds per second alive (bench/worker_cpu.py) and, from the media worker's
`audio-stats` lines (apps/media-worker/src/stats.rs), capture gaps and playback
underruns per minute and capture-to-send latency percentiles. Writes
one-core.json and prints it beside the baseline; numbers, not a gate."""
import json
import re

STATS = re.compile(r"audio-stats pid=(\d+) capture_gaps=(\d+) underruns=(\d+) send_ms=(\S*)")
JITTER = re.compile(r"Jitter buffer: (\d+) ms of audio, \d+ packets dropped, (\d+) silent 20 ms windows")


def percentile(histogram, fraction):
    total = sum(histogram.values())
    seen = 0
    for ms in sorted(histogram):
        seen += histogram[ms]
        if seen >= fraction * total:
            return ms
    return None


def measure(machine, command, sampler):
    machine.succeed(f"systemd-run --unit=worker-cpu --property=StandardOutput=truncate:/tmp/worker-cpu.json {sampler}")
    status, output = machine.execute(f"{command} 2>&1", timeout=3600)
    machine.succeed("systemctl stop worker-cpu")
    cpu = json.loads(machine.succeed("cat /tmp/worker-cpu.json") or "{}")
    last = {match[0]: match for match in STATS.findall(output)}
    return status == 0, cpu, list(last.values()), JITTER.findall(output), output


def summarize(attempts):
    cpu, gaps, underruns, histogram, played, silent = {}, 0, 0, {}, 0, 0
    for _, workers, stats, jitter, _ in attempts:
        for name, total in workers.items():
            total_ = cpu.setdefault(name, {"cpu_s": 0., "alive_s": 0.})
            total_["cpu_s"] += total["cpu_s"]
            total_["alive_s"] += total["alive_s"]
        for _, capture_gaps, output_underruns, send_ms in stats:
            gaps += int(capture_gaps)
            underruns += int(output_underruns)
            for entry in filter(None, send_ms.split(",")):
                ms, count = map(int, entry.split(":"))
                histogram[ms] = histogram.get(ms, 0) + count
        for audio_ms, windows in jitter:
            played += int(audio_ms)
            silent += int(windows)
    media_minutes = cpu.get("nuschl-media-worker", {}).get("alive_s", 0.) / 60
    result = {
        "passed": sum(attempt[0] for attempt in attempts),
        "runs": len(attempts),
        "cpu_per_second": {name: round(t["cpu_s"] / t["alive_s"], 4) for name, t in cpu.items() if t["alive_s"]},
    }
    if stats_seen := any(attempt[2] for attempt in attempts):
        result |= {
            "capture_gaps_per_minute": round(gaps / media_minutes, 2) if media_minutes else None,
            "underruns_per_minute": round(underruns / media_minutes, 2) if media_minutes else None,
            "send_packets": sum(histogram.values()),
            "send_ms": {f"p{p}": percentile(histogram, p / 100) for p in (50, 95, 99)},
        }
    if played:
        result["jitter_silent_windows_per_minute"] = round(silent / (played / 60000), 2)
    return result | {"audio_stats": stats_seen}


def run(machine, tests, runs, sampler, out_dir, baseline):
    machine.succeed("systemd-run --unit=busy-loop sh -c 'while :; do :; done'")
    results = {}
    for name, command in tests.items():
        attempts = [measure(machine, command, sampler) for _ in range(runs)]
        for number, attempt in enumerate(attempts):
            (out_dir / f"{name}-{number}.log").write_text(attempt[4])
        results[name] = summarize(attempts)
        print(f"one-core {name}: {json.dumps(results[name])}")
        print(f"  baseline: {json.dumps(baseline.get(name))}")
    (out_dir / "one-core.json").write_text(json.dumps(results, indent=2) + "\n")
