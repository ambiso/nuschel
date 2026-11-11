#!/usr/bin/env python3
"""Capture desktop screenshots against a private loopback stock Mumble server.

Development aid: `python3 tests/screenshots.py target/debug --murmur <mumble-server>`
under `xvfb-run`. Only a server bound to 127.0.0.1 is used; no host audio devices
are opened (the media worker is pointed at a private PulseAudio if one is given).
"""
import argparse
import base64
import struct
import zlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import urllib.parse
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402

def SEARCH_BUTTON(width):
    """The conversation header's search button, in window screenshot pixels."""
    return (width - 77, 86)


# The voice card's warning line, in window screenshot pixels.
CLIPPING_WARNING = (205, 728)
# Settings: "Test echo" before the microphone test has started.
ECHO_TEST_BUTTON = (862, 225)
# The cog next to the server name, Lobby's chevron, and the channel list's right edge.
SERVER_SETTINGS_BUTTON = (279, 86)
LOBBY_CHEVRON = (129, 234)
SIDEBAR_EDGE = (346, 400)
# Wheel clicks for about one screen of the timeline, and the pill that jumps back down.
SCROLL_CLICKS = 8


def JUMP_TO_LATEST(width, height):
    """The "Jump to latest" pill, centred over the timeline's bottom edge."""
    return (width // 2 + 160, height - 112)


# The screenshot sent into #Development, and the Lobby row.
CHAT_IMAGE = (580, 670)
LOBBY_ROW = (189, 233)
# The patch's copy button, and words in the first messages: "patch:", "println",
# "good" and the end of "Ship" in Grace's reply.
COPY_CODE = (1240, 288)
PATCH_WORD = (536, 254)
PRINTLN_WORD = (478, 350)
GOOD_WORD = (472, 450)
SHIP_END = (464, 474)
# In the message the links scene sends: "the guide" and "come to the Lobby".
GUIDE_LINK = (497, 684)
LOBBY_LINK = (835, 684)


# GNOME Shell exports its freedesktop notification daemon but, headless, never
# claims the bus name: claim it, so Nuschel's notifications reach the real one.
# Ask GNOME Shell for the clipboard's text; CLIPBOARD_TEXT reads the answer.
READ_CLIPBOARD = """
global._nuschelClipboard = null;
imports.gi.St.Clipboard.get_default().get_text(imports.gi.St.ClipboardType.CLIPBOARD,
    (_, text) => { global._nuschelClipboard = text; });
'ok'
"""
CLIPBOARD_TEXT = "global._nuschelClipboard"

CLAIM_NOTIFICATIONS = """
imports.gi.Gio.bus_own_name_on_connection(imports.gi.Gio.DBus.session,
    'org.freedesktop.Notifications', 0, null, null);
'ok'
"""


# The message box, and a clipboard holding a file's PNG (as screenshot tools leave it).
COMPOSER = (700, 735)
SET_CLIPBOARD = """
const [, data] = imports.gi.GLib.file_get_contents(%s);
imports.gi.St.Clipboard.get_default().set_content(imports.gi.St.ClipboardType.CLIPBOARD,
    'image/png', new imports.gi.GLib.Bytes(data)); 'ok'
"""
PASTE = """
const k = global._nuschelKeyboard, C = imports.gi.Clutter, t = imports.gi.GLib.get_monotonic_time();
k.notify_keyval(t, C.KEY_Control_L, C.KeyState.PRESSED);
k.notify_keyval(t + 10000, C.KEY_v, C.KeyState.PRESSED);
k.notify_keyval(t + 20000, C.KEY_v, C.KeyState.RELEASED);
k.notify_keyval(t + 30000, C.KEY_Control_L, C.KeyState.RELEASED); 'ok'
"""


def make_photo(path, width, height):
    """A detailed picture that PNG cannot squeeze into an image message."""
    noise = os.urandom(width * 3)
    rows = b"".join(
        b"\x00" + bytes((x * 255 // width ^ noise[(x * 3 + y * 7) % len(noise)] & 0x1f) % 256
                        if channel == 0 else (y * 255 // height + noise[(x + y) % len(noise)] % 24) % 256
                        if channel == 1 else 140
                        for x in range(width) for channel in range(3))
        for y in range(height)
    )
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
                     + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))
    return path


def make_png(path, width, seed, height=None):
    """A small gradient PNG written with the standard library only."""
    height = height or width
    rows = b"".join(
        b"\x00" + bytes(
            channel
            for x in range(width)
            for channel in ((x * 255 // width + seed * 40) % 256, (y * 255 // height + seed * 90) % 256, (128 + seed * 60) % 256)
        )
        for y in range(height)
    )
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))
    path.write_bytes(png)
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--size", default="1280x800")
    parser.add_argument("--settle", type=float, default=4.0)
    parser.add_argument("--scenes", default="all", help="comma-separated: select,links,rich,dialog,settings,server,menus,long,search,emoji,toggles,clipping,viewer,notify,scroll,paste,maximize")
    parser.add_argument("--pulse-bin", type=Path, help="start a private PulseAudio with a sine-wave microphone")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    width, height = map(int, args.size.split("x"))
    with tempfile.TemporaryDirectory(prefix="nuschel-screens-") as temporary:
        root = Path(temporary)
        port = free_port()
        murmur = Murmur(args.murmur, root, port)
        if {"all", "scroll"} & set(args.scenes.split(",")):
            # The scroll scene fills several pages of history quickly; the
            # stock server otherwise drops text beyond one message a second.
            murmur.ini.write_text(murmur.ini.read_text() + "messagelimit=1000\nmessageburst=1000\n")
        murmur.seed()
        murmur.start()
        bots = []
        app = None
        shell = GnomeSession(args.gnome_shell, width + 160, height + 100,
                             log=(args.output / "gnome-shell.log").open("w"))
        pulse = None
        try:
            shell.start()
            if args.pulse_bin:
                socket_dir = shell.runtime / "pulse"
                socket_dir.mkdir(mode=0o700, exist_ok=True)
                script = shell.runtime / "default.pa"
                script.write_text(f"load-module module-native-protocol-unix socket={socket_dir / 'native'} auth-anonymous=1\n"
                                  "load-module module-null-sink sink_name=speakers sink_properties=device.description=Speakers\n"
                                  # A tone played into a null sink: its monitor is the microphone.
                                  # (Nuschel records nothing from module-sine-source; see TODO.md.)
                                  "load-module module-null-sink sink_name=microphone sink_properties=device.description=Mic\n"
                                  "load-module module-sine sink=microphone frequency=220\n"
                                  "update-source-proplist microphone.monitor device.description=\"Test microphone\"\n"
                                  "set-default-sink speakers\nset-default-source microphone.monitor\n")
                pulse = subprocess.Popen([str(args.pulse_bin / "pulseaudio"), "--daemonize=no", "--use-pid-file=no",
                                          "--exit-idle-time=-1", "-n", "--file=" + str(script)],
                                         env=shell.environment() | {"PULSE_RUNTIME_PATH": str(socket_dir),
                                                                    "PULSE_STATE_PATH": str(shell.runtime / "pulse-state")},
                                         stdout=(args.output / "pulse.log").open("w"), stderr=subprocess.STDOUT,
                                         start_new_session=True)
                eventually(lambda: (socket_dir / "native").exists(), "private PulseAudio did not start")
            pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
            for index, (name, channel) in enumerate([("Ada Lovelace", 2), ("Grace", 2), ("Linus", 5), ("Margaret", 7)]):
                bot = Client(args.binaries, "127.0.0.1", port, pin, name, root / f"bot{index}")
                bot.send(kind="join", channel=channel)
                bots.append(bot)
            time.sleep(1.2)
            profile = root / "profile"
            (profile / "config").mkdir(parents=True)
            bookmark = {"id": str(uuid.uuid4()), "name": "Screenshot server", "host": "127.0.0.1",
                        "port": port, "username": "Andi", "certificate_sha256": pin, "cached_channels": []}
            second = {"id": str(uuid.uuid4()), "name": "Friends", "host": "127.0.0.1",
                      "port": 1, "username": "Andi", "certificate_sha256": None, "cached_channels": []}
            (profile / "config/servers.json").write_text(json.dumps([bookmark, second]))
            env = shell.app_environment() | {"RUST_LOG": os.environ.get("NUSCHEL_SCREENSHOT_LOG", "nuschel=info")}
            log = (args.output / "desktop.log").open("w")
            app = subprocess.Popen([str(args.binaries / "nuschel"), "--profile-dir", str(profile),
                                    f"mumble://Andi@127.0.0.1:{port}/Lobby/Development"],
                                   env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            eventually(lambda: app.poll() is None and shell.window("Nuschel"), "Nuschel window did not appear")
            shell.place("Nuschel", 40, 40, width, height)
            # Channel messages reach only current members: wait until Andi has joined.
            bots[0].wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                         and e["user"]["channel"] == 2, timeout=30)
            time.sleep(1.0)
            avatars = [make_png(root / f"avatar{index}.png", 96, index) for index in range(2)]
            bots[0].send(kind="avatar", path=str(avatars[0]))
            bots[1].send(kind="avatar", path=str(avatars[1]))
            bots[0].send(kind="text", channel=2, body="Hi! Here is the patch:\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```")
            time.sleep(0.4)
            bots[1].send(kind="text", channel=2, body="Looks good to me.\nShip it!")
            time.sleep(0.4)
            # Encoded as stock Mumble's Log::imageToImg does: base64 in 72-character
            # lines, each percent-encoded.
            encoded = base64.b64encode(make_png(root / "shot.png", 240, 7, height=135).read_bytes()).decode()
            mumble_image = "\n".join(urllib.parse.quote(encoded[i:i + 72], safe="")
                                     for i in range(0, len(encoded), 72))
            bots[0].send(kind="html", channel=2,
                         html=f"Here is a screenshot:<br/><img src=\"data:image/PNG;base64,{mumble_image}\" />")
            time.sleep(args.settle)

            def shot(name):
                window = shell.window("Nuschel")
                path = shell.screenshot(args.output / f"{name}.png",
                                        crop=(window["x"] - 12, window["y"] - 12, window["width"] + 24, window["height"] + 24))
                print(path, flush=True)

            shot("01-connected")
            scenes = set(args.scenes.split(","))

            def want(scene):
                return "all" in scenes or scene in scenes

            def click(x, y, count=1, button=1):
                # Coordinates are pixels in the window screenshots above.
                window = shell.window("Nuschel")
                shell.click(window["x"] - 12 + x, window["y"] - 12 + y, button=button, count=count)

            def hover(x, y):
                window = shell.window("Nuschel")
                shell.move(window["x"] - 12 + x, window["y"] - 12 + y)

            def wheel(x, y, clicks):
                window = shell.window("Nuschel")
                shell.wheel(window["x"] - 12 + x, window["y"] - 12 + y, clicks)

            def drag(start, end):
                window = shell.window("Nuschel")
                origin = (window["x"] - 12, window["y"] - 12)
                shell.drag((origin[0] + start[0], origin[1] + start[1]), (origin[0] + end[0], origin[1] + end[1]))
            click(width // 2, 140)  # the first click after mapping can be swallowed; spend it on the timeline
            time.sleep(0.4)

            def clipboard():
                shell.eval(READ_CLIPBOARD)
                time.sleep(0.5)
                return shell.eval(CLIPBOARD_TEXT)

            def copy_shortcut():
                # Ctrl+C on the virtual keyboard: press Control, press and release C, release Control.
                keyval = "imports.gi.Clutter.KEY_"
                state = "imports.gi.Clutter.KeyState."
                shell.eval("const k = global._nuschelKeyboard; const t = imports.gi.GLib.get_monotonic_time();"
                           f" k.notify_keyval(t, {keyval}Control_L, {state}PRESSED);"
                           f" k.notify_keyval(t + 10000, {keyval}c, {state}PRESSED);"
                           f" k.notify_keyval(t + 20000, {keyval}c, {state}RELEASED);"
                           f" k.notify_keyval(t + 30000, {keyval}Control_L, {state}RELEASED); 'ok'")
                time.sleep(0.4)

            if want("select"):
                # Runs first, while the patch is where the coordinates expect it.
                click(*COPY_CODE)
                time.sleep(0.6)
                shot("27-code-copied")
                copied = clipboard()
                assert copied == 'fn main() {\n    println!("hello");\n}', copied
                # Drag over Grace's reply: "good to me." and "Ship".
                drag(GOOD_WORD, SHIP_END)
                time.sleep(0.6)
                shot("28-text-selected")
                copy_shortcut()
                copied = clipboard()
                assert copied.startswith("good to me.\nShi"), copied
                click(SHIP_END[0] + 40, SHIP_END[1] - 12, button=3)  # right-click the selected message
                time.sleep(0.6)
                shot("29-message-menu")
                click(SHIP_END[0] + 80, SHIP_END[1] + 46)  # "Copy message", below "Copy"
                time.sleep(0.4)
                copied = clipboard()
                assert copied == "Looks good to me.\nShip it!", copied
                # Across a text and a code block, then a word by double click.
                drag(PATCH_WORD, PRINTLN_WORD)
                time.sleep(0.6)
                shot("30-selected-into-code")
                copy_shortcut()
                copied = clipboard()
                assert copied.startswith("patch:\nfn main() {\n    pr"), copied
                click(SHIP_END[0] - 20, SHIP_END[1], count=2)
                time.sleep(0.4)
                copy_shortcut()
                copied = clipboard()
                assert copied == "Ship", copied
                click(width // 2, 140)  # clears the selection
                time.sleep(0.4)
            if want("links"):
                # Only the mumble link is clicked: an http one would start a browser.
                bots[0].send(kind="html", channel=2, html=(
                    "Read <a href=\"https://docs.example/guide\">the guide</a> or https://example.org/faq. "
                    f"Then <a href=\"mumble://Andi@127.0.0.1:{port}/Lobby\">come to the Lobby</a>"
                    " <a href=\"javascript:alert(1)\">(not a link)</a>"))
                time.sleep(1.5)
                shot("31-links")
                hover(*GUIDE_LINK)
                time.sleep(0.8)
                shot("32-link-hover")
                click(*GUIDE_LINK, button=3)
                time.sleep(0.6)
                shot("33-link-menu")
                click(GUIDE_LINK[0] + 40, GUIDE_LINK[1] + 22)  # "Copy link"
                time.sleep(0.4)
                copied = clipboard()
                assert copied == "https://docs.example/guide", copied
                # A mumble link to this server's Lobby: Nuschel goes there itself.
                click(*LOBBY_LINK)
                time.sleep(1.5)
                shot("34-mumble-link")
                click(228, 299, count=2)  # back into Development, as the other scenes expect
                time.sleep(1.5)
            if want("rich"):
                # Formatting as stock Mumble's rich text editor and markdown send it.
                bots[1].send(kind="html", channel=2, html=(
                    "<h2>Release notes</h2><p><b>Bold</b>, <i>italic</i>, <u>underlined</u>, <s>struck</s>, "
                    "<code>inline code</code>, <span style=\" color:#000080;\">navy</span> and "
                    "<font color=\"#ff8800\">orange</font>.</p><ul><li>First</li><li>Second<ul><li>Nested</li>"
                    "</ul></li></ul><ol><li>One</li><li>Two</li></ol><blockquote>A quote<br/>on two lines</blockquote>"
                    "<hr/><pre>fn main() {\n    println!(\"hi\");\n}</pre>"
                    "<table><tr><th>Key</th><th>Value</th></tr><tr><td>a</td><td>1</td></tr></table>"))
                time.sleep(1.5)
                shot("35-rich-text")
                click(461, 685)  # Show more
                time.sleep(1.0)
                shot("36-rich-text-expanded")
                click(461, 685)  # Show less, so the timeline is as later scenes expect
                time.sleep(0.6)

            if want("dialog"):
                click(59, 227)
                time.sleep(1.0)
                shot("02-add-server")
                # The address has focus, and the name follows it.
                shell.type("voice.example.org")
                time.sleep(0.6)
                shot("02a-add-server-typed")
                shell.key("Escape")
                time.sleep(0.5)
            if want("settings"):
                click(239, 773)
                time.sleep(1.0)
                shot("04-settings")
                # Still muted: the echo test opens the sine microphone itself
                # (and so starts the microphone test with its meter).
                click(*ECHO_TEST_BUTTON)
                time.sleep(2.0)
                shot("05-settings-live")
                time.sleep(5.0)
                shot("05b-echo-test")
                shell.key("Escape")
                time.sleep(0.4)
            if want("server"):
                click(*SERVER_SETTINGS_BUTTON)
                time.sleep(1.0)
                shot("02b-server-settings")
                shell.key("Escape")
                time.sleep(0.4)
                # Collapse Lobby: its people show as a badge, and widen the list.
                click(*LOBBY_CHEVRON)
                time.sleep(0.6)
                drag(SIDEBAR_EDGE, (SIDEBAR_EDGE[0] + 140, SIDEBAR_EDGE[1]))
                time.sleep(0.8)
                shot("16-collapsed-wide")
            if want("menus"):
                bots[2].send(kind="join", channel=2)  # Linus joins Development
                time.sleep(0.8)
                bots[3].close()  # Margaret disconnects
                bots.pop(3)
                time.sleep(1.2)
                shot("06-activity")
                click(236, 301)  # left: opens a DM with Ada
                time.sleep(0.8)
                shot("06b-direct")
                click(236, 301, button=3)  # Ada Lovelace
                time.sleep(0.8)
                shot("07-user-menu")
                click(290, 317)  # first entry: profile
                time.sleep(1.5)
                shot("08-profile")
                shell.key("Escape")
                time.sleep(0.4)
                click(200, 233, button=3)  # Lobby
                time.sleep(0.8)
                shot("09-channel-menu")
                shell.key("Escape")
                time.sleep(0.4)
            if want("long"):
                click(228, 268)  # back to #Development (the menus scene opens a DM)
                time.sleep(0.6)
                # One paragraph that fits when wide but wraps past the collapse
                # height in a narrow window, and one that is always long.
                paragraph = " ".join(f"word{n}" for n in range(100))
                bots[1].send(kind="text", channel=2, body=paragraph)
                time.sleep(0.4)
                bots[0].send(kind="text", channel=2, body="\n".join(f"line {n}" for n in range(40)))
                time.sleep(1.5)
                shot("10-long-wide")
                shell.place("Nuschel", 40, 40, 900, height)
                time.sleep(1.5)
                shot("11-long-narrow")
                shell.place("Nuschel", 40, 40, width, height)
                time.sleep(1.0)
            if want("search"):
                click(*SEARCH_BUTTON(width))
                time.sleep(0.8)
                shell.type("patch")
                time.sleep(1.5)
                shot("12-search")
                shell.key("Escape")
                time.sleep(0.6)
                shot("13-search-closed")
            if want("toggles"):
                # Mute animates (slash, red); rapid clicks reverse it smoothly.
                def footer(name):
                    shot(name)
                    subprocess.run(["magick", str(args.output / f"{name}.png"), "-crop", "130x60+210+743",
                                    str(args.output / f"{name}-footer.png")], check=False)
                footer("30-muted")
                click(279, 773)
                time.sleep(0.6)
                footer("31-unmuted")
                click(279, 773)
                footer("32-muting")
                time.sleep(0.6)
                footer("33-muted-again")
                for _ in range(7):
                    click(279, 773)
                    time.sleep(0.04)
                time.sleep(0.6)
                footer("34-after-rapid-clicks")
            if want("emoji"):
                # Ctrl+. opens the picker; typing searches; Enter inserts the first match.
                click(800, 738)  # the composer
                time.sleep(0.4)
                shell.chord("Control_L", "period")
                time.sleep(1.0)
                shot("27-emoji")
                shell.type("thumbs")
                time.sleep(0.8)
                shot("28-emoji-search")
                shell.key("Return")
                time.sleep(0.8)
                shot("29-emoji-inserted")
            if want("clipping") and args.pulse_bin:
                # A second microphone driven to full scale: the adaptive controller warns.
                server = f"--server=unix:{shell.runtime / 'pulse' / 'native'}"
                pactl = [str(args.pulse_bin / "pactl"), server]
                subprocess.run([*pactl, "load-module", "module-null-sink", "sink_name=hot",
                                "sink_properties=device.description=Hot"], check=True, capture_output=True)
                subprocess.run([*pactl, "set-source-volume", "hot.monitor", "100%"], check=True)
                subprocess.run([*pactl, "set-default-source", "hot.monitor"], check=True)
                square = root / "square.raw"
                square.write_bytes((struct.pack("<f", 1.0) * 24 + struct.pack("<f", -1.0) * 24) * 1000)
                player = subprocess.Popen(
                    ["sh", "-c", f'while :; do cat "{square}"; done | "{args.pulse_bin / "pacat"}" {server} '
                                 '--device=hot --raw --format=float32le --rate=48000 --channels=1'],
                    start_new_session=True)
                try:
                    click(279, 773)  # unmute: the worker restarts on the new default microphone
                    time.sleep(9)
                    shot("14-clipping")
                    click(*CLIPPING_WARNING)
                    time.sleep(1.0)
                    shot("15-clipping-settings")
                    shell.key("Escape")
                    time.sleep(0.4)
                finally:
                    os.killpg(player.pid, signal.SIGTERM)
            if want("viewer"):
                click(*CHAT_IMAGE)
                time.sleep(2.0)
                shot("17-viewer")
                click(width // 2, height // 2 - 40, button=3)  # right-click the enlarged image
                time.sleep(0.8)
                shot("18-image-menu")
                shell.key("Escape")
                time.sleep(0.3)
                shell.key("Escape")
                time.sleep(0.5)
            if want("notify"):
                shell.eval(CLAIM_NOTIFICATIONS)
                andi = next(e["user"]["session"] for e in reversed(bots[1].events)
                            if e["kind"] == "user" and e["user"]["name"] == "Andi")
                click(*LOBBY_ROW)  # look at another channel: Development is out of sight
                time.sleep(0.8)
                bots[1].send(kind="direct", session=andi, body="hey Andi, <b>got a minute</b>? & more")
                time.sleep(2.0)
                path = shell.screenshot(args.output / "19-notification-direct.png")
                print(path, flush=True)
                bots[0].send(kind="html", channel=2,
                             html=f"<img src=\"data:image/PNG;base64,{mumble_image}\" />")
                time.sleep(2.0)
                path = shell.screenshot(args.output / "20-notification-image.png")
                print(path, flush=True)
                failures = [line for line in (args.output / "desktop.log").read_text().splitlines()
                            if "notification failed" in line]
                assert not failures, failures
                received = json.loads(shell.eval(
                    "JSON.stringify(Main.messageTray.getSources().flatMap(source => source.notifications"
                    ".map(note => ({title: note.title, body: note.body, gicon: Boolean(note.gicon)}))))"))
                print("GNOME received:", received, flush=True)
                # GNOME keeps the body as markup: the message's own tags must arrive escaped.
                assert any(note["title"] == "Grace"
                           and "&lt;b&gt;got a minute&lt;/b&gt;? &amp; more" in note["body"]
                           for note in received), received
                assert any(note["title"].endswith("in Development") and note["gicon"] for note in received), received
            if want("scroll"):
                click(228, 268)  # #Development (other scenes may have opened another conversation)
                time.sleep(0.6)
                # Several 40-message pages of local history, paced for the content worker's queue.
                for n in range(1, 151):
                    bots[(n // 3) % 2].send(kind="text", channel=2, body=f"History message {n}")
                    time.sleep(0.05)
                time.sleep(2.0)
                # They arrived live; reopened, the conversation starts from its latest page.
                click(*LOBBY_ROW)
                time.sleep(0.8)
                click(228, 268)
                time.sleep(4.0)
                shot("21-scroll-latest")
                timeline = (width // 2 + 160, height // 2)
                # Up the latest page, short of a screen below its top: nothing loads yet.
                wheel(*timeline, -5)
                time.sleep(0.8)
                # Then to its top: the previous page is on its way (one
                # content worker renders it, so it may already be there)…
                wheel(*timeline, -12)
                time.sleep(0.4)
                shot("22-scroll-loading")
                # …and is prepended above without moving what is on screen.
                time.sleep(4.0)
                shot("23-scroll-prepended")
                # Further up until local history begins.
                for _ in range(6):
                    wheel(*timeline, -2 * SCROLL_CLICKS)
                    time.sleep(3.0)
                shot("24-scroll-beginning")
                wheel(*timeline, SCROLL_CLICKS)
                time.sleep(0.6)
                bots[0].send(kind="text", channel=2, body="A live message while you read older ones")
                time.sleep(1.2)
                shot("25-scroll-new-messages")
                click(*JUMP_TO_LATEST(width, height))
                time.sleep(1.0)
                shot("26-scroll-jumped")
            if want("paste"):
                click(228, 268)  # #Development
                time.sleep(0.6)
                photo = make_photo(root / "paste.png", 1920, 1080)
                shell.eval(SET_CLIPBOARD % json.dumps(str(photo)))
                click(*COMPOSER)
                time.sleep(0.4)
                started = time.monotonic()
                shell.eval(PASTE)
                sent = bots[0].wait(lambda e: e["kind"] == "text" and e["author"] == "Andi", timeout=60)
                print(f"pasted image sent after {time.monotonic() - started:.1f} s: "
                      f"{len(sent['body'])} bytes, {sent['body'][:40]}", flush=True)
                # Stock Mumble's format, within the stock image message limit.
                assert sent["html"] and sent["body"].startswith('<img src="data:image/jpeg;base64,'), sent["body"][:80]
                assert len(sent["body"]) <= 131072 and ("%2B" in sent["body"] or "%2F" in sent["body"])
                time.sleep(2.0)
                shot("27-pasted-image")
            if want("maximize"):
                click(width // 2, 41, count=2)
                time.sleep(1.5)
                window = shell.window("Nuschel")
                path = shell.screenshot(args.output / "03-maximized.png")
                print(path, flush=True)
                shell.click(width // 2, window["y"] + 18, count=2)
                time.sleep(1.0)
        finally:
            if app and app.poll() is None:
                os.killpg(app.pid, signal.SIGTERM)
                try:
                    app.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    os.killpg(app.pid, signal.SIGKILL)
            for bot in bots:
                try:
                    bot.close()
                except Exception:  # noqa: BLE001 - best-effort cleanup
                    pass
            if pulse and pulse.poll() is None:
                os.killpg(pulse.pid, signal.SIGTERM)
            shell.stop()
            murmur.stop()


if __name__ == "__main__":
    main()
