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
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from desktop_mumble import DesktopTest, eventually, terminate  # noqa: E402
from local_murmur import Murmur, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402


def worker_pids(app, binary):
    """The app's own workers run from `binary`, never another Nuschel's (another
    test's, or the user's own): its descendants only. Sandboxed, a worker runs
    as /worker under bubblewrap, whose command line binds `binary` there; the
    worker is the process at the bottom of that bwrap."""
    parents, children, commands = {}, {}, {}
    for stat in Path("/proc").glob("[0-9]*/stat"):
        pid = int(stat.parent.name)
        try:
            parents[pid] = int(stat.read_text().rsplit(")", 1)[1].split()[1])
            commands[pid] = (stat.parent / "cmdline").read_bytes().replace(b"\0", b" ").decode()
        except (OSError, IndexError, ValueError):
            continue
        children.setdefault(parents[pid], []).append(pid)

    def below(pid):
        for child in children.get(pid, []):
            yield child
            yield from below(child)
    found = []
    for pid in below(app.pid):
        if binary in commands[pid] and binary not in commands.get(parents[pid], ""):
            found += [leaf for leaf in below(pid) if not children.get(leaf)] or [pid]
    return found


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
            self.start_app(f"mumble://RecoveryDesktop@127.0.0.1:{port}/Lobby")
            desktop = self.peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "RecoveryDesktop"
                                     and e["user"]["channel"] == 1, timeout=30)["user"]
            self.connected()

            # Worker failures keep the session and history working.
            streams = eventually(lambda: {stream["index"] for stream in self.pulse_json("sink-inputs")},
                                 "the desktop did not start playback")
            media = worker_pids(self.app, "nuschel-media-worker")
            assert media, "the desktop has no media worker to kill"
            for pid in media:
                subprocess.run(["kill", "-9", str(pid)], check=False)
            # A new worker plays through a stream of its own.
            eventually(lambda: {stream["index"] for stream in self.pulse_json("sink-inputs")} - streams,
                       "media worker was not restarted", timeout=20)
            storage = worker_pids(self.app, "nuschel-storage-worker")
            assert storage, "the desktop has no storage worker to kill"
            for pid in storage:
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
            self.wait_for(None, "Reconnecting", partial=True)
            self.capture("reconnecting")
            murmur.start()
            self.peer = Client(self.args.binaries, "127.0.0.1", port, pin, "RecoveryPeer", self.root / "peer")
            back = self.peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "RecoveryDesktop"
                                  and e["user"]["channel"] == 1, timeout=60)["user"]
            assert back["self_muted"], "reconnect lost the mute state"
            self.wait_for("list item", "Disconnected from", partial=True)
            self.capture("reconnected")
            print("Server restart: reconnected, rejoined Lobby muted and marked the history gap.", flush=True)

            # Starting Nuschel again (no link) connects and rejoins the channel it was last in.
            terminate(self.app)
            self.start_app()
            again = self.peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "RecoveryDesktop"
                                   and e["user"]["channel"] == 1, timeout=60)["user"]
            assert again["self_muted"] and again["self_deafened"], "a fresh start must join muted and deafened"
            print("Restart: Nuschel rejoined Lobby, its last channel.", flush=True)
        finally:
            if self.peer:
                self.peer.close()
                self.peer = None
            murmur.stop()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True, help="at-spi2-core package (bus launcher, registry)")
    parser.add_argument("--dbus-daemon", type=Path, default=shutil.which("dbus-daemon"),
                        help="dbus-daemon for the private session bus")
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
