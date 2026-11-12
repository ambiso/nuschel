#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Bounded acquisition/inspection of ONE catalog-pinned Sonoma installer.

Never extracts, mounts, executes or installs package contents. `prefix` authenticates
only the signed XAR TOC; `verify` additionally streams every signed entry and accounts
for every byte of the retained file. Passing `verify` means "authenticated for
acquisition integrity", never "safe to install". Coverage/sources: docs/ios-vm.md.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import resource
import shutil
import stat
import struct
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
# The catalog `Digest` of a XAR .pkg is the SHA-1 of its zlib-compressed TOC, i.e. the
# XAR TOC checksum whose digest the RSA signature covers. It is NOT a whole-file hash
# (whole-file SHA-1 a8b796d4...; that wrong assumption caused the original quarantine).
CATALOG_TOC_DIGEST = "10632733f9b4ab1397fffc98b53e6790e4c2c1b0"
PREFIX_BYTES = 65536
RESERVE = 64 * 1024**3
EXPECTED = {"SharedSupport.dmg", "PackageInfo", "Scripts", "Payload", "Bom"}
# The only bytes no signed TOC entry references. NOT signed; pinned, then reported.
# Gap: as observed in the retained file. Trailer: SHA-256 of a separately retrieved
# 1,024-byte HTTPS tail range. Both are also cross-checked by the chunklist below.
GAP = (11710189, bytes.fromhex("646576696464736b") + bytes(24))
TRAILER = (SIZE - 1024, 1024, "931cf14d3526a46c400cda1a2134b7988adf19669f110834a2301a2a5353a547")
# Catalog IntegrityDataURL `InstallAssistant.pkg.integrityDataV1` (HTTPS, IntegrityDataSize).
CHUNKLIST_BYTES = 46976
CHUNKLIST_SHA256 = "0c7892da5ae690bd88b9637f90b635cbfbea1e10750f05ef40ac8675ecf48472"
BLOCK = 4 * 1024**2


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
                    receipt.get("catalog_binding", {}).get("matches") is True and
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
            # The catalog Digest binds the compressed TOC, which lies inside the already
            # authenticated prefix; whole-file hashes are recorded, never compared to it.
            xip.require(prefix_hash.hexdigest() == receipt["prefix_sha256"], "installer prefix changed")
        evidence.update({"complete": True, "bytes": count, "sha1": sha1.hexdigest(),
                         "sha256": sha256.hexdigest(), "elapsed_seconds": time.monotonic() - start})
        if not prefix_only:
            # Stays a quarantined .partial: only a passing `verify` authenticates payload bytes.
            evidence.update({"quarantined": True, "payload_verified": False})
    except BaseException as error:
        evidence.update({"bytes": count, "error": str(error)})
        raise
    finally:
        (directory / ("prefix-http.json" if prefix_only else "acquisition.json")).write_text(
            json.dumps(evidence, indent=2) + "\n")
    return evidence


def catalog_binding(toc_digest, compressed_toc_bytes):
    """The catalog pins the exact Apple-signed TOC (its XAR TOC checksum), hence revision."""
    xip.require(toc_digest.hex() == CATALOG_TOC_DIGEST, "compressed XAR TOC SHA-1 differs from catalog Digest")
    return {"catalog_Digest": CATALOG_TOC_DIGEST, "compressed_toc_bytes": compressed_toc_bytes,
            "compressed_toc_sha1": toc_digest.hex(), "matches": True,
            "binding": "SHA-1 of zlib-compressed XAR TOC, not of the whole file"}


def signed_entries(toc, heap):
    entries = {}
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
        entries[name] = {"offset": heap + offset, "length": length, "size": size, "raw": raw,
                         "archived_sha1": archived, "extracted_sha1": extracted}
    xip.require(set(entries) == EXPECTED, "missing installer entries")
    return entries


