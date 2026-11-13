"""A minimal TURN client (RFC 8489/8656) and UDP echo peer for nix/turn-stream-vm.nix.

  probe binding --server ADDR
  probe allocate --server ADDR --username U --password P [--transport udp|tcp|tls]
                 [--relay-transport udp|tcp] [--allocations N] [--family 4|6]
                 [--peer ADDR:PORT ...] [--cafile PEM --sni NAME]
  probe echo --port N [--port M ...]

Results are printed as one JSON object; the test script asserts on them.
"""

import argparse
import hashlib
import hmac
import ipaddress
import json
import os
import select
import socket
import ssl
import struct
import sys
import time

MAGIC = 0x2112A442
BINDING, ALLOCATE, SEND, DATA, CREATE_PERMISSION = 0x001, 0x003, 0x006, 0x007, 0x008
REQUEST, INDICATION, SUCCESS, ERROR = 0, 1, 2, 3
USERNAME, INTEGRITY, ERROR_CODE = 0x0006, 0x0008, 0x0009
PEER, PAYLOAD, REALM, NONCE = 0x0012, 0x0013, 0x0014, 0x0015
RELAYED, ADDRESS_FAMILY, TRANSPORT = 0x0016, 0x0017, 0x0019


def message_type(method, cls):
    return (
        (method & 0x00F)
        | ((method & 0x070) << 1)
        | ((method & 0xF80) << 2)
        | ((cls & 1) << 4)
        | ((cls & 2) << 7)
    )


def attribute(kind, value):
    return struct.pack("!HH", kind, len(value)) + value + b"\0" * (-len(value) % 4)


def build(method, cls, txid, attributes, key=None):
    body = b"".join(attributes)
    kind = message_type(method, cls)
    if key is not None:
        header = struct.pack("!HHI", kind, len(body) + 24, MAGIC) + txid
        body += attribute(INTEGRITY, hmac.new(key, header + body, hashlib.sha1).digest())
    return struct.pack("!HHI", kind, len(body), MAGIC) + txid + body


def parse(data):
    kind, length, magic = struct.unpack("!HHI", data[:8])
    if magic != MAGIC or len(data) < 20 + length:
        raise ValueError("not a STUN message")
    method = (kind & 0x00F) | ((kind >> 1) & 0x070) | ((kind >> 2) & 0xF80)
    cls = ((kind >> 4) & 1) | ((kind >> 7) & 2)
    attributes = {}
    offset = 20
    while offset + 4 <= 20 + length:
        a_kind, a_length = struct.unpack("!HH", data[offset : offset + 4])
        attributes.setdefault(a_kind, data[offset + 4 : offset + 4 + a_length])
        offset += 4 + a_length + (-a_length % 4)
    return method, cls, data[8:20], attributes


def xor_address(address, port, txid):
    ip = ipaddress.ip_address(address)
    key = struct.pack("!I", MAGIC) + txid
    raw = bytes(byte ^ key[i] for i, byte in enumerate(ip.packed))
    return struct.pack("!BBH", 0, 1 if ip.version == 4 else 2, port ^ (MAGIC >> 16)) + raw


def unxor_address(value, txid):
    key = struct.pack("!I", MAGIC) + txid
    port = struct.unpack("!H", value[2:4])[0] ^ (MAGIC >> 16)
    ip = ipaddress.ip_address(bytes(byte ^ key[i] for i, byte in enumerate(value[4:])))
    return str(ip), port


def error_text(attributes):
    value = attributes.get(ERROR_CODE)
    if value is None:
        return "error"
    code = (value[2] & 7) * 100 + value[3]
    return f"error {code} {value[4:].decode(errors='replace').strip()}"


class Connection:
    def __init__(self, args):
        self.udp = args.transport == "udp"
        family = socket.AF_INET6 if ":" in args.server else socket.AF_INET
        port = args.port or (5349 if args.transport == "tls" else 3478)
        if self.udp:
            self.sock = socket.socket(family, socket.SOCK_DGRAM)
            self.sock.connect((args.server, port))
        else:
            self.sock = socket.create_connection((args.server, port), timeout=5)
            if args.transport == "tls":
                context = ssl.create_default_context(cafile=args.cafile)
                self.sock = context.wrap_socket(self.sock, server_hostname=args.sni)
        self.buffer = b""
        self.indications = []

    def send(self, data):
        self.sock.sendall(data)

    def receive(self, timeout):
        """One STUN message, or None after the timeout."""
        deadline = time.monotonic() + timeout
        while True:
            if not self.udp and len(self.buffer) >= 20:
                length = struct.unpack("!H", self.buffer[2:4])[0] + 20
                if len(self.buffer) >= length:
                    data, self.buffer = self.buffer[:length], self.buffer[length:]
                    return data
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None
            pending = isinstance(self.sock, ssl.SSLSocket) and self.sock.pending()
            if not pending and not select.select([self.sock], [], [], remaining)[0]:
                return None
            chunk = self.sock.recv(65536)
            if self.udp:
                return chunk
            if not chunk:
                return None
            self.buffer += chunk

    def transact(self, method, attributes, key=None, txid=None):
        txid = txid or os.urandom(12)
        message = build(method, REQUEST, txid, attributes(txid), key)
        for _ in range(4 if self.udp else 1):
            self.send(message)
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline:
                data = self.receive(deadline - time.monotonic())
                if data is None:
                    break
                reply = parse(data)
                if reply[1] == INDICATION:
                    self.indications.append(reply)
                elif reply[2] == txid and reply[0] == method:
                    return reply[1], reply[3], txid
        return None, {}, txid


