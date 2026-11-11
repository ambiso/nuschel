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


# GNOME Shell exports its freedesktop notification daemon but, headless, never
# claims the bus name: claim it, so Nuschel's notifications reach the real one.
CLAIM_NOTIFICATIONS = """
imports.gi.Gio.bus_own_name_on_connection(imports.gi.Gio.DBus.session,
    'org.freedesktop.Notifications', 0, null, null);
'ok'
"""


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
    parser.add_argument("--scenes", default="all", help="comma-separated: dialog,settings,server,menus,long,search,clipping,viewer,notify,scroll,maximize")
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

            def wheel(x, y, clicks):
                window = shell.window("Nuschel")
                shell.wheel(window["x"] - 12 + x, window["y"] - 12 + y, clicks)

            def drag(start, end):
                window = shell.window("Nuschel")
                origin = (window["x"] - 12, window["y"] - 12)
                shell.drag((origin[0] + start[0], origin[1] + start[1]), (origin[0] + end[0], origin[1] + end[1]))
            click(width // 2, 140)  # the first click after mapping can be swallowed; spend it on the timeline
            time.sleep(0.4)

            if want("dialog"):
                click(59, 227)
                time.sleep(1.0)
                shot("02-add-server")
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
                # Then to its top: the previous page is on its way (a debug
                # build takes a moment, rendering each message anew)…
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
