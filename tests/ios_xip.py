#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Read-only, confined inspection of the supplied Apple Software Update XIP.

This deliberately supports a narrow XAR v1/RSA/SHA1 Metadata+Content profile,
not arbitrary extraction. No command starts a guest, installs or executes payloads.
See docs/ios-vm.md for provenance, policy sources and limitations.
"""
from __future__ import annotations

import argparse
import base64
import bz2
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import stat
import struct
import subprocess
import sys
import time
import xml.etree.ElementTree as ET
import zlib

APPLE_ROOT_SHA256 = "b0b1730ecbc7ff4505142c49f1295e6eda6bcaed7e2c68c5be91b5a11001f024"
SOFTWARE_UPDATE_EKU = "1.2.840.113635.100.4.1"
MAX_TOC = 1024 * 1024
MAX_ARCHIVE = 16 * 1024**3


class InspectionError(Exception):
    pass


def require(condition, message):
    if not condition:
        raise InspectionError(message)


def one(node, path):
    found = node.findall(path)
    require(len(found) == 1, "missing or duplicate XAR field: " + path)
    return found[0]


def number(node, path):
    value = one(node, path).text or ""
    require(bool(re.fullmatch(r"[0-9]{1,20}", value)), "invalid XAR integer: " + path)
    return int(value)


def sha1_field(node, name):
    value = one(node, name)
    require(value.attrib == {"style": "sha1"} and
            bool(re.fullmatch(r"[a-f0-9]{40}", value.text or "")), "unsupported payload checksum")
    return value.text


def read_at(stream, offset, length, total, maximum=None):
    require(0 <= offset <= total and 0 <= length <= total - offset,
            "XAR range exceeds archive")
    require(maximum is None or length <= maximum, "XAR object exceeds limit")
    stream.seek(offset)
    result = stream.read(length)
    require(len(result) == length, "truncated XAR object")
    return result


def parse_toc(stream, total):
    header = read_at(stream, 0, 28, total)
    magic, header_size, version, compressed_size, raw_size, algorithm = struct.unpack(
        ">IHHQQI", header)
    require((magic, header_size, version, algorithm) == (0x78617221, 28, 1, 1),
            "unsupported XAR header/checksum profile")
    require(0 < compressed_size <= MAX_TOC and 0 < raw_size <= MAX_TOC,
            "TOC exceeds limit")
    compressed = read_at(stream, 28, compressed_size, total, MAX_TOC)
    decoder = zlib.decompressobj()
    raw = decoder.decompress(compressed, raw_size + 1)
    require(len(raw) == raw_size and decoder.eof and not decoder.unused_data and
            not decoder.unconsumed_tail, "TOC compression bounds/length mismatch")
    require(b"<!DOCTYPE" not in raw.upper() and b"<!ENTITY" not in raw.upper(),
            "TOC document types/entities forbidden")
    tree = ET.fromstring(raw)
    require(tree.tag == "xar" and len(list(tree.iter())) <= 256, "unsupported TOC tree")
    toc = one(tree, "toc")
    require(len(tree) == 1, "unexpected XAR root entries")
    heap = 28 + compressed_size
    checksum = one(toc, "checksum")
    require(checksum.attrib == {"style": "sha1"} and number(checksum, "offset") == 0 and
            number(checksum, "size") == 20, "unsupported TOC checksum layout")
    digest = hashlib.sha1(compressed).digest()
    require(read_at(stream, heap, 20, total) == digest, "TOC checksum mismatch")
    signature = one(toc, "signature")
    require(signature.attrib == {"style": "RSA"} and number(signature, "offset") == 20 and
            number(signature, "size") == 256, "unsupported RSA signature layout")
    sig_bytes = read_at(stream, heap + 20, 256, total)
    cert_nodes = signature.findall(".//{http://www.w3.org/2000/09/xmldsig#}X509Certificate")
    require(len(cert_nodes) == 3, "software-update policy requires exactly three certificates")
    certificates = []
    for cert in cert_nodes:
        encoded = re.sub(r"\s", "", cert.text or "")
        require(0 < len(encoded) <= 32768, "certificate exceeds limit")
        certificates.append(base64.b64decode(encoded, validate=True))
    entries = {}
    for item in toc.findall("file"):
        name = one(item, "name").text
        require(name in {"Metadata", "Content"} and name not in entries and
                not item.findall("file") and one(item, "type").text == "file",
                "unsupported or duplicate XIP entry")
        data = one(item, "data")
        offset, length, size = (number(data, k) for k in ("offset", "length", "size"))
        require(offset >= 276 and length <= total - heap - offset,
                "payload range exceeds archive or overlaps signature")
        entries[name] = {"offset": heap + offset, "length": length, "size": size,
                         "encoding": one(data, "encoding").attrib,
                         "archived_sha1": sha1_field(data, "archived-checksum"),
                         "extracted_sha1": sha1_field(data, "extracted-checksum")}
    require(set(entries) == {"Content", "Metadata"}, "missing XIP payload entry")
    ranges = sorted((e["offset"], e["offset"] + e["length"]) for e in entries.values())
    require(ranges[0][0] == heap + 276 and ranges[0][1] == ranges[1][0] and
            ranges[1][1] == total, "unexpected payload gaps/overlaps/trailing bytes")
    require(entries["Content"]["encoding"] == {"style": "application/octet-stream"} and
            entries["Content"]["length"] == entries["Content"]["size"] and
            entries["Content"]["archived_sha1"] == entries["Content"]["extracted_sha1"],
            "unsupported Content encoding")
    require(entries["Metadata"]["encoding"] == {"style": "application/x-bzip2"} and
            entries["Metadata"]["length"] <= 65536 and entries["Metadata"]["size"] <= 65536,
            "unsupported Metadata encoding/bounds")
    return {"entries": entries, "compressed_toc_sha1": digest.hex(),
            "creation_time": one(toc, "creation-time").text}, certificates, sig_bytes, digest


def openssl_run(openssl, *args):
    result = subprocess.run([openssl, *args], stdin=subprocess.DEVNULL,
                            capture_output=True, timeout=5)
    require(len(result.stdout) + len(result.stderr) <= 65536, "OpenSSL output exceeds limit")
    require(result.returncode == 0, "OpenSSL verification failed: " +
            result.stderr.decode("utf-8", "replace")[:2048])
    return result.stdout


def authenticate(openssl, root, certificates, signature, digest, directory):
    """Apple SWUpdate profile, not generic Apple-issued Developer ID trust.

    SecPolicyCreateAppleSWUpdateSigning requires a three-cert Apple-rooted chain
    and OID 1.2.840.113635.100.4.1 on BOTH leaf and intermediate. Current validity
    is additionally checked here. No test/development roots or EKUs are accepted.
    """
    require(hashlib.sha256(root).hexdigest() == APPLE_ROOT_SHA256,
            "independently supplied Apple production root fingerprint mismatch")
    require(len(certificates) == 3 and certificates[2] == root,
            "embedded root differs from independently supplied Apple root")
    evidence = []
    for i, der in enumerate(certificates):
        source, pem = directory / f"cert{i}.der", directory / f"cert{i}.pem"
        source.write_bytes(der)
        openssl_run(openssl, "x509", "-inform", "DER", "-in", str(source), "-out", str(pem))
        report = openssl_run(openssl, "x509", "-in", str(pem), "-noout", "-subject",
                             "-issuer", "-dates", "-ext", "extendedKeyUsage").decode()
        if i < 2:
            require("X509v3 Extended Key Usage:" in report,
                    "missing Apple software-update EKU")
            eku = report.split("X509v3 Extended Key Usage:", 1)[1]
            eku = eku.removeprefix(" critical").strip()
            require(eku == SOFTWARE_UPDATE_EKU, "not an Apple production software-update signer")
        evidence.append({"sha256": hashlib.sha256(der).hexdigest(), "certificate": report})
    # purpose=any alone is NOT authority: the above mandatory SWUpdate EKUs and
    # independently pinned Apple root restrict it to the reviewed Apple policy.
    chain = openssl_run(openssl, "verify", "-no-CAfile", "-no-CApath", "-no-CAstore",
                        "-trusted", str(directory / "cert2.pem"), "-untrusted",
                        str(directory / "cert1.pem"), "-purpose", "any",
                        str(directory / "cert0.pem")).decode().strip()
    public = directory / "public.pem"
    public.write_bytes(openssl_run(openssl, "x509", "-in", str(directory / "cert0.pem"),
                                   "-pubkey", "-noout"))
    (directory / "signature").write_bytes(signature)
    (directory / "digest").write_bytes(digest)
    openssl_run(openssl, "pkeyutl", "-verify", "-pubin", "-inkey", str(public),
                "-pkeyopt", "digest:sha1", "-in", str(directory / "digest"),
                "-sigfile", str(directory / "signature"))
    return {"software_update_policy": True, "chain": chain, "certificates": evidence,
            "rsa_pkcs1_sha1_toc_signature": True, "current_certificate_dates": True,
            "revocation_checked": False, "native_apple_xip_policy": False}


def stream_digest(stream, offset, length, algorithm):
    digest = hashlib.new(algorithm)
    stream.seek(offset)
    while length:
        chunk = stream.read(min(length, 1024 * 1024))
        require(bool(chunk), "payload truncated during hash")
        digest.update(chunk)
        length -= len(chunk)
    return digest.hexdigest()


def inspect_worker(openssl):
    import plistlib
    started = time.monotonic()
    fd = os.open("/input.xip", os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        before = os.fstat(stream.fileno())
        require(stat.S_ISREG(before.st_mode) and 28 <= before.st_size <= MAX_ARCHIVE,
                "archive must be a bounded regular file")
        result, certificates, signature, digest = parse_toc(stream, before.st_size)
        root = Path("/apple-root.der").read_bytes()
        require(len(root) <= 32768, "anchor exceeds limit")
        result["authentication"] = authenticate(openssl, root, certificates, signature,
                                                digest, Path("/tmp"))
        for name, entry in result["entries"].items():
            actual = stream_digest(stream, entry["offset"], entry["length"], "sha1")
            require(actual == entry["archived_sha1"], name + " archived checksum mismatch")
            entry["archived_checksum_verified"] = True
        m = result["entries"]["Metadata"]
        compressed = read_at(stream, m["offset"], m["length"], before.st_size, 65536)
        decoder = bz2.BZ2Decompressor()
        metadata = decoder.decompress(compressed, max_length=m["size"] + 1)
        require(len(metadata) == m["size"] and decoder.eof and not decoder.unused_data,
                "Metadata decompression bounds/length mismatch")
        require(hashlib.sha1(metadata).hexdigest() == m["extracted_sha1"],
                "Metadata extracted checksum mismatch")
        plist = plistlib.loads(metadata)
        require(isinstance(plist, dict) and set(plist) == {
            "FileSystemCompressionFormat", "UncompressedSize", "Version", "xbs_sortcpio"} and
            plist["Version"] == 1 and type(plist["UncompressedSize"]) is int and
            0 < plist["UncompressedSize"] <= 64 * 1024**3 and
            plist["FileSystemCompressionFormat"] == "10.10" and plist["xbs_sortcpio"] is True,
            "unsupported Metadata profile")
        result["metadata"] = plist
        result["archive_sha256"] = stream_digest(stream, 0, before.st_size, "sha256")
        after = os.fstat(stream.fileno())
        require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns,
                 before.st_ctime_ns) == (after.st_dev, after.st_ino, after.st_size,
                                         after.st_mtime_ns, after.st_ctime_ns),
                "archive changed during inspection")
        result.update({"archive_bytes": before.st_size, "elapsed_seconds": time.monotonic() - started,
                       "payload_extracted": False, "bundle_version_verified": False,
                       "sandbox_process_status": {k: v.strip() for k, v in
                           (line.split(":", 1) for line in Path("/proc/self/status").read_text().splitlines()
                            if line.startswith(("CapEff:", "CapBnd:", "NoNewPrivs:")))}})
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path, nargs="?")
    parser.add_argument("--apple-root", type=Path)
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--openssl", default=shutil.which("openssl"))
    args = parser.parse_args()
    if args.worker:
        print(json.dumps(inspect_worker(args.openssl), indent=2))
        return
    require(sys.platform == "linux" and args.archive is not None and args.apple_root is not None,
            "Linux, archive and --apple-root required")
    for path in (args.archive, args.apple_root):
        require(not path.is_symlink() and path.is_file(), "input must be a regular non-symlink file")
    from ios_vm import bounded_run
    tools = {name: shutil.which(name) for name in ("bwrap", "prlimit")}
    require(all(tools.values()) and args.openssl, "bwrap, prlimit and OpenSSL required")
    command = [tools["prlimit"], "--as=268435456", "--cpu=30", "--fsize=1048576",
               tools["bwrap"], "--unshare-all", "--die-with-parent", "--new-session",
               "--cap-drop", "ALL", "--ro-bind", "/nix/store", "/nix/store",
               "--ro-bind", str(args.archive.resolve()), "/input.xip",
               "--ro-bind", str(args.apple_root.resolve()), "/apple-root.der",
               "--ro-bind", str(Path(__file__).resolve()), "/worker.py",
               "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--clearenv",
               "--", str(Path(sys.executable).resolve()), "-I", "/worker.py", "--worker",
               "--openssl", str(Path(args.openssl).resolve())]
    result = bounded_run(command, timeout=45, limit=128 * 1024)
    require(result["exit_code"] == 0, result["output"])
    print(result["output"], end="")


if __name__ == "__main__":
    try:
        main()
    except (InspectionError, ValueError, OSError, ET.ParseError, subprocess.SubprocessError) as error:
        print(json.dumps({"error": str(error)}), file=sys.stderr)
        sys.exit(2)
