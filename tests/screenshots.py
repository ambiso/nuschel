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
import socket
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gnome_session import GnomeSession  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402

CHANNELS = [
    (1, 0, "Lobby"),
    (2, 1, "Development"),
    (3, 1, "Gaming"),
    (4, 3, "Strategy and very long channel names that need an ellipsis in the tree"),
    (5, 3, "Shooters"),
    (6, 0, "Music"),
    (7, 6, "Listening party"),
    (8, 0, "AFK"),
]


def free_port():
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()[1]


def eventually(predicate, message, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.2)
    raise AssertionError(message)


class Murmur:
    def __init__(self, binary, root, port):
        self.binary, self.root, self.port = binary, root, port
        self.database = root / "murmur.sqlite"
        self.ini = root / "murmur.ini"
        self.ini.write_text(
            f"database={self.database}\nport={port}\nhost=127.0.0.1\nautobanAttempts=0\n"
            f"logfile={root / 'murmur.log'}\npidfile={root / 'murmur.pid'}\n"
            "welcometext=\"Welcome to the <b>Nuschel</b> screenshot server.\"\n"
            "allowhtml=true\nbandwidth=72000\nusers=50\n")
        self.process = None

    def start(self):
        self.process = subprocess.Popen([str(self.binary), "-ini", str(self.ini), "-fg"],
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                        start_new_session=True)
        eventually(self.listening, "stock Mumble server did not start")

    def listening(self):
        try:
            socket.create_connection(("127.0.0.1", self.port), timeout=0.5).close()
            return True
        except OSError:
            return False

    def stop(self):
        if self.process and self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            self.process.wait(timeout=10)

    def seed(self):
        self.start()
        self.stop()
        with sqlite3.connect(self.database) as database:
            database.executemany(
                "INSERT OR IGNORE INTO channels (server_id, channel_id, parent_id, name, inheritacl)"
                " VALUES (1, ?, ?, ?, 1)", CHANNELS)
            database.execute(
                "INSERT OR REPLACE INTO channel_info (server_id, channel_id, key, value)"
                " VALUES (1, 2, 0, 'Rust, protocols and <i>audio</i> work')")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--size", default="1280x800")
    parser.add_argument("--settle", type=float, default=4.0)
    parser.add_argument("--scenes", default="all")
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
        try:
            shell.start()
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
            shell.stop()
            murmur.stop()


if __name__ == "__main__":
    main()
