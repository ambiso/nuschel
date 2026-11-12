#!/usr/bin/env python3
"""Rendered glyph weight, slant and monospace in the native desktop, measured
from pixels.

A scripted nuschl-cli sends stock-Mumble HTML (bold, regular, italic and bold
italic rows of the same words, inline code, a preformatted block, and Unicode
that needs fallback fonts) to a private loopback Murmur, the desktop user
types a fenced code block, and the welcome card holds `<b>something.de</b>`
next to the plain domain. Everything runs in a private headless GNOME Shell
with null audio (never the user's session). Screenshots are taken live and
again after the desktop restarts (history restored from storage), and each
message is cropped by its accessible bounds and measured:

- ink: the summed brightness above the message background (dark theme) of a
  text row; bold / regular of the same words is the weight ratio;
- slant: how far right the ink of a row's upper half sits from its lower
  half, per pixel of height (0 upright, about 0.2 italic);
- inline code: the widths of the `<code>` backgrounds of "iiiiii" and
  "MMMMMM" (1.0 for monospace, about 3 to 4 for a proportional font);
- code blocks: the pitch between the six glyphs of "iiiiii" and of "MMMMMM";
- geometry: each row's ink top, bottom and the pitch between rows.

Code must be monospace whatever the configured code font: the code spans and
code blocks are checked for equal advances too. Run with `--mono-font
Iosevka` (not installed) for a code font the desktop has to replace with a
monospace face, and with `--mono-font hack --font-dir HACK/share/fonts` for an
installed one configured in another case (the text system finds a family only
by its exact name; Hack's name does not sound monospace to GPUI's fallback).

The desktop user also types Markdown (docs/desktop.md, "Markdown"), and the
typed messages are checked, live and restored: `**Wordmark Wordmark**` over
the same words plain has a bold/regular ink ratio of at least 1.3; the code
spans of `` `iiiiii` `MMMMMM` `` are equally wide; and lines are kept, from
row positions: `# Heading` with two blank lines before the next line sits two
message line heights (24 px at the default 15 px text) lower than without
them, a blank line after a fence or between list items one line height, all
±1 px, and the restored history measures the same.

    NUSCHL_WL_TMP=... python3 tests/font_rendering.py --binaries BIN --murmur MURMUR \\
        --at-spi AT_SPI --pulse-bin PULSE/bin --gnome-shell SHELL --output DIR \\
        [--ui-font "Adwaita Sans" --mono-font "Adwaita Mono"]

`--analyze DIR` measures an earlier run's screenshots again.
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import items, snapshot, start_registry  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402

# The root channel: its timeline also shows the server's welcome card.
CHANNEL = 0
WORD = "Wordmark Wordmark"
WELCOME = "Welcome: <b>something.de</b> something.de"
# (label, html, rows of the message body measured, from its last row up)
MESSAGES = [
    (
        "weights",
        f"<b>{WORD}</b><br/>{WORD}<br/><i>{WORD}</i><br/><b><i>{WORD}</i></b>",
        ["bold", "regular", "italic", "bold italic"],
    ),
    ("inline code", "<code>iiiiii</code> <code>MMMMMM</code>", ["code"]),
    ("block", "<pre>iiiiii\nMMMMMM</pre>", ["iiiiii", "MMMMMM"]),
    (
        "unicode",
        "漢字かな 😀🎉 Ünïcödé àéîõü Привет мир<br/><b>漢字かな Привет Ünïcödé</b>",
        ["unicode", "unicode bold"],
    ),
]
TYPED = "```\niiiiii\nMMMMMM\n```"
# Typed Markdown: (label, text, the start of its accessible name, rows
# measured from its last up).
TYPED_MARKDOWN = [
    ("typed weights", "**Wordmark Wordmark**\nWordmark Wordmark",
     "Andi: Wordmark Wordmark\nWordmark Wordmark,", ["bold", "regular"]),
    ("typed inline code", "`iiiiii` `MMMMMM`", "Andi: iiiiii MMMMMM,", ["code"]),
    ("heading tight", "# Heading\nafter one", "Andi: Heading\nafter one,", ["first", "second"]),
    ("heading spaced", "# Heading\n\n\nafter three", "Andi: Heading\n\n\nafter three,", ["first", "second"]),
    ("fence tight", "```\ncodeA\n```\nafterA", "Andi: codeA\nafterA,", ["first", "second"]),
    ("fence spaced", "```\ncodeB\n```\n\nafterB", "Andi: codeB\n\nafterB,", ["first", "second"]),
    ("items tight", "- itemA\n- itemB", "Andi: • itemA\n• itemB,", ["first", "second"]),
    ("items spaced", "- itemC\n\n- itemD", "Andi: • itemC\n\n• itemD,", ["first", "second"]),
]
# One line of message text: GPUI's default line height (1.618) at 15 px.
LINE = round(15 * 1.618034)
# (spaced, tight, the extra line heights the spaced one has)
LINE_PAIRS = [("heading spaced", "heading tight", 2), ("fence spaced", "fence tight", 1),
              ("items spaced", "items tight", 1)]
MAGICK = os.environ.get("MAGICK", "magick")
TESSERACT = os.environ.get("TESSERACT", "tesseract")


def gray(path, crop):
    """Luminance rows (0-255) of a crop of a PNG."""
    x, y, width, height = crop
    raw = subprocess.run(
        [MAGICK, str(path), "-crop", f"{width}x{height}+{x}+{y}", "+repage",
         "-colorspace", "Gray", "-depth", "8", "gray:-"],
        check=True, capture_output=True, timeout=30,
    ).stdout
    return [list(raw[row * width:(row + 1) * width]) for row in range(height)]


def mode(pixels):
    counts = {}
    for row in pixels:
        for value in row:
            counts[value] = counts.get(value, 0) + 1
    return max(counts, key=counts.get)


def text_rows(pixels, background, threshold=40):
    """(top, bottom) of each run of rows with ink, bottom exclusive."""
    rows, start = [], None
    for y, row in enumerate(pixels):
        inked = any(value - background > threshold for value in row)
        if inked and start is None:
            start = y
        elif not inked and start is not None:
            rows.append((start, y))
            start = None
    if start is not None:
        rows.append((start, len(pixels)))
    return rows


def ink(pixels, background, top, bottom, floor=8):
    return sum(max(0, value - background) for row in pixels[top:bottom] for value in row
               if value - background > floor)


def slant(pixels, background, top, bottom):
    """x-centroid of the upper half's ink minus the lower half's, per pixel
    of height between the two halves' centres."""
    middle = (top + bottom) // 2

    def centroid(rows):
        total = weighted = 0
        for row in rows:
            for x, value in enumerate(row):
                amount = max(0, value - background - 8)
                total += amount
                weighted += amount * x
        return weighted / total if total else 0.0
    upper, lower = centroid(pixels[top:middle]), centroid(pixels[middle:bottom])
    return round((upper - lower) / max(1, (bottom - top) / 2), 3)