def coverage(heap, entries):
    """Account for EVERY byte: which ranges the Apple signature chain fixes, which not.

    XAR: the RSA signature covers the SHA-1 of the compressed TOC; that signed TOC
    holds each entry's heap offset/length/archived-checksum. Unreferenced heap bytes
    and bytes after the heap are covered by nothing, so only the pinned gap/trailer
    are tolerated, and they are never called signed.
    """
    known = [(0, 28, "XAR header", "fixed: v1/SHA-1 profile constants and exact lengths of the signed TOC"),
             (28, heap, "compressed TOC", "signed: its SHA-1 is the RSA-signed TOC checksum and catalog Digest"),
             (heap, heap + 20, "TOC checksum", "fixed: must equal the recomputed, signed TOC SHA-1"),
             (heap + 20, heap + 276, "RSA signature", "verified: Apple SoftwareUpdate chain with current CRLs")]
    known += [(e["offset"], e["offset"] + e["length"], name,
               "signed: streamed SHA-1 must equal archived-checksum in the signed TOC")
              for name, e in entries.items()]
    regions, position = [], 0
    for start, end, region, protection in sorted(known):
        xip.require(start >= position, "overlapping installer regions")
        if start > position:
            regions.append((position, start, "unreferenced heap gap", None))
        regions.append((start, end, region, protection))
        position = end
    if position < SIZE:
        regions.append((position, SIZE, "trailer after XAR heap", None))
    unsigned = [(start, end) for start, end, _, protection in regions if protection is None]
    xip.require(unsigned == [(GAP[0], GAP[0] + len(GAP[1])), (TRAILER[0], TRAILER[0] + TRAILER[1])],
                "unexpected installer overlap/gap/trailing data")
    xip.require(sum(end - start for start, end, *_ in regions) == SIZE, "installer bytes unaccounted")
    return [{"start": start, "end": end, "bytes": end - start, "region": region,
             "bound_by_signature_chain": protection is not None,
             "protection": protection or "NOT signed: pinned bytes and chunklist cross-check only"}
            for start, end, region, protection in regions]


def chunklist(data):
    """Apple CNKL chunk table (header layout as in OpenCorePkg AppleChunklist.h).

    Cross-check only. Its SigMethod 2 tail is a bare SHA-256 of header+table (a
    self-digest, no key), so it is NOT an independent Apple signature.
    """
    xip.require(len(data) == CHUNKLIST_BYTES and hashlib.sha256(data).hexdigest() == CHUNKLIST_SHA256,
                "chunklist differs from the retained catalog integrity data")
    magic, length, version, method, sig_method, unused, count, offset, sig_offset = struct.unpack(
        "<4sIBBBBQQQ", data[:36])
    xip.require((magic, length, version, method, sig_method, unused, offset) == (b"CNKL", 36, 1, 1, 2, 0, 36)
                and sig_offset == 36 + 36 * count and sig_offset + 32 == len(data),
                "unsupported chunklist profile")
    xip.require(hashlib.sha256(data[:sig_offset]).digest() == data[sig_offset:], "chunklist self-digest mismatch")
    chunks = [struct.unpack_from("<I32s", data, 36 + 36 * i) for i in range(count)]
    xip.require(all(0 < size <= 16 * 1024**2 for size, _ in chunks) and sum(s for s, _ in chunks) == SIZE,
                "chunklist does not span the exact installer")
    return chunks


def stream_pass(stream, entries, chunks):
    """ONE sequential bounded read of every byte: entry SHA-1s, chunk and whole SHA-256."""
    whole, sha1 = hashlib.sha256(), {name: hashlib.sha1() for name in entries}
    ranges = [(e["offset"], e["offset"] + e["length"], sha1[name]) for name, e in entries.items()]
    bad, index, left, chunk, position = [], 0, chunks[0][0], hashlib.sha256(), 0
    stream.seek(0)
    while position < SIZE:
        block = memoryview(stream.read(min(BLOCK, SIZE - position)))
        xip.require(len(block) > 0, "installer truncated during hash")
        end = position + len(block)
        whole.update(block)
        for start, stop, digest in ranges:
            if start < end and stop > position:
                digest.update(block[max(start, position) - position:min(stop, end) - position])
        cursor = 0
        while cursor < len(block):
            xip.require(index < len(chunks), "chunklist exhausted before installer end")
            take = min(left, len(block) - cursor)
            chunk.update(block[cursor:cursor + take])
            cursor, left = cursor + take, left - take
            if not left:
                if chunk.digest() != chunks[index][1]:
                    bad.append(index)
                index += 1
                if index < len(chunks):
                    left, chunk = chunks[index][0], hashlib.sha256()
        position = end
    xip.require(index == len(chunks), "installer ended before chunklist")
    return {name: digest.hexdigest() for name, digest in sha1.items()}, bad, whole.hexdigest()


def decoded_entry(stream, name, entry):
    encoded = xip.read_at(stream, entry["offset"], entry["length"], SIZE, 65536)
    z = zlib.decompressobj()
    try:
        decoded = z.decompress(encoded, entry["size"] + 1)
    except zlib.error as error:
        raise xip.InspectionError(name + " signed entry does not decode: " + str(error)) from None
    xip.require(len(decoded) == entry["size"] and z.eof and not z.unused_data and not z.unconsumed_tail,
                name + " compression bounds/length mismatch")
    return decoded


