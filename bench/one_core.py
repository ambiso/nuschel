# SPDX-License-Identifier: EUPL-1.2
"""The one-core suite's NixOS test driver part: each test `runs` times beside
a busy loop on the guest's only core. Per test: the pass rate, the workers' CPU
seconds per second alive (bench/worker_cpu.py) and, from the media worker's
`audio-stats` lines (apps/media-worker/src/stats.rs), capture gaps and playback
underruns, late packets and capture-to-send latency percentiles (nearest rank
over 1 ms bins: "p50 29" is 29-30 ms). Writes one-core.json and prints the
numbers beside the baseline."""
import datetime
import json
import re

STATS = re.compile(r"audio-stats pid=(\d+) capture_gaps=(\d+) underruns=(\d+) late_packets=(\d+) send_ms=(\S*)")
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
    machine.succeed(f"systemd-run --collect --unit=worker-cpu --property=StandardOutput=truncate:/tmp/worker-cpu.json {sampler}")
    status, output = machine.execute(f"{command} 2>&1", timeout=datetime.timedelta(hours=1))
    machine.succeed("systemctl stop worker-cpu")
    cpu = json.loads(machine.succeed("cat /tmp/worker-cpu.json") or "{}")
    last = {match[0]: match for match in STATS.findall(output)}
    return status == 0, cpu, list(last.values()), JITTER.findall(output), output


def summarize(attempts):
    cpu, gaps, underruns, late, histogram, played, silent = {}, 0, 0, 0, {}, 0, 0
    for _, workers, stats, jitter, _ in attempts:
        for name, total in workers.items():
            for key in ("cpu_s", "alive_s"):
                cpu.setdefault(name, {"cpu_s": 0., "alive_s": 0.})[key] += total[key]
        for _, capture_gaps, output_underruns, late_packets, send_ms in stats:
            gaps += int(capture_gaps)
            underruns += int(output_underruns)
            late += int(late_packets)
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
    if any(attempt[2] for attempt in attempts):
        result |= {
            "capture_gaps_per_minute": round(gaps / media_minutes, 2) if media_minutes else None,
            "underruns_per_minute": round(underruns / media_minutes, 2) if media_minutes else None,
            "send_packets": sum(histogram.values()),
            "late_packets": late,
            "send_ms": {f"p{p}": percentile(histogram, p / 100) for p in (50, 95, 99)},
        }
    if played:
        result["jitter_silent_windows_per_minute"] = round(silent / (played / 60000), 2)
    return result


def run(machine, tests, runs, slice_, sampler, out_dir, baseline):
    """The busy loop runs in the tested processes' slice, at the default CPU weight of its siblings."""
    machine.succeed(f"systemd-run --slice={slice_} --unit=busy-loop sh -c 'while :; do :; done'")
    results = {}
    for name, command in tests.items():
        attempts = [measure(machine, command, sampler) for _ in range(runs)]
        for number, attempt in enumerate(attempts):
            (out_dir / f"{name}-{number}.log").write_text(attempt[4])
        results[name] = summarize(attempts)
        print(f"one-core {name}: {json.dumps(results[name])}")
        print(f"  baseline: {json.dumps(baseline.get(name))}")
    (out_dir / "one-core.json").write_text(json.dumps(results, indent=2) + "\n")
