#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Bounded acquisition/inspection of ONE catalog-pinned Sonoma installer.

Never extracts, mounts, executes or installs package contents. `prefix` authenticates
only the signed TOC; `verify` additionally checks all actual signed payload bytes.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import time
import urllib.request
import xml.etree.ElementTree as ET
import zlib

if "/worker.py" == __file__:
    sys.path.insert(0, "/helpers")  # Only two explicitly mounted trusted helpers.
import ios_xip as xip

URL = ("https://swcdn.apple.com/content/downloads/13/30/140-88558-A_TN3FIE7J63/"
       "p61cqj3ktmcqilz3q64ls4ekp1os9c8qbp/InstallAssistant.pkg")
SIZE = 13654790295
SHA1 = "10632733f9b4ab1397fffc98b53e6790e4c2c1b0"
PREFIX_BYTES = 65536
RESERVE = 64 * 1024**3
EXPECTED = {"SharedSupport.dmg", "PackageInfo", "Scripts", "Payload", "Bom"}
TRAILER_SHA256 = "931cf14d3526a46c400cda1a2134b7988adf19669f110834a2301a2a5353a547"


def response_identity(response, *, prefix, etag=None):
    headers = response.headers
    xip.require(response.geturl() == URL and headers.get("Content-Encoding", "identity") == "identity",
                "unexpected installer redirect/encoding")
    expected = PREFIX_BYTES if prefix else SIZE
    xip.require(headers.get("Content-Length") == str(expected), "installer response length mismatch")
    if prefix or response.status == 206:
        xip.require(response.status == 206 and headers.get("Content-Range") ==
                    f"bytes 0-{expected - 1}/{SIZE}", "installer response range mismatch")
    else:
        xip.require(response.status == 200, "installer response status mismatch")
    actual_etag = headers.get("ETag")
    xip.require(isinstance(actual_etag, str) and 0 < len(actual_etag) <= 256 and
                not actual_etag.startswith("W/"), "missing/weak installer ETag")
    xip.require(etag is None or actual_etag == etag, "installer ETag changed")
    return {"requested_url": URL, "final_url": response.geturl(), "status": response.status,
            "etag": actual_etag, "content_length": expected,
            "content_range": headers.get("Content-Range"),
            "last_modified": headers.get("Last-Modified"), "date": headers.get("Date")}


def acquire(directory, prefix_only):
    from ios_vm import disk_path
    directory = disk_path(directory)
    xip.require(directory.is_dir() and directory.stat().st_uid == os.getuid() and
                not directory.stat().st_mode & 0o077, "use an owned 0700 disk cache directory")
    if prefix_only:
        path = directory / "sonoma-prefix.bin"
        receipt = None
    else:
        path = directory / "InstallAssistant-14.8.9.pkg.partial"
        receipt = json.loads((directory / "prefix-proof.json").read_text())
        xip.require(receipt["authentication"]["revocation_checked"] is True and
                    receipt["authentication"]["software_update_policy"] is True and
                    receipt["whole_payload_verified"] is False and receipt["expected_bytes"] == SIZE,
                    "authenticate exact prefix with current CRLs before acquisition")
    expected = PREFIX_BYTES if prefix_only else SIZE
    xip.require(shutil.disk_usage(directory).free >= expected + RESERVE, "insufficient disk reserve")
    headers = {"Accept-Encoding": "identity"}
    if prefix_only:
        headers["Range"] = f"bytes=0-{PREFIX_BYTES - 1}"
    else:
        headers["If-Match"] = receipt["http"]["etag"]
    request = urllib.request.Request(URL, headers=headers)
    # No overwrite/resume: a failed partial is retained for diagnosis, never
    # silently concatenated with a different source revision.
    fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
    start, count, last_guard = time.monotonic(), 0, 0
    sha1, sha256, prefix_hash = hashlib.sha1(), hashlib.sha256(), hashlib.sha256()
    evidence = {"expected_bytes": expected, "path": str(path), "complete": False}
    try:
        with os.fdopen(fd, "wb") as output, urllib.request.urlopen(request, timeout=30) as response:
            evidence["http"] = response_identity(response, prefix=prefix_only,
                etag=None if prefix_only else receipt["http"]["etag"])
            while True:
                now = time.monotonic()
                xip.require(now - start < 1800, "installer acquisition exceeded 30 minutes")
                if now - last_guard >= 5:
                    xip.require(shutil.disk_usage(directory).free >= RESERVE, "disk reserve exhausted")
                    last_guard = now
                block = response.read(min(1024**2, expected - count + 1))
                if not block:
                    break
                xip.require(count + len(block) <= expected, "installer exceeds exact size cap")
                if count < PREFIX_BYTES:
                    prefix_hash.update(block[:PREFIX_BYTES - count])
                sha1.update(block)
                sha256.update(block)
                output.write(block)
                count += len(block)
            output.flush()
            os.fsync(output.fileno())
        xip.require(count == expected, "installer truncated")
        if not prefix_only:
            xip.require(sha1.hexdigest() == SHA1, "installer catalog SHA1 mismatch")
            xip.require(prefix_hash.hexdigest() == receipt["prefix_sha256"], "installer prefix changed")
        evidence.update({"complete": True, "bytes": count, "sha1": sha1.hexdigest(),
                         "sha256": sha256.hexdigest(), "elapsed_seconds": time.monotonic() - start})
    except BaseException as error:
        evidence.update({"bytes": count, "error": str(error)})
        raise
    finally:
        (directory / ("prefix-http.json" if prefix_only else "acquisition.json")).write_text(
            json.dumps(evidence, indent=2) + "\n")
    if not prefix_only:
        final = directory / "InstallAssistant-14.8.9.pkg"
        xip.require(not final.exists(), "installer final path already exists")
        path.rename(final)
        evidence["path"] = str(final)
        (directory / "acquisition.json").write_text(json.dumps(evidence, indent=2) + "\n")
    return evidence


