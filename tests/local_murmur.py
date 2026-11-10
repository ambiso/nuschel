#!/usr/bin/env python3
"""A stock Mumble server on 127.0.0.1 with seeded channels, for local tests.

Only loopback is used. The VM suite uses the NixOS murmur service instead.
"""
import os
from pathlib import Path
import signal
import socket
import sqlite3
import subprocess
import time

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
