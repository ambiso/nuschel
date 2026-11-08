#!/usr/bin/env python3
"""Exercise real worker executables; this is not a Mumble interoperability test."""

import argparse
import io
import json
import math
import os
from pathlib import Path
import socket
import struct
import subprocess
import sys
import tempfile


def frame(request_id, payload):
    body = json.dumps({"version": 1, "request_id": request_id, "payload": payload}).encode()
    return struct.pack(">I", len(body)) + body


def exchange(executable, payloads, arguments=()):
    process = subprocess.run(
        [str(executable), *map(str, arguments)],
        input=b"".join(frame(index + 1, payload) for index, payload in enumerate(payloads)),
        capture_output=True,
        timeout=30,
        check=True,
    )
    stream = io.BytesIO(process.stdout)
    results = []
    for index in range(len(payloads)):
        header = stream.read(4)
        assert len(header) == 4, process.stderr.decode(errors="replace")
        size, = struct.unpack(">I", header)
        assert size <= 1024 * 1024
        body = stream.read(size)
        assert len(body) == size
        envelope = json.loads(body)
        assert envelope["version"] == 1 and envelope["request_id"] == index + 1
        results.append(envelope["payload"])
    assert stream.read() == b""
    return results


def check_history_pages(binaries, arguments):
    message = {"server": "paged-fixture", "channel": "general", "author": "fixture-user",
               "observed_at_ms": 123, "body": "Repeated text\nwith identical timestamps"}
    appended = exchange(binaries / "nuschel-storage-worker", [
        {"kind": "append", "message": message} for _ in range(11)
    ], arguments)
    expected = [response["value"] for response in appended]
    assert all(response["status"] == "appended" for response in appended)
    seen = []
    before = None
    while True:
        # Each page reopens the persistent store in a new worker process.
        response, = exchange(binaries / "nuschel-storage-worker", [
            {"kind": "history_page", "server": "paged-fixture", "channel": "general",
             "before": before, "limit": 4},
        ], arguments)
        assert response["status"] == "history_page", response
        page = response["value"]
        ids = [entry["id"] for entry in page["messages"]]
        assert ids == sorted(ids) and 0 < len(ids) <= 4
        assert all(entry["message"] == message for entry in page["messages"])
        seen.extend(ids)
        cursor = page["next_before"]
        if cursor is None:
            break
        assert cursor == ids[0] and (before is None or cursor < before)
        before = cursor
    assert sorted(seen) == expected, "all history must be reachable without gaps or duplicates"


def check_workers(binaries):
    source = "first\n\nsecond\n```rust\nfn main() {}\n```\nlast"
    rendered, = exchange(binaries / "nuschel-content-worker", [{"text": source}])
    assert rendered["status"] == "rendered"
    blocks = rendered["value"]["blocks"]
    assert blocks[0]["text"] == "first\n\nsecond\n"
    code = next(block for block in blocks if block["kind"] == "code")
    assert code["language"] == "rust"
    assert "".join(run["text"] for run in code["runs"]) == "fn main() {}\n"
    assert len({tuple(run["foreground"]) for run in code["runs"]}) > 1

    failed = subprocess.run(
        [str(binaries / "nuschel-content-worker")], input=struct.pack(">I", 2**32 - 1),
        capture_output=True, timeout=10,
    )
    assert failed.returncode != 0, "oversized input must fail without a body allocation"

    message = {
        "server": "fixture-server", "channel": "general", "author": "fixture-user",
        "observed_at_ms": 123, "body": source,
    }
    with tempfile.TemporaryDirectory(prefix="nuschel-history-") as directory:
        arguments = ["--database", Path(directory) / "history.sqlite3"]
        saved, = exchange(binaries / "nuschel-storage-worker", [{"kind": "append", "message": message}], arguments)
        assert saved["status"] == "appended"
        # A new process reopens the database; this checks persistence, not just an in-memory value.
        restored, = exchange(binaries / "nuschel-storage-worker", [
            {"kind": "recent", "server": "fixture-server", "channel": "general", "limit": 10},
        ], arguments)
        assert restored == {"status": "messages", "value": [message]}
        check_history_pages(binaries, arguments)

    encoded, = exchange(binaries / "nuschel-media-worker", [{"kind": "encode_opus", "samples": [0.0] * 960}])
    assert encoded["status"] == "packet", encoded
    decoded, = exchange(binaries / "nuschel-media-worker", [{"kind": "decode_opus", "packet": encoded["value"]}])
    assert decoded["status"] == "samples", decoded
    assert len(decoded["value"]) == 960
    assert all(math.isfinite(value) for value in decoded["value"])


def check_sandbox(binaries, server):
    # This trusted executable is a test instrument, never a command from a chat message.
    with tempfile.TemporaryDirectory(prefix="nuschel-secret-") as directory:
        secret = Path(directory) / "private"
        secret.write_text("not available to the content worker")
        probe = r"""
import os, socket, sys
assert 'NUSCHEL_TEST_SECRET' not in os.environ
try:
    open(sys.argv[1]).read()
except (FileNotFoundError, PermissionError):
    pass
else:
    raise AssertionError('sandbox exposed an unrelated file')
try:
    socket.create_connection((sys.argv[2], 64738), timeout=1)
except OSError:
    pass
else:
    raise AssertionError('sandbox allowed a network connection')
"""
        environment = os.environ | {"NUSCHEL_TEST_SECRET": "must not be inherited"}
        subprocess.run([
            str(binaries / "nuschel"), "sandbox-probe", sys.executable, "--", "-c", probe,
            str(secret), socket.gethostbyname(server),
        ], check=True, timeout=15, env=environment)
        document = subprocess.run([
            str(binaries / "nuschel"), "render", "line one\nline two",
        ], capture_output=True, check=True, timeout=15)
        assert json.loads(document.stdout)["blocks"] == [{"kind": "text", "text": "line one\nline two"}]


def check_postgres(binaries, url_file):
    message = {"server": "postgres-fixture", "channel": "general", "author": "fixture-user",
               "observed_at_ms": 456, "body": "PostgreSQL\nkeeps newlines"}
    arguments = ["--database-url-file", url_file]
    saved, = exchange(binaries / "nuschel-storage-worker", [{"kind": "append", "message": message}], arguments)
    assert saved["status"] == "appended"
    restored, = exchange(binaries / "nuschel-storage-worker", [
        {"kind": "recent", "server": "postgres-fixture", "channel": "general", "limit": 1},
    ], arguments)
    assert restored == {"status": "messages", "value": [message]}
    check_history_pages(binaries, arguments)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path, nargs="?")
    parser.add_argument("--sandbox-server")
    parser.add_argument("--postgres-url-file", type=Path)
    args = parser.parse_args()
    if args.binaries is None:
        metadata = subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1"],
                                  capture_output=True, text=True, check=True)
        args.binaries = Path(json.loads(metadata.stdout)["target_directory"]) / "debug"
    check_workers(args.binaries)
    if args.sandbox_server:
        check_sandbox(args.binaries, args.sandbox_server)
    if args.postgres_url_file:
        check_postgres(args.binaries, args.postgres_url_file)
    print("Worker persistence, rendering, framing, and codec checks passed.")
