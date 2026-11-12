#!/usr/bin/env python3
"""Recovered speech-detector faults keep the bot's real Mumble TLS session.

Usage: with_local_murmur.py <mumble-server> tests/musicbot_duck_faults.py <binaries> PORT
           --espeak <espeak-ng> --opus-library <libopus.so>
       musicbot_duck_faults.py <binaries> 64738 --service --espeak ... --opus-library ...
           (as root, in the owned NixOS VM: the running nuschl-musicbot.service)

<binaries> holds nuschl-cli and a packaged nuschl-musicbot whose codec worker
sits in ../libexec/nuschl-musicbot. Only an owned loopback stock server, a
local fake library and generated speech are used. The bot's actual confined
duck worker is stopped past its request deadline, then stopped and killed in
the middle of requests; after each fault the same TLS session (same session
ID, no reconnection) must still play music to listeners and answer text, and
the faulted worker must have been reaped. The third fault suspends ducking
with one notice. An unconfirmed child exit cannot be produced with a real
process; mumble.rs's real_session tests cover that path with a stand-in.

--service faults the NixOS service's own duck worker instead (stopped past the
deadline, then a fresh one killed mid-request). A worker is signalled only after
positive identification: the service's cgroup, a descendant of its main
process, the fixed `--codec-worker duck` argv, and the device/inode of the
packaged worker binary; exactly one match, pinned with a pidfd and checked
again before any signal. Continuity comes from Murmur's session ID, the unit's
journal, music heard and !web answered.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from musicbot import (  # noqa: E402
    API_KEY, MUSIC, PASSWORD, BotProcess, Client, Library, api, audio_from, certificate_pin,
    close_resources, command, connected, drain, ducked_while_talking, eventually, link_from,
    session_of, speech_packets, talk, write_config,
)

DUCK = [b"/worker", b"--codec-worker", b"duck", b""]
NOTICE = ("Ducking stopped on this connection after the speech detector failed three times; "
          "it resumes after the next reconnection.")


def descendants(root):
    children = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            parent = int((entry / "stat").read_text().rsplit(")", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            continue
        children.setdefault(parent, []).append(int(entry.name))
    found, pending = [], [root]
    while pending:
        for child in children.get(pending.pop(), []):
            found.append(child)
            pending.append(child)
    return found


def duck_workers(bot):
    workers = []
    for pid in descendants(bot.process.pid):
        try:
            if (Path("/proc") / str(pid) / "cmdline").read_bytes().split(b"\0") == DUCK:
                workers.append(pid)
        except OSError:
            continue
    return workers


def only_duck_worker(bot):
    return eventually(lambda: duck_workers(bot), lambda workers: len(workers) == 1,
                      "exactly one confined duck worker")[0]


def reaped(bot, pid):
    """The faulted worker is gone (not a zombie) and no replacement runs yet."""
    def gone():
        try:
            state = (Path("/proc") / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()[0]
        except OSError:
            state = None
        return state is None and not duck_workers(bot)
    eventually(gone, bool, f"duck worker {pid} reaped", timeout=10)


def same_session(bot, alice, dj, link, label, notice=None):
    """The bot's original TLS session is still the one on the server, both ways."""
    assert session_of(alice, "DJ") == dj and dj in alice.users, (label, alice.users)
    log = "".join(bot.log)
    for marker in ("connecting again", "session failed", "session task failed", "exit unconfirmed"):
        assert marker not in log, (label, marker, log[-2000:])
    channel = api(link)["channels"][0]
    assert channel["connected"] and channel["error"] == notice, (label, channel)
    drain(alice)
    heard = audio_from(alice, dj, 5)
    assert heard, f"{label}: no music after the fault"
    assert link_from(alice, dj).startswith("http"), label
    return len(heard)