def worker(openssl, full):
    fd = os.open("/input.pkg", os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        before = os.fstat(stream.fileno())
        xip.require(stat.S_ISREG(before.st_mode) and before.st_size == (SIZE if full else PREFIX_BYTES),
                    "installer input size mismatch")
        toc, heap, certificates, signature, digest = xip.parse_envelope(stream, SIZE)
        root = Path("/apple-root.der").read_bytes()
        crls = {name: Path(f"/{name}.crl").read_bytes() for name in ("root", "software")}
        authentication = xip.authenticate(openssl, root, certificates, signature, digest, Path("/tmp"), crls)
        entries, ranges = {}, []
        for node in toc.findall("file"):
            name = xip.one(node, "name").text
            xip.require(name in EXPECTED and name not in entries and not node.findall("file") and
                        xip.one(node, "type").text == "file", "unexpected installer entry")
            data = xip.one(node, "data")
            offset, length, size = (xip.number(data, k) for k in ("offset", "length", "size"))
            xip.require(offset >= 276 and 0 < length <= SIZE - heap - offset,
                        "installer entry range exceeds signed bounds")
            encoding = xip.one(data, "encoding").attrib
            raw = name not in {"Bom", "PackageInfo"}
            xip.require(encoding == {"style": "application/octet-stream" if raw else "application/x-gzip"}
                        and (raw or (length <= 65536 and size <= 1024**2)), "unsupported installer encoding")
            archived, extracted = (xip.sha1_field(data, k) for k in ("archived-checksum", "extracted-checksum"))
            xip.require(not raw or (length == size and archived == extracted), "raw installer size/hash mismatch")
            ranges.append((heap + offset, heap + offset + length))
            entries[name] = {"offset": heap + offset, "length": length, "size": size,
                             "archived_sha1": archived, "extracted_sha1": extracted}
            if full:
                actual = xip.stream_digest(stream, heap + offset, length, "sha1")
                xip.require(actual == archived, name + " signed payload checksum mismatch")
                if not raw:
                    encoded = xip.read_at(stream, heap + offset, length, SIZE, 65536)
                    z = zlib.decompressobj()
                    decoded = z.decompress(encoded, size + 1)
                    xip.require(len(decoded) == size and z.eof and not z.unused_data and
                                not z.unconsumed_tail and hashlib.sha1(decoded).hexdigest() == extracted,
                                name + " extracted checksum/compression mismatch")
                    if name == "PackageInfo":
                        xip.require(b"<!DOCTYPE" not in decoded.upper() and b"<!ENTITY" not in decoded.upper(),
                                    "PackageInfo document types forbidden")
                        info = ET.fromstring(decoded)
                        xip.require(info.tag == "pkg-info" and info.attrib.get("identifier") ==
                                    "com.apple.pkg.InstallAssistant.macOSSonoma" and
                                    info.attrib.get("version") == "23.9.631", "package identity/version mismatch")
                        entries[name]["package_attributes"] = info.attrib
                entries[name]["payload_hash_verified"] = True
        xip.require(set(entries) == EXPECTED, "missing installer entries")
        ranges.sort()
        # Observed pinned package has a 32-byte gap before SharedSupport and an
        # opaque 1024-byte trailer beginning `koly` OUTSIDE its signed XAR entries.
        # Do not parse it as a disk image or call those bytes signed payload.
        # Full verification binds ALL bytes to the HTTPS catalog's whole SHA1,
        # and independently compares the exact retained trailer fingerprint.
        xip.require(ranges[0][0] == heap + 276 and ranges[-1][1] == SIZE - 1024 and
                    all(0 <= b[0] - a[1] <= 32 for a, b in zip(ranges, ranges[1:])),
                    "unexpected installer overlap/gap/trailing data")
        result = {"expected_bytes": SIZE, "authentication": authentication, "entries": entries,
                  "whole_payload_verified": full, "package_extracted_or_executed": False,
                  "nested_dmg_opened": False, "version": "14.8.9", "build": "23J631"}
        result["outside_signed_entries"] = {
            "gap_bytes": sum(b[0] - a[1] for a, b in zip(ranges, ranges[1:])),
            "trailer_bytes": 1024, "expected_trailer_sha256": TRAILER_SHA256,
            "actual_trailer_fingerprint_verified": full,
            "whole_catalog_hash_verified": full}
        stream.seek(0)
        result["prefix_sha256"] = hashlib.sha256(stream.read(PREFIX_BYTES)).hexdigest()
        if full:
            trailer = xip.read_at(stream, SIZE - 1024, 1024, SIZE)
            xip.require(hashlib.sha256(trailer).hexdigest() == TRAILER_SHA256,
                        "opaque outside-XAR trailer fingerprint mismatch")
            xip.require(xip.stream_digest(stream, 0, SIZE, "sha1") == SHA1, "whole installer catalog SHA1 mismatch")
            result["package_sha256"] = xip.stream_digest(stream, 0, SIZE, "sha256")
        after = os.fstat(stream.fileno())
        xip.require((before.st_size, before.st_mtime_ns, before.st_ctime_ns) ==
                    (after.st_size, after.st_mtime_ns, after.st_ctime_ns), "installer changed while inspecting")
        return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("command", choices=["range", "acquire", "prefix", "verify", "worker"])
    p.add_argument("path", type=Path)
    p.add_argument("--apple-root", type=Path)
    p.add_argument("--root-crl", type=Path)
    p.add_argument("--software-crl", type=Path)
    p.add_argument("--openssl", default=shutil.which("openssl"))
    p.add_argument("--full", action="store_true", help=argparse.SUPPRESS)
    args = p.parse_args()
    if args.command in {"range", "acquire"}:
        print(json.dumps(acquire(args.path, args.command == "range"), indent=2));return
    if args.command == "worker":
        print(json.dumps(worker(args.openssl, args.full), indent=2));return
    from ios_vm import bounded_run
    for path in (args.path, args.apple_root, args.root_crl, args.software_crl):
        xip.require(path is not None and path.is_file() and not path.is_symlink(), "regular explicit inputs required")
    for path in (args.apple_root, args.root_crl, args.software_crl):
        xip.require(path.stat().st_size <= 65536, "oversized public trust input")
    bwrap, prlimit = shutil.which("bwrap"), shutil.which("prlimit")
    xip.require(bwrap and prlimit and args.openssl, "bwrap/prlimit/OpenSSL required")
    command = [prlimit, "--as=268435456", "--cpu=120", "--fsize=1048576", bwrap,
               "--unshare-all", "--die-with-parent", "--new-session", "--cap-drop", "ALL",
               "--ro-bind", "/nix/store", "/nix/store", "--ro-bind", str(args.path.resolve()), "/input.pkg",
               "--ro-bind", str(args.apple_root.resolve()), "/apple-root.der",
               "--ro-bind", str(args.root_crl.resolve()), "/root.crl",
               "--ro-bind", str(args.software_crl.resolve()), "/software.crl",
               "--ro-bind", str(Path(__file__).resolve()), "/worker.py",
               "--ro-bind", str(Path(xip.__file__).resolve()), "/helpers/ios_xip.py",
               "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--clearenv", "--",
               str(Path(sys.executable).resolve()), "-I", "/worker.py", "worker", "/input.pkg",
               "--openssl", str(Path(args.openssl).resolve())]
    if args.command == "verify":command.append("--full")
    result = bounded_run(command, timeout=150, limit=128 * 1024)
    xip.require(result["exit_code"] == 0, result["output"])
    proof = json.loads(result["output"])
    if args.command == "prefix":
        http = json.loads((args.path.parent / "prefix-http.json").read_text())
        xip.require(http["complete"] is True and http["bytes"] == PREFIX_BYTES and
                    http["sha256"] == proof["prefix_sha256"] and
                    http["http"]["requested_url"] == URL, "prefix HTTP receipt mismatch")
        proof["http"] = http["http"]
    print(json.dumps(proof, indent=2))


if __name__ == "__main__":
    try:main()
    except (xip.InspectionError, ValueError, OSError, ET.ParseError, subprocess.SubprocessError) as error:
        print(json.dumps({"error": str(error)}), file=sys.stderr);sys.exit(2)
