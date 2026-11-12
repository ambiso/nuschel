#!/usr/bin/env python3
"""Bold interface labels in a narrow native window, measured from pixels and
accessible bounds: they stay inside their boxes and the window, overlap no
neighbour, and long ones still end in an ellipsis.

The desktop opens a long-named channel of a private loopback Murmur in a
window of the minimum size (860 x 520) inside a private headless GNOME Shell
with null audio (never the user's session). A bot with a long name joins the
channel, posts a message (a semibold author line) and sends a direct message
(a semibold entry under "DIRECT MESSAGES"); the saved server has a long name.

By default the shell's monitor has scale 1 and the window is captured at
Nuschl's zoom 100 % (latest message, timeline scrolled to its top, channel
list scrolled down) and, after Ctrl+= up to 200 %, at the same window size:
a 430 x 260 viewport. With `--monitor-scale 2` the monitor has scale 2 and
the window keeps 860 x 520 logical pixels at zoom 100 %, drawn with twice the
pixels.

For every bold or semibold label (app name, server name, section headers, the
open channel in the tree and in the header, "Welcome to ...", the author line,
the direct-message entry and the own name) each capture records its
accessible box, the part of it its scroll viewport shows, the ink inside and
just right of it, whether ink crosses the box's right edge, whether the text
ends in an ellipsis (three dots at the baseline), and every other visible
accessible box it partly overlaps. `--compare BEFORE AFTER` sets two builds
side by side (unpatched, patched) with a crop of each label.

    NUSCHL_WL_TMP=... python3 tests/font_geometry.py --binaries BIN --murmur MURMUR \\
        --at-spi AT_SPI --pulse-bin PULSE/bin --gnome-shell SHELL --output DIR [--monitor-scale 2]
"""

import argparse
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
import urllib.parse
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import items, snapshot, start_registry  # noqa: E402
from font_rendering import MAGICK, columns, gray, mode  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import CHANNELS, Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402

WIDTH, HEIGHT = 860, 520  # the desktop's minimum window size, logical pixels
CHANNEL = 4
CHANNEL_NAME = next(name for channel, _, name in CHANNELS if channel == CHANNEL)
CHANNEL_PATH = "Lobby/Gaming/" + CHANNEL_NAME
SERVER = "Wordmark Weights Server With A Long Bold Name"
BOT = "Mark Bot With A Considerably Long Display Name"
# (label, role, accessible name, enclosing role or None) of the bold and
# semibold labels. The bot's and Andi's names also label (regular) rows of
# the channel tree.
TARGETS = [
    ("app name", "label", "Nuschl", None),
    ("server name", "label", SERVER, None),
    ("section header", "label", "VOICE CHANNELS", None),
    ("section header", "label", "DIRECT MESSAGES", None),
    ("open channel in tree", "tree item", CHANNEL_NAME, None),
    ("channel title", "label", CHANNEL_NAME, None),
    ("welcome heading", "label", f"Welcome to {CHANNEL_NAME}", None),
    ("author line", "label", BOT, "list item"),
    ("direct entry", "tree item", f"@ {BOT}", None),
    ("own name", "label", "Andi", "frame"),
]
# Accessible boxes that are scroll viewports: what lies outside one is hidden.
VIEWPORTS = {"tree", "list", "scroll pane"}
# Roles of boxes that hold others; a label inside one is not overlapping it.
CONTAINERS = {"application", "frame", "window", "panel", "filler", "section", "tool bar", "status bar",
              "layered pane", "viewport", "page tab list"} | VIEWPORTS


def window_geometry(shell, scale):
    """The window's frame and buffer (logical) and the scale of the monitor."""
    geometry = json.loads(shell.eval(
        "JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window)"
        ".find(w=>(w.get_title()||'').startsWith('Nuschl'));const f=w.get_frame_rect(),b=w.get_buffer_rect();"
        "return {frame:[f.x,f.y,f.width,f.height],buffer:[b.x,b.y,b.width,b.height]};})())"))
    frame, buffer = geometry["frame"], geometry["buffer"]
    return geometry | {"inset": [frame[0] - buffer[0], frame[1] - buffer[1]], "scale": scale}


