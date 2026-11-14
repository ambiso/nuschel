#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Synthetic peers for the real native link-preview fetch privacy fixture.

Run ONLY inside the fixture's owned user/network/PID namespace. Public-numbered
addresses are local aliases there; the namespace has no default route. This is
not a public-server test or permission to touch host network configuration.
The coordinator supplies the reviewed namespace/resource wrapper and exact
ignored Rust test binary. This process does not weaken production address/TLS
policy. Certificates are ephemeral test inputs written by the Rust test.
The peer namespace keeps its invoking nonroot UID; the test actor drops all
inherited/ambient capabilities before starting its own nested content sandbox.

    link_preview_fetch.py serve --directory OWNED --ip PINNED_IP --net-inode HOST

All protocol peers and bounded scalar logs are synthetic. No profile, user
content, credential, device or ambient proxy is consulted.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import socket
import socketserver
import ssl
import struct
import subprocess
import sys
import threading
import time
import urllib.parse
import zlib

PAGE = "93.184.215.14"
CONTROL = 39211
MAX_EVENTS = 4096
def png_chunk(kind, body):
    return struct.pack("!I", len(body)) + kind + body + struct.pack("!I", zlib.crc32(kind + body))


PNG = (b"\x89PNG\r\n\x1a\n" + png_chunk(b"IHDR", struct.pack("!IIBBBBB", 2, 2, 8, 6, 0, 0, 0))
       + png_chunk(b"IDAT", zlib.compress((b"\x00" + b"\x20\x80\xc0\xff" * 2) * 2)) + png_chunk(b"IEND", b""))


class State:
    def __init__(self, directory):
        self.directory = directory
        self.lock = threading.Lock()
        self.events = []
        self.overrun = False
        self.release_dns = threading.Event()
        self.stop = threading.Event()

    def event(self, kind, **values):
        with self.lock:
            if len(self.events) >= MAX_EVENTS:
                self.overrun = True
                raise RuntimeError("aggregate fixture event budget exceeded")
            self.events.append(dict(kind=kind, **values))

    def snapshot(self):
        with self.lock:
            return dict(events=list(self.events), overrun=self.overrun)


def exact(stream, size):
    data = bytearray()
    while len(data) < size:
        block = stream.recv(size - len(data))
        if not block:
            raise EOFError("short fixture protocol")
        data.extend(block)
    return bytes(data)


def head(stream):
    data = bytearray()
    while not data.endswith(b"\r\n\r\n"):
        data.extend(exact(stream, 1))
        if len(data) > 8192:
            raise ValueError("fixture request header bound")
    return data.decode("ascii")


def reply(stream, body=b"", status="200 OK", mime="text/html", extra=b""):
    stream.sendall((f"HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {len(body)}\r\nConnection: close\r\n").encode() + extra + b"\r\n" + body)


class Server(socketserver.ThreadingTCPServer):
    # Sequential fixture services may rebind through TCP TIME_WAIT. The VM
    # driver independently requires the previous unit's processes to retire.
    allow_reuse_address = True
    daemon_threads = False
    block_on_close = True
    request_queue_size = 4

    def __init__(self, address, handler, state):
        self.state = state
        self.credit = threading.BoundedSemaphore(16)
        super().__init__(address, handler)

    def process_request(self, request, client_address):
        if not self.credit.acquire(blocking=False):
            self.shutdown_request(request)
            self.state.event("peer_busy")
            return
        super().process_request(request, client_address)

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self.credit.release()

    def handle_error(self, request, client_address):
        # Expected TLS rejections/socket cancellation are scalar observations.
        self.state.event("peer_error", port=self.server_address[1], error=sys.exc_info()[0].__name__)


