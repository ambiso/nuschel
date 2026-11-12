#!/usr/bin/env python3
"""Recovered speech-detector faults keep the bot's real Mumble TLS session.

Usage: with_local_murmur.py <mumble-server> tests/musicbot_duck_faults.py <binaries> PORT
           --espeak <espeak-ng> --opus-library <libopus.so>

<binaries> holds nuschl-cli and a packaged nuschl-musicbot whose codec worker
sits in ../libexec/nuschl-musicbot. Only an owned loopback stock server, a
local fake library and generated speech are used. The bot's actual confined
duck worker is stopped past its request deadline, then stopped and killed in
the middle of requests; after each fault the same TLS session (same session
ID, no reconnection) must still play music to listeners and answer text, and
the faulted worker must have been reaped. The third fault suspends ducking
with one notice. An unconfirmed child exit cannot be produced with a real
process; mumble.rs's real_session tests cover that path with a stand-in.
"""
import argparse
import http.server
import json
import os
from pathlib import Path
import signal
import socket
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


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("port", type=int)
    parser.add_argument("--espeak", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    args = parser.parse_args()
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
