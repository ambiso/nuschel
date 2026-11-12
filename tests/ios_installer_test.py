#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
import copy
import json
import os
from pathlib import Path
import subprocess
import sys
import unittest

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


@unittest.skipUnless(os.environ.get("NUSCHL_INSTALLER_CACHE"), "explicit real public installer fixture cache required")
class RealSignedPrefix(unittest.TestCase):
    def invoke(self, path):
        c = Path(os.environ["NUSCHL_INSTALLER_CACHE"])
        return subprocess.run([sys.executable, str(Path(installer.__file__)), "prefix", str(path),
            "--apple-root", str(c / "public-signature/apple-root.der"),
            "--root-crl", str(c / "public-signature/root-http.crl"),
            "--software-crl", str(c / "public-signature/softwareupdate-http.crl")],
            capture_output=True, timeout=20)

    def test_actual_production_signer_prefix_does_not_claim_payload(self):
        c = Path(os.environ["NUSCHL_INSTALLER_CACHE"])
        r = self.invoke(c / "sonoma-prefix.bin")
        self.assertEqual(r.returncode, 0, r.stderr.decode())
        proof = json.loads(r.stdout)
        self.assertTrue(proof["authentication"]["software_update_policy"])
        self.assertTrue(proof["authentication"]["revocation_checked"])
        self.assertEqual(set(proof["entries"]), installer.EXPECTED)
        self.assertFalse(proof["whole_payload_verified"])
        self.assertFalse(proof["outside_signed_entries"]["whole_catalog_hash_verified"])
        self.assertFalse(proof["package_extracted_or_executed"])

    def test_actual_signature_corruption_rejected_before_download(self):
        import struct
        c = Path(os.environ["NUSCHL_INSTALLER_CACHE"])
        data = bytearray((c / "sonoma-prefix.bin").read_bytes())
        header = struct.unpack(">IHHQQI", data[:28]);data[28 + header[3] + 20] ^= 1
        path = c / "corrupt-installer-prefix.bin";path.write_bytes(data)
        try:
            r = self.invoke(path)
            self.assertNotEqual(r.returncode, 0)
            self.assertIn("OpenSSL verification failed", r.stderr.decode())
        finally:
            path.unlink()


if __name__ == "__main__":
    unittest.main()
