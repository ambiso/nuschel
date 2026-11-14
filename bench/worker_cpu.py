#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Sample the CPU time of the named executables' processes from /proc until
SIGTERM, then print JSON per name: CPU seconds (user + system) and seconds
alive while sampled. Processes already running count from the first sample."""
import json
import os
import signal
import sys
import time

TICK = os.sysconf("SC_CLK_TCK")


def uptime():
    with open("/proc/uptime") as file:
        return float(file.read().split()[0])


def name_of(pid, commands, parents, names):
    """A sandboxed worker runs as /worker below the bubblewrap that binds its executable there."""
    arguments = commands.get(pid, [])[:1]
    if arguments == ["/worker"]:
        arguments = commands.get(parents.get(pid), [])
    return next((name for name in map(os.path.basename, arguments) if name in names), None)


def sample(names, start, processes):
    now = uptime()
    commands, parents, stats = {}, {}, {}
    for pid in filter(str.isdigit, os.listdir("/proc")):
        try:
            with open(f"/proc/{pid}/cmdline", "rb") as file:
                commands[pid] = file.read().decode(errors="replace").split("\0")
            with open(f"/proc/{pid}/stat") as file:
                stats[pid] = file.read().rsplit(")", 1)[1].split()
        except OSError:
            continue
        parents[pid] = stats[pid][1]
    for pid, fields in stats.items():
        if name := name_of(pid, commands, parents, names):
            cpu = (int(fields[11]) + int(fields[12])) / TICK
            began = int(fields[19]) / TICK
            first = processes.setdefault((pid, began), [name, cpu if began < start else 0., max(began, start)])
            first[3:] = [cpu, now]


def main():
    names = set(sys.argv[1:])
    start = uptime()
    processes = {}
    signal.signal(signal.SIGTERM, lambda *_: sys.exit())
    try:
        while True:
            sample(names, start, processes)
            time.sleep(0.25)
    finally:
        totals = {}
        for name, cpu0, since, cpu, last in (p for p in processes.values() if len(p) == 5):
            total = totals.setdefault(name, {"cpu_s": 0., "alive_s": 0., "processes": 0})
            total["cpu_s"] += cpu - cpu0
            total["alive_s"] += last - since
            total["processes"] += 1
        print(json.dumps(totals), flush=True)


if __name__ == "__main__":
    main()