class Session:
    """An authenticated allocation on one connection."""

    def __init__(self, args, connection):
        self.args = args
        self.connection = connection
        self.key = None
        self.auth = []

    def request(self, method, attributes):
        cls, reply, txid = self.connection.transact(
            method, lambda txid: attributes(txid) + self.auth, self.key
        )
        if cls == ERROR and error_text(reply).startswith(("error 401", "error 438")):
            realm, nonce = reply.get(REALM), reply.get(NONCE)
            if realm is None or nonce is None:
                return cls, reply, txid
            if self.key is None or error_text(reply).startswith("error 438"):
                secret = f"{self.args.username}:{realm.decode()}:{self.args.password}"
                self.key = hashlib.md5(secret.encode()).digest()
                self.auth = [
                    attribute(USERNAME, self.args.username.encode()),
                    attribute(REALM, realm),
                    attribute(NONCE, nonce),
                ]
                cls, reply, txid = self.connection.transact(
                    method, lambda txid: attributes(txid) + self.auth, self.key
                )
        return cls, reply, txid

    def allocate(self):
        protocol = 6 if self.args.relay_transport == "tcp" else 17
        extra = []
        if self.args.family == 6:
            extra.append(attribute(ADDRESS_FAMILY, bytes([2, 0, 0, 0])))
        cls, reply, txid = self.request(
            ALLOCATE, lambda txid: [attribute(TRANSPORT, bytes([protocol, 0, 0, 0]))] + extra
        )
        if cls == SUCCESS:
            return {"result": "ok", "relayed": "%s:%d" % unxor_address(reply[RELAYED], txid)}
        if cls is None:
            return {"result": "timeout"}
        return {"result": error_text(reply)}

    def permission(self, address):
        cls, reply, _ = self.request(
            CREATE_PERMISSION, lambda txid: [attribute(PEER, xor_address(address, 0, txid))]
        )
        if cls == SUCCESS:
            return "ok"
        return "timeout" if cls is None else error_text(reply)

    def echo(self, address, port):
        payload = b"nuschl-turn-probe-" + os.urandom(8).hex().encode()
        txid = os.urandom(12)
        self.connection.send(
            build(
                SEND,
                INDICATION,
                txid,
                [attribute(PEER, xor_address(address, port, txid)), attribute(PAYLOAD, payload)],
            )
        )
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            for indication in self.connection.indications:
                if indication[0] == DATA and indication[3].get(PAYLOAD) == payload:
                    return True
            data = self.connection.receive(deadline - time.monotonic())
            if data is None:
                break
            reply = parse(data)
            if reply[0] == DATA and reply[3].get(PAYLOAD) == payload:
                return True
        return False


def split_peer(peer):
    address, _, port = peer.rpartition(":")
    return address.strip("[]"), int(port)


def run_allocate(args):
    sessions = []
    results = []
    for _ in range(args.allocations):
        session = Session(args, Connection(args))
        result = session.allocate()
        results.append(result)
        if result["result"] == "ok":
            sessions.append(session)
    output = {"allocations": results, "peers": {}}
    if sessions:
        session = sessions[0]
        for peer in args.peer:
            address, port = split_peer(peer)
            permission = session.permission(address)
            echoed = permission == "ok" and session.echo(address, port)
            output["peers"][peer] = {"permission": permission, "echo": echoed}
    return output


def run_binding(args):
    connection = Connection(args)
    cls, reply, _ = connection.transact(BINDING, lambda txid: [])
    if cls == SUCCESS:
        return {"binding": "ok"}
    return {"binding": "timeout" if cls is None else error_text(reply)}


def run_echo(args):
    sockets = []
    for port in args.port:
        sock = socket.socket(socket.AF_INET6, socket.SOCK_DGRAM)
        sock.setsockopt(socket.IPPROTO_IPV6, socket.IPV6_V6ONLY, 0)
        sock.bind(("::", port))
        sockets.append((sock, port))
    print(f"echo listening on {args.port}", flush=True)
    ports = {sock: port for sock, port in sockets}
    while True:
        for sock in select.select(list(ports), [], [])[0]:
            data, source = sock.recvfrom(65536)
            print(f"echo port={ports[sock]} from={source[0]} bytes={len(data)}", flush=True)
            sock.sendto(data, source)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["binding", "allocate", "echo"])
    parser.add_argument("--server")
    parser.add_argument("--port", type=int, action="append")
    parser.add_argument("--transport", choices=["udp", "tcp", "tls"], default="udp")
    parser.add_argument("--relay-transport", choices=["udp", "tcp"], default="udp")
    parser.add_argument("--family", type=int, choices=[4, 6], default=4)
    parser.add_argument("--allocations", type=int, default=1)
    parser.add_argument("--username", default="")
    parser.add_argument("--password", default="")
    parser.add_argument("--peer", action="append", default=[])
    parser.add_argument("--cafile")
    parser.add_argument("--sni")
    args = parser.parse_args()
    if args.mode == "echo":
        run_echo(args)
        return
    args.port = args.port[0] if args.port else None
    result = run_binding(args) if args.mode == "binding" else run_allocate(args)
    print(json.dumps(result, sort_keys=True))


if __name__ == "__main__":
    sys.exit(main())
