# SPDX-License-Identifier: EUPL-1.2
"""Bounded TEST-ONLY raw Mumble control-frame evidence, owned loopback only."""

import hashlib
import socket
import ssl
import struct
import time


def varint(number):
    result = bytearray()
    while number > 127:
        result.append((number & 127) | 128)
        number >>= 7
    result.append(number)
    return bytes(result)


def fields(raw):
    cursor = 0

    def integer():
        nonlocal cursor
        result = 0
        for shift in range(0, 70, 7):
            assert cursor < len(raw)
            value = raw[cursor]
            cursor += 1
            result |= (value & 127) << shift
            if not value & 128:
                return result
        raise AssertionError("oversized fixture varint")

    result = {}
    while cursor < len(raw):
        tag = integer()
        key, wire = tag >> 3, tag & 7
        assert key > 0
        if wire == 0:
            value = integer()
        elif wire in (1, 2, 5):
            size = integer() if wire == 2 else 8 if wire == 1 else 4
            assert cursor + size <= len(raw)
            value = raw[cursor : cursor + size]
            cursor += size
        else:
            raise AssertionError("unsupported fixture wire kind")
        result.setdefault(key, []).append(value)
    return result


def capture(port, pin, peer, description, comment):
    """Own disposable Murmur, pinned certificate, no host/user credentials."""
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    context.check_hostname = False
    context.verify_mode = ssl.CERT_NONE
    deadline = time.monotonic() + 20
    with context.wrap_socket(
        socket.create_connection(("127.0.0.1", port), timeout=5),
        server_hostname="localhost",
    ) as stream:
        assert hashlib.sha256(stream.getpeercert(binary_form=True)).hexdigest() == pin

        def send(kind, body):
            stream.sendall(struct.pack(">HI", kind, len(body)) + body)

        def read(size):
            result = bytearray()
            while len(result) < size:
                assert time.monotonic() < deadline, "raw fixture deadline"
                part = stream.recv(size - len(result))
                assert part, "raw fixture closed"
                result.extend(part)
            return bytes(result)

        def frame():
            kind, size = struct.unpack(">HI", read(6))
            assert size <= 256 * 1024
            raw = read(size)
            return kind, raw, fields(raw)

        username = b"Hash raw observer"
        send(0, b"\x08" + varint(0x010500) + b"\x12\x0cHash fixture")
        send(2, b"\x0a" + varint(len(username)) + username + b"\x28\x01")
        announced = {}
        for _ in range(200):
            kind, raw, value = frame()
            if kind == 7 and value.get(1) == [0]:
                announced["description"] = {
                    "raw_hex": raw.hex(),
                    "hash": value.get(10, [b""])[0].hex(),
                    "body_absent": 5 not in value,
                }
            if kind == 9 and value.get(1) == [peer]:
                announced["comment"] = {
                    "raw_hex": raw.hex(),
                    "hash": value.get(16, [b""])[0].hex(),
                    "body_absent": 14 not in value,
                }
            if kind == 5:
                break
        else:
            raise AssertionError("raw fixture never synchronized")
        for name, source in [("description", description), ("comment", comment)]:
            assert announced[name]["body_absent"]
            assert announced[name]["hash"] == hashlib.sha1(source.encode()).hexdigest()
        send(23, b"\x10" + varint(peer) + b"\x18\x00")
        replies = {}
        for _ in range(200):
            kind, raw, value = frame()
            if kind == 7 and value.get(1) == [0] and 5 in value:
                assert value[5] == [description.encode()]
                replies["description"] = {
                    "raw_hex": raw.hex(),
                    "hash_absent": 10 not in value,
                }
            if kind == 9 and value.get(1) == [peer] and 14 in value:
                assert value[14] == [comment.encode()]
                replies["comment"] = {
                    "raw_hex": raw.hex(),
                    "hash_absent": 16 not in value,
                }
            if len(replies) == 2:
                break
        assert len(replies) == 2 and all(
            value["hash_absent"] for value in replies.values()
        )
        return {
            "loopback_pinned": True,
            "announced": announced,
            "body_replies": replies,
            "request_ids_absent": True,
        }