def verify_bytes(stream, entries):
    """Collect EVERY content failure, so tamper reports name exactly what changed."""
    failures = []
    with Path("/integrity.cnkl").open("rb") as source:
        chunks = chunklist(source.read(CHUNKLIST_BYTES + 1))
    actual, bad_chunks, package_sha256 = stream_pass(stream, entries, chunks)
    for name, entry in sorted(entries.items(), key=lambda item: item[1]["offset"]):
        entry["streamed_sha1"] = actual[name]
        entry["archived_checksum_verified"] = actual[name] == entry["archived_sha1"]
        # Raw entries were required to have archived == extracted checksum above.
        entry["extracted_checksum_verified"] = entry["archived_checksum_verified"] and entry["raw"]
        if not entry["archived_checksum_verified"]:
            failures.append(f"signed entry {name}: streamed SHA-1 differs from signed archived-checksum")
        elif not entry["raw"]:
            decoded = decoded_entry(stream, name, entry)
            entry["extracted_checksum_verified"] = hashlib.sha1(decoded).hexdigest() == entry["extracted_sha1"]
            if not entry["extracted_checksum_verified"]:
                failures.append(f"signed entry {name}: decoded SHA-1 differs from signed extracted-checksum")
            elif name == "PackageInfo":
                xip.require(b"<!DOCTYPE" not in decoded.upper() and b"<!ENTITY" not in decoded.upper(),
                            "PackageInfo document types forbidden")
                info = ET.fromstring(decoded)
                xip.require(info.tag == "pkg-info" and info.attrib.get("identifier") ==
                            "com.apple.pkg.InstallAssistant.macOSSonoma" and
                            info.attrib.get("version") == "23.9.631", "package identity/version mismatch")
                entry["package_attributes"] = info.attrib
    gap = xip.read_at(stream, GAP[0], len(GAP[1]), SIZE)
    trailer = hashlib.sha256(xip.read_at(stream, TRAILER[0], TRAILER[1], SIZE)).hexdigest()
    unsigned = {"gap": {"start": GAP[0], "bytes": len(GAP[1]), "hex": gap.hex(), "matches_pin": gap == GAP[1],
                        "pin_source": "observed in retained file; chunklist chunk cross-check"},
                "trailer": {"start": TRAILER[0], "bytes": TRAILER[1], "sha256": trailer,
                            "matches_pin": trailer == TRAILER[2],
                            "pin_source": "separately retrieved HTTPS tail range; chunklist cross-check"},
                "signed": False}
    if gap != GAP[1]:
        failures.append("UNSIGNED 32-byte heap gap differs from pinned bytes (outside Apple signature)")
    if trailer != TRAILER[2]:
        failures.append("UNSIGNED 1024-byte trailer differs from pinned HTTPS fingerprint (outside Apple signature)")
    if bad_chunks:
        failures.append(f"chunklist cross-check (not a signature): {len(bad_chunks)} chunk(s) differ: "
                        + ",".join(map(str, bad_chunks[:32])))
    return {"signed_entry_count": len(entries), "package_sha256": package_sha256,
            "all_signed_entries_verified": all(e["archived_checksum_verified"] and
                                               e["extracted_checksum_verified"] for e in entries.values()),
            "unsigned_regions": unsigned,
            "chunklist_cross_check": {"independent_signature": False, "signature_method": 2,
                                      "self_sha256_verified": True, "chunks": len(chunks),
                                      "chunk_bytes": chunks[0][0], "mismatched_chunks": bad_chunks[:64],
                                      "mismatched_chunk_count": len(bad_chunks)},
            "failures": failures, "acquisition_integrity_authenticated": not failures}


