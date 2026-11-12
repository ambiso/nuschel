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
import lzma
import os
from pathlib import Path
import re
import resource
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


def authenticate(openssl, root, certificates, signature, digest, directory, crls=None):
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
    revocation = []
    if crls is not None:
        combined = bytearray()
        for name, issuer_index in (("software", 1), ("root", 2)):
            der = crls[name]
            require(0 < len(der) <= 65536, "CRL exceeds limit")
            source, pem = directory / (name + ".crl.der"), directory / (name + ".crl.pem")
            source.write_bytes(der)
            openssl_run(openssl, "crl", "-inform", "DER", "-in", str(source), "-out", str(pem))
            info = openssl_run(openssl, "crl", "-in", str(pem), "-noout", "-issuer",
                               "-lastupdate", "-nextupdate", "-verify", "-CAfile",
                               str(directory / f"cert{issuer_index}.pem")).decode()
            combined.extend(pem.read_bytes())
            revocation.append({"kind": name, "sha256": hashlib.sha256(der).hexdigest(),
                               "signed_crl": info})
        (directory / "crls.pem").write_bytes(combined)
        # Includes signed CRL issuer/signature, current last/next-update and
        # certificate serial checks. Expired cached CRLs fail, never soft-pass.
        openssl_run(openssl, "verify", "-no-CAfile", "-no-CApath", "-no-CAstore",
                    "-trusted", str(directory / "cert2.pem"), "-untrusted",
                    str(directory / "cert1.pem"), "-purpose", "any", "-crl_check_all",
                    "-CRLfile", str(directory / "crls.pem"), str(directory / "cert0.pem"))
    return {"software_update_policy": True, "chain": chain, "certificates": evidence,
            "rsa_pkcs1_sha1_toc_signature": True, "current_certificate_dates": True,
            "revocation_checked": crls is not None, "crls": revocation,
            "native_apple_xip_policy": False}


def stream_digest(stream, offset, length, algorithm):
    digest = hashlib.new(algorithm)
    stream.seek(offset)
    while length:
        chunk = stream.read(min(length, 1024 * 1024))
        require(bool(chunk), "payload truncated during hash")
        digest.update(chunk)
        length -= len(chunk)
    return digest.hexdigest()


def pbzx_chunks(stream, entry, expanded_limit, evidence):
    """Narrow length-framed PBZX profile; at most one 16MiB compressed chunk.

    Read-only format inspection, not an extractor. Each independent XZ stream
    must reach its own checked end and exact declared expanded length. Framing
    interpretation is tested against this actual authenticated archive.
    """
    start, length = entry["offset"], entry["length"]
    end = start + length
    require(read_at(stream, start, 4, end) == b"pbzx", "unsupported Content magic")
    nominal, = struct.unpack(">Q", read_at(stream, start + 4, 8, end))
    require(nominal == 16 * 1024**2, "unsupported PBZX chunk size")
    offset, expanded, count = start + 12, 0, 0
    while offset < end:
        uncompressed, compressed = struct.unpack(">QQ", read_at(stream, offset, 16, end))
        require(0 < uncompressed <= nominal and 0 < compressed <= nominal,
                "PBZX chunk bounds exceeded")
        require(expanded + uncompressed <= expanded_limit, "PBZX expanded budget exceeded")
        # Only the final chunk may be short; all ranges are checked before reads.
        next_offset = offset + 16 + compressed
        require(next_offset <= end and (uncompressed == nominal or next_offset == end),
                "invalid PBZX chunk framing")
        encoded = read_at(stream, offset + 16, compressed, end, nominal)
        count += 1
        require(count <= 2048, "PBZX chunk count exceeded")
        produced = 0
        if compressed == uncompressed:
            for position in range(0, compressed, 65536):
                piece = encoded[position:position + 65536]
                produced += len(piece)
                yield piece
        else:
            require(encoded.startswith(b"\xfd7zXZ\0"), "unsupported PBZX compression")
            decoder = lzma.LZMADecompressor(format=lzma.FORMAT_XZ, memlimit=80 * 1024**2)
            piece = decoder.decompress(encoded, max_length=65536)
            while True:
                produced += len(piece)
                require(produced <= uncompressed, "PBZX chunk expanded beyond declaration")
                if piece:
                    yield piece
                if decoder.eof:
                    require(not decoder.unused_data, "PBZX chunk has trailing XZ data")
                    break
                require(not decoder.needs_input, "truncated PBZX XZ chunk")
                piece = decoder.decompress(b"", max_length=65536)
        require(produced == uncompressed, "PBZX expanded chunk length mismatch")
        expanded += produced
        offset = next_offset
    require(offset == end, "PBZX trailing bytes")
    evidence.update({"pbzx_chunks": count, "expanded_stream_bytes": expanded,
                     "all_xz_streams_checked": True})


