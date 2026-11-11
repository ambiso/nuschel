#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
r"""A disposable Windows 11 VM for testing Nuschl's Windows build.

    windows_vm.py up [--iso PATH]     install Windows unattended (first run) and boot it
    windows_vm.py run 'Get-Process'   run PowerShell in the logged-in desktop session
    windows_vm.py push FILE [NAME]    make a host file downloadable in the guest
                                      (Get-NuschlTestFile -Name NAME -OutFile C:\test\NAME)
    windows_vm.py screenshot OUT.png  capture the VM display
    windows_vm.py click X Y | type TEXT | key ctrl-alt-delete
    windows_vm.py status | pause | resume | down

State (disk, firmware variables, sockets) lives in $NUSCHL_WINDOWS_VM_DIR,
default ~/vms/nuschl-windows; keep it out of backups and off the RAM-backed
/tmp. The guest has no network except forwards into host loopback:
10.0.2.100:80 (this harness), 10.0.2.100:64738 (a test Murmur, --murmur-port)
and 10.0.2.100:PORT to 127.0.0.1:PORT for each --forward PORT (e.g. a
magic-wormhole rendezvous and relay for the sandboxed transfer worker).
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import hmac
import http.client
import http.server
import json
import os
import queue
import re
import secrets
import shutil
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent / "windows_vm"
STATE = Path(os.environ.get("NUSCHL_WINDOWS_VM_DIR", Path.home() / "vms" / "nuschl-windows"))
OVMF_FLAKE_ATTR = "nixpkgs#OVMF.fd"
XORRISO_FLAKE_ATTR = "nixpkgs#xorriso"
REPO = Path(__file__).resolve().parents[1]


def nix_file(attr: str, relative: str) -> Path:
    """A file from a nixpkgs package pinned by the repository's flake."""
    out = subprocess.check_output(
        ["nix", "build", "--inputs-from", str(REPO), attr, "--no-link", "--print-out-paths", "--cores", "2"],
        text=True,
    )
    for path in map(Path, out.split()):
        if (path / relative).exists():
            return path / relative
    raise FileNotFoundError(f"{relative} in {attr}")


MAX_SCRIPT = 256 * 1024
MAX_OUTPUT = 1024 * 1024
MAX_BODY = 2 * 1024 * 1024
MAX_JOBS = 32
FILE_NAME = re.compile(r"[A-Za-z0-9][A-Za-z0-9_.-]{0,127}\Z")
JOB_ID = re.compile(r"[0-9a-f]{32}\Z")


class Jobs:
    """Bounded jobs, with results accepted exactly once for issued jobs only."""

    def __init__(self) -> None:
        self.pending: queue.Queue[dict] = queue.Queue(MAX_JOBS)
        self.active: dict[str, str] = {}
        self.results: dict[str, str] = {}
        self.done = threading.Condition()
        self.last_seen = 0.0

    def submit(self, script: str) -> str:
        if not isinstance(script, str) or len(script.encode("utf-8")) > MAX_SCRIPT:
            raise ValueError("script must be text, at most 256 KiB")
        job = {"id": uuid.uuid4().hex, "script": script}
        with self.done:
            if len(self.active) >= MAX_JOBS:
                raise ValueError("too many outstanding jobs")
            # Remove cancelled pending jobs before admitting another one.
            retained = []
            while not self.pending.empty():
                item = self.pending.get_nowait()
                if item["id"] in self.active:
                    retained.append(item)
            for item in retained:
                self.pending.put_nowait(item)
            self.active[job["id"]] = "pending"
            self.pending.put_nowait(job)
        return job["id"]

    def next(self, timeout: float = 25) -> dict:
        deadline = time.monotonic() + timeout
        while True:
            try:
                job = self.pending.get(timeout=max(0, deadline - time.monotonic()))
            except queue.Empty:
                return {}
            with self.done:
                if self.active.get(job["id"]) == "pending":
                    self.active[job["id"]] = "issued"
                    return job

    def complete(self, job: str, output: str) -> bool:
        with self.done:
            if self.active.get(job) != "issued":
                return False
            self.active[job] = "complete"
            self.results[job] = output
            self.done.notify_all()
            return True

    def wait(self, job: str, timeout: float) -> str | None:
        deadline = time.monotonic() + timeout
        with self.done:
            try:
                while job not in self.results:
                    left = deadline - time.monotonic()
                    if left <= 0:
                        return None
                    self.done.wait(left)
                return self.results.pop(job)
            finally:
                self.active.pop(job, None)