def worker(openssl, full):
    started = time.monotonic()
    fd = os.open("/input.pkg", os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        before = os.fstat(stream.fileno())
        xip.require(stat.S_ISREG(before.st_mode) and before.st_size == (SIZE if full else PREFIX_BYTES),
                    "installer input size mismatch (truncated or extended)")
        toc, heap, certificates, signature, digest = xip.parse_envelope(stream, SIZE)
        with Path("/apple-root.der").open("rb") as source:
            root = source.read(32769)
        crls = {}
        for name in ("root", "software"):
            with Path(f"/{name}.crl").open("rb") as source:
                crls[name] = source.read(65537)
        # Apple's signature first: an altered TOC must fail on the signature itself,
        # not merely on the catalog pin checked next.
        authentication = xip.authenticate(openssl, root, certificates, signature, digest, Path("/tmp"), crls)
        binding = catalog_binding(digest, heap - 28)
        entries = signed_entries(toc, heap)
        result = {"expected_bytes": SIZE, "authentication": authentication, "catalog_binding": binding,
                  "coverage": coverage(heap, entries), "entries": entries,
                  "whole_payload_verified": full, "package_extracted_or_executed": False,
                  "nested_dmg_opened": False, "native_pkgutil_or_installer_run": False,
                  "version": "14.8.9", "build": "23J631"}
        if full:
            result.update(verify_bytes(stream, entries))
            result["claim"] = ("authenticated for acquisition integrity, not safe to install"
                               if result["acquisition_integrity_authenticated"] else "refused; stays quarantined")
        stream.seek(0)
        result["prefix_sha256"] = hashlib.sha256(stream.read(PREFIX_BYTES)).hexdigest()
        after = os.fstat(stream.fileno())
        xip.require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns, before.st_ctime_ns) ==
                    (after.st_dev, after.st_ino, after.st_size, after.st_mtime_ns, after.st_ctime_ns),
                    "installer changed while inspecting")
        usage = resource.getrusage(resource.RUSAGE_SELF)
        result.update({"elapsed_seconds": time.monotonic() - started, "worker_peak_rss_kib": usage.ru_maxrss,
                       "worker_cpu_seconds": usage.ru_utime + usage.ru_stime,
                       "sandbox_process_status": {k: v.strip() for k, v in
                           (line.split(":", 1) for line in Path("/proc/self/status").read_text().splitlines()
                            if line.startswith(("CapEff:", "CapBnd:", "NoNewPrivs:")))}})
        return result


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("command", choices=["range", "acquire", "prefix", "verify", "worker"])
    p.add_argument("path", type=Path)
    p.add_argument("--apple-root", type=Path)
    p.add_argument("--root-crl", type=Path)
    p.add_argument("--software-crl", type=Path)
    p.add_argument("--integrity", type=Path, help="catalog IntegrityDataURL chunklist (verify only)")
    p.add_argument("--openssl", default=shutil.which("openssl"))
    p.add_argument("--full", action="store_true", help=argparse.SUPPRESS)
    args = p.parse_args()
    if args.command in {"range", "acquire"}:
        print(json.dumps(acquire(args.path, args.command == "range"), indent=2));return
    if args.command == "worker":
        report = worker(args.openssl, args.full)
        print(json.dumps(report, indent=2))
        # 3 = parsed and authenticated far enough to report, but refused.
        sys.exit(3 if args.full and not report["acquisition_integrity_authenticated"] else 0)
    from ios_vm import bounded_run
    full = args.command == "verify"
    inputs = [args.apple_root, args.root_crl, args.software_crl] + ([args.integrity] if full else [])
    for path in [args.path] + inputs:
        xip.require(path is not None and path.is_file() and not path.is_symlink(), "regular explicit inputs required")
    for path in inputs:
        xip.require(path.stat().st_size <= 65536, "oversized public trust input")
    bwrap, prlimit = shutil.which("bwrap"), shutil.which("prlimit")
    xip.require(bwrap and prlimit and args.openssl, "bwrap/prlimit/OpenSSL required")
    # One streamed pass over 13.6 GB is about three SHA-x passes of CPU (~25 s here).
    command = [prlimit, "--as=268435456", "--cpu=" + ("300" if full else "120"), "--fsize=1048576", bwrap,
               "--unshare-all", "--die-with-parent", "--new-session", "--cap-drop", "ALL",
               "--ro-bind", "/nix/store", "/nix/store", "--ro-bind", str(args.path.resolve()), "/input.pkg",
               "--ro-bind", str(args.apple_root.resolve()), "/apple-root.der",
               "--ro-bind", str(args.root_crl.resolve()), "/root.crl",
               "--ro-bind", str(args.software_crl.resolve()), "/software.crl",
               *(["--ro-bind", str(args.integrity.resolve()), "/integrity.cnkl"] if full else []),
               "--ro-bind", str(Path(__file__).resolve()), "/worker.py",
               "--ro-bind", str(Path(xip.__file__).resolve()), "/helpers/ios_xip.py",
               "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--clearenv", "--",
               str(Path(sys.executable).resolve()), "-I", "/worker.py", "worker", "/input.pkg",
               "--openssl", str(Path(args.openssl).resolve())] + (["--full"] if full else [])
    result = bounded_run(command, timeout=900 if full else 150, limit=128 * 1024)
    xip.require(result["exit_code"] == 0 or (full and result["exit_code"] == 3), result["output"])
    proof = json.loads(result["output"])
    if full and result["exit_code"] == 3:
        print(json.dumps(proof, indent=2))
        raise xip.InspectionError("installer refused, stays quarantined: " + "; ".join(proof["failures"]))
    if args.command == "prefix":
        http = json.loads((args.path.parent / "prefix-http.json").read_text())
        xip.require(http["complete"] is True and http["bytes"] == PREFIX_BYTES and
                    http["sha256"] == proof["prefix_sha256"] and
                    http["http"]["requested_url"] == URL, "prefix HTTP receipt mismatch")
        proof["http"] = http["http"]
    print(json.dumps(proof, indent=2))


if __name__ == "__main__":
    try:main()
    except (xip.InspectionError, ValueError, OSError, ET.ParseError, subprocess.SubprocessError,
            zlib.error) as error:
        print(json.dumps({"error": str(error)}), file=sys.stderr);sys.exit(2)
