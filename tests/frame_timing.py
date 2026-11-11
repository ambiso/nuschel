#!/usr/bin/env python3
"""Summarise frame timings: the desktop's NUSCHL_FRAME_TIMING log (a build with
`--features frame-timing`) and the compositor-side tick logs of
gnome_session.resize_sweep and drag_resize.

`python3 tests/frame_timing.py frames.log [start_us end_us]` prints the
distribution of each draw phase, in milliseconds, for the frames in that
window of the monotonic clock.
"""
import re
import sys


def read_log(path):
    """Events as (kind, start µs on the monotonic clock, values in µs)."""
    events, origin = [], 0
    for line in open(path):
        if line.startswith("origin"):
            seconds, nanos = map(int, re.findall(r"\d+", line))
            origin = seconds * 1_000_000 + nanos // 1000
            continue
        kind, start, *values = line.split()
        events.append((kind, origin + int(start), [int(value) for value in values]))
    return events


def percentiles(values):
    if not values:
        return {"n": 0}
    ordered = sorted(values)
    pick = lambda q: ordered[min(len(ordered) - 1, int(q * len(ordered)))]  # noqa: E731
    return {"n": len(ordered), "p50": pick(0.5) / 1000, "p95": pick(0.95) / 1000,
            "max": ordered[-1] / 1000}


def summarize(events, start=0, end=float("inf")):
    """Per-phase distributions (ms) of the frames drawn between start and end (µs)."""
    draws = [(at, values) for kind, at, values in events if kind == "draw" and start <= at <= end]
    renders = [values[0] for kind, at, values in events if kind == "render" and start <= at <= end]
    presents = [(at, values[0]) for kind, at, values in events if kind == "present" and start <= at <= end]
    intervals = [b[0] - a[0] for a, b in zip(presents, presents[1:])]
    return {
        "render": percentiles(renders),
        "request_layout": percentiles([values[0] for _, values in draws]),
        "prepaint": percentiles([values[1] for _, values in draws]),
        "paint": percentiles([values[2] for _, values in draws]),
        "draw": percentiles([values[3] for _, values in draws]),
        "present": percentiles([duration for _, duration in presents]),
        "frame_interval": percentiles(intervals),
    }


def lag(ticks, steps):
    """How far the committed size trails the requested one, in ticks, at every
    tick after the first request; and how many ticks showed a new size."""
    behind, changes, previous = [], 0, None
    for at, index, width, height in ticks:
        if previous is not None and (width, height) != previous:
            changes += 1
        previous = (width, height)
        if index == 0:
            continue
        latest = min(index, len(steps)) - 1
        requested = [i for i in range(latest + 1) if tuple(steps[i]) == (width, height)]
        if requested:
            behind.append(latest - requested[-1])
    return {"ticks": len(ticks), "changes": changes,
            "lag_ticks": percentiles([value * 1000 for value in behind])}


def moving(ticks, steps):
    """The span (µs) from the first tick until the size was last asked to change, and
    a frame after: frame intervals beyond it are only the window at rest."""
    last = max((i for i in range(1, len(steps)) if steps[i] != steps[i - 1]), default=0)
    end = next((at for at, index, *_ in ticks if index > last), ticks[-1][0])
    return [ticks[0][0], end + 17_000]


def format_summary(summary):
    rows = []
    for name, stats in summary.items():
        if stats.get("n"):
            rows.append(f"  {name:15} n={stats['n']:5}  p50={stats['p50']:7.2f}  "
                        f"p95={stats['p95']:7.2f}  max={stats['max']:7.2f}")
        else:
            rows.append(f"  {name:15} n=    0")
    return "\n".join(rows)


if __name__ == "__main__":
    log = read_log(sys.argv[1])
    window = [int(value) for value in sys.argv[2:4]]
    print(format_summary(summarize(log, *window)))
