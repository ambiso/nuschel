#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Bounds regressions plus opt-in real supplied-XIP tamper checks.

NUSCHL_XIP_ARCHIVE and NUSCHL_APPLE_ROOT enable real confined checks. Scratch
must be disk backed; the original archive is never changed. Copies require CoW.
"""
import io
import json
import lzma
import os
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import unittest
import zlib

import ios_xip as xip


class Bounds(unittest.TestCase):
    def test_header_profiles_and_truncation(self):
        for data in (b"", b"xar!", struct.pack(">IHHQQI", 0x78617221, 28, 1, 1, 1, 2)):
            with self.subTest(data=data), self.assertRaises(xip.InspectionError):
                xip.parse_toc(io.BytesIO(data), len(data))

    def test_no_large_toc_allocation(self):
        data = struct.pack(">IHHQQI", 0x78617221, 28, 1, 2**64 - 1, 1, 1)
        with self.assertRaisesRegex(xip.InspectionError, "TOC exceeds limit"):
            xip.parse_toc(io.BytesIO(data), len(data))

    def test_compressed_toc_cannot_expand_past_declared_size(self):
        compressed = zlib.compress(b"a" * 100000)
        data = struct.pack(">IHHQQI", 0x78617221, 28, 1, len(compressed), 10, 1) + compressed
        with self.assertRaisesRegex(xip.InspectionError, "compression bounds"):
            xip.parse_toc(io.BytesIO(data), len(data))

    def test_no_entities_or_external_dtd(self):
        for declaration in (b"<!DOCTYPE xar SYSTEM 'http://example.invalid/a'>",
                            b"<!ENTITY x SYSTEM 'file:///private'>"):
            raw = declaration + b"<xar/>"
            compressed = zlib.compress(raw)
            data = struct.pack(">IHHQQI", 0x78617221, 28, 1, len(compressed), len(raw), 1) + compressed
            with self.subTest(declaration=declaration), self.assertRaisesRegex(
                    xip.InspectionError, "types/entities"):
                xip.parse_toc(io.BytesIO(data), len(data))

    def test_ranges_reject_negative_overflow_and_short_read(self):
        for start, length, total in ((-1, 1, 5), (0, -1, 5), (5, 1, 5), (0, 6, 5),
                                     (0, 5, 5)):
            with self.subTest(start=start, length=length), self.assertRaises(xip.InspectionError):
                xip.read_at(io.BytesIO(b"1234"), start, length, total)

    def test_duplicate_signed_fields_rejected(self):
        from xml.etree import ElementTree as ET
        node = ET.fromstring("<x><size>1</size><size>2</size></x>")
        with self.assertRaisesRegex(xip.InspectionError, "duplicate"):
            xip.number(node, "size")


class PayloadBounds(unittest.TestCase):
    @staticmethod
    def odc(name, data=b"", mode=0o100644):
        encoded = name.encode() + b"\0"
        return (b"070707" + ("000000" * 2).encode() + f"{mode:06o}".encode() +
                b"000000000000000001000000" + b"00000000000" +
                f"{len(encoded):06o}{len(data):011o}".encode() + encoded + data)

    def pbzx(self, data, *, corrupt=False):
        encoded = lzma.compress(data)
        if corrupt:
            encoded = encoded[:-1] + bytes([encoded[-1] ^ 1])
        stream = b"pbzx" + struct.pack(">QQQ", 16 * 1024**2, len(data), len(encoded)) + encoded
        evidence = {}
        chunks = list(xip.pbzx_chunks(io.BytesIO(stream), {"offset": 0, "length": len(stream)},
                                     len(data), evidence))
        return b"".join(chunks), evidence

    def test_checked_xz_and_exact_length(self):
        data = b"odc bounded stream\n" * 1000
        result, evidence = self.pbzx(data)
        self.assertEqual(result, data)
        self.assertEqual(evidence["expanded_stream_bytes"], len(data))
        with self.assertRaises((xip.InspectionError, lzma.LZMAError)):
            self.pbzx(data, corrupt=True)

    def test_no_unbounded_pbzx_allocation_or_expansion(self):
        for expanded, compressed in ((2**64 - 1, 1), (1, 2**64 - 1), (100, 1)):
            data = b"pbzx" + struct.pack(">QQQ", 16 * 1024**2, expanded, compressed)
            with self.subTest(expanded=expanded), self.assertRaises(xip.InspectionError):
                list(xip.pbzx_chunks(io.BytesIO(data), {"offset": 0, "length": len(data)}, 10, {}))

    def test_actual_odc_layout_preserves_plist_version(self):
        import plistlib
        info = {"CFBundleIdentifier": "com.apple.dt.Xcode", "CFBundleShortVersionString": "16.2"}
        data = b"".join(self.odc(name, plistlib.dumps(value)) for name, value in (
            ("./Xcode.app/Contents/Info.plist", info),
            ("./Xcode.app/Contents/version.plist", {"ProductBuildVersion": "fixture"}),
            ("./Xcode.app/Contents/Developer/Platforms/iPhoneSimulator.platform/Info.plist", {}),
            ("./Xcode.app/Contents/Developer/Platforms/iPhoneSimulator.platform/Developer/SDKs/"
             "iPhoneSimulator18.2.sdk/SDKSettings.plist", {"Version": "18.2"})))
        data += self.odc("TRAILER!!!") + bytes(512)
        result = xip.cpio_inspect(xip.ChunkReader([data[:31], data[31:]]))
        self.assertEqual(result["selected_plists"]["./Xcode.app/Contents/Info.plist"]["fields"], info)
        self.assertEqual(result["cpio_zero_padding_bytes"], 512)
        self.assertFalse(result["payload_written_or_executed"])

    def test_traversal_similar_prefix_symlink_and_truncated_paths_rejected(self):
        cases = [self.odc("./Xcode.app/../escape"), self.odc("./Xcode.application/other"),
                 self.odc("./Xcode.app/Contents/Info.plist", b"target", mode=0o120777),
                 self.odc("./Xcode.app/Contents/Info.plist", b"data")[:-2]]
        for data in cases:
            with self.subTest(data=data), self.assertRaises(xip.InspectionError):
                xip.cpio_inspect(xip.ChunkReader([data]))


@unittest.skipUnless(os.environ.get("NUSCHL_XIP_ARCHIVE") and os.environ.get("NUSCHL_APPLE_ROOT"),
                     "set explicit supplied archive and independent Apple root for real checks")
class RealArchive(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.archive = Path(os.environ["NUSCHL_XIP_ARCHIVE"])
        cls.root = Path(os.environ["NUSCHL_APPLE_ROOT"])
        cls.temp = tempfile.TemporaryDirectory(prefix="ios-xip-tamper-")
        cls.directory = Path(cls.temp.name)
        from ios_vm import mount_type, MEMORY_FS
        if mount_type(cls.directory) in MEMORY_FS:
            cls.temp.cleanup()
            raise RuntimeError("real archive tests require disk-backed TMPDIR")
        with cls.archive.open("rb") as stream:
            _, _, _, cls.compressed, _, _ = struct.unpack(">IHHQQI", stream.read(28))

    @classmethod
    def tearDownClass(cls):
        cls.temp.cleanup()

    def invoke(self, archive, root=None, payload=False, crls=None):
        # Only the trusted wrapper handles paths on the host; all archive/native
        # XML/compression/crypto processing is inside its no-network sandbox.
        command = [sys.executable, str(Path(xip.__file__)), str(archive),
                   "--apple-root", str(root or self.root)]
        if payload:
            command.append("--inspect-payload")
        if crls is not None:
            command.extend(["--root-crl", str(crls[0]), "--software-crl", str(crls[1])])
        return subprocess.run(command, capture_output=True, timeout=260 if payload else 50)

    def copy_and_flip(self, name, offset):
        target = self.directory / name
        subprocess.run(["cp", "--reflink=always", "--", str(self.archive), str(target)], check=True)
        with target.open("r+b") as stream:
            stream.seek(offset)
            value = stream.read(1)
            stream.seek(offset)
            stream.write(bytes([value[0] ^ 1]))
        return target

    def test_actual_archive_policy_and_all_checksums(self):
        result = self.invoke(self.archive)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        proof = json.loads(result.stdout)
        self.assertTrue(proof["authentication"]["software_update_policy"])
        self.assertTrue(proof["authentication"]["rsa_pkcs1_sha1_toc_signature"])
        self.assertEqual(proof["sandbox_process_status"], {
            "CapEff": "0000000000000000", "CapBnd": "0000000000000000", "NoNewPrivs": "1"})
        self.assertTrue(all(e["archived_checksum_verified"] for e in proof["entries"].values()))
        self.assertFalse(proof["payload_extracted"])
        self.assertFalse(proof["authentication"]["revocation_checked"])

    def test_actual_signature_bit_corruption_fails(self):
        archive = self.copy_and_flip("signature.xip", 28 + self.compressed + 20)
        result = self.invoke(archive)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("OpenSSL verification failed", result.stderr.decode())

    def test_actual_payload_corruption_fails_after_signature(self):
        archive = self.copy_and_flip("content.xip", 28 + self.compressed + 276)
        result = self.invoke(archive)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Content archived checksum mismatch", result.stderr.decode())

    def test_truncated_actual_archive_fails(self):
        target = self.directory / "truncated.xip"
        subprocess.run(["cp", "--reflink=always", "--", str(self.archive), str(target)], check=True)
        with target.open("r+b") as stream:
            stream.truncate(self.archive.stat().st_size - 1)
        result = self.invoke(target)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("range exceeds archive", result.stderr.decode())

    def test_untrusted_anchor_fails(self):
        bad_root = self.directory / "untrusted-root.der"
        bad_root.write_bytes(self.root.read_bytes() + b"\0")
        result = self.invoke(self.archive, bad_root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("root fingerprint mismatch", result.stderr.decode())

    @unittest.skipUnless(os.environ.get("NUSCHL_XIP_PAYLOAD") == "1", "explicit full stream budget required")
    def test_actual_authenticated_bundle_version_and_sdk(self):
        result = self.invoke(self.archive, payload=True)
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        proof = json.loads(result.stdout)
        self.assertTrue(proof["bundle_version_verified"])
        inspection = proof["payload_inspection"]
        self.assertTrue(inspection["all_xz_streams_checked"])
        info = inspection["selected_plists"]["./Xcode.app/Contents/Info.plist"]["fields"]
        self.assertEqual(info["CFBundleShortVersionString"], "16.2")
        version = inspection["selected_plists"]["./Xcode.app/Contents/version.plist"]["fields"]
        self.assertEqual(version["ProductBuildVersion"], "16C5032a")
        self.assertTrue(any(p["fields"].get("CanonicalName") == "iphonesimulator18.2"
                            for p in inspection["selected_plists"].values()))
        self.assertLess(proof["worker_peak_rss_kib"], 256 * 1024)
        self.assertFalse(inspection["installed_runtime_proven"])

    @unittest.skipUnless(os.environ.get("NUSCHL_ROOT_CRL") and os.environ.get("NUSCHL_SOFTWARE_CRL"),
                         "explicit independently retrieved current CRLs required")
    def test_actual_signed_current_crls_and_corruption(self):
        root, software = (Path(os.environ[k]) for k in ("NUSCHL_ROOT_CRL", "NUSCHL_SOFTWARE_CRL"))
        result = self.invoke(self.archive, crls=(root, software))
        self.assertEqual(result.returncode, 0, result.stderr.decode())
        self.assertTrue(json.loads(result.stdout)["authentication"]["revocation_checked"])
        changed = bytearray(software.read_bytes())
        changed[-1] ^= 1
        damaged = self.directory / "corrupt.crl"
        damaged.write_bytes(changed)
        result = self.invoke(self.archive, crls=(root, damaged))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("OpenSSL verification failed", result.stderr.decode())

    @unittest.skipUnless(os.environ.get("NUSCHL_STALE_ROOT_CRL") and os.environ.get("NUSCHL_SOFTWARE_CRL"),
                         "explicit stale signed CRL negative fixture required")
    def test_actual_signed_but_expired_root_crl_is_rejected(self):
        crls = tuple(Path(os.environ[k]) for k in ("NUSCHL_STALE_ROOT_CRL", "NUSCHL_SOFTWARE_CRL"))
        result = self.invoke(self.archive, crls=crls)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("CRL has expired", result.stderr.decode())


if __name__ == "__main__":
    unittest.main()
