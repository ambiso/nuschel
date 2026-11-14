#!/usr/bin/env python3
"""A TLS connection lost while the server keeps running (issue #108).

    python3 tests/tls_drop.py <binaries> --root DIR [--control no-drop]

<binaries> holds nuschld, nuschl-cli, nuschl-tui and the workers (a package's
bin/ or target/debug). DIR must not exist: the run creates it and keeps all of
it (server state and log, profiles, transcripts, results.json), passed or
failed. HOME, XDG folders and TMPDIR point into DIR. Only 127.0.0.1 is used.

One seeded loopback nuschld runs for the whole test and is never restarted.
Ada (nuschl-tui --plain, which reconnects by itself) and Cleo (nuschl-cli,
which does not) reach it through TCP relays owned by the test. A relay cuts the
TLS connection (RST), refuses reconnections for a while, or silently discards
traffic in one or both directions. Bob talks to the server directly and writes
while they are away. Checks:

(a) the reconnection is a new relay connection to the same server process (pid,
    start time and executable unchanged, no second "listening" or a "shutting
    down" in its log; its storage workers are recorded), and Bob sees Ada leave
    and come back;
(b) Ada's open conversation (reopened, so the plain lines list the timeline in
    order) shows what was said meanwhile exactly once, in the server's order,
    compared with a page of the server's history;
(c) the gap line appears, and paging (/older, reopening the conversation)
    closes the gap; Cleo pages after her last seq until more_after is false and
    covers every seq up to the head;
(d) what was typed during the loss: while reconnecting, refused and not queued
    ("Connect before sending a message." on main); into a silent loss, resent
    after the reconnection with its idempotency key and stored once; Cleo's
    unacknowledged message gets no chat_acked/chat_failed before she exits.

Every check runs and is recorded; the exit status is non-zero if one failed.

Negative controls: a send retried with the same idempotency key keeps its seq,
while the same text under a new key is stored twice and the exactly-once check
must reject that real server page. --control no-drop runs the first loss with
nothing cut: the reconnection checks must fail and the messages arrive live
exactly once; it exits 0 when the control behaves so.
"""
import argparse
import hashlib
import json
import os
import queue
import re
import secrets
import selectors
import signal
import socket
import struct
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from local_nuschld import Nuschld, free_port  # noqa: E402
from mumble_client import certificate_pin  # noqa: E402
from tui_loopback import Terminal, environment  # noqa: E402

LOBBY, DEVELOPMENT = 1, 2
GAP_LINE = "Messages sent meanwhile were not received."
REFUSED_OFFLINE = "Connect before sending a message."
MESSAGE = re.compile(r"^(?:\d{4}-\d{2}-\d{2} )?\d{2}:\d{2} (?P<author>[^:()]+?)"
                     r"(?P<flags>(?: \([^()]*\))*): (?P<body>.*)$")


def log(text):
    print(f"  {time.strftime('%H:%M:%S')} {text}", flush=True)


class Checks:
    """Every check is recorded, passed or failed, in results.json."""

    def __init__(self, path, meta):
        self.path, self.meta, self.results = path, meta, []
        self.meta["notes"] = []

    def note(self, text):
        """An observation outside the checks (it fails nothing)."""
        self.meta["notes"].append(text)
        print(f"  NOTE {text}", flush=True)
        self.save()

    def record(self, name, ok, detail=""):
        self.results.append({"name": name, "ok": bool(ok), "detail": detail,
                             "at": time.strftime("%H:%M:%S")})
        print(f"  {'PASS' if ok else 'FAIL'} {name}" + (f"\n       {detail}" if detail else ""), flush=True)
        self.save()
        return ok

    def attempt(self, name, function):
        try:
            detail = function()
        except AssertionError as error:
            return self.record(name, False, str(error)[:6000])
        return self.record(name, True, detail or "")

    def save(self):
        self.path.write_text(json.dumps({**self.meta, "checks": self.results}, indent=1))

    def failed(self):
        return [result["name"] for result in self.results if not result["ok"]]


def exactly_once_in_order(observed, expected, what):
    """`observed` holds each of `expected` once, in that order, and nothing twice."""
    problems = []
    for item in expected:
        count = observed.count(item)
        if count != 1:
            problems.append(f"{item!r} seen {count} times")
    for item in set(observed):
        if observed.count(item) > 1 and item not in expected:
            problems.append(f"unexpected {item!r} seen {observed.count(item)} times")
    present = [item for item in observed if item in expected]
    deduplicated = list(dict.fromkeys(present))
    if not problems and deduplicated != [item for item in expected if item in deduplicated]:
        problems.append(f"order differs: {deduplicated}")
    assert not problems, f"{what}: " + "; ".join(problems) + f"\n       observed {observed}\n       expected {expected}"


