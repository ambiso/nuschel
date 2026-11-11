#!/usr/bin/env python3
"""Nuschel's server (nuscheld) on 127.0.0.1 with the stock fixture's channels.

The same channel tree and SuperUser password as local_murmur.py, so the client
scripts run unchanged against both servers. Channels are created over the
protocol by SuperUser (with a generous rate limit), then the server restarts
with Murmur's default limits: seeding also checks that state survives a restart.
"""
import os
from pathlib import Path
import signal
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from local_murmur import CHANNELS, eventually, free_port  # noqa: E402,F401


class Nuscheld:
    SUPERUSER_PASSWORD = "nuschel-super"

    def __init__(self, binaries, root, port, extra=""):
        self.binaries, self.root, self.port = Path(binaries), Path(root), port
        # "compatible" for seeding (stock clients create the channels); a test may
        # switch to "e2ee" afterwards: the state carries over.
        self.security = "compatible"
        self.config = self.root / "nuscheld.toml"
        self.password = self.root / "superuser-password"
        self.password.write_text(self.SUPERUSER_PASSWORD)
        self.log = self.root / "nuscheld.log"
        self.extra = extra
        self.process = None
        self.write_config(message_limit=1, message_burst=5)

    def write_config(self, message_limit, message_burst):
        self.config.write_text(
            f'security = "{self.security}"\nbind = ["127.0.0.1"]\nport = {self.port}\n'
            f'state_directory = "{self.root / "state"}"\n'
            f'superuser_password_file = "{self.password}"\n'
            'welcome_text = "Welcome to the <b>Nuschel</b> test server."\n'
            'max_bandwidth = 72000\nmax_users = 50\n'
            f"message_limit = {message_limit}\nmessage_burst = {message_burst}\n"
            f"{self.extra}\n"
            "[autoban]\nattempts = 0\n"
            # Cheap hashing keeps logins fast in tests; production uses the default.
            "[password_hashing]\nmemory_kib = 1024\npasses = 1\n")

    def start(self):
        self.process = subprocess.Popen(
            [str(self.binaries / "nuscheld"), "--config", str(self.config)],
            stdout=subprocess.DEVNULL, stderr=self.log.open("a"), start_new_session=True)
        eventually(self.listening, f"nuscheld did not start: {self.log.read_text()[-2000:]}")

    def listening(self):
        if self.process.poll() is not None:
            raise AssertionError(f"nuscheld exited: {self.log.read_text()[-2000:]}")
        import socket
        try:
            socket.create_connection(("127.0.0.1", self.port), timeout=0.5).close()
            return True
        except OSError:
            return False

    def stop(self):
        if self.process and self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=5)
        assert self.process is None or self.process.returncode in (0, -signal.SIGKILL), self.log.read_text()[-2000:]

    def seed(self):
        from seed_server import seed
        self.write_config(message_limit=100, message_burst=100)
        self.start()
        try:
            seed(self.binaries, "127.0.0.1", self.port, self.password, CHANNELS, self.root / "seed")
        finally:
            self.stop()
        self.write_config(message_limit=1, message_burst=5)
