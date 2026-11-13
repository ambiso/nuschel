#!/usr/bin/env python3
"""Emoji in the desktop, end to end: Twemoji in messages and the composer,
and the picker beside it. Against a private loopback nuschld in a private
headless GNOME Shell (never the user's session). Ada is a scripted
nuschl-cli client; Andi is the desktop.

    NUSCHL_WL_TMP=/tmp/... python3 tests/emoji_smoke.py <binaries> --output DIR

Checks along the way (each scene is also a screenshot to look at), by
counting pixels of colours only Twemoji's drawings have (system emoji fonts
use other shades; the dark theme's text is light):

- Ada's "Nuschl™" (U+2122, text by default) paints no Twemoji ™, and her
  "Nuschl™️" (U+2122 U+FE0F) does: the same character, text and emoji;
- her 😀 is Twemoji's yellow face, not the system font's;
- Ctrl+. opens the picker; typing searches; Shift+Enter inserts the first
  result and keeps the picker open, Enter inserts and closes it; the chosen
  skin tone applies; the composer shows the inserted emoji as Twemoji;
- the sent message is exactly the inserted text.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from conversation_screenshots import DEVELOPMENT, Ada, register  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_nuschld import Nuschld, eventually, free_port  # noqa: E402
from mumble_client import certificate_pin  # noqa: E402

TRADE_MARK_IMAGE = (0x31, 0x37, 0x3D)  # Twemoji 2122.svg
GRINNING_FACE = (0xFF, 0xCC, 0x4D)  # Twemoji 1f600.svg
MEDIUM_SKIN = (0xD4, 0xAB, 0x88)  # Twemoji's medium skin tone (1f3fd)


def count(path, rgb, crop=None, tolerance=4):
    """Pixels of `path` (in `crop`, x/y/width/height) within `tolerance` of `rgb`."""
    command = ["magick", str(path)]
    if crop:
        x, y, width, height = crop
        command += ["-crop", f"{width}x{height}+{x}+{y}", "+repage"]
    raw = subprocess.run(command + ["-depth", "8", "rgb:-"], capture_output=True, check=True, timeout=30).stdout
    found = 0
    for at in range(0, len(raw) - 2, 3):
        if all(abs(raw[at + i] - rgb[i]) <= tolerance for i in range(3)):
            found += 1
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--size", default="1280x800")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    width, height = map(int, args.size.split("x"))
    with tempfile.TemporaryDirectory(prefix="nuschl-emoji-smoke-") as temporary:
        root = Path(temporary)
        port = free_port()
        (root / "server").mkdir()
        server = Nuschld(args.binaries, root / "server", port)
        server.seed()
        server.start()
        shell = GnomeSession(args.gnome_shell, width + 160, height + 100,
                             log=(args.output / "gnome-shell.log").open("w"))
        app = None
        ada = None
        try:
            shell.start()
            pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
            profile = root / "profile"
            (profile / "config").mkdir(parents=True)
            register(args.binaries, port, pin, "Andi", profile)
            register(args.binaries, port, pin, "Ada", root / "ada")
            ada = Ada(args.binaries, port, pin, root)

            bookmark = {"id": str(uuid.uuid4()), "name": "Nuschl test server", "host": "127.0.0.1",
                        "port": port, "username": "Andi", "certificate_sha256": pin}
            (profile / "config/servers.json").write_text(json.dumps([bookmark]))
            (profile / "config/settings.json").write_text(json.dumps({"appearance": {"theme": "dark"}}))
            log = (args.output / "desktop.log").open("w")
            app = subprocess.Popen([str(args.binaries / "nuschl"), "--profile-dir", str(profile),
                                    f"mumble://Andi@127.0.0.1:{port}/Lobby/Development"],
                                   env=shell.app_environment() | {"RUST_LOG": "nuschl=info"},
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            eventually(lambda: app.poll() is None and shell.window("Nuschl"), "Nuschl window did not appear")

            def placed():
                shell.place("Nuschl", 40, 40, width, height)
                time.sleep(0.3)
                window = shell.window("Nuschl")
                return window["width"] == width and window["height"] == height

            eventually(placed, "Nuschl window could not be placed")
            ada.client.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                            and e["user"]["channel"] == DEVELOPMENT, timeout=30)
            time.sleep(3.0)

            def shot(name):
                window = shell.window("Nuschl")
                path = shell.screenshot(args.output / f"{name}.png",
                                        crop=(window["x"], window["y"], window["width"], window["height"]))
                print(path, flush=True)
                return path

            def at(x, y):
                window = shell.window("Nuschl")
                return window["x"] + x, window["y"] + y

            composer = (width // 2 + 100, height - 62)
            # The newest two lines of the timeline, just above the composer
            # (absolute counts: other parts of the window scroll away).
            newest = (340, height - 165, width - 360, 70)
            shell.click(*at(*composer))  # the first click after mapping can be swallowed
            time.sleep(0.4)
            shot("emoji-00-empty")

            ada.say("Text presentation: Nuschl™ stays text")
            time.sleep(1.5)
            text_only = shot("emoji-01-text-trade-mark")
            text_marks = count(text_only, TRADE_MARK_IMAGE, newest)
            print(f"plain ™: {text_marks} Twemoji pixels", flush=True)
            assert text_marks < 15, f"™ without U+FE0F painted {text_marks} Twemoji pixels"

            ada.say("Emoji presentation: Nuschl™️ is an image")
            time.sleep(1.5)
            emoji_mark = shot("emoji-02-emoji-trade-mark")
            image_marks = count(emoji_mark, TRADE_MARK_IMAGE, newest)
            print(f"™ with U+FE0F: {image_marks} Twemoji pixels", flush=True)
            assert image_marks >= 15, f"™ with U+FE0F painted only {image_marks} Twemoji pixels"

            ada.say("Faces and more: \U0001F600 \U0001F44D\U0001F3FD ❤ ❤️ \U0001F1E9\U0001F1EA 1️⃣ ❤️‍\U0001F525")
            time.sleep(1.5)
            faces = shot("emoji-03-sequences")
            yellow = count(faces, GRINNING_FACE, newest)
            print(f"😀: {yellow} Twemoji yellow pixels", flush=True)
            assert yellow >= 30, f"😀 painted only {yellow} pixels of Twemoji's yellow"

            # The picker: Ctrl+. from the composer, search, Shift+Enter keeps it open.
            shell.click(*at(*composer))
            time.sleep(0.3)
            shell.chord("Control_L", "period")
            time.sleep(0.6)
            shot("emoji-04-picker")
            # The skin tone: the button beside the search, then "Medium".
            picker_right, picker_top = width - 24, height - 78 - 400
            shell.click(*at(picker_right - 8 - 15, picker_top + 8 + 28 + 2 + 15))
            time.sleep(0.4)
            shot("emoji-05-tones")
            tone_row_left = picker_right - (36 * 9 + 26) + 8
            tone_width = (36 * 9 + 26 - 16) / 6
            shell.click(*at(int(tone_row_left + tone_width * 3.5), picker_top + 8 + 28 + 2 + 19))
            time.sleep(0.4)
            shell.type("wave")
            time.sleep(0.5)
            shot("emoji-06-search")
            shell.press("Shift_L", "Return")
            time.sleep(0.5)
            shot("emoji-07-kept-open")
            for _ in "wave":
                shell.key("BackSpace")
            shell.type("thumbs up")
            time.sleep(0.5)
            shell.key("Return")
            time.sleep(0.6)
            composing = shot("emoji-08-composer")
            # The composer's text, left of the picker's column.
            composer_area = (380, height - 95, width - 380 - 420, 45)
            skin = count(composing, MEDIUM_SKIN, composer_area)
            print(f"composer: {skin} medium skin pixels", flush=True)
            assert skin >= 20, f"the composer painted only {skin} pixels of Twemoji's medium skin tone"

            shell.key("Return")
            expected = "\U0001F44B\U0001F3FD\U0001F44D\U0001F3FD"
            sent = ada.client.wait(lambda e: e["kind"] == "text" and e["body"] == expected, timeout=15)
            assert sent["body"] == expected, sent
            time.sleep(1.5)
            shot("emoji-09-sent")

            # The mouse: Shift+click keeps the picker open, a click closes it.
            # The first cell is now the most recent pick, 👍 in medium tone.
            def shift_click(x, y):
                keyboard = ("const k = global._nuschlKeyboard, C = imports.gi.Clutter;"
                            " k.notify_keyval(imports.gi.GLib.get_monotonic_time(), C.KEY_Shift_L, C.KeyState.{}); 'ok'")
                shell.eval(keyboard.format("PRESSED"))
                time.sleep(0.15)
                shell.click(*at(x, y))
                time.sleep(0.15)
                shell.eval(keyboard.format("RELEASED"))

            first_cell = (picker_right - (36 * 9 + 26) + 8 + 6 + 18, picker_top + 8 + 28 + 2 + 38 + 8 + 36 + 18)
            shell.click(*at(*composer))
            time.sleep(0.3)
            shell.chord("Control_L", "period")
            time.sleep(0.6)
            shift_click(*first_cell)
            time.sleep(0.3)
            shift_click(*first_cell)
            time.sleep(0.4)
            shot("emoji-10-shift-clicks")
            shell.click(*at(*first_cell))
            time.sleep(0.6)
            shot("emoji-11-click-closed")
            shell.key("Return")
            expected = "\U0001F44D\U0001F3FD" * 3
            sent = ada.client.wait(lambda e: e["kind"] == "text" and e["body"] == expected, timeout=15)
            assert sent["body"] == expected, sent
            print("emoji smoke: ok", flush=True)
        finally:
            if ada:
                ada.client.close()
            if app and app.poll() is None:
                app.terminate()
                try:
                    app.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    app.kill()
                    app.wait()
            shell.stop()
            server.stop()


if __name__ == "__main__":
    main()
