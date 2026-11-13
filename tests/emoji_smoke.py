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
SELECTED_TAB = (0x2C, 0x34, 0x43)  # the dark theme's SELECTED


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


def tallest_mark(path, crop, threshold=60):
    """The longest vertical run of pixels in `crop` (x/y/width/height) of
    `path` that differ by more than `threshold` (summed over R, G and B)
    from the crop's most common colour, its background."""
    x, y, width, height = crop
    raw = subprocess.run(["magick", str(path), "-crop", f"{width}x{height}+{x}+{y}", "+repage",
                          "-depth", "8", "rgb:-"], capture_output=True, check=True, timeout=30).stdout
    pixels = [raw[at:at + 3] for at in range(0, width * height * 3, 3)]
    background = max(set(pixels), key=pixels.count)

    def differs(pixel):
        return sum(abs(pixel[i] - background[i]) for i in range(3)) > threshold

    tallest = 0
    for column in range(width):
        run = 0
        for row in range(height):
            run = run + 1 if differs(pixels[row * width + column]) else 0
            tallest = max(tallest, run)
    return tallest


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--size", default="1280x800")
    parser.add_argument("--no-category-row", action="store_true",
                        help="the picker has no row of category tabs (builds before it), to see these checks fail there")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    width, height = map(int, args.size.split("x"))
    # The row of category tabs between the search and the grid.
    categories = 0 if args.no_category_row else 40
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

            # Below the search bar and the row of category tabs.
            first_cell = (picker_right - (36 * 9 + 26) + 8 + 6 + 18,
                          picker_top + 8 + 28 + 2 + 38 + 8 + categories + 36 + 18)
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

            # The search field is a text field: it shows its cursor, a click
            # on a result while searching inserts it, Ctrl+V pastes and
            # Ctrl+Backspace deletes a word. The grid has a scrollbar, as the
            # channel list does, and a tab per category. Failures are collected, so one run reports
            # all of them.
            failures = []
            ACCENT = (0x5B, 0x7C, 0xFA)  # the dark theme's accent: the search field's border
            smiley = (width - 92, height - 73)  # the picker button beside the composer

            def find_field(path):
                """The search field's border box (x, y, width, height), from its
                accent: in the right half (the channel list's selection is
                accent too), above the composer (so is its send button)."""
                raw = subprocess.run(["magick", str(path), "-alpha", "off", "-depth", "8", "rgb:-"],
                                     capture_output=True, check=True, timeout=30).stdout
                image_width = len(raw) // 3 // height
                xs, ys = [], []
                for y in range(height - 100):
                    for x in range(image_width // 2, image_width):
                        at = (y * image_width + x) * 3
                        if all(abs(raw[at + i] - ACCENT[i]) < 12 for i in range(3)):
                            xs.append(x)
                            ys.append(y)
                assert xs, f"no search field found in {path}"
                field = min(xs), min(ys), max(xs) - min(xs) + 1, max(ys) - min(ys) + 1
                assert 200 < field[2] < 400 and 25 < field[3] < 60, f"no search field found in {path}: {field}"
                return field

            def open_search(name):
                """Open the picker with its button, click into the search field;
                returns the field's box."""
                shell.click(*at(*composer))
                time.sleep(0.3)
                shell.click(*at(*smiley))
                time.sleep(0.6)
                field = find_field(shot(name))
                shell.click(*at(field[0] + field[2] // 2, field[1] + field[3] // 2))
                time.sleep(0.3)
                return field

            def sent_after(action, expected, what):
                """`action` puts `expected` into the composer; Enter sends it."""
                action()
                shell.key("Return")
                try:
                    ada.client.wait(lambda e: e["kind"] == "text" and e["body"] == expected, timeout=8)
                except Exception as error:  # noqa: BLE001 - reported with the others
                    shot(f"emoji-failed-{len(failures)}")
                    failures.append(f"{what}: {expected!r} did not arrive ({error})")
                    # Leave nothing behind for the next check.
                    shell.key("Escape")
                    time.sleep(0.3)
                    shell.click(*at(*composer))
                    shell.chord("Control_L", "a")
                    shell.key("BackSpace")
                time.sleep(0.5)

            def search_and_click():
                field = open_search("emoji-12-scrollbar")
                print(f"search field at {field}", flush=True)
                # Right of the last column of emoji, inside the picker's border.
                strip = (field[0] + field[2] + 30, field[1] + field[3] + 10, 11, 200)
                thumb = tallest_mark(shot("emoji-12-scrollbar-focused"), strip, threshold=30)
                print(f"emoji grid: scrollbar thumb {thumb} px", flush=True)
                if thumb < 20:
                    failures.append(f"the emoji grid shows no scrollbar (tallest mark {thumb} px)")
                shell.type("wave")
                time.sleep(0.4)
                # The cursor: a thin line, taller than any letter of "wave",
                # in at least one of several frames (it may blink). Right of
                # the magnifier, inside the border.
                text_area = (field[0] + 30, field[1] + 3, field[2] - 36, field[3] - 6)
                tallest = 0
                for frame in range(6):
                    tallest = max(tallest, tallest_mark(shot(f"emoji-13-search-cursor-{frame}"), text_area))
                    time.sleep(0.25)
                print(f"search field: tallest mark {tallest} px", flush=True)
                if tallest < 14:
                    failures.append(f"the search field shows no cursor (tallest mark {tallest} px)")
                # As a hand does: move there, rest until the tooltip shows, then
                # press and release a moment later.
                first = (field[0] + 18, field[1] + field[3] + 8 + categories + 18)
                shell.move(*at(first[0] - 6, first[1] - 4))
                time.sleep(0.1)
                shell.move(*at(*first))
                time.sleep(1.2)
                shot("emoji-14-search-hover")
                shell.eval("global._nuschlPointer.notify_button(imports.gi.GLib.get_monotonic_time(), 1,"
                           " imports.gi.Clutter.ButtonState.PRESSED); 'ok'")
                time.sleep(0.12)
                shell.eval("global._nuschlPointer.notify_button(imports.gi.GLib.get_monotonic_time(), 1,"
                           " imports.gi.Clutter.ButtonState.RELEASED); 'ok'")
                time.sleep(0.6)
                shot("emoji-15-search-click")

            sent_after(search_and_click, "\U0001F44B\U0001F3FD", "a click on a search result")

            # The last column too: "face" lists 😘 as its 18th result, the
            # end of the second line (reported: 💤 could be clicked, 😘 not).
            def last_column():
                field = open_search("emoji-15a-face-open")
                shell.type("face")
                time.sleep(0.4)
                kiss = (field[0] + 8 * 36 + 18, field[1] + field[3] + 8 + categories + 36 + 18)
                shell.move(*at(kiss[0] - 30, kiss[1] - 4))
                time.sleep(0.1)
                shell.move(*at(*kiss))
                time.sleep(1.2)
                shot("emoji-15b-face-hover")
                shell.eval("global._nuschlPointer.notify_button(imports.gi.GLib.get_monotonic_time(), 1,"
                           " imports.gi.Clutter.ButtonState.PRESSED); 'ok'")
                time.sleep(0.12)
                shell.eval("global._nuschlPointer.notify_button(imports.gi.GLib.get_monotonic_time(), 1,"
                           " imports.gi.Clutter.ButtonState.RELEASED); 'ok'")
                time.sleep(0.6)
                shot("emoji-15c-face-click")

            sent_after(last_column, "\U0001F618", "a click on the last column of results")

            def paste():
                shell.eval("const St = imports.gi.St; St.Clipboard.get_default()"
                           ".set_text(St.ClipboardType.CLIPBOARD, 'thumbs up'); 'ok'")
                time.sleep(0.3)
                open_search("emoji-16-paste-open")
                shell.chord("Control_L", "v")
                time.sleep(0.5)
                shot("emoji-17-search-paste")
                shell.key("Return")  # inserts the first result
                time.sleep(0.5)

            sent_after(paste, "\U0001F44D\U0001F3FD", "Ctrl+V in the search")

            def delete_word():
                open_search("emoji-18-delete-open")
                shell.type("thumbs up wave")
                shell.chord("Control_L", "BackSpace")
                time.sleep(0.5)
                shot("emoji-19-search-delete-word")
                shell.key("Return")  # inserts the first result
                time.sleep(0.5)

            sent_after(delete_word, "\U0001F44D\U0001F3FD", "Ctrl+Backspace in the search")

            # The category tabs under the search: the last, Flags, jumps to
            # its heading; the first emoji under it is the chequered flag.
            def category():
                field = open_search("emoji-20-categories")
                tabs_middle = field[1] + field[3] + 8 + 18
                shell.click(*at(field[0] + 8 * 36 + 18, tabs_middle))
                time.sleep(0.3)
                # Away from the tab (hover would cover it): it is the selected one.
                shell.move(*at(field[0] + 4 * 36, field[1] - 60))
                time.sleep(0.6)
                flags = shot("emoji-21-flags")
                tab_corner = (field[0] + 8 * 36 + 3, tabs_middle - 15, 3, 3)
                selected = count(flags, SELECTED_TAB, tab_corner, tolerance=3)
                print(f"Flags tab: {selected}/9 pixels of the selected colour", flush=True)
                if selected < 5:
                    failures.append("the Flags tab is not shown as selected after jumping to it")
                shell.click(*at(field[0] + 18, tabs_middle + 18 + 4 + 36 + 18))
                time.sleep(0.6)

            sent_after(category, "\U0001F3C1", "the Flags tab")
            assert not failures, "\n".join(failures)
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