def strict_json(raw: bytes) -> object:
    def pairs(items):
        result = {}
        for key, value in items:
            if key in result:
                raise ValueError("duplicate JSON member")
            result[key] = value
        return result

    def invalid_constant(_):
        raise ValueError("invalid JSON constant")

    return json.loads(raw.decode("utf-8"), object_pairs_hook=pairs, parse_constant=invalid_constant)


class HeaderBudget:
    """Bound the whole header block, including individually long lines."""

    def __init__(self, source):
        self.source = source
        self.left = 16 * 1024

    def readline(self, size=-1):
        line = self.source.readline(min(8193, self.left + 1, size if size >= 0 else 8193))
        self.left -= len(line)
        if self.left < 0 or len(line) > 8192:
            raise http.client.LineTooLong("header budget exceeded")
        return line


class HarnessServer(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, address, handler):
        self.slots = threading.BoundedSemaphore(16)
        super().__init__(address, handler)

    def process_request(self, request, address):
        if not self.slots.acquire(blocking=False):
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, address)
        except Exception:
            self.slots.release()
            raise

    def process_request_thread(self, request, address):
        try:
            super().process_request_thread(request, address)
        finally:
            self.slots.release()

    def handle_error(self, request, client_address):
        # Never print request data or credentials, even on disconnect/timeouts.
        pass


