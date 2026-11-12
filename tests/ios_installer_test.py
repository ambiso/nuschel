#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Installer binding/coverage regressions plus opt-in real public-package checks.

NUSCHL_INSTALLER_CACHE (read-only public trust inputs and prefix) enables the prefix
tests. NUSCHL_INSTALLER_PACKAGE (the retained package, never modified) and a
disk-backed NUSCHL_INSTALLER_SCRATCH on the same CoW filesystem enable the full
package tests; every tamper case mutates a `cp --reflink=always` copy there.
"""
import hashlib
import io
import json
import os
from pathlib import Path
import struct
import subprocess
import sys
import unittest
from unittest import mock
import zlib

import ios_installer as installer
import ios_xip as xip


class Response:
    status = 206
    headers = {"Content-Length": "65536", "Content-Range": f"bytes 0-65535/{installer.SIZE}",
               "ETag": '"strong-fixture-etag"'}

    def geturl(self):
        return installer.URL


class HttpBinding(unittest.TestCase):
    def test_exact_prefix_is_bound(self):
        proof = installer.response_identity(Response(), prefix=True)
        self.assertEqual(proof["content_length"], 65536)
        self.assertEqual(proof["final_url"], installer.URL)

    def test_ignored_or_different_range_redirect_encoding_and_identity_rejected(self):
        cases = [({"Content-Length": str(installer.SIZE)}, 200),
                 ({"Content-Range": f"bytes 1-65536/{installer.SIZE}"}, 206),
                 ({"Content-Range": "bytes 0-65535/65536"}, 206),
                 ({"ETag": 'W/"weak"'}, 206), ({"ETag": ""}, 206),
                 ({"Content-Encoding": "gzip"}, 206)]
        for headers, status in cases:
            r = Response();r.headers = {**Response.headers, **headers};r.status = status
            with self.subTest(headers=headers), self.assertRaises(xip.InspectionError):
                installer.response_identity(r, prefix=True)
        r = Response();r.geturl = lambda: "https://unexpected.invalid/installer.pkg"
        with self.assertRaises(xip.InspectionError):
            installer.response_identity(r, prefix=True)

    def test_full_response_must_keep_prefix_etag_and_exact_total(self):
        r = Response();r.status = 200;r.headers = {"Content-Length": str(installer.SIZE), "ETag": '"full"'}
        self.assertEqual(installer.response_identity(r, prefix=False, etag='"full"')["content_length"], installer.SIZE)
        with self.assertRaisesRegex(xip.InspectionError, "ETag changed"):
            installer.response_identity(r, prefix=False, etag='"old"')
        r.headers = {**r.headers, "Content-Length": str(installer.SIZE + 1)}
        with self.assertRaises(xip.InspectionError):
            installer.response_identity(r, prefix=False)


HEAP = 4335
LAYOUT = {"Bom": (4611, 53184), "Payload": (57795, 11651320), "Scripts": (11709115, 644),
          "PackageInfo": (11709759, 430), "SharedSupport.dmg": (11710221, 13643079050)}


def entries(layout=LAYOUT):
    return {name: {"offset": offset, "length": length} for name, (offset, length) in layout.items()}


class Binding(unittest.TestCase):
    def test_catalog_digest_binds_compressed_toc_not_whole_file(self):
        toc = bytes.fromhex(installer.CATALOG_TOC_DIGEST)
        self.assertTrue(installer.catalog_binding(toc, 4307)["matches"])
        whole_file_sha1 = bytes.fromhex("a8b796d408b5b3870be14161b43042b8bfca40f9")
        for digest in (whole_file_sha1, bytes(20), toc[:-1] + bytes([toc[-1] ^ 1])):
            with self.subTest(digest=digest.hex()), self.assertRaisesRegex(xip.InspectionError, "catalog Digest"):
                installer.catalog_binding(digest, 4307)

    def test_every_byte_accounted_and_only_pinned_bytes_unsigned(self):
        regions = installer.coverage(HEAP, entries())
        self.assertEqual(sum(r["bytes"] for r in regions), installer.SIZE)
        self.assertTrue(all(a["end"] == b["start"] for a, b in zip(regions, regions[1:])))
        self.assertEqual([(r["start"], r["bytes"]) for r in regions if not r["bound_by_signature_chain"]],
                         [(11710189, 32), (installer.SIZE - 1024, 1024)])

    def test_any_other_gap_overlap_or_trailing_layout_rejected(self):
        bad = [{**LAYOUT, "Bom": (4612, 53183)},                          # hole after signature
               {**LAYOUT, "Scripts": (11709115, 643)},                     # new hole
               {**LAYOUT, "PackageInfo": (11709759, 462)},                 # gap absorbed
               {**LAYOUT, "Payload": (57794, 11651321)},                   # overlap
               {**LAYOUT, "SharedSupport.dmg": (11710221, 13643079049)},   # longer trailer
               {**LAYOUT, "SharedSupport.dmg": (11710221, 13643080074)}]   # no trailer
        for layout in bad:
            with self.subTest(layout=layout), self.assertRaises(xip.InspectionError):
                installer.coverage(HEAP, entries(layout))

    def test_single_pass_hashes_straddle_blocks_and_names_bad_chunks(self):
        data = bytes(range(256)) * 4
        layout = {"a": (3, 100), "b": (103, 500), "c": (700, 324)}
        chunks = [(256, hashlib.sha256(data[i:i + 256]).digest()) for i in range(0, len(data), 256)]
        with mock.patch.object(installer, "SIZE", len(data)), mock.patch.object(installer, "BLOCK", 7):
            sha1, bad, whole = installer.stream_pass(io.BytesIO(data), entries(layout), chunks)
            self.assertEqual(bad, [])
            self.assertEqual(whole, hashlib.sha256(data).hexdigest())
            self.assertEqual(sha1, {name: hashlib.sha1(data[offset:offset + length]).hexdigest()
                                    for name, (offset, length) in layout.items()})
            tampered = bytearray(data);tampered[600] ^= 1
            sha1, bad, _ = installer.stream_pass(io.BytesIO(bytes(tampered)), entries(layout), chunks)
            self.assertEqual(bad, [2])
            self.assertNotEqual(sha1["b"], hashlib.sha1(data[103:603]).hexdigest())
            with self.assertRaisesRegex(xip.InspectionError, "truncated"):
                installer.stream_pass(io.BytesIO(data[:-1]), entries(layout), chunks)


def trust_args(cache):
    return ["--apple-root", str(cache / "public-signature/apple-root.der"),
            "--root-crl", str(cache / "public-signature/root-http.crl"),
            "--software-crl", str(cache / "public-signature/softwareupdate-http.crl")]


def invoke(command, path, cache, *extra, timeout=60):
    return subprocess.run([sys.executable, str(Path(installer.__file__)), command, str(path),
                           *trust_args(cache), *extra], capture_output=True, timeout=timeout)


def flip(path, offset):
    with open(path, "r+b") as handle:
        handle.seek(offset);value = handle.read(1)[0]
        handle.seek(offset);handle.write(bytes([value ^ 0x01]))


@unittest.skipUnless(os.environ.get("NUSCHL_INSTALLER_CACHE") and os.environ.get("NUSCHL_INSTALLER_SCRATCH"),
                     "explicit real public installer fixture cache and disk scratch required")
class RealSignedPrefix(unittest.TestCase):
    def setUp(self):
        self.cache = Path(os.environ["NUSCHL_INSTALLER_CACHE"])
        self.scratch = Path(os.environ["NUSCHL_INSTALLER_SCRATCH"])

    def test_actual_production_signer_prefix_does_not_claim_payload(self):
        r = invoke("prefix", self.cache / "sonoma-prefix.bin", self.cache, timeout=20)
        self.assertEqual(r.returncode, 0, r.stderr.decode())
        proof = json.loads(r.stdout)
        self.assertTrue(proof["authentication"]["software_update_policy"])
        self.assertTrue(proof["authentication"]["revocation_checked"])
        self.assertEqual(proof["catalog_binding"]["compressed_toc_sha1"], installer.CATALOG_TOC_DIGEST)
        self.assertEqual(set(proof["entries"]), installer.EXPECTED)
        self.assertFalse(proof["whole_payload_verified"])
        self.assertNotIn("acquisition_integrity_authenticated", proof)
        self.assertFalse(proof["package_extracted_or_executed"])

    def test_actual_signature_corruption_rejected_before_download(self):
        data = bytearray((self.cache / "sonoma-prefix.bin").read_bytes())
        header = struct.unpack(">IHHQQI", data[:28]);data[28 + header[3] + 20] ^= 1
        path = self.scratch / "corrupt-installer-prefix.bin";path.write_bytes(data)
        try:
            r = invoke("prefix", path, self.cache, timeout=20)
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("OpenSSL verification failed", r.stderr.decode())
        finally:
            path.unlink()


def same_length_toc(raw, length):
    """Re-deflate an edited TOC to the original length so the heap does not move."""
    for level in range(9, 0, -1):
        for memory in range(9, 0, -1):
            for strategy in (zlib.Z_DEFAULT_STRATEGY, zlib.Z_FILTERED, zlib.Z_HUFFMAN_ONLY, zlib.Z_RLE):
                encoder = zlib.compressobj(level, zlib.DEFLATED, 15, memory, strategy)
                encoded = encoder.compress(raw) + encoder.flush()
                if len(encoded) == length:
                    return encoded
    raise AssertionError("no same-length TOC encoding found")


@unittest.skipUnless(os.environ.get("NUSCHL_INSTALLER_PACKAGE") and os.environ.get("NUSCHL_INSTALLER_CACHE")
                     and os.environ.get("NUSCHL_INSTALLER_SCRATCH"),
                     "explicit retained real package, trust cache and same-filesystem CoW scratch required")
class RealRetainedPackage(unittest.TestCase):
    """About 22 s per complete streamed verification; the original is bound read-only."""

    @classmethod
    def setUpClass(cls):
        cls.package = Path(os.environ["NUSCHL_INSTALLER_PACKAGE"])
        cls.cache = Path(os.environ["NUSCHL_INSTALLER_CACHE"])
        cls.scratch = Path(os.environ["NUSCHL_INSTALLER_SCRATCH"])
        assert cls.package.stat().st_size == installer.SIZE

    def verify(self, path, integrity=None):
        r = invoke("verify", path, self.cache, "--integrity", str(integrity or self.cache / "sonoma.integrity"),
                   timeout=1000)
        return r, (json.loads(r.stdout) if r.stdout else None)

    def copy(self, name):
        target = self.scratch / name
        self.addCleanup(lambda: target.unlink(missing_ok=True))
        # Never a full 13.6 GB copy: fail rather than fall back if CoW is unavailable.
        subprocess.run(["cp", "--reflink=always", "--", str(self.package), str(target)], check=True, timeout=120)
        return target

    def assert_refused(self, r, report, failures):
        self.assertEqual(r.returncode, 2, r.stderr.decode())
        self.assertIn("stays quarantined", r.stderr.decode())
        self.assertFalse(report["acquisition_integrity_authenticated"])
        self.assertEqual(report["claim"], "refused; stays quarantined")
        self.assertEqual(len(report["failures"]), len(failures), report["failures"])
        for failure, expected in zip(report["failures"], failures):
            self.assertIn(expected, failure)

    def test_actual_package_authenticated_for_acquisition_integrity(self):
        r, report = self.verify(self.package)
        self.assertEqual(r.returncode, 0, r.stderr.decode())
        self.assertTrue(report["acquisition_integrity_authenticated"])
        self.assertEqual(report["claim"], "authenticated for acquisition integrity, not safe to install")
        self.assertEqual(report["failures"], [])
        self.assertEqual(report["signed_entry_count"], 5)
        self.assertTrue(all(e["archived_checksum_verified"] and e["extracted_checksum_verified"]
                            for e in report["entries"].values()))
        self.assertEqual(report["catalog_binding"]["compressed_toc_sha1"], installer.CATALOG_TOC_DIGEST)
        self.assertEqual(report["package_sha256"],
                         "ecc36bf172105f8c3325ceeb4e5ab8cc297260be806eac10b21b784ff40d8173")
        self.assertEqual(report["chunklist_cross_check"]["mismatched_chunk_count"], 0)
        self.assertFalse(report["chunklist_cross_check"]["independent_signature"])
        self.assertFalse(report["native_pkgutil_or_installer_run"])
        self.assertEqual(report["sandbox_process_status"]["NoNewPrivs"], "1")

    def test_one_payload_byte_in_each_signed_entry_fails_only_that_entry(self):
        for name, (offset, length) in LAYOUT.items():
            with self.subTest(entry=name):
                target = self.copy(f"flip-{name}.pkg")
                position = offset + length // 2
                flip(target, position)
                r, report = self.verify(target)
                self.assert_refused(r, report, [f"signed entry {name}: streamed SHA-1 differs",
                                                "chunklist cross-check (not a signature): 1 chunk"])
                self.assertEqual(report["chunklist_cross_check"]["mismatched_chunks"], [position // 10485760])
                for other, entry in report["entries"].items():
                    self.assertEqual(entry["archived_checksum_verified"], other != name, other)
                target.unlink()

    def test_unsigned_trailer_and_gap_flips_are_reported_not_ignored(self):
        cases = {"trailer": (installer.SIZE - 1, "UNSIGNED 1024-byte trailer"),
                 "trailer-koly": (installer.SIZE - 1024, "UNSIGNED 1024-byte trailer"),
                 "gap": (installer.GAP[0], "UNSIGNED 32-byte heap gap")}
        for case, (position, message) in cases.items():
            with self.subTest(case=case):
                target = self.copy(f"flip-{case}.pkg")
                flip(target, position)
                r, report = self.verify(target)
                self.assert_refused(r, report, [message, "chunklist cross-check (not a signature): 1 chunk"])
                # Coverage statement: the signature chain cannot see these bytes.
                self.assertTrue(report["all_signed_entries_verified"])
                self.assertFalse(report["unsigned_regions"]["signed"])
                self.assertEqual(report["chunklist_cross_check"]["mismatched_chunks"], [position // 10485760])
                target.unlink()

    def test_toc_altered_with_consistent_checksum_fails_apple_signature(self):
        target = self.copy("altered-toc.pkg")
        with open(target, "r+b") as handle:
            magic, size, version, compressed, raw_size, algorithm = struct.unpack(">IHHQQI", handle.read(28))
            raw = zlib.decompress(handle.read(compressed))
            old = b"<archived-checksum style=\"sha1\">91b38910f0078bcf64baf876e4f44c8a0c906ebf<"
            self.assertEqual(raw.count(old), 1)
            edited = raw.replace(old, old.replace(b"6ebf<", b"6ebe<"))
            encoded = same_length_toc(edited, compressed)
            handle.seek(28);handle.write(encoded)
            # What a forger would do: make the XAR TOC checksum consistent again.
            handle.write(hashlib.sha1(encoded).digest())
        r, report = self.verify(target)
        self.assertEqual(r.returncode, 2)
        self.assertIsNone(report)
        self.assertIn("OpenSSL verification failed", r.stderr.decode())

    def test_toc_signature_and_header_byte_flips_refused(self):
        cases = {"toc": (100, "incorrect data check"), "signature": (HEAP + 20 + 128, "OpenSSL verification failed"),
                 "toc-checksum": (HEAP + 5, "TOC checksum mismatch"),
                 "header": (27, "unsupported XAR header")}
        for case, (position, message) in cases.items():
            with self.subTest(case=case):
                target = self.copy(f"flip-{case}.pkg")
                flip(target, position)
                r, report = self.verify(target)
                self.assertEqual(r.returncode, 2)
                self.assertIsNone(report)
                self.assertIn(message, r.stderr.decode())
                target.unlink()

    def test_truncated_or_extended_package_refused(self):
        for case, size in {"minus-one": installer.SIZE - 1, "no-trailer": installer.SIZE - 1024,
                           "plus-one": installer.SIZE + 1}.items():
            with self.subTest(case=case):
                target = self.copy(f"size-{case}.pkg")
                os.truncate(target, size)
                r, report = self.verify(target)
                self.assertEqual(r.returncode, 2)
                self.assertIn("installer input size mismatch", r.stderr.decode())
                target.unlink()

    def test_altered_chunklist_refused(self):
        data = bytearray((self.cache / "sonoma.integrity").read_bytes());data[100] ^= 1
        path = self.scratch / "altered.integrity";path.write_bytes(data)
        self.addCleanup(lambda: path.unlink(missing_ok=True))
        r, report = self.verify(self.package, integrity=path)
        self.assertEqual(r.returncode, 2)
        self.assertIn("chunklist differs", r.stderr.decode())


if __name__ == "__main__":
    unittest.main()
