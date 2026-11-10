#!/usr/bin/env python3
"""Capture desktop screenshots against a private loopback stock Mumble server.

Development aid: `python3 tests/screenshots.py target/debug --murmur <mumble-server>`
under `xvfb-run`. Only a server bound to 127.0.0.1 is used; no host audio devices
are opened (the media worker is pointed at a private PulseAudio if one is given).
"""
import argparse
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

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--size", default="1280x800")
    parser.add_argument("--settle", type=float, default=4.0)
    parser.add_argument("--scenes", default="all")
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
            time.sleep(1.0)
            bots[0].send(kind="text", channel=2, body="Hi! Here is the patch:\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```")
            time.sleep(0.4)
            bots[1].send(kind="text", channel=2, body="Looks good to me.\nShip it!")
            time.sleep(args.settle)

            def shot(name):
                window = shell.window("Nuschel")
                path = shell.screenshot(args.output / f"{name}.png",
                                        crop=(window["x"] - 12, window["y"] - 12, window["width"] + 24, window["height"] + 24))
                print(path, flush=True)

            shot("01-connected")

            def click(x, y, count=1, button=1):
                # Coordinates are pixels in the window screenshots above.
                window = shell.window("Nuschel")
                shell.click(window["x"] - 12 + x, window["y"] - 12 + y, button=button, count=count)

            if args.scenes in ("all", "dialog"):
                click(59, 227)
                time.sleep(1.0)
                shot("02-add-server")
                shell.key("Escape")
                time.sleep(0.5)
            if args.scenes in ("all", "settings"):
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
            if args.scenes in ("all", "menus"):
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
            if args.scenes in ("all", "maximize"):
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