def http_handler(jobs: Jobs, files: Path, token: str, installed: Path, revision: str):
    if not re.fullmatch(r"[0-9a-f]{64}", token):
        raise ValueError("a 256-bit per-run token is required")

    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def setup(self):
            super().setup()
            self.connection.settimeout(60)

        def parse_request(self):
            if len(self.raw_requestline) > 2048:
                self.requestline = ""
                self.request_version = "HTTP/1.1"
                self.command = None
                self.send_error(414)
                return False
            source = self.rfile
            self.rfile = HeaderBudget(source)
            try:
                return super().parse_request()
            finally:
                self.rfile = source

        def log_message(self, *_: object) -> None:
            pass

        def reply(self, status: int, body: bytes = b"{}", kind: str = "application/json") -> None:
            self.close_connection = True
            self.send_response(status)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Connection", "close")
            self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)

        def validate(self) -> int | None:
            auth = self.headers.get_all("Authorization", [])
            if len(auth) != 1 or not hmac.compare_digest(auth[0].encode("utf-8"), ("Bearer " + token).encode("ascii")):
                self.reply(401)
                return None
            if self.headers.get_all("Transfer-Encoding", []):
                self.reply(400)
                return None
            lengths = self.headers.get_all("Content-Length", [])
            if len(lengths) > 1 or (lengths and not re.fullmatch(r"[0-9]{1,10}", lengths[0])):
                self.reply(400)
                return None
            if self.command == "POST" and not lengths:
                self.reply(411)
                return None
            length = int(lengths[0]) if lengths else 0
            if length > MAX_BODY or (self.command == "GET" and length):
                self.reply(413)
                return None
            if self.path in ("/next", "/result") and self.headers.get_all("X-Nuschl-Agent", []) != [revision]:
                self.reply(409)
                return None
            return length

        def handle_expect_100(self):
            if self.validate() is None:
                return False
            self.send_response_only(100)
            self.end_headers()
            return True

        def do_GET(self) -> None:
            if self.validate() is None:
                return
            if self.path == "/next":
                jobs.last_seen = time.time()
                installed.touch(mode=0o600)
                self.reply(200, json.dumps(jobs.next()).encode())
            elif self.path.startswith("/files/"):
                name = self.path.removeprefix("/files/")
                if not FILE_NAME.fullmatch(name):
                    self.reply(404)
                    return
                try:
                    fd = os.open(files / name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
                except OSError:
                    self.reply(404)
                    return
                info = os.fstat(fd)
                if not stat.S_ISREG(info.st_mode):
                    os.close(fd)
                    self.reply(404)
                    return
                with os.fdopen(fd, "rb") as source:
                    self.close_connection = True
                    self.send_response(200)
                    self.send_header("Content-Type", "application/octet-stream")
                    self.send_header("Content-Length", str(info.st_size))
                    self.send_header("Connection", "close")
                    self.send_header("Cache-Control", "no-store")
                    self.end_headers()
                    shutil.copyfileobj(source, self.wfile)
            else:
                self.reply(404)

        def do_POST(self) -> None:
            length = self.validate()
            if length is None:
                return
            if self.path != "/result":
                self.reply(404)
                return
            if self.headers.get_all("Content-Type", []) not in (["application/json"], ["application/json; charset=utf-8"]):
                self.reply(415)
                return
            try:
                raw = self.rfile.read(length)
                if len(raw) != length:
                    raise ValueError("incomplete body")
                body = strict_json(raw)
                if (not isinstance(body, dict) or set(body) != {"id", "output"}
                        or not isinstance(body["id"], str) or not JOB_ID.fullmatch(body["id"])
                        or not isinstance(body["output"], str)
                        or len(body["output"].encode("utf-8")) > MAX_OUTPUT):
                    raise ValueError("invalid result")
            except (ValueError, UnicodeError, RecursionError):
                self.reply(400)
                return
            if not jobs.complete(body["id"], body["output"]):
                self.reply(409)
                return
            jobs.last_seen = time.time()
            self.reply(200)

    return Handler


class Qmp:
    def __init__(self, path: Path) -> None:
        for _ in range(100):
            try:
                self.sock = socket.socket(socket.AF_UNIX)
                self.sock.connect(str(path))
                break
            except OSError:
                time.sleep(0.1)
        else:
            raise RuntimeError("QMP socket did not appear")
        self.file = self.sock.makefile("rw")
        self.lock = threading.Lock()
        json.loads(self.file.readline())
        self.command("qmp_capabilities")

    def command(self, name: str, **arguments: object) -> object:
        with self.lock:
            self.file.write(json.dumps({"execute": name, "arguments": arguments}) + "\n")
            self.file.flush()
            while True:
                reply = json.loads(self.file.readline())
                if "return" in reply:
                    return reply["return"]
                if "error" in reply:
                    raise RuntimeError(reply["error"])


def png_size(path: Path) -> tuple[int, int]:
    with path.open("rb") as image:
        header = image.read(24)
    return struct.unpack(">II", header[16:24])


def build_seed(state: Path, token: str) -> None:
    """Rotate credentials on private read-only media on EVERY boot, including upgrades."""
    xorriso = nix_file(XORRISO_FLAKE_ATTR, "bin/xorriso")
    with tempfile.TemporaryDirectory(prefix="seed-", dir=state) as directory:
        staging = Path(directory)
        for name in ["autounattend.xml", "firstlogon.ps1", "agent.ps1"]:
            shutil.copyfile(HERE / name, staging / name)
        (staging / "control.token").write_text(token, encoding="ascii")
        # Output must sit outside the source tree, or xorriso may include itself.
        output = state / "seed.next.iso"
        try:
            subprocess.check_call([str(xorriso), "-as", "mkisofs", "-quiet", "-V", "NUSCHLVM",
                                   "-J", "-r", "-o", str(output), str(staging)])
            output.chmod(0o600)
            os.replace(output, state / "seed.iso")
        finally:
            output.unlink(missing_ok=True)


def bootstrap_command() -> str:
    # No secret in QMP, process arguments or logs. Old installations need no reinstall:
    # their existing auto-login session runs the trusted refresh script from new media.
    return ('powershell -NoProfile -ExecutionPolicy Bypass -Command "'
            '& ((Get-Volume -FileSystemLabel NUSCHLVM).DriveLetter + ' + "':\\firstlogon.ps1') -RefreshOnly" + '"')


def refresh_agent(qmp: Qmp, jobs: Jobs, qemu: subprocess.Popen) -> None:
    for delay in (15, 30, 45, 60, 90):
        time.sleep(delay)
        if qemu.poll() is not None or jobs.last_seen:
            return
        # The guest desktop must remain idle until wait-agent reports ready.
        qmp.command("send-key", keys=[{"type": "qcode", "data": "meta_l"}, {"type": "qcode", "data": "r"}])
        time.sleep(1)
        for char in bootstrap_command():
            qmp.command("send-key", keys=qcodes(char))
            time.sleep(0.02)
        qmp.command("send-key", keys=[{"type": "qcode", "data": "ret"}])


def daemon(args: argparse.Namespace) -> None:
    os.umask(0o077)
    STATE.mkdir(parents=True, exist_ok=True, mode=0o700)
    STATE.chmod(0o700)
    # Keep the inode: unlinking a lock file permits two simultaneous owners.
    with (STATE / "daemon.lock").open("a") as guard:
        try:
            fcntl.flock(guard, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            sys.exit("another harness owns this VM state directory")
        daemon_locked(args)


def daemon_locked(args: argparse.Namespace) -> None:
    files = STATE / "files"
    files.mkdir(exist_ok=True)
    disk = STATE / "disk.qcow2"
    variables = STATE / "OVMF_VARS.fd"
    seed = STATE / "seed.iso"
    # Written once the guest agent first answers: the install is complete.
    installed = STATE / "installed"
    installing = not disk.exists()
    if disk.exists() and not installed.exists():
        sys.exit("existing disk has no installed marker; refusing to reinstall; inspect it and restore the marker if installed")
    ovmf = nix_file(OVMF_FLAKE_ATTR, "FV")
    if installing:
        iso = Path(args.iso) if args.iso else next(STATE.glob("*.iso"), None)
        if iso is None or not iso.exists():
            sys.exit(f"no Windows ISO: pass --iso or put one into {STATE}")
        subprocess.check_call(["qemu-img", "create", "-q", "-f", "qcow2", str(disk), "64G"])
        shutil.copyfile(ovmf / "OVMF_VARS.fd", variables)
        variables.chmod(0o600)
    token = secrets.token_hex(32)
    revision = hashlib.sha256((HERE / "agent.ps1").read_bytes()).hexdigest()
    build_seed(STATE, token)
    jobs = Jobs()
    server = HarnessServer(("127.0.0.1", 0), http_handler(jobs, files, token, installed, revision))
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    socat = shutil.which("socat") or sys.exit("socat is required for the guest forwards")
    forwards = f"guestfwd=tcp:10.0.2.100:80-cmd:{socat} STDIO TCP:127.0.0.1:{port}"
    if args.murmur_port:
        forwards += f",guestfwd=tcp:10.0.2.100:64738-cmd:{socat} STDIO TCP:127.0.0.1:{args.murmur_port}"
    for forward in args.forward:
        forwards += f",guestfwd=tcp:10.0.2.100:{forward}-cmd:{socat} STDIO TCP:127.0.0.1:{forward}"
    qmp_path = STATE / "qmp.sock"
    qmp_path.unlink(missing_ok=True)
    command = [
        "qemu-system-x86_64",
        "-name", "nuschl-windows",
        "-machine", "q35,accel=kvm",
        "-cpu", "host",
        "-smp", str(args.cpus),
        "-m", f"{args.memory}M",
        "-drive", f"if=pflash,format=raw,readonly=on,file={ovmf / 'OVMF_CODE.fd'}",
        "-drive", f"if=pflash,format=raw,file={variables}",
        "-drive", f"file={disk},if=none,id=disk0,format=qcow2,discard=unmap",
        "-device", "ide-hd,drive=disk0,bus=ide.0,bootindex=1",
        "-netdev", f"user,id=net0,restrict=on,{forwards}",
        "-device", "e1000e,netdev=net0",
        "-device", "VGA,edid=on,xres=1280,yres=800",
        "-device", "qemu-xhci", "-device", "usb-tablet",
        # A sound card without a host backend: Windows sees real endpoints,
        # nothing reaches the host's audio.
        "-audiodev", "none,id=sound0",
        "-device", "ich9-intel-hda", "-device", "hda-duplex,audiodev=sound0",
        "-qmp", f"unix:{qmp_path},server=on,wait=off",
        "-display", "none",
        "-rtc", "base=utc",
    ]
    if installing:
        command += [
            "-drive", f"file={iso},media=cdrom,if=none,id=cd0,readonly=on",
            "-device", "ide-cd,drive=cd0,bus=ide.1,bootindex=0",
        ]
    if seed.exists():
        command += [
            "-drive", f"file={seed},media=cdrom,if=none,id=cd1,readonly=on",
            "-device", "ide-cd,drive=cd1,bus=ide.2",
        ]
    log = (STATE / "qemu.log").open("a")
    qemu = subprocess.Popen(command, stdout=log, stderr=log)
    qmp = Qmp(qmp_path)
    if installing:
        # UEFI asks to "press any key" before booting the installer.
        for _ in range(25):
            qmp.command("send-key", keys=[{"type": "qcode", "data": "ret"}])
            time.sleep(1)
    if not installing:
        threading.Thread(target=refresh_agent, args=(qmp, jobs, qemu), daemon=True).start()
    control = STATE / "ctl.sock"
    control.unlink(missing_ok=True)
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(str(control))
    listener.listen(4)
    (STATE / "daemon.pid").write_text(str(os.getpid()))
    print(f"VM running (harness port {port}); installing={installing}", flush=True)

    def serve(connection: socket.socket) -> None:
        with connection, connection.makefile("rw") as stream:
            connection.settimeout(30)
            try:
                line = stream.readline(MAX_BODY + 1)
                if len(line) > MAX_BODY or not line.endswith("\n"):
                    raise ValueError("control request too large or incomplete")
                request = strict_json(line.encode("utf-8"))
                if not isinstance(request, dict):
                    raise ValueError("control request must be an object")
                reply = handle(request)
            except Exception as error:  # noqa: BLE001 - report every failure to the client
                reply = {"error": str(error)}
            stream.write(json.dumps(reply) + "\n")
            stream.flush()

    def handle(request: dict) -> dict:
        op = request["op"]
        if op == "status":
            return {
                "qemu": qemu.poll() is None,
                "agent_seen_seconds_ago": time.time() - jobs.last_seen if jobs.last_seen else None,
            }
        if op == "run":
            timeout = request.get("timeout", 600)
            if not isinstance(timeout, (int, float)) or not 0 < timeout <= 3600:
                raise ValueError("job timeout must be between 0 and 3600 seconds")
            job = jobs.submit(request["script"])
            output = jobs.wait(job, timeout)
            return {"output": output} if output is not None else {"error": "timed out"}
        if op == "screenshot":
            qmp.command("screendump", filename=request["path"], format="png")
            return {}
        if op == "click":
            shot = STATE / "last-click.png"
            qmp.command("screendump", filename=str(shot), format="png")
            width, height = png_size(shot)
            x, y = int(request["x"] * 32767 / width), int(request["y"] * 32767 / height)
            qmp.command("input-send-event", events=[
                {"type": "abs", "data": {"axis": "x", "value": x}},
                {"type": "abs", "data": {"axis": "y", "value": y}},
            ])
            time.sleep(0.1)
            button = request.get("button", "left")
            for down in (True, False):
                qmp.command("input-send-event", events=[{"type": "btn", "data": {"down": down, "button": button}}])
                time.sleep(0.05)
            return {}
        if op == "key":
            keys = [{"type": "qcode", "data": key} for key in request["keys"]]
            qmp.command("send-key", keys=keys)
            return {}
        if op == "type":
            for char in request["text"]:
                qmp.command("send-key", keys=qcodes(char))
                time.sleep(0.02)
            return {}
        if op == "down":
            qmp.command("system_powerdown")
            return {}
        if op == "pause":
            qmp.command("stop")
            return {}
        if op == "resume":
            qmp.command("cont")
            return {}
        return {"error": f"unknown op {op}"}

    def accept() -> None:
        while True:
            connection, _ = listener.accept()
            threading.Thread(target=serve, args=(connection,), daemon=True).start()

    threading.Thread(target=accept, daemon=True).start()
    qemu.wait()
    server.shutdown()
    server.server_close()
    listener.close()
    log.close()
    control.unlink(missing_ok=True)
    print(f"QEMU exited with {qemu.returncode}", flush=True)


SHIFTED = {'!': '1', '@': '2', '#': '3', '$': '4', '%': '5', '^': '6', '&': '7', '*': '8', '(': '9', ')': '0',
           '_': 'minus', '+': 'equal', '{': 'bracket_left', '}': 'bracket_right', ':': 'semicolon',
           '"': 'apostrophe', '<': 'comma', '>': 'dot', '?': 'slash', '|': 'backslash', '~': 'grave_accent'}
PLAIN = {' ': 'spc', '-': 'minus', '=': 'equal', '[': 'bracket_left', ']': 'bracket_right', ';': 'semicolon',
         "'": 'apostrophe', ',': 'comma', '.': 'dot', '/': 'slash', '\\': 'backslash', '`': 'grave_accent',
         '\n': 'ret', '\t': 'tab'}


def qcodes(char: str) -> list[dict]:
    if char.isalpha():
        keys = (["shift"] if char.isupper() else []) + [char.lower()]
    elif char.isdigit():
        keys = [char]
    elif char in PLAIN:
        keys = [PLAIN[char]]
    elif char in SHIFTED:
        keys = ["shift", SHIFTED[char]]
    else:
        raise ValueError(f"cannot type {char!r}")
    return [{"type": "qcode", "data": key} for key in keys]


def client(request: dict, timeout: float = 900) -> dict:
    sock = socket.socket(socket.AF_UNIX)
    sock.settimeout(timeout)
    sock.connect(str(STATE / "ctl.sock"))
    with sock, sock.makefile("rw") as stream:
        stream.write(json.dumps(request) + "\n")
        stream.flush()
        reply = json.loads(stream.readline())
    if "error" in reply:
        sys.exit(f"error: {reply['error']}")
    return reply


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    up = commands.add_parser("up")
    up.add_argument("--iso")
    # Two virtual CPUs: the host's build jobs are limited to two cores as well.
    up.add_argument("--cpus", type=int, default=2)
    up.add_argument("--memory", type=int, default=6144)
    up.add_argument("--murmur-port", type=int)
    up.add_argument("--forward", type=int, action="append", default=[],
                    help="also forward 10.0.2.100:PORT to the host's 127.0.0.1:PORT (repeatable)")
    run = commands.add_parser("run")
    run.add_argument("script")
    run.add_argument("--timeout", type=float, default=600)
    push = commands.add_parser("push")
    push.add_argument("file")
    push.add_argument("name", nargs="?")
    shot = commands.add_parser("screenshot")
    shot.add_argument("output")
    click = commands.add_parser("click")
    click.add_argument("x", type=int)
    click.add_argument("y", type=int)
    click.add_argument("--button", default="left")
    typing = commands.add_parser("type")
    typing.add_argument("text")
    key = commands.add_parser("key")
    key.add_argument("combo", help="qcodes joined by '-', e.g. ctrl-alt-delete or meta_l-r")
    commands.add_parser("status")
    commands.add_parser("down")
    commands.add_parser("pause", help="stop the guest's CPUs while it is not needed")
    commands.add_parser("resume")
    wait = commands.add_parser("wait-agent")
    wait.add_argument("--timeout", type=float, default=3600)
    args = parser.parse_args()
    if args.command == "up":
        daemon(args)
    elif args.command == "run":
        reply = client({"op": "run", "script": args.script, "timeout": args.timeout}, args.timeout + 30)
        print(reply["output"], end="")
    elif args.command == "push":
        name = args.name or Path(args.file).name
        if not FILE_NAME.fullmatch(name):
            sys.exit("file name must be a flat ASCII name, at most 128 characters")
        target = STATE / "files" / name
        target.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
        with tempfile.NamedTemporaryFile(dir=target.parent, delete=False) as output:
            temporary = Path(output.name)
            try:
                with Path(args.file).open("rb") as source:
                    shutil.copyfileobj(source, output)
                output.flush()
                os.replace(temporary, target)
            finally:
                temporary.unlink(missing_ok=True)
        print(rf"Get-NuschlTestFile -Name '{name}' -OutFile 'C:\test\{name}'")
    elif args.command == "screenshot":
        client({"op": "screenshot", "path": str(Path(args.output).resolve())})
    elif args.command == "click":
        client({"op": "click", "x": args.x, "y": args.y, "button": args.button})
    elif args.command == "type":
        client({"op": "type", "text": args.text})
    elif args.command == "key":
        client({"op": "key", "keys": args.combo.split("-")})
    elif args.command == "status":
        print(json.dumps(client({"op": "status"})))
    elif args.command == "down":
        client({"op": "down"})
    elif args.command in ("pause", "resume"):
        client({"op": args.command})
    elif args.command == "wait-agent":
        deadline = time.monotonic() + args.timeout
        while time.monotonic() < deadline:
            try:
                status = client({"op": "status"}, 10)
                if status["agent_seen_seconds_ago"] is not None and status["agent_seen_seconds_ago"] < 60:
                    print("agent online")
                    return
            except (OSError, SystemExit):
                pass
            time.sleep(10)
        sys.exit("the guest agent never connected")


if __name__ == "__main__":
    main()