def stop_and_kill_mid_request(talker, pid, speech):
    """Stops the worker, starts talking, then kills it while a request waits."""
    os.kill(pid, signal.SIGSTOP)
    speaker = threading.Thread(target=talk, args=(talker, speech[:100]), name="duck-fault-speech")
    speaker.start()
    time.sleep(0.5)
    os.kill(pid, signal.SIGKILL)
    speaker.join(timeout=10)
    assert not speaker.is_alive()


SERVICE = "nuschl-musicbot"
SERVICE_CGROUP = f"/system.slice/{SERVICE}.service"


def main_pid():
    pid = int(subprocess.check_output(
        ["systemctl", "show", SERVICE, "--property=MainPID", "--value"], text=True))
    assert pid > 1, pid
    return pid


def journal():
    return subprocess.check_output(["journalctl", "-u", SERVICE, "-o", "cat", "--no-pager"], text=True)


def process_state(pid):
    """(state, kernel start time) of `pid`, or None once it has been reaped."""
    try:
        fields = (Path("/proc") / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()
    except OSError:
        return None
    return fields[0], fields[19]


def reaped(pid, start):
    """No process with `pid` and `start` remains, not even as a zombie."""
    state = process_state(pid)
    return state is None or state[1] != start


def parent_of(pid):
    try:
        return int((Path("/proc") / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()[1])
    except (OSError, IndexError, ValueError):
        return None


def worker_identity(bot):
    """The packaged codec worker beside the service's own executable."""
    executable = (Path("/proc") / str(bot) / "exe").resolve()
    worker = os.stat(executable.parent.parent / "libexec/nuschl-musicbot/nuschl-musicbot-codec-worker")
    return worker.st_dev, worker.st_ino


def owned_duck_worker(pid, bot, identity):
    """Whether `pid` is positively the service's confined duck worker."""
    process = Path("/proc") / str(pid)
    try:
        if process.joinpath("cmdline").read_bytes().split(b"\0") != DUCK:
            return False
        if not any(line.endswith(":" + SERVICE_CGROUP) for line in process.joinpath("cgroup").read_text().splitlines()):
            return False
        executable = os.stat(process / "exe")
    except OSError:
        return False
    if (executable.st_dev, executable.st_ino) != identity:
        return False
    ancestor = parent_of(pid)
    while ancestor not in (None, 0, 1):
        if ancestor == bot:
            return True
        ancestor = parent_of(ancestor)
    return False


def service_duck_worker(bot, identity):
    """Pins the one positively identified worker with a pidfd; refuses ambiguity.

    Returns the worker pid, its pidfd, and the bot's direct child that launched
    it (which only the bot can reap), each with its kernel start time."""
    candidates = [int(entry.name) for entry in Path("/proc").iterdir()
                  if entry.name.isdecimal() and owned_duck_worker(int(entry.name), bot, identity)]
    assert len(candidates) == 1, f"refusing to signal: duck worker identification is ambiguous: {candidates}"
    pid = candidates[0]
    pidfd = os.pidfd_open(pid)
    # The pidfd pins this process; check it is still the one identified.
    assert owned_duck_worker(pid, bot, identity), "duck worker changed while being pinned"
    launcher = pid
    while parent_of(launcher) != bot:
        launcher = parent_of(launcher)
        assert launcher not in (None, 0, 1), "duck worker is not under the service's main process"
    return {"pid": pid, "pidfd": pidfd, "start": process_state(pid)[1],
            "launcher": launcher, "launcher_start": process_state(launcher)[1]}


def send(worker, bot, identity, number):
    assert owned_duck_worker(worker["pid"], bot, identity), "refusing to signal a process no longer identified"
    signal.pidfd_send_signal(worker["pidfd"], number)


def reaped_by_service(worker, bot, identity):
    """The worker exited, and neither it nor the launcher the bot owns lingers as a zombie."""
    readable, _, _ = select.select([worker["pidfd"]], [], [], 15)
    assert readable, f"duck worker {worker['pid']} did not exit"
    os.close(worker["pidfd"])
    eventually(lambda: (reaped(worker["pid"], worker["start"]),
                        reaped(worker["launcher"], worker["launcher_start"])),
               all, f"duck worker {worker['pid']} and launcher {worker['launcher']} reaped", timeout=15)
    eventually(lambda: [pid for pid in descendants(bot) if owned_duck_worker(pid, bot, identity)],
               lambda workers: not workers, "no duck worker until the next speech", timeout=15)


def service_continuity(alice, dj, link, connects, label):
    assert session_of(alice, "DJ") == dj and dj in alice.users, (label, alice.users)
    log = journal()
    assert log.count(": connected to ") == connects, (label, "the service reconnected")
    for marker in ("connecting again", "session failed", "session task failed", "exit unconfirmed"):
        assert marker not in log, (label, marker)
    channel = api(link)["channels"][0]
    assert channel["connected"] and channel["error"] is None, (label, channel)
    drain(alice)
    heard = audio_from(alice, dj, 5)
    assert heard, f"{label}: no music after the fault"
    assert link_from(alice, dj).startswith("http"), label
    return len(heard)


def service(args):
    """Recovered duck faults of the running NixOS service, its session kept."""
    bot = main_pid()
    identity = worker_identity(bot)
    report = {}
    with tempfile.TemporaryDirectory(prefix="nuschl-duck-faults-") as temporary:
        root = Path(temporary)
        pin = certificate_pin(args.binaries, "127.0.0.1", args.port, root / "probe")
        talkers = []
        try:
            for name in ("dora", "erik"):
                talkers.append(Client(args.binaries, "127.0.0.1", args.port, pin, name, root / name))
            dora, erik = talkers
            dj = session_of(dora, "DJ")
            link = link_from(dora, dj)
            connects = journal().count(": connected to ")
            assert connects >= 1
            speech = speech_packets(args.espeak, args.opus_library, root)
            command(link, "ducking", enabled=True, level=30, hold_ms=800, sensitivity=50)
            for _ in range(20):
                command(link, "add", kind="album", library=0, id="a1", index=None)
            command(link, "resume")
            for talker in talkers:
                talker.unmute()
            dora.wait(lambda event: event["kind"] == "audio" and event["sender"] == dj, timeout=15)
            report["dj_session"] = dj
            assert main_pid() == bot

            # 1. Stopped past the 2 s request deadline: the service reaps it.
            assert ducked_while_talking(link, dora, speech), "speech did not duck the service's music"
            stalled = service_duck_worker(bot, identity)
            send(stalled, bot, identity, signal.SIGSTOP)
            started = time.monotonic()
            talk(dora, speech[:60])
            reaped_by_service(stalled, bot, identity)
            report["stall"] = {"worker": stalled["pid"], "launcher": stalled["launcher"],
                               "recovered_s": round(time.monotonic() - started, 2),
                               "packets_after": service_continuity(dora, dj, link, connects, "service stall")}

            # 2. A fresh worker killed while answering another talker.
            assert ducked_while_talking(link, erik, speech), "a fresh service worker did not duck"
            killed = service_duck_worker(bot, identity)
            assert killed["pid"] != stalled["pid"]
            send(killed, bot, identity, signal.SIGSTOP)
            speaker = threading.Thread(target=talk, args=(erik, speech[:100]), name="service-fault-speech")
            speaker.start()
            time.sleep(0.5)
            send(killed, bot, identity, signal.SIGKILL)
            speaker.join(timeout=10)
            assert not speaker.is_alive()
            reaped_by_service(killed, bot, identity)
            report["kill"] = {"worker": killed["pid"], "launcher": killed["launcher"],
                              "packets_after": service_continuity(dora, dj, link, connects, "service kill")}
            assert main_pid() == bot, "the service restarted"
            command(link, "clear")
            command(link, "skip")
        finally:
            close_resources(talkers)
    print(json.dumps({"service_duck_faults": report}))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("port", type=int)
    parser.add_argument("--espeak", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--service", action="store_true",
                        help="fault the running NixOS service (as root in the owned VM)")
    args = parser.parse_args()
    if args.service:
        service(args)
        return
    args.web_assets = None
    library = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Library)
    library_thread = threading.Thread(target=library.serve_forever, name="fake-subsonic")
    library_thread.start()
    report = {}
    try:
        with tempfile.TemporaryDirectory(prefix="nuschl-duck-faults-") as temporary:
            root = Path(temporary)
            pin = certificate_pin(args.binaries, "127.0.0.1", args.port, root / "probe")
            (root / "password").write_text(PASSWORD + "\n")
            (root / "key").write_text(API_KEY + "\n")
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", 0))
                web_port = probe.getsockname()[1]
            config = root / "musicbot.toml"
            write_config(config, root, f"http://127.0.0.1:{library.server_address[1]}",
                         web_port, args.port, pin)
            bot = BotProcess(args, config, root)
            talkers = []
            try:
                for name in ("alice", "bob", "carol"):
                    talkers.append(Client(args.binaries, "127.0.0.1", args.port, pin, name, root / name))
                    talkers[-1].send(kind="join", channel=MUSIC)
                alice, bob, carol = talkers
                dj, _ = connected(alice)
                link = link_from(alice, dj)
                speech = speech_packets(args.espeak, args.opus_library, root)
                command(link, "ducking", enabled=True, level=30, hold_ms=800, sensitivity=50)
                for _ in range(40):
                    command(link, "add", kind="album", library=0, id="a1", index=None)
                for talker in talkers:
                    talker.unmute()
                alice.wait(lambda event: event["kind"] == "audio" and event["sender"] == dj, timeout=15)
                report["dj_session"] = dj

                # Healthy: real speech ducks through the confined worker.
                assert ducked_while_talking(link, alice, speech), "speech did not duck music"
                stalled = only_duck_worker(bot)

                # 1. A stopped worker: the 2 s request deadline, then a confirmed reap.
                os.kill(stalled, signal.SIGSTOP)
                started = time.monotonic()
                talk(alice, speech[:60])
                reaped(bot, stalled)
                report["stall"] = {"worker": stalled, "recovered_s": round(time.monotonic() - started, 2),
                                   "packets_after": same_session(bot, alice, dj, link, "stall")}

                # 2. Killed while answering a new talker: a fresh worker, then EOF.
                assert ducked_while_talking(link, bob, speech), "a fresh worker did not duck"
                killed = only_duck_worker(bot)
                assert killed != stalled
                stop_and_kill_mid_request(bob, killed, speech)
                reaped(bot, killed)
                report["kill"] = {"worker": killed,
                                  "packets_after": same_session(bot, alice, dj, link, "kill")}

                # 3. The third recovered failure suspends ducking, with one notice.
                assert ducked_while_talking(link, carol, speech), "a third worker did not duck"
                third = only_duck_worker(bot)
                stop_and_kill_mid_request(carol, third, speech)
                reaped(bot, third)
                eventually(lambda: api(link)["channels"][0], lambda channel: channel["error"] == NOTICE,
                           "suspension notice")
                report["suspended"] = {"worker": third, "packets_after":
                                       same_session(bot, alice, dj, link, "suspended", notice=NOTICE)}
                # Suspended: talk starts no worker and ducks nothing.
                for talker in talkers:
                    talk(talker, speech[:40])
                time.sleep(1)
                assert not duck_workers(bot)
                assert not api(link)["players"][0]["ducked"]
                assert session_of(alice, "DJ") == dj
            finally:
                try:
                    close_resources(talkers)
                finally:
                    bot.stop()
    finally:
        library.shutdown()
        library_thread.join(timeout=5)
    print(json.dumps({"duck_faults": report}))


if __name__ == "__main__":
    main()