class Relay:
    """A TCP relay on 127.0.0.1 to the server, driven by one selector thread.

    The test can cut every live connection (RST, or FIN), refuse new ones
    (accepted, then reset at once, so the port stays ours), or discard
    traffic in either direction while the sockets stay open."""

    def __init__(self, name, upstream, log_path):
        self.name, self.upstream = name, upstream
        self.log_file = open(log_path, "a", buffering=1)
        self.listener = socket.socket()
        self.listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(16)
        self.listener.setblocking(False)
        self.port = self.listener.getsockname()[1]
        self.selector = selectors.DefaultSelector()
        self.wake_read, self.wake_write = socket.socketpair()
        self.wake_read.setblocking(False)
        self.selector.register(self.listener, selectors.EVENT_READ, "listen")
        self.selector.register(self.wake_read, selectors.EVENT_READ, "wake")
        self.commands = queue.Queue()
        self.links, self.next_id = [], 0
        self.refuse, self.refused, self.stopping = False, 0, False
        self.thread = threading.Thread(target=self.run, name=f"relay-{name}", daemon=True)
        self.thread.start()

    def note(self, text):
        self.log_file.write(f"{time.time():.3f} {self.name}: {text}\n")

    def call(self, function):
        done, box = threading.Event(), {}

        def wrapped():
            try:
                box["value"] = function()
            except BaseException as error:  # handed back to the caller
                box["error"] = error
            finally:
                done.set()
        self.commands.put(wrapped)
        self.wake_write.send(b"x")
        if not done.wait(10):
            raise AssertionError(f"relay {self.name} did not answer")
        if "error" in box:
            raise box["error"]
        return box.get("value")

    def run(self):
        while not self.stopping:
            for key, _ in self.selector.select(timeout=0.5):
                if key.data == "listen":
                    self.accept()
                elif key.data == "wake":
                    try:
                        self.wake_read.recv(4096)
                    except BlockingIOError:
                        pass
                    while not self.commands.empty():
                        self.commands.get()()
                else:
                    link, side = key.data
                    self.pump(link, side)
        for link in self.links:
            self.close(link, "relay stopped")
        self.selector.close()
        self.listener.close()

    def accept(self):
        try:
            client, address = self.listener.accept()
        except BlockingIOError:
            return
        if self.refuse:
            client.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            client.close()
            self.refused += 1
            self.note(f"refused {address} (reset)")
            return
        try:
            upstream = socket.create_connection(("127.0.0.1", self.upstream), timeout=5)
        except OSError as error:
            self.note(f"upstream connect failed: {error}")
            client.close()
            return
        for sock in (client, upstream):
            sock.settimeout(10)
        self.next_id += 1
        link = {"id": self.next_id, "client": client, "upstream": upstream, "open": True,
                "drop": {"c": False, "s": False}, "bytes": {"c": 0, "s": 0},
                "dropped": {"c": 0, "s": 0}, "accepted": time.monotonic(), "closed": None}
        self.links.append(link)
        self.selector.register(client, selectors.EVENT_READ, (link, "c"))
        self.selector.register(upstream, selectors.EVENT_READ, (link, "s"))
        self.note(f"accepted #{link['id']} from {address}")

    def pump(self, link, side):
        source = link["client"] if side == "c" else link["upstream"]
        target = link["upstream"] if side == "c" else link["client"]
        try:
            data = source.recv(65536)
        except OSError:
            data = b""
        if not data:
            self.close(link, f"end of stream from the {'client' if side == 'c' else 'server'}")
            return
        if link["drop"][side]:
            link["dropped"][side] += len(data)
            return
        link["bytes"][side] += len(data)
        try:
            target.sendall(data)
        except OSError as error:
            self.close(link, f"forwarding failed: {error}")

    def close(self, link, reason, reset=False):
        if not link["open"]:
            return
        link["open"], link["closed"] = False, (time.monotonic(), reason)
        for sock in (link["client"], link["upstream"]):
            try:
                self.selector.unregister(sock)
            except (KeyError, ValueError):
                pass
            if reset:
                sock.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, struct.pack("ii", 1, 0))
            else:
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
            sock.close()
        self.note(f"closed #{link['id']}: {reason}; forwarded c->s {link['bytes']['c']} "
                  f"s->c {link['bytes']['s']}, discarded c->s {link['dropped']['c']} s->c {link['dropped']['s']}")

    # What the test asks for, run in the relay's thread.

    def sever(self, how="rst"):
        def cut():
            live = [link for link in self.links if link["open"]]
            for link in live:
                self.close(link, f"severed by the test ({how})", reset=how == "rst")
            return len(live)
        return self.call(cut)

    def blackhole(self, client_to_server, server_to_client):
        def discard():
            live = [link for link in self.links if link["open"]]
            for link in live:
                link["drop"]["c"], link["drop"]["s"] = client_to_server, server_to_client
                self.note(f"#{link['id']} now discards c->s={client_to_server} s->c={server_to_client}")
            return len(live)
        return self.call(discard)

    def set_refuse(self, refuse):
        def apply():
            self.refuse = refuse
            self.note(f"refusing new connections: {refuse}")
        self.call(apply)

    def accepted(self):
        return self.call(lambda: len(self.links))

    def stop(self):
        self.stopping = True
        self.wake_write.send(b"x")
        self.thread.join(timeout=10)
        self.log_file.close()