def columns(pixels, background, top, bottom, threshold=40):
    """Runs of columns with ink in a row band: (start, end) each."""
    width = len(pixels[0])
    inked = [any(pixels[y][x] - background > threshold for y in range(top, bottom))
             for x in range(width)]
    runs, start = [], None
    for x, value in enumerate(inked + [False]):
        if value and start is None:
            start = x
        elif not value and start is not None:
            runs.append((start, x))
            start = None
    return runs


def code_backgrounds(pixels, background, top, bottom):
    """Widths of the lighter `<code>` backgrounds across a row band: a column
    is inside one when even its darkest pixel in the band is lighter than the
    message background."""
    width = len(pixels[0])
    inside = [min(pixels[y][x] for y in range(top, bottom)) > background + 3 for x in range(width)]
    runs, start = [], None
    for x, value in enumerate(inside + [False]):
        if value and start is None:
            start = x
        elif not value and start is not None:
            if x - start > 4:
                runs.append(x - start)
            start = None
    return runs


def pitch(runs):
    starts = [start for start, _ in runs]
    gaps = [b - a for a, b in zip(starts, starts[1:])]
    return round(sum(gaps) / len(gaps), 2) if gaps else None


# Accessible bounds are in surface coordinates, which include GPUI's
# client-side shadow inset; screenshots are cropped to the window's frame.
INSET = 10
# Message text starts right of the avatar column (window pixels from the
# list item's left edge); the avatar would merge the header with line one.
BODY_LEFT = 46