class ChunkReader:
    def __init__(self, chunks):
        self.chunks = iter(chunks)
        self.buffer = b""
        self.position = 0

    def read(self, size):
        require(0 <= size <= 1024 * 1024, "stream read exceeds limit")
        result = bytearray()
        while len(result) < size:
            if not self.buffer:
                self.buffer = next(self.chunks, b"")
                if not self.buffer:
                    break
            taken = min(size - len(result), len(self.buffer))
            result.extend(self.buffer[:taken])
            self.buffer = self.buffer[taken:]
        self.position += len(result)
        return bytes(result)

    def exact(self, size):
        result = self.read(size)
        require(len(result) == size, "truncated CPIO stream")
        return result

    def skip(self, size):
        while size:
            taken = min(size, 65536)
            self.exact(taken)
            size -= taken


def cpio_inspect(reader):
    """Inspect odc headers and a few bounded regular plists; never follow paths."""
    import plistlib
    selected = {
        "./Xcode.app/Contents/Info.plist",
        "./Xcode.app/Contents/version.plist",
        "./Xcode.app/Contents/Developer/Platforms/iPhoneSimulator.platform/Info.plist",
    }
    plists, entries, declared_file_bytes = {}, 0, 0
    sdk_paths, binaries = [], []
    while True:
        header = reader.exact(76)
        require(header[:6] == b"070707" and re.fullmatch(b"[0-7]{70}", header[6:]),
                "unsupported CPIO header")
        mode = int(header[18:24], 8)
        namesize, size = int(header[59:65], 8), int(header[65:76], 8)
        require(1 <= namesize <= 4096 and size <= 16 * 1024**3,
                "CPIO name/file budget exceeded")
        encoded_name = reader.exact(namesize)
        require(encoded_name[-1:] == b"\0" and b"\0" not in encoded_name[:-1],
                "invalid CPIO filename")
        name = encoded_name[:-1].decode("utf-8")
        if name == "TRAILER!!!":
            require(size == 0, "CPIO trailer has data")
            break
        require(name in {".", "./Xcode.app"} or (name.startswith("./Xcode.app/") and
                not any(part == ".." for part in name.split("/"))), "unexpected CPIO root/path")
        entries += 1
        require(entries <= 1000000, "CPIO entry count exceeded")
        declared_file_bytes += size
        sdk = bool(re.fullmatch(
            r"\./Xcode\.app/Contents/Developer/Platforms/iPhoneSimulator\.platform/Developer/SDKs/"
            r"iPhoneSimulator[0-9.]*\.sdk/SDKSettings\.plist", name))
        if sdk:
            sdk_paths.append(name)
            require(len(sdk_paths) <= 16, "too many SDK declarations")
        if name.endswith(("/usr/bin/xcodebuild", "/usr/bin/simctl")):
            binaries.append(name)
            require(len(binaries) <= 16, "too many tool declarations")
        if name in selected or sdk:
            require(stat.S_ISREG(mode) and 0 < size <= 1024 * 1024 and name not in plists,
                    "selected plist is not a unique bounded regular file")
            data = reader.exact(size)
            plist = plistlib.loads(data)
            require(isinstance(plist, dict), "selected plist is not a dictionary")
            keys = {"CFBundleIdentifier", "CFBundleShortVersionString", "CFBundleVersion",
                    "ProductBuildVersion", "ProductVersion", "DTXcodeBuild", "Version",
                    "CanonicalName", "DisplayName", "MinimumSDKVersion", "MaximumDeploymentTarget",
                    "MinimumDeploymentTarget"}
            summary = {k: v for k, v in plist.items() if k in keys}
            require(all(isinstance(v, (str, int)) and len(str(v)) <= 1024
                        for v in summary.values()), "unsupported plist summary field")
            plists[name] = {"sha256": hashlib.sha256(data).hexdigest(), "bytes": size,
                            "fields": summary}
        else:
            reader.skip(size)
    # Complete the PBZX generator too: all XZ checksums/counts are required,
    # including its last chunk; permit only conventional bounded zero padding.
    padding = 0
    while True:
        tail = reader.read(65536)
        if not tail:
            break
        padding += len(tail)
        require(padding <= 1024 * 1024 and not any(tail), "unexpected data after CPIO trailer")
    require(selected <= set(plists), "required Xcode/platform plist absent")
    info = plists["./Xcode.app/Contents/Info.plist"]["fields"]
    require(info.get("CFBundleIdentifier") == "com.apple.dt.Xcode", "unexpected Xcode bundle identity")
    require(bool(sdk_paths), "iPhoneSimulator SDKSettings absent")
    return {"cpio_entries": entries, "cpio_declared_file_bytes": declared_file_bytes,
            "cpio_zero_padding_bytes": padding, "selected_plists": plists,
            "simulator_sdk_settings_paths": sdk_paths, "tool_paths": binaries,
            "payload_written_or_executed": False, "installed_runtime_proven": False}