class Json:
    """nuschl-cli with every event kept in order (and in a log file)."""

    def __init__(self, binaries, port, pin, name, profile, logs, extra=()):
        self.name = name
        self.all = []
        self.log = open(logs / f"{name.lower()}-{int(time.time() * 1000)}.jsonl", "w", buffering=1)
        self.stderr = open(logs / f"{name.lower()}-{int(time.time() * 1000)}.stderr", "w")
        self.process = subprocess.Popen(
            [str(Path(binaries) / "nuschl-cli"), "--host", "127.0.0.1", "--port", str(port),
             "--username", name, "--certificate-sha256", pin, "--profile-dir", str(profile), *extra],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.stderr, text=True)
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()
        self.session = self.wait_seen(lambda e: e["kind"] == "connected", 0, 60)["session"]

    def read(self):
        for line in self.process.stdout:
            self.log.write(line)
            self.all.append(json.loads(line))
        self.all.append({"kind": "exited"})

    def send(self, **command):
        self.process.stdin.write(json.dumps(command) + "\n")
        self.process.stdin.flush()

    def wait_seen(self, predicate, since, timeout=20, pump=()):
        deadline = time.monotonic() + timeout
        index = since
        while True:
            while index < len(self.all):
                event = self.all[index]
                index += 1
                if predicate(event):
                    return event
                if event["kind"] == "exited":
                    raise AssertionError(f"{self.name}'s client exited: {self.all[-8:]}")
            if time.monotonic() > deadline:
                raise AssertionError(f"{self.name}: expected event missing after {timeout} s; last {self.all[-6:]}")
            for terminal in pump:
                drain(terminal)
            time.sleep(0.05)

    def seen(self, predicate, since=0):
        return [event for event in self.all[since:] if predicate(event)]

    def register(self):
        since = len(self.all)
        self.send(kind="register")
        self.wait_seen(lambda e: e["kind"] == "user" and e["user"]["session"] == self.session
                       and e["user"]["user_id"] is not None, since)

    def say(self, channel, body, pump=()):
        """Paced, idempotent channel text; returns the seq the server acknowledged."""
        since = len(self.all)
        self.send(kind="text", channel=channel, body=body)
        pending = self.wait_seen(lambda e: e["kind"] == "chat_pending", since, 30, pump)
        done = self.wait_seen(lambda e: e["kind"] in ("chat_acked", "chat_failed")
                              and e["key"] == pending["key"], since, 30, pump)
        assert done["kind"] == "chat_acked", done
        return done["seq"]

    def page(self, request_id, conversation, limit=100, pump=(), **anchor):
        since = len(self.all)
        self.send(kind="history", request_id=request_id, conversation=conversation, limit=limit, **anchor)
        page = self.wait_seen(lambda e: e.get("request_id") == request_id
                              and e["kind"] in ("history_page", "nuschl_error"), since, 20, pump)
        assert page["kind"] == "history_page", page
        return page

    def conversation(self, channel):
        return self.wait_seen(lambda e: e["kind"] == "channel_info" and e["channel"] == channel, 0)["conversation"]

    def close(self):
        if self.process.poll() is None:
            try:
                self.send(kind="disconnect")
            except (BrokenPipeError, OSError):
                pass
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.reader.join(timeout=5)
        self.log.close()
        self.stderr.close()
        return self.process.returncode


def drain(terminal):
    while terminal.pump(0):
        pass
    if terminal.reap() is not None:
        raise AssertionError(f"{terminal.name} exited with {terminal.status}:\n{terminal.output[-3000:]}")


def until(predicate, what, timeout, pump=()):
    deadline = time.monotonic() + timeout
    while True:
        for terminal in pump:
            drain(terminal)
        result = predicate()
        if result:
            return result
        if time.monotonic() > deadline:
            raise AssertionError(f"timed out after {timeout} s waiting for {what}")
        time.sleep(0.05)


def text_lines(text):
    return [line.rstrip("\r").strip() for line in text.split("\n")]


def shown_messages(text, prefix, token):
    """(author, body) of each message line in `text` starting with `prefix`
    and carrying the run's token."""
    found = []
    for line in text_lines(text):
        match = MESSAGE.match(line)
        if match and match["body"].startswith(prefix) and token in match["body"]:
            found.append((match["author"], match["body"]))
    return found


def process_identity(pid):
    """What changes if the process is replaced: its start time, exe and children."""
    stat = Path(f"/proc/{pid}/stat").read_text()
    start = stat[stat.rindex(")") + 2:].split()[19]
    children = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            child = (entry / "stat").read_text()
        except OSError:
            continue
        fields = child[child.rindex(")") + 2:].split()
        if fields[1] == str(pid):
            children.append((int(entry.name), child[child.index("(") + 1:child.rindex(")")], fields[19]))
    return {"pid": pid, "start": start, "exe": os.readlink(f"/proc/{pid}/exe"), "children": sorted(children)}


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