class Http(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(6)
        stream = self.request
        if self.server.server_address[1] == 443:
            context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
            context.minimum_version = ssl.TLSVersion.TLSv1_2
            context.load_cert_chain(self.server.state.directory / "server.pem", self.server.state.directory / "server-key.pem")
            stream = context.wrap_socket(stream, server_side=True)
        with stream:
            request = head(stream)
            first, *headers = request.split("\r\n")
            method, path, version = first.split(" ")
            assert method == "GET" and version == "HTTP/1.1"
            assert "#" not in path
            lowered = [line.lower() for line in headers]
            assert not any(line.startswith(("cookie:", "authorization:", "proxy-authorization:", "referer:")) for line in lowered)
            assert "accept-encoding: identity" in lowered
            host = next(line[5:].strip() for line in headers if line.lower().startswith("host:"))
            parsed = urllib.parse.urlsplit(path)
            self.server.state.event("http", destination=self.server.server_address[0], peer=self.client_address[0], host=host, path=parsed.path)
            if self.server.server_address[0] == "127.0.0.1":
                reply(stream, b"private honeypot")
            elif parsed.path == "/redirect":
                reply(stream, status="302 Found", extra=b"Location: /page\r\n")
            elif parsed.path == "/private-redirect":
                reply(stream, status="302 Found", extra=b"Location: http://127.0.0.1/forbidden\r\n")
            elif parsed.path.startswith("/loop/"):
                number = int(parsed.path.rsplit("/", 1)[1])
                reply(stream, status="302 Found", extra=f"Location: /loop/{number + 1}\r\n".encode())
            elif parsed.path == "/image.png":
                reply(stream, PNG, mime="image/png")
            elif parsed.path == "/compressed":
                reply(stream, b"not actually gzip", extra=b"Content-Encoding: gzip\r\n")
            elif parsed.path == "/ambiguous":
                stream.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\n")
            elif parsed.path == "/large-head":
                stream.sendall(b"HTTP/1.1 200 OK\r\nX-Padding: " + b"x" * (33 * 1024) + b"\r\n\r\n")
            elif parsed.path == "/large-body":
                reply(stream, b"x" * (256 * 1024 + 1))
            elif parsed.path == "/raw-overhead":
                stream.sendall(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: text/html\r\n\r\n" + b"1\r\nx\r\n" * 80000 + b"0\r\n\r\n")
            elif parsed.path == "/stall":
                stream.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 200\r\nContent-Type: text/html\r\n\r\n")
                self.server.state.event("stall_headers_sent", path=parsed.path)
                reason = "eof"
                try:
                    while stream.recv(1):
                        pass
                except ConnectionResetError:
                    reason = "ECONNRESET"
                self.server.state.event("tcp_closed", path=parsed.path, reason=reason)
            elif parsed.path in ("/page", "/private-image", "/text"):
                image = "http://127.0.0.1/forbidden" if parsed.path == "/private-image" else "/image.png"
                body = '<meta property="og:title" content="Synthetic preview"><meta property="og:description" content="Owned fixture">'
                if parsed.path != "/text":
                    body += f'<meta property="og:image" content="{image}">'
                reply(stream, body.encode())
            else:
                reply(stream, status="404 Not Found")


class Control(socketserver.BaseRequestHandler):
    def handle(self):
        self.request.settimeout(2)
        size = struct.unpack("!I", exact(self.request, 4))[0]
        assert size <= 4096
        value = json.loads(exact(self.request, size))
        if value == {"operation": "snapshot"}:
            response = self.server.state.snapshot()
        elif value == {"operation": "release_dns"}:
            self.server.state.release_dns.set()
            response = {"released": True}
        elif value == {"operation": "stop"}:
            self.server.state.release_dns.set()
            self.server.state.stop.set()
            response = {"stopping": True}
        else:
            raise ValueError("unknown fixture control")
        data = json.dumps(response, separators=(",", ":")).encode()
        assert len(data) <= 512 * 1024
        self.request.sendall(struct.pack("!I", len(data)) + data)


def dns(state):
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as server:
        server.bind(("127.0.0.53", 53))
        server.settimeout(0.1)
        while not state.stop.is_set():
            try:
                packet, peer = server.recvfrom(2048)
            except socket.timeout:
                continue
            assert 12 < len(packet) <= 512
            offset, labels = 12, []
            while packet[offset]:
                size = packet[offset]
                assert size <= 63 and offset + 1 + size < len(packet)
                labels.append(packet[offset + 1:offset + 1 + size].decode("ascii"))
                offset += size + 1
            offset += 1
            kind, klass = struct.unpack("!HH", packet[offset:offset + 4])
            assert klass == 1 and kind in (1, 28)
            name = ".".join(labels)
            state.event("dns", name=name, record=kind)
            if name.startswith("hold."):
                assert state.release_dns.wait(6)
            addresses = [PAGE] if kind == 1 else []
            if name.startswith("mixed.") and kind == 1:
                addresses.append("127.0.0.1")
            if name.startswith("private.") and kind == 1:
                addresses = ["127.0.0.1"]
            question = packet[12:offset + 4]
            answer = b"".join(b"\xc0\x0c" + struct.pack("!HHIH", 1, 1, 0, 4) + socket.inet_aton(address) for address in addresses)
            server.sendto(packet[:2] + struct.pack("!HHHHH", 0x8180, 1, len(addresses), 0, 0) + question + answer, peer)


def serve(args):
    directory = args.directory.resolve(strict=True)
    assert directory.is_dir() and directory.stat().st_uid == os.getuid()
    assert os.stat("/proc/self/ns/net").st_ino != args.net_inode, "refusing host network namespace"
    assert os.readlink("/proc/self/ns/user") != args.user_namespace, "refusing host user namespace"
    assert os.getuid() != 0 and os.getgid() != 0, "fixture requires its mapped nonroot identity"
    assert os.stat("/dev/tty").st_rdev == os.stat("/dev/null").st_rdev, "fixture tty must be inert null"
    # Fixed argv only, and only after namespace identity validation.
    def ip(*arguments):
        subprocess.run([str(args.ip), *arguments], check=True, stdin=subprocess.DEVNULL, timeout=2)
    ip("link", "set", "lo", "up")
    ip("address", "add", PAGE + "/32", "dev", "lo")
    routes = subprocess.check_output([str(args.ip), "route", "show", "table", "all"], text=True, timeout=2)
    assert "default" not in routes and " dev " not in routes.replace(" dev lo", " local-loopback"), routes
    state = State(directory)
    services = [Server((address, port), handler, state) for address, port, handler in (
        (PAGE, 80, Http), (PAGE, 443, Http),
        ("127.0.0.1", 80, Http), ("127.0.0.1", CONTROL, Control))]
    threads = [threading.Thread(target=server.serve_forever) for server in services]
    threads.append(threading.Thread(target=dns, args=(state,)))
    for thread in threads:
        thread.start()
    (directory / "peers-ready.json").write_text(json.dumps(dict(pid=os.getpid(), net_inode=os.stat("/proc/self/ns/net").st_ino, routes=routes)) + "\n")
    result = None
    try:
        if args.test_binary:
            assert args.setpriv and args.setpriv.is_file()
            environment = dict(os.environ, NUSCHL_PREVIEW_FIXTURE=str(directory), NUSCHL_PREVIEW_WORKER=str(args.worker))
            command = [str(args.setpriv), "--inh-caps=-all", "--ambient-caps=-all", "--no-new-privs",
                       str(args.test_binary), "--ignored", "--exact", "link_preview::tests::actual_fetch_privacy", "--nocapture", "--test-threads=1"]
            (directory / "test-command.json").write_text(json.dumps(command) + "\n")
            with (directory / "test.log").open("xb") as output:
                # The external reviewed subreaper guard owns ALL descendants,
                # including failure cleanup. Timeout is not retirement proof.
                result = subprocess.run(command, env=environment, stdin=subprocess.DEVNULL, stdout=output, stderr=subprocess.STDOUT, timeout=150)
        else:
            assert state.stop.wait(180), "fixture hard lifetime"
    finally:
        state.stop.set()
        state.release_dns.set()
        for server in services:
            server.shutdown()
            server.server_close()
        for thread in threads:
            thread.join(timeout=8)
            assert not thread.is_alive(), "fixture thread survived cleanup"
        (directory / "peers.json").write_text(json.dumps(state.snapshot(), indent=2) + "\n")
    if result is not None:
        assert result.returncode == 0, f"actual fetch test exited {result.returncode}"
        assert json.loads((directory / "fetch-result.json").read_text())["passed"] is True


def run(args):
    """Exec the fixed namespace command UNDER the coordinator's outer guard.

    This launcher intentionally has no new general process supervisor. Its
    caller must use the reviewed subreaper/device/resource guard and preserve
    leftovers/failures. bwrap monitor exit alone is not a cleanup assertion.
    """
    config = json.loads(args.config.read_text())
    assert set(config) == {"bwrap", "ip", "python", "prlimit", "setpriv", "test_binary", "worker", "path"}
    for key in ("bwrap", "ip", "python", "prlimit", "setpriv", "worker"):
        value = Path(config[key])
        assert str(value).startswith("/nix/store/") and value.is_file(), key
    assert Path(config["setpriv"]).name == "setpriv"
    assert Path(config["setpriv"]).parent == Path(config["prlimit"]).parent
    assert all(str(Path(path)).startswith("/nix/store/") for path in config["path"])
    assert Path(config["test_binary"]).is_file()
    uid, gid = os.getuid(), os.getgid()
    assert uid != 0 and gid != 0, "refusing a host-root fixture invocation"
    directory = args.directory.absolute()
    directory.mkdir(mode=0o700)  # Exclusive ownership; existing run is immutable.
    for name in ("etc", "home", "runtime"):
        (directory / name).mkdir(mode=0o700)
    (directory / "etc/resolv.conf").write_text("nameserver 127.0.0.53\noptions timeout:1 attempts:1\n")
    (directory / "etc/nsswitch.conf").write_text("hosts: dns\n")
    (directory / "etc/hosts").write_text("127.0.0.1 localhost\n")
    shutil.copyfile(config["test_binary"], directory / "session-tests")
    (directory / "session-tests").chmod(0o500)
    binary_hash = hashlib.file_digest((directory / "session-tests").open("rb"), "sha256").hexdigest()
    script = Path(__file__).resolve()
    command = [config["bwrap"], "--unshare-user", "--unshare-pid", "--unshare-net", "--unshare-ipc", "--unshare-uts", "--unshare-cgroup-try",
               "--die-with-parent", "--new-session", "--clearenv", "--cap-drop", "ALL", "--cap-add", "CAP_NET_ADMIN", "--cap-add", "CAP_NET_BIND_SERVICE",
               "--uid", str(uid), "--gid", str(gid), "--ro-bind", "/nix/store", "/nix/store", "--proc", "/proc", "--tmpfs", "/dev",
               "--dev-bind", "/dev/null", "/dev/null", "--dev-bind", "/dev/zero", "/dev/zero", "--dev-bind", "/dev/full", "/dev/full",
               "--dev-bind", "/dev/random", "/dev/random", "--dev-bind", "/dev/urandom", "/dev/urandom", "--dev-bind", "/dev/null", "/dev/tty",
               "--tmpfs", "/tmp", "--tmpfs", "/run",
               "--ro-bind", str(directory / "etc"), "/etc", "--bind", str(directory), "/fixture", "--ro-bind", str(script), "/fixture-script.py", "--chdir", "/fixture"]
    environment = {"PATH": ":".join(config["path"]), "HOME": "/fixture/home", "XDG_RUNTIME_DIR": "/fixture/runtime", "TMPDIR": "/tmp", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8",
                   "PYTHONDONTWRITEBYTECODE": "1", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/no-host-session-bus", "DBUS_SYSTEM_BUS_ADDRESS": "unix:path=/no-host-system-bus",
                   "PULSE_SERVER": "unix:/no-host-pulse", "PIPEWIRE_REMOTE": "/no-host-pipewire"}
    for name, value in environment.items():
        command += ["--setenv", name, value]
    command += [config["prlimit"], "--as=2147483648", "--cpu=150", "--nofile=256", "--fsize=1048576", "--", config["python"], "/fixture-script.py", "serve",
                "--directory", "/fixture", "--ip", config["ip"], "--net-inode", str(os.stat("/proc/self/ns/net").st_ino), "--user-namespace", os.readlink("/proc/self/ns/user"),
                "--test-binary", "/fixture/session-tests", "--worker", config["worker"], "--setpriv", config["setpriv"]]
    (directory / "launch.json").write_text(json.dumps(dict(command=command, config=config, copied_test_sha256=binary_hash,
        source_sha256=hashlib.sha256(script.read_bytes()).hexdigest(), cleanup="external reviewed subreaper guard required; no wrapper-exit retirement claim"), indent=2) + "\n")
    os.execve(command[0], command, {})


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    peer = sub.add_parser("serve")
    peer.add_argument("--directory", type=Path, required=True)
    peer.add_argument("--ip", type=Path, required=True)
    peer.add_argument("--net-inode", type=int, required=True)
    peer.add_argument("--user-namespace", required=True)
    peer.add_argument("--test-binary", type=Path)
    peer.add_argument("--worker", type=Path)
    peer.add_argument("--setpriv", type=Path)
    launcher = sub.add_parser("run", help="must run under reviewed outer subreaper/device/resource guard")
    launcher.add_argument("--config", type=Path, required=True)
    launcher.add_argument("--directory", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "run":
        run(args)
    else:
        assert bool(args.test_binary) == bool(args.worker)
        serve(args)


if __name__ == "__main__":
    main()
