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
    parser.add_argument("--scenes", default="all", help="comma-separated: dialog,settings,menus,long,search,clipping,maximize")
    parser.add_argument("--pulse-bin", type=Path, help="start a private PulseAudio with a sine-wave microphone")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    width, height = map(int, args.size.split("x"))
    with tempfile.TemporaryDirectory(prefix="nuschel-screens-") as temporary:
        root = Path(temporary)
        port = free_port()
        murmur = Murmur(args.murmur, root, port)
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
                                  "load-module module-sine-source source_name=microphone frequency=220 rate=48000\n"
                                  "update-source-proplist microphone device.description=\"Test microphone\"\n"
                                  "set-default-sink speakers\nset-default-source microphone\n")
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
            env = shell.app_environment() | {"RUST_LOG": "nuschel=info"}
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
            screenshot_png = base64.b64encode(make_png(root / "shot.png", 320, 7, height=180).read_bytes()).decode()
            bots[0].send(kind="html", channel=2, html=f"Here is a screenshot:<br/><img src=\"data:image/png;base64,{screenshot_png}\"/>")
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
                click(1200, 400)  # backdrop click closes the dialog
                time.sleep(0.4)
                click(279, 773)  # unmute: the sine microphone drives the meter
                time.sleep(2.5)
                click(239, 773)
                time.sleep(1.5)
                shot("05-settings-live")
                shell.key("Escape")
                time.sleep(0.4)
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