def device_boxes(geometry, nodes):
    """Whether accessible bounds are device pixels (the monitor's scale times
    logical ones) rather than logical pixels: a box right of the logical
    buffer's width settles it; at scale 1 both are the same."""
    right = max((n["extents"][0] + n["extents"][2] for n in nodes if n["extents"] and n["extents"][2] > 0),
                default=0)
    return geometry["scale"] > 1 and right > geometry["buffer"][2] + 2


def to_pixels(geometry, extents):
    """Accessible bounds as a box in the screenshot of the window's frame."""
    scale, (dx, dy) = geometry["scale"], geometry["inset"]
    x, y, width, height = extents
    if geometry["device_boxes"]:
        return [x - dx * scale, y - dy * scale, width, height]
    return [(x - dx) * scale, (y - dy) * scale, width * scale, height * scale]


def capture(shell, application, output, stage, scale):
    time.sleep(1.5)
    geometry = window_geometry(shell, scale)
    shell.screenshot(output / f"{stage}.png", tuple(value * scale for value in geometry["frame"]))
    nodes = []
    for depth, node, role, name in snapshot(application):
        try:
            extents = list(node.extents())
        except RuntimeError:
            extents = None
        nodes.append({"depth": depth, "role": role, "name": name, "extents": extents})
    geometry["device_boxes"] = device_boxes(geometry, nodes)
    (output / f"{stage}-tree.json").write_text(
        json.dumps({"geometry": geometry, "nodes": nodes}, indent=1, ensure_ascii=False) + "\n")
    return geometry, nodes


def monitor_scale(shell, scale):
    """Give the private shell's virtual monitor a scale (Mutter's DisplayConfig
    on the shell's own bus; never the user's session)."""
    env = shell.environment() | {"DBUS_SESSION_BUS_ADDRESS": shell.bus}
    call = ["gdbus", "call", "--session", "--dest", "org.gnome.Mutter.DisplayConfig", "--object-path",
            "/org/gnome/Mutter/DisplayConfig", "--method"]
    state = subprocess.run(call + ["org.gnome.Mutter.DisplayConfig.GetCurrentState"], env=env,
                           capture_output=True, text=True, timeout=15, check=True).stdout
    serial = re.match(r"\(uint32 (\d+),", state).group(1)
    connector = re.search(r"\[\(\('([^']+)',", state).group(1)
    current = re.search(r"\('([^']+)', \d+, \d+, [\d.]+, [\d.]+, \[[^\]]*\], \{'is-current': <true>", state).group(1)
    subprocess.run(call + ["org.gnome.Mutter.DisplayConfig.ApplyMonitorsConfig", serial, "1",
                           f"[(0, 0, {float(scale)}, uint32 0, true, [('{connector}', '{current}', @a{{sv}} {{}})])]",
                           "@a{sv} {}"], env=env, capture_output=True, text=True, timeout=15, check=True)
    eventually(lambda: shell.eval("global.display.get_monitor_scale(0)") == str(scale), "monitor scale not applied")


