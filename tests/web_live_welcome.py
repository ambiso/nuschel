#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Owned loopback TLS/framing fixture for live ServerConfig welcomes, not Murmur."""
import argparse
import json
import os
from pathlib import Path
import socket
import ssl
import struct
import subprocess
import threading
import time
from hash_document_wire import fields, varint
from local_murmur import eventually, free_port


def integer(key, value):
    return varint(key << 3) + varint(value)


def text(key, value):
    raw = value.encode()
    return varint(key << 3 | 2) + varint(len(raw)) + raw


class Server:
    def __init__(self, root, openssl):
        self.log, self.errors, self.lock, self.stop = [], [], threading.Lock(), threading.Event()
        key, cert = root / "key.pem", root / "cert.pem"
        subprocess.run([str(openssl), "req", "-x509", "-newkey", "ed25519", "-nodes", "-subj", "/CN=localhost", "-days", "1", "-keyout", str(key), "-out", str(cert)], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
        key.chmod(0o600)
        self.tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        self.tls.load_cert_chain(cert, key)
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.port = self.listener.getsockname()[1]
        self.listener.listen(8)
        self.listener.settimeout(.2)
        self.sockets, self.threads = [], []
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def accept(self):
        while not self.stop.is_set():
            try:
                stream, _ = self.listener.accept()
            except TimeoutError:
                continue
            except OSError:
                return
            if len(self.threads) >= 16:
                stream.close()
                continue
            worker = threading.Thread(target=self.connection, args=(stream,), daemon=True)
            self.threads.append(worker)
            worker.start()

    def connection(self, raw):
        raw.settimeout(10)
        try:
            with self.tls.wrap_socket(raw, server_side=True) as stream:
                self.sockets.append(stream)
                stream.settimeout(.2)
                deadline, buffer, authenticated = time.monotonic() + 100, bytearray(), False
                def send(kind, body):
                    stream.sendall(struct.pack(">HI", kind, len(body)) + body)
                while not self.stop.is_set() and time.monotonic() < deadline:
                    try:
                        chunk = stream.recv(64 * 1024)
                    except TimeoutError:
                        continue
                    if not chunk:
                        return
                    buffer.extend(chunk)
                    assert len(buffer) <= 1024 * 1024
                    while len(buffer) >= 6:
                        kind, size = struct.unpack(">HI", buffer[:6])
                        assert size <= 1024 * 1024
                        if len(buffer) < size + 6:
                            break
                        body = bytes(buffer[6:size + 6])
                        del buffer[:size + 6]
                        value = fields(body)
                        if kind == 2 and not authenticated:
                            authenticated = True
                            send(7, integer(1, 0) + text(3, "Root"))
                            send(9, integer(1, 3) + text(3, "live-browser") + integer(5, 0) + integer(9, 1) + integer(10, 1))
                            send(24, text(2, "must-not-show-before-sync"))
                            send(5, integer(1, 3) + integer(2, 48_000) + text(3, "<b>initial.test</b>"))
                            send(24, integer(1, 48_000) + integer(3, 1) + integer(4, 128) + integer(5, 256) + integer(7, 0))
                        elif kind == 11 and authenticated:
                            command = value[5][0].decode()
                            if command == "x" * 129:
                                with self.lock:
                                    self.log.append("send-cap-probe-129")
                                continue
                            assert command in {"repeat", "changed", "empty", "absent", "oversized", "delayed", "original"}
                            with self.lock:
                                self.log.append(command)
                            source = {"repeat": "<b>initial.test</b>", "changed": "<b>changed.test</b><br/>literal &lt;em&gt;tag&lt;/em&gt;<img src='/canary-live-welcome'>", "empty": "", "oversized": "界" * (256 * 1024), "delayed": "<b>old-delayed.test</b>", "original": "<b>initial.test</b>"}.get(command)
                            if command == "delayed":
                                time.sleep(.5)
                            send(24, b"" if source is None else text(2, source))
                            send(11, integer(1, 3) + integer(3, 0) + text(5, "done:" + command))
        except (OSError, ssl.SSLError):
            pass  # Expected closed old sessions and initial untrusted TLS handshakes.
        except Exception as error:
            with self.lock:
                self.errors.append(str(error))
        finally:
            raw.close()

    def close(self):
        self.stop.set()
        self.listener.close()
        for stream in self.sockets:
            try:
                stream.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        self.thread.join(2)
        for worker in self.threads:
            worker.join(2)
        assert not self.thread.is_alive() and not any(t.is_alive() for t in self.threads)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("site", "relay", "openssl", "wrapper", "browser-tmp", "playwright", "output"):
        parser.add_argument("--" + name, required=True, type=Path)
    parser.add_argument("--check-send-cap", action="store_true", help="independent diagnostic for the separately queued negotiated TX-cap defect")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    fixture = Server(args.output, args.openssl)
    port = free_port()
    config = args.output / "relay.toml"
    config.write_text(f'listen="127.0.0.1:{port}"\nports=["1024-65535"]\nallow_networks=["127.0.0.0/8"]\n')
    with (args.output / "relay.log").open("w") as log:
        relay = subprocess.Popen([str(args.relay), "--config", str(config)], stdout=log, stderr=log)
        completed = False
        try:
            def listening():
                with socket.socket() as probe:
                    return probe.connect_ex(("127.0.0.1", port)) == 0
            eventually(listening, "private relay failed to start")
            subprocess.run(["node", str(Path(__file__).with_suffix(".browser.mjs")), str(args.site), f"ws://127.0.0.1:{port}/relay", str(fixture.port), str(args.wrapper), str(args.playwright), str(args.output)], env={**os.environ, "TMPDIR": str(args.browser_tmp), "NUSCHL_REQUIRE_BROWSER_SEND_CAP": "1" if args.check_send_cap else "0"}, timeout=100, check=True)
            completed = True
        finally:
            relay.terminate()
            relay.wait(timeout=10)
            fixture.close()
            (args.output / "wire-actions.json").write_text(json.dumps(fixture.log, indent=2) + "\n")
            (args.output / "wire-errors.json").write_text(json.dumps(fixture.errors, indent=2) + "\n")
            assert not fixture.errors, fixture.errors
            if completed:
                assert fixture.log == ["repeat", "absent", "changed", "empty", "absent", "original", "oversized", "delayed"], fixture.log


if __name__ == "__main__":
    main()
