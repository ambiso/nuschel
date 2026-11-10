#!/usr/bin/env python3
"""Desktop recovery: server restarts and worker failures (run inside xvfb-run).

Runs its own loopback stock Mumble server so it can be restarted. Checks that
the desktop reconnects with backoff, rejoins its channel with mute state
intact, marks the disconnected interval in the timeline, and that killing the
media or storage worker neither drops the session nor loses chat history.
"""
import argparse
import json
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from desktop_mumble import DesktopTest, eventually, terminate  # noqa: E402
from local_murmur import Murmur, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402


def worker_pids(name):
    result = subprocess.run(["pgrep", "-f", name], capture_output=True, text=True)
    return [int(pid) for pid in result.stdout.split()]


class RecoveryTest(DesktopTest):
    def run(self):
        port = free_port()
        (self.root / "murmur").mkdir()
        murmur = Murmur(self.args.murmur, self.root / "murmur", port)
        murmur.seed()
        murmur.start()
        try:
            pin = certificate_pin(self.args.binaries, "127.0.0.1", port, self.root / "probe")
            self.peer = Client(self.args.binaries, "127.0.0.1", port, pin, "RecoveryPeer", self.root / "peer")
            (self.profile / "config").mkdir(parents=True)
            (self.profile / "config/servers.json").write_text(json.dumps([dict(
                id=str(uuid.uuid4()), name="Recovery", host="127.0.0.1", port=port, username="RecoveryDesktop",
                certificate_sha256=pin)]))
            self.start_audio()
            self.args.url = f"mumble://RecoveryDesktop@127.0.0.1:{port}/Lobby"
            self.start_app()
            desktop = self.peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "RecoveryDesktop"
                                     and e["user"]["channel"] == 1, timeout=30)["user"]
            self.wait_text("Connected")

            # Worker failures keep the session and history working.
            for pid in worker_pids("nuschel-media-worker --live"):
                subprocess.run(["kill", "-9", str(pid)], check=False)
            eventually(lambda: self.pulse_json("sink-inputs"), "media worker was not restarted", timeout=20)
            for pid in worker_pids("nuschel-storage-worker"):
                subprocess.run(["kill", "-9", str(pid)], check=False)
            marker = "AFTERSTORAGEWORKERKILL"
            self.peer.send(kind="join", channel=1)
            time.sleep(1.1)
            self.peer.send(kind="text", channel=1, body=marker)
            self.peer.send(kind="text", channel=1, body=marker + "2")
            eventually(lambda: self.has_message(marker + "2"), "history did not recover after a storage worker crash")
            assert self.peer.users[desktop["session"]]["channel"] == 1, "worker failure moved or dropped the session"
            print("Media and storage worker crashes recovered without dropping the session.", flush=True)

            # Server restart: reconnect with backoff, rejoin the channel, keep mute state.
            self.peer.close()
            self.peer = None
            murmur.stop()
            self.wait_text("Reconnecting")
            self.capture("reconnecting")
            murmur.start()
            self.peer = Client(self.args.binaries, "127.0.0.1", port, pin, "RecoveryPeer", self.root / "peer")
            back = self.peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "RecoveryDesktop"
                                  and e["user"]["channel"] == 1, timeout=60)["user"]
            assert back["self_muted"], "reconnect lost the mute state"
            self.wait_text("Disconnected from")
            self.capture("reconnected")
            print("Server restart: reconnected, rejoined Lobby muted and marked the history gap.", flush=True)
        finally:
            if self.peer:
                self.peer.close()
                self.peer = None
            murmur.stop()

    def start_app(self):
        url = getattr(self.args, "url", None)
        self.app = subprocess.Popen([str(self.args.binaries / "nuschel"), "--profile-dir", str(self.profile), url],
                                    env=self.env, stdout=(self.output / "desktop.log").open("a"),
                                    stderr=subprocess.STDOUT, start_new_session=True)

        def window():
            if self.app.poll() is not None:
                raise AssertionError((self.output / "desktop.log").read_text()[-4000:])
            result = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "^Nuschel"],
                                    env=self.env, capture_output=True, text=True)
            return result.stdout.splitlines()[0] if result.returncode == 0 else None
        self.window = eventually(window, "Nuschel did not open a window")
        self.xdo("windowsize", self.window, "1120", "850")
        self.xdo("windowmove", self.window, "0", "0")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschel-recovery-") as temporary:
        test = RecoveryTest(args, Path(temporary), args.output)
        try:
            test.run()
        except BaseException:
            if test.app is not None and test.app.poll() is None and test.window:
                test.capture("failure")
            raise
        finally:
            test.close()