def inspect_worker(openssl, payload=False, crl_check=False):
    import plistlib
    started = time.monotonic()
    fd = os.open("/input.xip", os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as stream:
        before = os.fstat(stream.fileno())
        require(stat.S_ISREG(before.st_mode) and 28 <= before.st_size <= MAX_ARCHIVE,
                "archive must be a bounded regular file")
        result, certificates, signature, digest = parse_toc(stream, before.st_size)
        with Path("/apple-root.der").open("rb") as root_file:
            root = root_file.read(32769)
        require(len(root) <= 32768, "anchor exceeds limit")
        crls = None
        if crl_check:
            crls = {}
            for name in ("root", "software"):
                with Path(f"/{name}.crl").open("rb") as crl_file:
                    crls[name] = crl_file.read(65537)
        result["authentication"] = authenticate(openssl, root, certificates, signature,
                                                digest, Path("/tmp"), crls)
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
        if payload:
            evidence = {}
            # Metadata's UncompressedSize describes file data, not CPIO framing.
            # Bound framing overhead separately, rather than pretending the two
            # byte counts are interchangeable or trusting the filename's version.
            limit = min(plist["UncompressedSize"] + 512 * 1024**2, 32 * 1024**3)
            chunks = pbzx_chunks(stream, result["entries"]["Content"], limit, evidence)
            result["payload_inspection"] = cpio_inspect(ChunkReader(chunks))
            result["payload_inspection"].update(evidence)
        after = os.fstat(stream.fileno())
        require((before.st_dev, before.st_ino, before.st_size, before.st_mtime_ns,
                 before.st_ctime_ns) == (after.st_dev, after.st_ino, after.st_size,
                                         after.st_mtime_ns, after.st_ctime_ns),
                "archive changed during inspection")
        result.update({"archive_bytes": before.st_size, "elapsed_seconds": time.monotonic() - started,
                       "payload_extracted": False, "bundle_version_verified": payload,
                       "worker_peak_rss_kib": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss,
                       "worker_cpu_seconds": sum((resource.getrusage(resource.RUSAGE_SELF).ru_utime,
                                                  resource.getrusage(resource.RUSAGE_SELF).ru_stime)),
                       "sandbox_process_status": {k: v.strip() for k, v in
                           (line.split(":", 1) for line in Path("/proc/self/status").read_text().splitlines()
                            if line.startswith(("CapEff:", "CapBnd:", "NoNewPrivs:")))}})
        return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path, nargs="?")
    parser.add_argument("--apple-root", type=Path)
    parser.add_argument("--root-crl", type=Path, help="independently retrieved current DER root CRL")
    parser.add_argument("--software-crl", type=Path, help="current DER Software Update CRL; both CRLs required")
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--inspect-payload", action="store_true",
                        help="also stream authenticated PBZX/CPIO plists, without extraction")
    parser.add_argument("--openssl", default=shutil.which("openssl"))
    args = parser.parse_args()
    if args.worker:
        print(json.dumps(inspect_worker(args.openssl, args.inspect_payload, args.root_crl is not None), indent=2))
        return
    require(sys.platform == "linux" and args.archive is not None and args.apple_root is not None,
            "Linux, archive and --apple-root required")
    for path in (args.archive, args.apple_root):
        require(not path.is_symlink() and path.is_file(), "input must be a regular non-symlink file")
    require((args.root_crl is None) == (args.software_crl is None), "both CRLs must be supplied together")
    for path in (args.root_crl, args.software_crl):
        if path is not None:
            require(not path.is_symlink() and path.is_file() and path.stat().st_size <= 65536,
                    "CRL must be a bounded regular non-symlink file")
    from ios_vm import bounded_run
    tools = {name: shutil.which(name) for name in ("bwrap", "prlimit")}
    require(all(tools.values()) and args.openssl, "bwrap, prlimit and OpenSSL required")
    command = [tools["prlimit"], "--as=268435456", "--cpu=" + ("180" if args.inspect_payload else "30"),
               "--fsize=1048576",
               tools["bwrap"], "--unshare-all", "--die-with-parent", "--new-session",
               "--cap-drop", "ALL", "--ro-bind", "/nix/store", "/nix/store",
               "--ro-bind", str(args.archive.resolve()), "/input.xip",
               "--ro-bind", str(args.apple_root.resolve()), "/apple-root.der",
               "--ro-bind", str(Path(__file__).resolve()), "/worker.py"]
    if args.root_crl is not None:
        command.extend(["--ro-bind", str(args.root_crl.resolve()), "/root.crl",
                        "--ro-bind", str(args.software_crl.resolve()), "/software.crl"])
    command.extend(["--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp", "--clearenv",
               "--", str(Path(sys.executable).resolve()), "-I", "/worker.py", "--worker",
               "--openssl", str(Path(args.openssl).resolve())])
    if args.root_crl is not None:
        command.extend(["--root-crl", "/root.crl", "--software-crl", "/software.crl"])
    if args.inspect_payload:
        command.append("--inspect-payload")
    result = bounded_run(command, timeout=240 if args.inspect_payload else 45, limit=128 * 1024)
    require(result["exit_code"] == 0, result["output"])
    print(result["output"], end="")


if __name__ == "__main__":
    try:
        main()
    except (InspectionError, ValueError, OSError, ET.ParseError, subprocess.SubprocessError) as error:
        print(json.dumps({"error": str(error)}), file=sys.stderr)
        sys.exit(2)