def measure_item(path, crop, labels):
    """Measurements of one message: its last `len(labels)` text rows, right of
    the avatar column."""
    x, y, width, height = crop
    crop = [x + BODY_LEFT, y, width - BODY_LEFT, height]
    pixels = gray(path, crop)
    background = mode(pixels)
    # Whole rows only: a row cut by the crop's edge belongs to a neighbour.
    rows = [(top, bottom) for top, bottom in text_rows(pixels, background)
            if top > 0 and bottom < len(pixels) and bottom - top >= 5]
    measured = {"crop": crop, "background": background, "rows": {}}
    if len(rows) < len(labels):
        measured["error"] = f"{len(rows)} text rows, expected at least {len(labels)}"
        return measured
    for label, (top, bottom) in zip(labels, rows[-len(labels):]):
        runs = columns(pixels, background, top, bottom)
        measured["rows"][label] = {
            "top": top, "bottom": bottom, "height": bottom - top,
            "ink": ink(pixels, background, top, bottom),
            "slant": slant(pixels, background, top, bottom),
            "ink_left": runs[0][0] if runs else None,
            "ink_right": runs[-1][1] if runs else None,
            "glyph_runs": len(runs),
            "pitch": pitch(runs),
            "code_backgrounds": code_backgrounds(pixels, background, top, bottom),
        }
    return measured


def summarize(measured):
    """The headline ratios of one screenshot's measurements."""
    summary = {}
    weights = measured.get("weights", {}).get("rows", {})
    if {"bold", "regular"} <= weights.keys():
        regular = weights["regular"]
        summary["bold_ink_ratio"] = round(weights["bold"]["ink"] / regular["ink"], 3)
        if "bold italic" in weights:
            summary["bold_italic_ink_ratio"] = round(weights["bold italic"]["ink"] / regular["ink"], 3)
        # The same words upright: only the shear of italic is left.
        summary["slant_vs_regular"] = {label: round(row["slant"] - regular["slant"], 3)
                                       for label, row in weights.items() if label != "regular"}
        summary["regular_row"] = {key: regular[key] for key in ("top", "bottom", "height")}
        summary["bold_row"] = {key: weights["bold"][key] for key in ("top", "bottom", "height")}
        summary["row_pitch"] = [b["top"] - a["top"] for a, b in
                                zip(list(weights.values()), list(weights.values())[1:])]
    code = measured.get("inline code", {}).get("rows", {}).get("code")
    if code and len(code["code_backgrounds"]) >= 2:
        i, m = code["code_backgrounds"][:2]
        summary["inline_code_widths"] = [i, m]
        summary["inline_code_M_over_i"] = round(m / i, 3)
    for source in ("block", "typed block"):
        block = measured.get(source, {}).get("rows", {})
        if {"iiiiii", "MMMMMM"} <= block.keys():
            summary[f"{source}_pitch"] = {key: block[key]["pitch"] for key in ("iiiiii", "MMMMMM")}
            summary[f"{source}_runs"] = {key: block[key]["glyph_runs"] for key in ("iiiiii", "MMMMMM")}
            summary[f"{source}_ink_width"] = {key: block[key]["ink_right"] - block[key]["ink_left"]
                                              for key in ("iiiiii", "MMMMMM")
                                              if block[key]["ink_left"] is not None}
    welcome = measured.get("welcome", {})
    if "bold_domain_ink_ratio" in welcome:
        summary["welcome_bold_domain_ink_ratio"] = welcome["bold_domain_ink_ratio"]
    # Typed Markdown.
    typed = measured.get("typed weights", {}).get("rows", {})
    if {"bold", "regular"} <= typed.keys() and typed["regular"]["ink"]:
        summary["typed_bold_ink_ratio"] = round(typed["bold"]["ink"] / typed["regular"]["ink"], 3)
    code = measured.get("typed inline code", {}).get("rows", {}).get("code")
    if code and len(code["code_backgrounds"]) >= 2:
        summary["typed_inline_code_widths"] = code["code_backgrounds"][:2]
    pitches = {}
    for label, _, _, _ in TYPED_MARKDOWN:
        rows = measured.get(label, {}).get("rows", {})
        if {"first", "second"} <= rows.keys():
            pitches[label] = rows["second"]["top"] - rows["first"]["top"]
    summary["row_pitch"] = summary.get("row_pitch", [])
    summary["typed_row_pitch"] = pitches
    summary["line_height"] = LINE
    summary["extra_lines"] = {
        spaced: round((pitches[spaced] - pitches[tight]) / LINE, 3)
        for spaced, tight, _ in LINE_PAIRS if spaced in pitches and tight in pitches}
    return summary