def run(args):
    args.output.mkdir(parents=True, exist_ok=True)
    scale = args.monitor_scale
    with tempfile.TemporaryDirectory(prefix="nuschl-geometry-") as temporary:
        root = Path(temporary)
        port = free_port()
        (root / "server").mkdir()
        server = Murmur(args.murmur, root / "server", port)
        server.seed()
        server.start()
        shell = GnomeSession(args.gnome_shell, 1600 * scale, 1200 * scale, log=(args.output / "gnome.log").open("w"))
        helpers, app, bot = [], None, None
        try:
            shell.start()
            if scale != 1:
                monitor_scale(shell, scale)

            def spawn(command, env, log):
                process = subprocess.Popen(command, env=env, stdout=(args.output / log).open("a"),
                                           stderr=subprocess.STDOUT, start_new_session=True)
                helpers.append(process)
                return process

            base_env = shell.app_environment()
            base_env.pop("NO_AT_BRIDGE", None)
            registry = start_registry(args.at_spi, base_env, spawn)
            pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
            bot = Client(args.binaries, "127.0.0.1", port, pin, BOT, root / "bot")
            bot.send(kind="join", channel=CHANNEL)
            bot.wait(lambda e: e["kind"] == "user" and e["user"]["session"] == bot.session
                     and e["user"]["channel"] == CHANNEL)
            profile = root / "profile"
            (profile / "config").mkdir(parents=True)
            (profile / "config/servers.json").write_text(json.dumps([{
                "id": str(uuid.uuid4()), "name": SERVER, "host": "127.0.0.1", "port": port,
                "username": "Andi", "certificate_sha256": pin}]))
            (profile / "config/settings.json").write_text(json.dumps(
                {"appearance": {"theme": "dark"}, "audio": {"transmit": "push_to_talk"}}))
            # Private null audio only: never the host's microphone or speakers.
            pulse = shell.runtime / "pulse"
            pulse.mkdir(mode=0o700, exist_ok=True)
            script = pulse / "default.pa"
            script.write_text(
                f"load-module module-native-protocol-unix socket={pulse / 'native'} auth-anonymous=1\n"
                "load-module module-null-sink sink_name=speakers rate=48000\n"
                "load-module module-null-sink sink_name=microphone rate=48000\n"
                "set-default-sink speakers\nset-default-source microphone.monitor\n")
            spawn([str(args.pulse_bin / "pulseaudio"), "--daemonize=no", "--use-pid-file=no",
                   "--exit-idle-time=-1", "-n", f"--file={script}"],
                  shell.environment() | {"PULSE_RUNTIME_PATH": str(pulse),
                                         "PULSE_STATE_PATH": str(pulse / "state"),
                                         "DBUS_SESSION_BUS_ADDRESS": f"unix:path={root}/no-pulse-bus"},
                  "pulse.log")
            eventually(lambda: (pulse / "native").exists(), "private null audio missing")
            env = base_env | {"RUST_LOG": "nuschl=info,gpui=warn,gpui_wgpu=warn",
                              "PULSE_SERVER": f"unix:{pulse}/native", "NUSCHL_AUDIO_BACKEND": "pulseaudio",
                              "PULSE_SINK": "speakers", "PULSE_SOURCE": "microphone.monitor",
                              "PIPEWIRE_REMOTE": str(root / "no-pipewire")}
            url = f"mumble://Andi@127.0.0.1:{port}/" + urllib.parse.quote(CHANNEL_PATH)
            app = subprocess.Popen([str(args.binaries / "nuschl"), "--profile-dir", str(profile), url],
                                   env=env, stdout=(args.output / "desktop.log").open("a"),
                                   stderr=subprocess.STDOUT, start_new_session=True)
            eventually(lambda: shell.window("Nuschl"), "native window missing", timeout=45)
            shell.place("Nuschl", 40, 40, WIDTH, HEIGHT)
            desktop = bot.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                               and e["user"]["channel"] == CHANNEL, timeout=60)["user"]

            def application():
                assert app.poll() is None, (args.output / "desktop.log").read_text()[-3000:]
                return next((n for n in registry.children() if n.name.lower() == "nuschl"), None)

            nuschl = eventually(application, "desktop not in the accessibility registry", timeout=45)
            eventually(lambda: any(role == "entry" and name == "Message" for _, role, name, _ in items(nuschl)),
                       "composer missing", timeout=30)
            bot.send(kind="text", channel=CHANNEL, body="Wordmark heading check")
            bot.send(kind="direct", session=desktop["session"], body="Wordmark direct check")
            eventually(lambda: any(name == f"@ {BOT}" for _, _, name, _ in items(nuschl))
                       and any(name.startswith(f"{BOT}: Wordmark") for _, _, name, _ in items(nuschl)),
                       "author line or direct entry missing", timeout=30)

            def settle(stage):
                shell.place("Nuschl", 40, 40, WIDTH, HEIGHT)
                # No hover state or tooltip: the pointer over the timeline's
                # empty right margin (which hides a tooltip), then off the window.
                shell.move(40 + WIDTH - 6, 40 + HEIGHT // 2)
                time.sleep(0.5)
                shell.move(40 + WIDTH + 60, 100)
                time.sleep(1.0)
                return capture(shell, nuschl, args.output, stage, scale)

            def scroll(geometry, nodes, role, prefix, clicks):
                """Turn the wheel over the centre of a scroll viewport."""
                found = next((n for n in nodes if n["role"] == role and (n["name"] or "").startswith(prefix)
                              and n["extents"] and n["extents"][3] > 0), None)
                if not found:
                    return False
                x, y, width, height = to_pixels(geometry, found["extents"])
                frame = geometry["frame"]
                shell.wheel(frame[0] + (x + width // 2) // scale, frame[1] + (y + height // 2) // scale, clicks)
                time.sleep(0.8)
                return True

            def stages(name):
                geometry, nodes = settle(name)
                if scroll(geometry, nodes, "list", "Messages in", -40):
                    settle(f"{name}-top")
                if scroll(geometry, nodes, "tree", "Channels", 40):
                    settle(f"{name}-sidebar")

            if scale == 1:
                stages("zoom100")
                frame = shell.window("Nuschl")
                shell.click(frame["x"] + WIDTH * 3 // 4, frame["y"] + 20)
                for _ in range(5):  # 110, 125, 150, 175, 200 %
                    shell.chord("Control_L", "equal")
                    time.sleep(0.15)
                eventually(lambda: abs(json.loads((profile / "config/settings.json").read_text())
                                       ["appearance"].get("zoom", 1.0) - 2.0) < 0.001, "zoom did not reach 200 %")
                stages("zoom200")
            else:
                stages(f"scale{100 * scale}")
        finally:
            for process in ([app] if app else []) + helpers:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait(timeout=5)
            if bot:
                try:
                    bot.close()
                except AssertionError:
                    pass
            shell.stop()
            server.stop()
    report = analyze(args.output)
    print(json.dumps(summary(report), indent=1, ensure_ascii=False))


def stage_names(directory):
    return sorted((path.name[:-len("-tree.json")] for path in directory.glob("*-tree.json")),
                  key=lambda name: (re.sub(r"-.*", "", name), name))


def intersect(a, b):
    left, top = max(a[0], b[0]), max(a[1], b[1])
    right, bottom = min(a[0] + a[2], b[0] + b[2]), min(a[1] + a[3], b[1] + b[3])
    return [left, top, max(0, right - left), max(0, bottom - top)]


def area(box):
    return box[2] * box[3]


def contains(outer, inner, slack=1):
    return (inner[0] >= outer[0] - slack and inner[1] >= outer[1] - slack
            and inner[0] + inner[2] <= outer[0] + outer[2] + slack
            and inner[1] + inner[3] <= outer[1] + outer[3] + slack)


def ellipsis(pixels, background, width):
    """Whether the text shows an ellipsis: three small blobs of ink (each
    4-connected, wholly in the lower half of the text) on one line, evenly
    spaced. Blobs rather than columns: a bold "v" and the first dot can share
    columns."""
    height = len(pixels)
    ink = [[pixels[y][x] - background > 40 for x in range(width)] for y in range(height)]
    rows = [y for y in range(height) if any(ink[y])]
    if not rows:
        return False
    top, bottom = rows[0], rows[-1] + 1
    seen = [[False] * width for _ in range(height)]
    dots = []
    for y0 in range(height):
        for x0 in range(width):
            if not ink[y0][x0] or seen[y0][x0]:
                continue
            seen[y0][x0] = True
            stack, points = [(y0, x0)], []
            while stack:
                y, x = stack.pop()
                points.append((y, x))
                for ny, nx in ((y + 1, x), (y - 1, x), (y, x + 1), (y, x - 1)):
                    if 0 <= ny < height and 0 <= nx < width and ink[ny][nx] and not seen[ny][nx]:
                        seen[ny][nx] = True
                        stack.append((ny, nx))
            ys, xs = [y for y, _ in points], [x for _, x in points]
            w, h = max(xs) - min(xs) + 1, max(ys) - min(ys) + 1
            if min(ys) >= top + (bottom - top) / 2 and w <= max(4, (bottom - top) // 3) and h <= w + 2:
                dots.append((min(xs), max(xs) + 1, max(ys)))
    dots.sort()
    for a, b, c in zip(dots, dots[1:], dots[2:]):
        gaps = (b[0] - a[1], c[0] - b[1])
        if min(gaps) > 0 and max(gaps) - min(gaps) <= 2 and max(gaps) <= 3 * (a[1] - a[0]) + 2 \
                and abs(a[2] - c[2]) <= 1:
            return True
    return False


def measure(shot, box, visible, margin, size):
    """Ink in the visible part of a label's box (and `margin` pixels right of
    it): its extent, whether a run of ink crosses the box's right edge, and
    whether the text ends in an ellipsis."""
    x, y, w, h = visible
    if w <= 0 or h <= 0:
        return {"visible": False}
    right_edge_shown = visible[0] + visible[2] == box[0] + box[2]
    extended = intersect([x, y, w + margin, h], [0, 0, *size])
    pixels = gray(shot, extended)
    inside = [row[:w] for row in pixels]
    background = mode(inside)
    runs = columns(pixels, background, 0, len(pixels))
    inner = [run for run in runs if run[0] < w]
    return {
        "visible": True,
        "fully_visible": visible == box,
        "ink_left": x + inner[0][0] if inner else None,
        "ink_right": x + max(min(end, w) for _, end in inner) if inner else None,
        "ink_runs": len(inner),
        "crosses_right_edge": right_edge_shown and any(start < w < end for start, end in runs),
        "touches_right_edge": right_edge_shown and any(row[w - 1] - background > 40 for row in inside),
        "ellipsis": ellipsis(inside, background, w),
    }


def analyze(directory):
    report = {}
    for stage in stage_names(directory):
        tree = json.loads((directory / f"{stage}-tree.json").read_text())
        geometry, nodes = tree["geometry"], tree["nodes"]
        size = [value * geometry["scale"] for value in geometry["frame"][2:]]
        window = [0, 0, *size]
        # Each node's box in screenshot pixels and the part of it on screen:
        # inside the window and every enclosing scroll viewport.
        viewports, ancestors = [], []
        for node in nodes:
            while viewports and viewports[-1][0] >= node["depth"]:
                viewports.pop()
            while ancestors and ancestors[-1][0] >= node["depth"]:
                ancestors.pop()
            node["parent"] = ancestors[-1][1] if ancestors else None
            ancestors.append((node["depth"], node["role"]))
            node["box"] = to_pixels(geometry, node["extents"]) if node["extents"] and node["extents"][2] > 0 else None
            node["shown"] = None
            if node["box"]:
                shown = intersect(node["box"], window)
                for _, viewport in viewports:
                    shown = intersect(shown, viewport)
                node["shown"] = shown
                if node["role"] in VIEWPORTS:
                    viewports.append((node["depth"], node["box"]))
        per_pixel = geometry["scale"] * (2 if stage.startswith("zoom200") else 1)
        others = [n for n in nodes if n["shown"] and area(n["shown"]) > 0 and n["role"] not in CONTAINERS]
        shot = directory / f"{stage}.png"
        labels = []
        draw = [MAGICK, str(shot), "-fill", "none", "-stroke", "red", "-strokewidth", str(per_pixel)]
        for label, role, name, parent in TARGETS:
            for node in nodes:
                if node["name"] != name or node["role"] != role or not node["box"]:
                    continue
                if parent and node["parent"] != parent:
                    continue
                entry = {"label": label, "role": role, "name": name, "box": node["box"], "shown": node["shown"],
                         "inside_window": contains(window, node["box"], slack=0)}
                entry |= measure(shot, node["box"], node["shown"], 8 * per_pixel, size)
                overlaps = []
                for other in others if entry["visible"] else []:
                    if other is node or contains(other["box"], node["box"]) or contains(node["box"], other["box"]):
                        continue
                    shared = area(intersect(node["shown"], other["shown"]))
                    if shared:
                        overlaps.append({"role": other["role"], "name": (other["name"] or "")[:60],
                                         "box": other["box"], "area": shared})
                entry["overlaps"] = overlaps
                labels.append(entry)
                if entry["visible"]:
                    x, y, w, h = entry["shown"]
                    draw += ["-draw", f"rectangle {x},{y} {x + w - 1},{y + h - 1}"]
        subprocess.run(draw + [str(directory / f"{stage}-boxes.png")], check=True, timeout=60)
        report[stage] = {"window": geometry, "labels": labels}
    (directory / "geometry.json").write_text(json.dumps(report, indent=1, ensure_ascii=False) + "\n")
    return report


def summary(report):
    """One line per label and stage: box, ink extent and the findings."""
    lines = {}
    for stage, value in report.items():
        for entry in value["labels"]:
            key = f"{stage} {entry['label']}"
            if not entry["visible"]:
                lines[key] = "hidden (outside its scroll viewport or the window)"
                continue
            findings = ["ink crosses the box's right edge"] if entry["crosses_right_edge"] else []
            findings += ["box outside the window"] if not entry["inside_window"] else []
            findings += [f"overlaps {o['role']} '{o['name']}' ({o['area']} px)" for o in entry["overlaps"]]
            lines[key] = (f"box {entry['box']}{'' if entry['fully_visible'] else ' shown ' + str(entry['shown'])}"
                          f" ink {entry['ink_left']}..{entry['ink_right']}"
                          f"{' ellipsis' if entry['ellipsis'] else ''}"
                          f"{' touches right edge' if entry['touches_right_edge'] else ''}"
                          f"{' | ' + '; '.join(findings) if findings else ''}")
    return lines


def compare(before, after):
    """Labels of two runs side by side: boxes, ink extents and findings, with
    a crop of each label (before above after, a red line between) to look at."""
    old, new = (json.loads((directory / "geometry.json").read_text()) for directory in (before, after))
    crops = after / "compare-crops"
    crops.mkdir(exist_ok=True)
    result = {}
    for stage in old:
        if stage not in new:
            continue
        per_pixel = new[stage]["window"]["scale"] * (2 if stage.startswith("zoom200") else 1)
        def keyed(labels):
            seen = {}
            for entry in labels:
                seen[entry["label"]] = seen.get(entry["label"], -1) + 1
                yield (entry["label"], seen[entry["label"]]), entry
        before_labels, after_labels = dict(keyed(old[stage]["labels"])), dict(keyed(new[stage]["labels"]))
        pairs = []
        for index, key in enumerate(before_labels | after_labels):
            a, b = before_labels.get(key), after_labels.get(key)
            if not a or not b:
                pairs.append({"label": key[0], "only_in": "before" if a else "after"})
                continue
            entry = {"label": a["label"], "same_box": a["box"] == b["box"], "box": [a["box"], b["box"]]}
            for key in ("visible", "ink_right", "ellipsis", "crosses_right_edge", "touches_right_edge",
                        "inside_window"):
                entry[key] = [a.get(key), b.get(key)]
            entry["overlaps"] = [sorted(o["name"] for o in a["overlaps"]), sorted(o["name"] for o in b["overlaps"])]
            if a["visible"] and b["visible"]:
                x, y, w, h = a["shown"]
                pad = 8 * per_pixel
                region = f"{w + 2 * pad}x{h + pad}+{max(0, x - pad)}+{max(0, y - pad // 2)}"
                target = crops / f"{stage}-{index:02}-{a['label'].replace(' ', '-')}.png"
                subprocess.run([MAGICK, "(", str(before / f"{stage}.png"), "-crop", region, "+repage", ")",
                                "(", str(after / f"{stage}.png"), "-crop", region, "+repage", ")",
                                "-background", "red", "-splice", "0x2", "-append", str(target)],
                               check=True, timeout=30)
                entry["crop"] = str(target)
            pairs.append(entry)
        result[stage] = pairs
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binaries", type=Path)
    parser.add_argument("--murmur", type=Path)
    parser.add_argument("--at-spi", type=Path)
    parser.add_argument("--pulse-bin", type=Path)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--monitor-scale", type=int, default=1, choices=(1, 2),
                        help="the private monitor's scale (2: the window's logical size at twice the pixels)")
    parser.add_argument("--analyze", type=Path, help="measure an earlier run's captures again")
    parser.add_argument("--compare", type=Path, nargs=2, metavar=("BEFORE", "AFTER"),
                        help="compare two analysed runs (unpatched, patched)")
    arguments = parser.parse_args()
    if arguments.compare:
        print(json.dumps(compare(*arguments.compare), indent=1, ensure_ascii=False))
    elif arguments.analyze:
        print(json.dumps(summary(analyze(arguments.analyze)), indent=1, ensure_ascii=False))
    else:
        for required in ("binaries", "murmur", "at_spi", "pulse_bin", "output"):
            if getattr(arguments, required) is None:
                parser.error(f"--{required.replace('_', '-')} is required")
        run(arguments)