class Run:
    def __init__(self, args):
        self.args = args
        self.binaries = args.binaries.resolve()
        self.root = args.root
        self.token = secrets.token_hex(3)
        self.logs = self.root / "logs"
        self.logs.mkdir()
        meta = {"binaries": str(self.binaries), "control": args.control, "token": self.token,
                "test_sha256": sha256(__file__),
                "hashes": {name: sha256(self.binaries / name) for name in
                           ("nuschld", "nuschl-cli", "nuschl-tui", "nuschl-storage-worker",
                            "nuschl-content-worker", "nuschl-crypto-worker") if (self.binaries / name).exists()}}
        for wrapped in (".nuschl-cli-wrapped", ".nuschl-tui-wrapped"):
            if (self.binaries / wrapped).exists():
                meta["hashes"][wrapped] = sha256(self.binaries / wrapped)
        self.checks = Checks(self.root / "results.json", meta)
        self.server = self.bob = self.ada = self.cleo = None
        self.relays = []
        self.request = 100

    def next_request(self):
        self.request += 1
        return self.request

    # The cast.

    def start_server(self):
        port = free_port()
        (self.root / "server").mkdir()
        self.server = Nuschld(self.binaries, self.root / "server", port)
        self.server.seed()
        self.server.start()
        self.server_log_start = self.server.log.stat().st_size
        self.identity = process_identity(self.server.process.pid)
        self.checks.meta["server"] = {"port": port, **self.identity}
        log(f"nuschld pid {self.identity['pid']} on 127.0.0.1:{port}, children {self.identity['children']}")

    def same_server(self):
        assert self.server.process.poll() is None, f"nuschld exited with {self.server.process.returncode}"
        now = process_identity(self.server.process.pid)
        same = ("pid", "start", "exe")
        assert all(now[key] == self.identity[key] for key in same), \
            f"server process changed: {self.identity} -> {now}"
        tail = self.server.log.read_text(errors="replace")[self.server_log_start:]
        restarts = [line for line in tail.splitlines()
                    if "listening on TCP and UDP" in line or "shutting down" in line]
        assert not restarts, f"server log shows a restart: {restarts}"
        # nuschld starts storage workers as it needs them; they are recorded, not compared.
        return f"pid {now['pid']} start {now['start']}; storage workers then {[c[0] for c in self.identity['children']]}, " \
               f"now {[c[0] for c in now['children']]}"

    def start_bob(self):
        self.pin = certificate_pin(self.binaries, "127.0.0.1", self.server.port, self.root / "probe")
        self.bob = Json(self.binaries, self.server.port, self.pin, "Bob", self.root / "bob", self.logs)
        self.bob.register()
        since = len(self.bob.all)
        self.bob.send(kind="join", channel=LOBBY)
        self.bob.wait_seen(lambda e: e["kind"] == "user" and e["user"]["session"] == self.bob.session
                           and e["user"]["channel"] == LOBBY, since)
        self.lobby = self.bob.conversation(LOBBY)
        self.development = self.bob.conversation(DEVELOPMENT)

    def start_ada(self, first=True):
        relay = self.ada_relay
        tui = str(self.binaries / "nuschl-tui")
        argv = [tui, "--plain", "--no-audio", "--opener", "/bin/true", "--profile-dir",
                str(self.root / "ada"), f"mumble://Ada@127.0.0.1:{relay.port}/Lobby"]
        self.ada = Terminal("Ada", argv, self.env, plain=True)
        if first:
            self.ada.wait_for("Trust it with /trust", timeout=60)
            self.ada.line("/trust")
        self.wait_ada(0, "* 127.0.0.1: Connected", 60)
        if first:
            self.ada.line("/register")
            self.wait_ada(0, "You are registered on", 30)
        self.wait_ada(0, "Your voice is in Lobby", 30)

    def wait_ada(self, offset, what, timeout=30):
        check = (lambda: what in self.ada.output[offset:]) if isinstance(what, str) \
            else (lambda: what(self.ada.output[offset:]))
        try:
            until(check, f"Ada to show {what!r}", timeout, (self.ada,))
        except AssertionError as error:
            tail = [line for line in text_lines(self.ada.output[offset:]) if line][-15:]
            raise AssertionError(f"{error}; Ada's lines since: {tail}") from None

    def ada_session(self, since=0):
        """Ada's session as Bob sees it: the newest user event for her."""
        events = self.bob.seen(lambda e: e["kind"] == "user" and e["user"]["name"] == "Ada", since)
        return events[-1]["user"] if events else None

    def reopen_lobby(self):
        """Reopening prints the open timeline afresh, in its order."""
        offset = len(self.ada.output)
        self.ada.line("/go Development")
        self.wait_ada(offset, "--- Development on 127.0.0.1 ---")
        offset = len(self.ada.output)
        self.ada.line("/go Lobby")
        self.wait_ada(offset, "--- Lobby on 127.0.0.1 ---")
        # Let the timeline print fully (and any page arrive).
        time.sleep(3)
        drain(self.ada)
        return self.ada.output[offset:]

    def server_order(self, prefix):
        page = self.bob.page(self.next_request(), self.lobby, pump=(self.ada,))
        return [(message["author"], message["content"]) for message in page["messages"]
                if message["kind"] == 0 and not message["deleted"]
                and message["content"].startswith(prefix) and self.token in message["content"]]

    def server_count(self, body):
        page = self.bob.page(self.next_request(), self.lobby, pump=(self.ada,))
        return [message["seq"] for message in page["messages"] if message["content"] == body]

    # Phases.

    def setup(self):
        log("seeding and starting nuschld")
        self.start_server()
        self.ada_relay = Relay("ada", self.server.port, self.logs / "relay-ada.log")
        self.cleo_relay = Relay("cleo", self.server.port, self.logs / "relay-cleo.log")
        self.relays = [self.ada_relay, self.cleo_relay]
        log(f"relays: Ada 127.0.0.1:{self.ada_relay.port}, Cleo 127.0.0.1:{self.cleo_relay.port}")
        self.start_bob()
        self.start_ada()
        until(lambda: self.ada_session(), "Bob to see Ada", 20, (self.ada,))
        # Guests read no history: the page Ada asked for before registering
        # was refused. Reopened as a registered member, Lobby loads from the
        # server, so every loss below starts from a loaded timeline.
        refused = "Server history:"
        if refused in self.ada.output:
            self.checks.note("Ada's first Lobby page, asked for before /register, was refused (guests read no "
                             "history); registering does not reload the open conversation until it is reopened: "
                             + next(l for l in text_lines(self.ada.output) if refused in l))
        offset = len(self.ada.output)
        self.reopen_lobby()
        assert refused not in self.ada.output[offset:], text_lines(self.ada.output[offset:])
        tok = self.token
        bob_pre = f"P-bob before {tok}"
        self.bob.say(LOBBY, bob_pre, (self.ada,))
        self.wait_ada(0, f"Bob: {bob_pre}")
        ada_pre = f"P-ada before {tok}"
        since = len(self.bob.all)
        self.ada.line(ada_pre)
        self.bob.wait_seen(lambda e: e["kind"] == "text" and e["body"] == ada_pre, since, 20, (self.ada,))
        # The first connection stops at the certificate; /trust opens another.
        log(f"Ada (through the relay, {self.ada_relay.accepted()} connections so far) "
            "and Bob (direct) talk in Lobby")

    def drop_and_hold(self, drop=True):
        """Phase B: the connection is reset and reconnections are refused
        while Bob writes; Ada types while she is reconnecting."""
        tok, checks = self.token, self.checks
        log("phase B: reset Ada's TLS connection, refuse reconnections" if drop
            else "phase B (control): nothing is cut")
        offset = len(self.ada.output)
        bob_since = len(self.bob.all)
        before = self.ada_session()
        accepted = self.ada_relay.accepted()
        if drop:
            self.ada_relay.set_refuse(True)
            cut = self.ada_relay.sever("rst")
            log(f"severed {cut} connection(s)")

        def lost():
            self.wait_ada(offset, "Reconnecting in", 15 if drop else 10)
            removed = until(lambda: self.bob.seen(lambda e: e["kind"] == "user_removed"
                                                  and e["session"] == before["session"], bob_since),
                            "Bob to see Ada's session end", 15, (self.ada,))
            return f"Ada: {[l for l in text_lines(self.ada.output[offset:]) if 'Reconnecting' in l][:1]}; Bob: {removed[0]}"
        checks.attempt("B: Ada notices the loss and Bob sees her session end", lost)

        offline = f"B-ada typed while reconnecting {tok}"
        if drop:
            offset_typed = len(self.ada.output)
            self.ada.line(offline)

            def refused():
                self.wait_ada(offset_typed, REFUSED_OFFLINE, 10)
                return [l for l in text_lines(self.ada.output[offset_typed:]) if REFUSED_OFFLINE in l]
            checks.attempt("B(d): a message typed while reconnecting is refused (main: not queued)", refused)
        gap = [f"B-bob meanwhile {n} {tok}" for n in (1, 2, 3)]
        seqs = [self.bob.say(LOBBY, body, (self.ada,)) for body in gap]
        log(f"Bob wrote {len(gap)} messages meanwhile, seqs {seqs}")
        if drop:
            def held():
                self.wait_ada(offset, "(attempt 2)", 20)
                assert self.ada_relay.refused >= 1, self.ada_relay.refused
                return f"{self.ada_relay.refused} reconnection(s) refused"
            checks.attempt("B: reconnections are refused while held", held)
            self.ada_relay.set_refuse(False)

        def back():
            self.wait_ada(offset, lambda out: out.count("* 127.0.0.1: Connected") >= 1, 30 if drop else 5)
            assert self.ada_relay.accepted() > accepted, "no new relay connection"
            after = until(lambda: (lambda u: u if u and u["session"] != before["session"]
                                   and u["channel"] == LOBBY else None)(self.ada_session(bob_since)),
                          "Bob to see Ada back in Lobby", 20, (self.ada,))
            return f"new relay connection #{self.ada_relay.accepted()}, session {before['session']} -> {after['session']}"
        checks.attempt("B(a): Ada reconnects over a new connection and rejoins Lobby", back)
        checks.attempt("B(a): the same server process served the reconnection", self.same_server)

        def gap_line():
            self.wait_ada(offset, GAP_LINE, 10)
            return [l for l in text_lines(self.ada.output[offset:]) if GAP_LINE in l][0]
        checks.attempt("B(c): the gap line appears", gap_line)
        # Let a client that catches up do so, then something live after.
        time.sleep(5)
        post = f"B-bob after {tok}"
        self.bob.say(LOBBY, post, (self.ada,))
        checks.attempt("B: a message after the reconnection arrives live",
                       lambda: self.wait_ada(offset, f"Bob: {post}", 20))
        live = shown_messages(self.ada.output[offset:], "B-", tok)
        server = self.server_order("B-")
        checks.attempt("B(b): the plain lines since the loss show each message once, in order",
                       lambda: exactly_once_in_order(live, server, "Ada's lines since the loss"))
        view = self.reopen_lobby()
        shown = shown_messages(view, "B-", tok)
        checks.attempt("B(b): Ada's reopened timeline shows the messages from the loss once, in order",
                       lambda: exactly_once_in_order(shown, server, "Ada's timeline"))
        if drop:
            def gap_in_timeline():
                lines = text_lines(view)
                positions = [i for i, l in enumerate(lines) if GAP_LINE in l]
                assert len(positions) == 1, f"{len(positions)} gap lines in the reopened timeline"
                before_gap = [i for i, l in enumerate(lines) if f"Ada: P-ada before {tok}" in l]
                after_gap = [i for i, l in enumerate(lines) if f"Bob: {post}" in l]
                assert before_gap and after_gap and before_gap[0] < positions[0] < after_gap[0], \
                    (before_gap, positions, after_gap)
                return lines[positions[0]]
            checks.attempt("B(c): the reopened timeline marks the gap between before and after", gap_in_timeline)
            offset = len(self.ada.output)
            self.ada.line("/older")
            time.sleep(3)
            view = self.reopen_lobby()
            checks.attempt("B(c): paging (/older, reopening) closes the gap",
                           lambda: exactly_once_in_order(shown_messages(view, "B-", tok), server, "Ada after paging"))

            def not_sent():
                assert not self.bob.seen(lambda e: e["kind"] == "text" and e["body"] == offline), "Bob received it"
                assert not self.server_count(offline), "the server stored it"
                assert f"Ada: {offline}" not in self.ada.output, "Ada shows it as sent"
                return "not delivered, not stored, not shown as sent"
            checks.attempt("B(d): the refused message was not sent later", not_sent)
        return server

    def silent_loss(self, phase, client_to_server):
        """Phases C and D: traffic to Ada (C), or both ways (D), is discarded
        while every socket stays open; Ada sends into it, Bob writes."""
        tok, checks = self.token, self.checks
        which = "server-to-client" if not client_to_server else "both directions"
        log(f"phase {phase}: silently discard Ada's traffic ({which})")
        offset = len(self.ada.output)
        bob_since = len(self.bob.all)
        before = self.ada_session()
        accepted = self.ada_relay.accepted()
        self.ada_relay.blackhole(client_to_server, True)
        mine = f"{phase}-ada sent into the loss {tok}"
        self.ada.line(mine)
        if not client_to_server:
            # It reaches the server; only the acknowledgement is lost.
            first = self.bob.wait_seen(lambda e: e["kind"] == "text" and e["body"] == mine, bob_since, 20, (self.ada,))
            log(f"the server stored Ada's message as seq {first['seq']}; its ack is discarded")
        theirs = [f"{phase}-bob meanwhile {n} {tok}" for n in (1, 2)]
        for body in theirs:
            self.bob.say(LOBBY, body, (self.ada,))
        if client_to_server:
            time.sleep(2)
            checks.record(f"{phase}: Ada's message has not reached the server during the loss",
                          not self.bob.seen(lambda e: e["kind"] == "text" and e["body"] == mine, bob_since))

        def reconnected():
            self.wait_ada(offset, "Reconnecting in", 60)
            self.wait_ada(offset, "* 127.0.0.1: Connected", 30)
            assert self.ada_relay.accepted() > accepted, "no new relay connection"
            after = until(lambda: (lambda u: u if u and u["session"] != before["session"]
                                   and u["channel"] == LOBBY else None)(self.ada_session(bob_since)),
                          "Bob to see Ada back in Lobby", 20, (self.ada,))
            reason = [l for l in text_lines(self.ada.output[offset:]) if "Reconnecting" in l][:1]
            return f"{reason}; session {before['session']} -> {after['session']}"
        checks.attempt(f"{phase}(a): Ada detects the silent loss and reconnects", reconnected)
        checks.attempt(f"{phase}(a): the same server process served the reconnection", self.same_server)
        checks.attempt(f"{phase}(c): the gap line appears", lambda: self.wait_ada(offset, GAP_LINE, 10))

        def resent_once():
            self.wait_ada(offset, f"Ada: {mine}", 30)
            copies = until(lambda: self.bob.seen(lambda e: e["kind"] == "text" and e["body"] == mine, bob_since),
                           "Bob to receive Ada's message", 20, (self.ada,))
            time.sleep(3)
            copies = self.bob.seen(lambda e: e["kind"] == "text" and e["body"] == mine, bob_since)
            stored = self.server_count(mine)
            shown = shown_messages(self.ada.output[offset:], mine, tok)
            assert len(copies) == 1, f"Bob received {len(copies)} copies: {copies}"
            assert len(stored) == 1, f"the server stored it {len(stored)} times: seqs {stored}"
            assert shown == [("Ada", mine)], f"Ada shows {shown}"
            assert copies[0]["seq"] == stored[0], (copies[0]["seq"], stored)
            return f"stored once as seq {stored[0]}"
        checks.attempt(f"{phase}(d): the message sent into the loss is resent and stored once", resent_once)
        time.sleep(2)
        post = f"{phase}-bob after {tok}"
        self.bob.say(LOBBY, post, (self.ada,))
        checks.attempt(f"{phase}: a message after the reconnection arrives live",
                       lambda: self.wait_ada(offset, f"Bob: {post}", 20))
        server = self.server_order(f"{phase}-")
        view = self.reopen_lobby()
        checks.attempt(f"{phase}(b): Ada's reopened timeline shows the messages from the loss once, in order",
                       lambda: exactly_once_in_order(shown_messages(view, f"{phase}-", tok), server, "Ada's timeline"))

    def json_client(self):
        """Phase E: Cleo (nuschl-cli) loses her connection with a message in
        flight, comes back as a new process and pages through the gap."""
        tok, checks = self.token, self.checks
        log("phase E: the JSON client through its relay")
        cleo = Json(self.binaries, self.cleo_relay.port, self.pin, "Cleo", self.root / "cleo", self.logs)
        cleo.register()
        latest = cleo.page(self.next_request(), self.lobby, pump=(self.ada,))
        last_seen = max(message["seq"] for message in latest["messages"])
        log(f"Cleo has read Lobby up to seq {last_seen} (head {latest['head']})")
        self.cleo_relay.blackhole(False, True)
        bob_since, since = len(self.bob.all), len(cleo.all)
        inflight = f"E-cleo in flight {tok}"
        cleo.send(kind="text", channel=LOBBY, body=inflight)
        pending = cleo.wait_seen(lambda e: e["kind"] == "chat_pending", since, 20, (self.ada,))
        stored = self.bob.wait_seen(lambda e: e["kind"] == "text" and e["body"] == inflight, bob_since, 20, (self.ada,))
        time.sleep(2)
        self.cleo_relay.sever("rst")
        until(lambda: cleo.process.poll() is not None, "Cleo's client to exit", 20, (self.ada,))
        until(lambda: cleo.all and cleo.all[-1]["kind"] == "exited", "Cleo's output to end", 5)
        status = cleo.close()
        outcome = [e for e in cleo.all[since:] if e.get("key") == pending["key"] or e["kind"] in ("disconnected", "chat_failed")]

        def documented():
            kinds = [e["kind"] for e in outcome]
            assert "chat_acked" not in kinds, outcome
            assert "chat_failed" not in kinds, f"main reports no chat_failed here; got {outcome}"
            assert kinds[-1] == "disconnected" and status != 0, (kinds, status)
            return f"events {kinds}, exit status {status}; the server stored it as seq {stored['seq']}"
        checks.attempt("E(d): main's JSON client reports nothing for its stored but unacknowledged message, "
                       "then exits", documented)
        bodies = [f"E-bob meanwhile {n} {tok}" for n in range(1, 6)]
        for body in bodies:
            self.bob.say(LOBBY, body, (self.ada,))
        cleo = Json(self.binaries, self.cleo_relay.port, self.pin, "Cleo", self.root / "cleo", self.logs)
        self.cleo = cleo
        since = len(cleo.all)
        cleo.send(kind="subscribe", request_id=self.next_request(), conversations=[self.lobby])
        head = cleo.wait_seen(lambda e: e["kind"] == "subscribed", since, 20, (self.ada,))["conversations"][0]["head"]
        rows, pages, anchor = [], [], last_seen
        for _ in range(20):
            page = cleo.page(self.next_request(), self.lobby, limit=2, pump=(self.ada,), after=anchor)
            pages.append({"after": anchor, "seqs": [m["seq"] for m in page["messages"]],
                          "more_after": page["more_after"], "head": page["head"]})
            rows.extend(page["messages"])
            if not page["more_after"] or not page["messages"]:
                break
            anchor = page["messages"][-1]["seq"]
        (self.logs / "cleo-pages.json").write_text(json.dumps(pages, indent=1))

        def closes():
            seqs = [row["seq"] for row in rows]
            assert seqs == sorted(set(seqs)), f"pages repeat or reorder seqs: {seqs}"
            assert seqs == list(range(last_seen + 1, head + 1)), \
                f"pages after {last_seen} cover {seqs}, not every seq up to head {head}"
            assert len(pages) >= 3 and all(p["more_after"] for p in pages[:-1]) and not pages[-1]["more_after"], pages
            texts = [row["content"] for row in rows if row["kind"] == 0 and tok in row["content"]]
            exactly_once_in_order(texts, [inflight, *bodies], "Cleo's pages")
            return f"{len(pages)} pages after seq {last_seen} cover {last_seen + 1}..{head}"
        checks.attempt("E(c): after reconnecting, Cleo's pages close the gap up to the head", closes)

    def duplicate_control(self):
        """Negative control: a retry under the same key keeps its seq; a new
        key stores the text twice, which the exactly-once check must reject."""
        tok, checks = self.token, self.checks
        body = f"N-replayed {tok}"
        key = secrets.token_hex(16)

        def send(key):
            request = self.next_request()
            since = len(self.bob.all)
            self.bob.send(kind="chat_send", request_id=request, conversation=self.development, content=body, key=key)
            ack = self.bob.wait_seen(lambda e: e.get("request_id") == request
                                     and e["kind"] in ("chat_ack", "nuschl_error"), since, 20, (self.ada,))
            assert ack["kind"] == "chat_ack", ack
            time.sleep(1.2)
            return ack["seq"]
        first, retry, replay = send(key), send(key), send(secrets.token_hex(16))
        checks.record("N: a retry with the same idempotency key returns the original seq",
                      retry == first, f"seqs {first}, {retry}")
        page = self.bob.page(self.next_request(), self.development, pump=(self.ada,))
        texts = [m["content"] for m in page["messages"] if m["kind"] == 0 and tok in m["content"]]
        try:
            exactly_once_in_order(texts, [body], "Development")
        except AssertionError as error:
            checks.record("N: the exactly-once check rejects the replay under a new key (control fired)",
                          True, f"seqs {first}, {replay}: {str(error).splitlines()[0]}")
        else:
            checks.record("N: the exactly-once check rejects the replay under a new key (control fired)",
                          False, f"no duplicate detected in {texts}")

    def restart_ada(self):
        """Phase F: a new client process loads the conversation from the server."""
        tok = self.token
        log("phase F: Ada's client restarts and loads Lobby from the server")
        self.ada.line("/quit")
        deadline = time.monotonic() + 15
        while self.ada.reap() is None and time.monotonic() < deadline:
            self.ada.pump(0.1)
        self.checks.record("F: Ada's client quits cleanly", self.ada.status == 0, f"status {self.ada.status}")
        self.save_transcript("ada-1")
        self.start_ada(first=False)
        view = self.reopen_lobby()
        server = self.server_order("")
        self.checks.attempt("F(b): a restarted client shows the whole conversation once, in order",
                            lambda: exactly_once_in_order(shown_messages(view, "", tok), server, "Ada after restart"))

    def save_transcript(self, name):
        if self.ada:
            (self.logs / f"{name}.txt").write_text(self.ada.output)

    def stop(self):
        if self.ada:
            if self.ada.reap() is None:
                try:
                    self.ada.close("/quit\r")
                except (AssertionError, OSError) as error:
                    log(f"Ada's client: {error}")
            self.save_transcript("ada-2" if (self.logs / "ada-1.txt").exists() else "ada-1")
        for client in (self.cleo, self.bob):
            if client:
                client.close()
        for relay in self.relays:
            relay.stop()
        if self.server:
            self.server.stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--root", type=Path, required=True, help="a new directory for everything the run leaves")
    parser.add_argument("--control", choices=("no-drop",), help="the negative control: nothing is cut")
    parser.add_argument("--timeout", type=int, default=900, help="seconds for the whole run")
    args = parser.parse_args()
    args.root = args.root.absolute()
    args.root.mkdir(mode=0o700)
    env = environment(args.root)
    env["TMPDIR"] = str(args.root / "tmp")
    Path(env["TMPDIR"]).mkdir(mode=0o700)
    # The JSON clients and the server get the same private folders.
    os.environ.update({key: value for key, value in env.items() if key not in ("TERM",)})

    def expired(signum, frame):
        raise TimeoutError(f"the run took longer than {args.timeout} s")
    signal.signal(signal.SIGALRM, expired)
    signal.alarm(args.timeout)
    run = Run(args)
    run.env = env
    error = None
    try:
        run.setup()
        if args.control:
            run.drop_and_hold(drop=False)
        else:
            run.drop_and_hold()
            run.duplicate_control()
            run.silent_loss("C", client_to_server=False)
            run.silent_loss("D", client_to_server=True)
            run.json_client()
            run.restart_ada()
    except BaseException as caught:  # recorded, then reported after cleanup
        error = caught
        run.checks.record("the scenario ran to its end", False, f"{type(caught).__name__}: {caught}"[:6000])
    finally:
        signal.alarm(0)
        try:
            run.stop()
        except BaseException as caught:
            run.checks.record("cleanup", False, f"{type(caught).__name__}: {caught}")
    failed = run.checks.failed()
    if args.control:
        expected = {"B: Ada notices the loss and Bob sees her session end",
                    "B(a): Ada reconnects over a new connection and rejoins Lobby",
                    "B(c): the gap line appears"}
        passed_live = {"B(b): the plain lines since the loss show each message once, in order",
                       "B(b): Ada's reopened timeline shows the messages from the loss once, in order"}
        ok = expected <= set(failed) and not (passed_live & set(failed)) and error is None
        run.checks.meta["control_result"] = {"expected_failures": sorted(expected), "observed_failures": failed,
                                             "behaved": ok}
        run.checks.save()
        print(f"Negative control (no drop): {'behaved as expected' if ok else 'DID NOT behave as expected'}; "
              f"failed checks: {failed}; results in {run.root / 'results.json'}")
        sys.exit(0 if ok else 1)
    print(f"TLS loss without a server restart: {'ok' if not failed else 'FAILED: ' + ', '.join(failed)}; "
          f"results in {run.root / 'results.json'}")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
