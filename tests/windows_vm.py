#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""A disposable Windows 11 VM for testing Nuschel's Windows build.

    windows_vm.py up [--iso PATH]     install Windows unattended (first run) and boot it
    windows_vm.py run 'Get-Process'   run PowerShell in the logged-in desktop session
    windows_vm.py push FILE [NAME]    make a host file downloadable in the guest
                                      (Invoke-WebRequest http://10.0.2.100/files/NAME)
    windows_vm.py screenshot OUT.png  capture the VM display
    windows_vm.py click X Y | type TEXT | key ctrl-alt-delete
    windows_vm.py status | pause | resume | down

State (disk, firmware variables, sockets) lives in $NUSCHEL_WINDOWS_VM_DIR,
default ~/vms/nuschel-windows; keep it out of backups and off the RAM-backed
/tmp. The guest has no network except two forwards into host loopback:
10.0.2.100:80 (this harness) and 10.0.2.100:64738 (a test Murmur, --murmur-port).
"""
from __future__ import annotations

import argparse
import http.server
import json
import os
import queue
import shutil
import socket
import struct
import subprocess
import sys
import threading
import time
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent / "windows_vm"
STATE = Path(os.environ.get("NUSCHEL_WINDOWS_VM_DIR", Path.home() / "vms" / "nuschel-windows"))
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


class Jobs:
    """PowerShell jobs for the guest agent, and their results."""

    def __init__(self) -> None:
        self.pending: queue.Queue[dict] = queue.Queue()
        self.results: dict[str, str] = {}
        self.done = threading.Condition()
        self.last_seen = 0.0

    def submit(self, script: str) -> str:
        job = {"id": uuid.uuid4().hex, "script": script}
        self.pending.put(job)
        return job["id"]

    def wait(self, job: str, timeout: float) -> str | None:
        deadline = time.monotonic() + timeout
        with self.done:
            while job not in self.results:
                left = deadline - time.monotonic()
                if left <= 0:
                    return None
                self.done.wait(left)
            return self.results.pop(job)


def http_handler(jobs: Jobs, files: Path):
    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_: object) -> None:
            pass

        def reply(self, status: int, body: bytes, kind: str = "application/json") -> None:
            self.send_response(status)
            self.send_header("Content-Type", kind)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def do_GET(self) -> None:
            if self.path == "/next":
                jobs.last_seen = time.time()
                (STATE / "installed").touch()
                try:
                    job = jobs.pending.get(timeout=25)
                except queue.Empty:
                    job = {}
                self.reply(200, json.dumps(job).encode())
            elif self.path.startswith("/files/"):
                name = self.path.removeprefix("/files/")
                target = (files / name).resolve()
                if target.parent != files.resolve() or not target.is_file():
                    self.reply(404, b"{}")
                    return
                self.send_response(200)
                self.send_header("Content-Type", "application/octet-stream")
                self.send_header("Content-Length", str(target.stat().st_size))
                self.end_headers()
                with target.open("rb") as source:
                    shutil.copyfileobj(source, self.wfile)
            else:
                self.reply(404, b"{}")

        def do_POST(self) -> None:
            length = int(self.headers.get("Content-Length", "0"))
            body = json.loads(self.rfile.read(length) or b"{}")
            if self.path == "/result" and "id" in body:
                with jobs.done:
                    jobs.results[body["id"]] = body.get("output", "")
                    jobs.done.notify_all()
            jobs.last_seen = time.time()
            self.reply(200, b"{}")

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


def daemon(args: argparse.Namespace) -> None:
    STATE.mkdir(parents=True, exist_ok=True)
    files = STATE / "files"
    files.mkdir(exist_ok=True)
    disk = STATE / "disk.qcow2"
    variables = STATE / "OVMF_VARS.fd"
    seed = STATE / "seed.iso"
    # Written once the guest agent first answers: the install is complete.
    installed = STATE / "installed"
    installing = not installed.exists()
    ovmf = nix_file(OVMF_FLAKE_ATTR, "FV")
    if installing:
        iso = Path(args.iso) if args.iso else next(STATE.glob("*.iso"), None)
        if iso is None or not iso.exists():
            sys.exit(f"no Windows ISO: pass --iso or put one into {STATE}")
        disk.unlink(missing_ok=True)
        subprocess.check_call(["qemu-img", "create", "-q", "-f", "qcow2", str(disk), "64G"])
        shutil.copyfile(ovmf / "OVMF_VARS.fd", variables)
        variables.chmod(0o644)
        staging = STATE / "seed"
        shutil.rmtree(staging, ignore_errors=True)
        staging.mkdir()
        for name in ["autounattend.xml", "firstlogon.ps1", "agent.ps1"]:
            shutil.copyfile(HERE / name, staging / name)
        xorriso = nix_file(XORRISO_FLAKE_ATTR, "bin/xorriso")
        subprocess.check_call(
            [str(xorriso), "-as", "mkisofs", "-quiet", "-V", "NUSCHELVM", "-J", "-r", "-o", str(seed), str(staging)]
        )
    jobs = Jobs()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), http_handler(jobs, files))
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    socat = shutil.which("socat") or sys.exit("socat is required for the guest forwards")
    forwards = f"guestfwd=tcp:10.0.2.100:80-cmd:{socat} STDIO TCP:127.0.0.1:{port}"
    if args.murmur_port:
        forwards += f",guestfwd=tcp:10.0.2.100:64738-cmd:{socat} STDIO TCP:127.0.0.1:{args.murmur_port}"
    qmp_path = STATE / "qmp.sock"
    qmp_path.unlink(missing_ok=True)
    command = [
        "qemu-system-x86_64",
        "-name", "nuschel-windows",
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
    control = STATE / "ctl.sock"
    control.unlink(missing_ok=True)
    listener = socket.socket(socket.AF_UNIX)
    listener.bind(str(control))
    listener.listen(4)
    (STATE / "daemon.pid").write_text(str(os.getpid()))
    print(f"VM running (harness port {port}); installing={installing}", flush=True)

    def serve(connection: socket.socket) -> None:
        with connection, connection.makefile("rw") as stream:
            request = json.loads(stream.readline())
            try:
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
            job = jobs.submit(request["script"])
            output = jobs.wait(job, request.get("timeout", 600))
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
        target = STATE / "files" / (args.name or Path(args.file).name)
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(args.file, target)
        print(f"http://10.0.2.100/files/{target.name}")
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