def checks(results):
    """The pass/fail verdicts of the typed Markdown, live and restored."""
    verdicts = {}
    for stage, result in results.items():
        summary = result["summary"]
        ratio = summary.get("typed_bold_ink_ratio")
        verdicts[f"{stage}: typed **bold** ink ratio >= 1.3 ({ratio})"] = ratio is not None and ratio >= 1.3
        widths = summary.get("typed_inline_code_widths")
        verdicts[f"{stage}: typed code spans equally wide ({widths})"] = (
            widths is not None and abs(widths[0] - widths[1]) <= 2)
        # Code is monospace whatever the code font: equal advances, even when
        # the configured family is missing (`--mono-font Iosevka`).
        ratio = summary.get("inline_code_M_over_i")
        verdicts[f"{stage}: <code> spans equally wide (M/i {ratio})"] = (
            ratio is not None and abs(ratio - 1) <= 0.05)
        # Equal pitches where all six glyphs stand apart; where some touch
        # (Hack's M), the rows' ink widths: in a monospace face they differ
        # by less than one advance (a sixth of the M row), in a proportional
        # one by about five times the difference of the two advances.
        for block in ("block", "typed block"):
            pitch = summary.get(f"{block}_pitch")
            runs = summary.get(f"{block}_runs", {})
            width = summary.get(f"{block}_ink_width", {})
            separate = runs.get("iiiiii") == 6 and runs.get("MMMMMM") == 6
            if separate:
                equal = pitch is not None and abs(pitch["iiiiii"] - pitch["MMMMMM"]) <= 0.5
            else:
                equal = len(width) == 2 and width["MMMMMM"] - width["iiiiii"] < width["MMMMMM"] / 6
            verdicts[f"{stage}: {block} glyphs at equal advances (pitch {pitch}, runs {runs}, "
                     f"ink width {width})"] = equal
        pitches = summary.get("typed_row_pitch", {})
        for spaced, tight, lines in LINE_PAIRS:
            if spaced in pitches and tight in pitches:
                extra = pitches[spaced] - pitches[tight]
                verdicts[f"{stage}: {spaced} is {lines} line(s) of {LINE} px lower ({extra} px)"] = (
                    abs(extra - lines * LINE) <= 1)
            else:
                verdicts[f"{stage}: {spaced} measured"] = False
    if {"live", "restored"} <= results.keys():
        live, restored = (results[stage]["summary"].get("typed_row_pitch") for stage in ("live", "restored"))
        verdicts["restored rows as live ones (±1 px)"] = bool(live) and live.keys() == restored.keys() and all(
            abs(live[label] - restored[label]) <= 1 for label in live)
    return verdicts


def words(path, crop, scale=3):
    """OCR word boxes (text, x, y, width, height) in crop coordinates."""
    x, y, width, height = crop
    with tempfile.TemporaryDirectory() as temporary:
        scaled = Path(temporary) / "crop.png"
        subprocess.run([MAGICK, str(path), "-crop", f"{width}x{height}+{x}+{y}", "+repage",
                        "-scale", f"{scale * 100}%", str(scaled)], check=True, timeout=30)
        tsv = subprocess.run([TESSERACT, str(scaled), "-", "tsv"], check=True, capture_output=True,
                             text=True, timeout=60).stdout
    found = []
    for line in tsv.splitlines()[1:]:
        fields = line.split("\t")
        if len(fields) == 12 and fields[0] == "5" and fields[11].strip():
            left, top, w, h = (int(value) // scale for value in fields[6:10])
            found.append((fields[11].strip(), left, top, w, h))
    return found


def measure_welcome(path, crop):
    """Ink of the bold and the plain "something.de" of the welcome card, each
    in its OCR word box (the same box height for both)."""
    pixels = gray(path, crop)
    background = mode(pixels)
    domains = [word for word in words(path, crop) if "something" in word[0]]
    if len(domains) < 2:
        return {"crop": crop, "error": f"domains not found by OCR: {domains}"}
    (_, bx, by, bw, bh), (_, px_, py, pw, ph) = domains[:2]
    top, bottom = min(by, py), max(by + bh, py + ph)

    def word_ink(left, width):
        return sum(max(0, pixels[y][x] - background) for y in range(top, bottom)
                   for x in range(left, min(left + width, len(pixels[0])))
                   if pixels[y][x] - background > 8)
    bold, plain = word_ink(bx, bw), word_ink(px_, pw)
    return {"crop": crop, "boxes": [list(domain[1:]) for domain in domains[:2]], "bold_ink": bold,
            "plain_ink": plain, "bold_domain_ink_ratio": round(bold / plain, 3) if plain else None,
            "bold_width": bw, "plain_width": pw}


def framed(extents):
    x, y, width, height = extents
    return [x - INSET, y - INSET, width, height]


def annotate(shot, layout, target):
    """The screenshot with every measured crop outlined, to check them by eye."""
    command = [MAGICK, str(shot), "-fill", "none", "-stroke", "red"]
    crops = [crop for _, crop, _ in layout["messages"]] + ([layout["welcome"]] if layout["welcome"] else [])
    for x, y, width, height in map(framed, crops):
        command += ["-draw", f"rectangle {x},{y} {x + width - 1},{y + height - 1}"]
    subprocess.run(command + [str(target)], check=True, timeout=30)


def analyze(directory):
    results = {}
    for stage in ("live", "restored"):
        layout_file = directory / f"{stage}-layout.json"
        if not layout_file.exists():
            continue
        layout = json.loads(layout_file.read_text())
        shot = directory / f"{stage}.png"
        measured = {}
        for label, crop, labels in layout["messages"]:
            measured[label] = measure_item(shot, framed(crop), labels)
        if layout.get("welcome"):
            measured["welcome"] = measure_welcome(shot, framed(layout["welcome"]))
        annotate(shot, layout, directory / f"{stage}-crops.png")
        results[stage] = {"measured": measured, "summary": summarize(measured)}
    verdicts = checks(results)
    (directory / "measurements.json").write_text(
        json.dumps(results | {"checks": verdicts}, indent=2, ensure_ascii=False) + "\n")
    return results


# Interface labels the theme draws bold or semibold, in frame pixels of the
# 1540 x 2040 window (1540 x 1140 before the typed Markdown) (default and Adwaita fonts): the server name, the open
# channel's title and the timeline heading.
UI_LABELS = {
    "server name": (80, 44, 160, 22),
    "channel title": (366, 48, 120, 22),
    "timeline heading": (338, 388, 260, 34),
}


def compare(before, after):
    """Geometry and ink of one run against another (unpatched, patched)."""
    report = {}
    for stage in ("live", "restored"):
        files = [directory / "measurements.json" for directory in (before, after)]
        if not all(file.exists() for file in files):
            continue
        old, new = (json.loads(file.read_text())[stage]["measured"] for file in files)
        layouts = [json.loads((directory / f"{stage}-layout.json").read_text()) for directory in (before, after)]
        heights = {}
        for (label, old_crop, _), (_, new_crop, _) in zip(layouts[0]["messages"], layouts[1]["messages"]):
            heights[label] = [old_crop[3], new_crop[3]]
        rows = {}
        for label in old:
            for row, values in old[label].get("rows", {}).items():
                other = new.get(label, {}).get("rows", {}).get(row)
                if other:
                    rows[f"{label}: {row}"] = {
                        "top": [values["top"], other["top"]],
                        "bottom": [values["bottom"], other["bottom"]],
                        "ink_ratio": round(other["ink"] / values["ink"], 3) if values["ink"] else None,
                    }
        labels = {}
        for name, crop in UI_LABELS.items():
            inks = []
            for directory in (before, after):
                pixels = gray(directory / f"{stage}.png", crop)
                background = mode(pixels)
                inks.append(ink(pixels, background, 0, len(pixels)))
            labels[name] = {"ink": inks, "ratio": round(inks[1] / inks[0], 3) if inks[0] else None}
        report[stage] = {"item_heights": heights, "rows": rows, "ui_labels": labels}
    return report


def run(args):
    args.output.mkdir(parents=True, exist_ok=True)
    notes = {"appearance": {}}
    with tempfile.TemporaryDirectory(prefix="nuschl-fonts-") as temporary:
        root = Path(temporary)
        port = free_port()
        (root / "server").mkdir()
        server = Murmur(args.murmur, root / "server", port)
        server.ini.write_text(
            server.ini.read_text().replace(
                'welcometext="Welcome to the <b>Nuschl</b> screenshot server."',
                f'welcometext="{WELCOME}"',
            )
            + "messagelimit=1000\nmessageburst=1000\n"
        )
        server.seed()
        server.start()
        # Tall enough for every message and the welcome card at once.
        shell = GnomeSession(args.gnome_shell, 1600, 2100, log=(args.output / "gnome.log").open("w"))
        # Fonts for this run only, in the private session's own font folder
        # ($XDG_DATA_HOME/fonts), never the user's.
        fonts = shell.home / "data" / "fonts"
        for directory in args.font_dir or []:
            fonts.mkdir(parents=True, exist_ok=True)
            for font in sorted(Path(directory).rglob("*")):
                if font.suffix.lower() in (".ttf", ".otf"):
                    (fonts / font.name).write_bytes(font.read_bytes())
        notes["fonts"] = sorted(font.name for font in fonts.glob("*")) if fonts.exists() else []
        helpers, app, bot = [], None, None
        try:
            shell.start()

            def spawn(command, env, log):
                process = subprocess.Popen(command, env=env, stdout=(args.output / log).open("a"),
                                           stderr=subprocess.STDOUT, start_new_session=True)
                helpers.append(process)
                return process

            base_env = shell.app_environment()
            base_env.pop("NO_AT_BRIDGE", None)
            registry = start_registry(args.at_spi, base_env, spawn)
            pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
            bot = Client(args.binaries, "127.0.0.1", port, pin, "Mark Bot", root / "bot",
                         ["--message-limit", "1000", "--message-burst", "1000"])
            bot.send(kind="join", channel=CHANNEL)
            bot.wait(lambda e: e["kind"] == "user" and e["user"]["session"] == bot.session
                     and e["user"]["channel"] == CHANNEL)
            profile = root / "profile"
            (profile / "config").mkdir(parents=True)
            (profile / "config/servers.json").write_text(json.dumps([{
                "id": str(uuid.uuid4()), "name": "Font fixture", "host": "127.0.0.1", "port": port,
                "username": "Andi", "certificate_sha256": pin}]))
            appearance = {"theme": "dark"}
            if args.ui_font:
                appearance["ui_font"] = args.ui_font
            if args.mono_font:
                appearance["mono_font"] = args.mono_font
            notes["appearance"] = appearance
            (profile / "config/settings.json").write_text(
                json.dumps({"appearance": appearance, "audio": {"transmit": "push_to_talk"}}))
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

            def launch(log):
                process = subprocess.Popen(
                    [str(args.binaries / "nuschl"), "--profile-dir", str(profile),
                     f"mumble://Andi@127.0.0.1:{port}/"],
                    env=env, stdout=(args.output / log).open("a"), stderr=subprocess.STDOUT,
                    start_new_session=True)
                eventually(lambda: shell.window("Nuschl"), "native window missing", timeout=45)
                shell.place("Nuschl", 20, 20, 1540, 2040)
                return process

            def application():
                assert app.poll() is None, (args.output / "desktop.log").read_text()[-3000:]
                return next((n for n in registry.children() if n.name.lower() == "nuschl"), None)

            def list_items(nuschl):
                found = []
                for node, role, name, _ in items(nuschl):
                    if role == "list item":
                        try:
                            extents = node.extents()
                        except RuntimeError:
                            extents = None
                        found.append((name, extents))
                return found

            def capture(stage, nuschl):
                time.sleep(1.5)
                window = shell.window("Nuschl")
                crop = (window["x"], window["y"], window["width"], window["height"])
                shot = shell.screenshot(args.output / f"{stage}.png", crop)
                found = list_items(nuschl)
                layout = {"window": crop, "items": found, "messages": [], "welcome": None}
                for label, _, labels in MESSAGES:
                    marker = {"weights": f"Mark Bot: {WORD}", "inline code": "Mark Bot: iiiiii MMMMMM",
                              "block": "Mark Bot: iiiiii\n", "unicode": "Mark Bot: 漢字かな"}[label]
                    match = [extents for name, extents in found if name.startswith(marker)
                             and extents and extents[2] > 0 and extents[3] > 0]
                    if match:
                        layout["messages"].append([label, list(match[-1]), labels])
                typed = [extents for name, extents in found if name.startswith("Andi: iiiiii\n")
                         and extents and extents[2] > 0]
                if typed:
                    layout["messages"].append(["typed block", list(typed[-1]), ["iiiiii", "MMMMMM"]])
                for label, _, name, labels in TYPED_MARKDOWN:
                    match = [extents for found_name, extents in found if found_name.startswith(name)
                             and extents and extents[2] > 0 and extents[3] > 0]
                    if match:
                        layout["messages"].append([label, list(match[-1]), labels])
                layout["accessible_names"] = [name for name, _ in found]
                # The welcome card: a label with the welcome text.
                for node, _, name, _ in items(nuschl):
                    if "something.de" in (name or ""):
                        try:
                            extents = node.extents()
                        except RuntimeError:
                            continue
                        if extents[2] > 0 and extents[3] > 0:
                            layout["welcome"] = list(extents)
                            break
                (args.output / f"{stage}-layout.json").write_text(
                    json.dumps(layout, indent=2, ensure_ascii=False) + "\n")
                return shot

            app = launch("desktop.log")
            desktop = bot.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                               and e["user"]["channel"] == CHANNEL, timeout=60)["user"]
            nuschl = eventually(application, "desktop not in the accessibility registry", timeout=45)
            eventually(lambda: any(role == "entry" and name == "Message"
                                   for _, _, role, name in snapshot(nuschl)), "composer missing", timeout=30)
            for _, html, _ in MESSAGES:
                bot.send(kind="html", channel=CHANNEL, html=html)
                time.sleep(0.6)
            composer = next(node for _, node, role, name in snapshot(nuschl)
                            if role == "entry" and name == "Message")
            composer.call("Component", "GrabFocus")
            time.sleep(0.5)
            lines = TYPED.split("\n")
            for index, line in enumerate(lines):
                shell.type(line)
                time.sleep(0.15)
                if index + 1 < len(lines):
                    shell.press("Shift_L", "Return")
                    time.sleep(0.15)
            shell.key("Return")
            bot.wait(lambda e: e["kind"] == "text" and not e["outgoing"] and "iiiiii" in e["body"], timeout=20)
            eventually(lambda: any(name.startswith("Andi: iiiiii") for name, _ in list_items(nuschl)),
                       "typed message missing", timeout=30)
            # Typed Markdown, line by line (Shift+Enter between lines).
            for label, text, name, _ in TYPED_MARKDOWN:
                lines = text.split("\n")
                for index, line in enumerate(lines):
                    shell.type(line)
                    time.sleep(0.15)
                    if index + 1 < len(lines):
                        shell.press("Shift_L", "Return")
                        time.sleep(0.15)
                shell.key("Return")
                bot.wait(lambda e, text=text: e["kind"] == "text" and not e["outgoing"]
                         and text.split("\n")[-1].replace("&", "&amp;") in e["body"], timeout=20)
                eventually(lambda name=name: any(found.startswith(name) for found, _ in list_items(nuschl)),
                           f"typed {label} missing", timeout=30)
                time.sleep(0.4)
            capture("live", nuschl)

            os.killpg(app.pid, signal.SIGTERM)
            app.wait(timeout=15)
            bot.wait(lambda e: e["kind"] == "user_removed" and e["session"] == desktop["session"], timeout=20)
            eventually(lambda: shell.window("Nuschl") is None, "window stayed", timeout=20)
            eventually(lambda: not any(n.name.lower() == "nuschl" for n in registry.children()),
                       "old desktop still registered", timeout=20)
            app = launch("desktop-restart.log")
            nuschl = eventually(application, "restarted desktop not registered", timeout=45)
            eventually(lambda: any(name.startswith("Andi: iiiiii") for name, _ in list_items(nuschl)),
                       "history was not restored", timeout=45)
            eventually(lambda: all(any(found.startswith(name) for found, _ in list_items(nuschl))
                                   for _, _, name, _ in TYPED_MARKDOWN),
                       "typed Markdown was not restored", timeout=45)
            capture("restored", nuschl)
            (args.output / "notes.json").write_text(json.dumps(notes, indent=2) + "\n")
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
    results = analyze(args.output)
    print(json.dumps({stage: value["summary"] for stage, value in results.items()}, indent=1,
                     ensure_ascii=False))
    verdicts = checks(results)
    print(json.dumps(verdicts, indent=1, ensure_ascii=False))
    if not all(verdicts.values()):
        sys.exit(1)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binaries", type=Path)
    parser.add_argument("--murmur", type=Path)
    parser.add_argument("--at-spi", type=Path)
    parser.add_argument("--pulse-bin", type=Path)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--ui-font")
    parser.add_argument("--mono-font")
    parser.add_argument("--font-dir", type=Path, action="append",
                        help="fonts to install for this run only (in the private session)")
    parser.add_argument("--analyze", type=Path, help="measure an earlier run's screenshots again")
    parser.add_argument("--compare", type=Path, nargs=2, metavar=("BEFORE", "AFTER"),
                        help="compare two analysed runs (geometry, ink, interface labels)")
    arguments = parser.parse_args()
    if arguments.compare:
        print(json.dumps(compare(*arguments.compare), indent=1, ensure_ascii=False))
    elif arguments.analyze:
        print(json.dumps({stage: value["summary"] for stage, value in analyze(arguments.analyze).items()},
                         indent=1, ensure_ascii=False))
    else:
        for required in ("binaries", "murmur", "at_spi", "pulse_bin", "output"):
            if getattr(arguments, required) is None:
                parser.error(f"--{required.replace('_', '-')} is required")
        run(arguments)
