#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Offline renewal with real fixture signatures; never production keys/network."""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("refresh_update", ROOT / "release/refresh-update.py")
refresh = importlib.util.module_from_spec(spec)
spec.loader.exec_module(refresh)
SIGNER = None


class SourceSpans(unittest.TestCase):
    def test_only_unsigned_top_level_time_tokens_change_even_with_escaped_keys_and_unicode(self):
        raw = b' { "notes":"\xc2\xb5 newline\\n released:99 expires:88", "\\u0072eleased" : 123, "platforms":{"x":[{"url":"https://example.invalid/released/123"}]}, "expires":456 }\r\n'
        result = refresh.replace_times(raw, 1000, 2000)
        self.assertEqual(result, raw.replace(b": 123", b": 1000").replace(b":456", b":2000"))
        self.assertEqual(refresh.preserved(result), refresh.preserved(raw))

    def test_duplicate_and_missing_time_fields_or_noninteger_tokens_are_not_rewritten(self):
        for raw in [b'{"released":1,"\\u0072eleased":2,"expires":3}', b'{"released":1}',
                    b'{"released":true,"expires":2}', b'{"released":1.0,"expires":2}',
                    b'{"released":-1,"expires":2}', b'x' * (refresh.LIMIT + 1)]:
            with self.assertRaises((ValueError, json.JSONDecodeError), msg=raw[:100]):
                refresh.replace_times(raw, 3, 4)


class SignedRefresh(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="update-refresh-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.secret, self.public = self.root / "secret.json", self.root / "public.json"
        self.cli("keygen", "--secret-file", self.secret, "--public-file", self.public)
        self.now = int(time.time())
        self.input = self.root / "old.json"
        self.signature = self.root / "old.sig"

    def cli(self, *args, okay=True):
        result = subprocess.run([SIGNER, *map(str, args)], capture_output=True, text=True, timeout=30)
        self.assertEqual(result.returncode == 0, okay, result.stderr)
        return result

    def original(self, released=None, expires=None, mutate=None):
        value = {"format": 2, "algorithm": "ed25519+mldsa44", "version": "0.1.0+unchanged",
                 "released": self.now - 100 if released is None else released,
                 "expires": self.now + 1000 if expires is None else expires,
                 "notes": "original binary: µ\nkeep exact newlines and spacing",
                 "platforms": {"windows-x86_64": [{"path": "nuschl.exe", "size": 11,
                     "url": "https://example.invalid/releases/v0.1.0/nuschl.exe", "sha256": "ab" * 32}]}}
        raw = (json.dumps(value, ensure_ascii=False, indent=2) + "\r\n").encode()
        if mutate:
            raw = mutate(raw)
        self.input.write_bytes(raw)
        self.cli("sign", "--secret-file", self.secret, "--public-file", self.public,
                 "--manifest", self.input, "--signature-file", self.signature)
        return raw

    def renew(self, extra=(), okay=True):
        output = self.root / "refreshed"
        result = subprocess.run([sys.executable, str(ROOT / "release/refresh-update.py"),
            "--manifest", str(self.input), "--signature", str(self.signature),
            "--expected-manifest-sha256", refresh.digest(self.input.read_bytes()),
            "--binary-provenance", "fixture immutable release/source/build record; never fetched",
            "--public-key", str(self.public), "--sign-key", str(self.secret),
            "--signer", SIGNER, "--valid-days", "1", "--output", str(output), *extra],
            capture_output=True, text=True, timeout=90)
        self.assertEqual(result.returncode == 0, okay, result.stderr)
        if not okay:
            self.assertFalse(output.exists(), "a failed renewal exposed publishable partial output")
        return output

    def test_current_source_renews_same_version_and_exact_artifact_bytes(self):
        original = self.original()
        output = self.renew()
        renewed = (output / "update.json").read_bytes()
        self.assertNotEqual(original, renewed)
        self.assertEqual(refresh.preserved(original), refresh.preserved(renewed))
        before, after = json.loads(original), json.loads(renewed)
        for key in ("version", "notes", "platforms"):
            self.assertEqual(before[key], after[key])
        self.assertGreater(after["released"], before["released"])
        self.assertLessEqual(after["expires"] - after["released"], 30 * refresh.DAY)
        self.assertEqual((output / "previous-update.json").read_bytes(), original)
        receipt = json.loads((output / "refresh-receipt.json").read_text())
        self.assertTrue(receipt["current_time_verify_passed"])
        self.assertFalse(receipt["published"])
        self.cli("verify", "--public-file", self.public, "--manifest", output / "update.json",
                 "--signature-file", output / "update.json.sig")
        self.cli("verify", "--public-file", self.public, "--manifest", output / "update.json",
                 "--signature-file", self.signature, okay=False)

    def test_expired_authentication_never_changes_current_time_acceptance_and_needs_explicit_recovery(self):
        self.original(released=self.now - 1000, expires=self.now - 1)
        authenticated = self.cli("authenticate", "--public-file", self.public, "--manifest", self.input,
                                  "--signature-file", self.signature)
        self.assertIn("status=expired current_time_accepted=false", authenticated.stdout)
        self.cli("verify", "--public-file", self.public, "--manifest", self.input,
                 "--signature-file", self.signature, okay=False)
        self.renew(okay=False)
        output = self.renew(("--recover-expired",))
        self.assertTrue(json.loads((output / "refresh-receipt.json").read_text())["expired_source_recovery"])

    def test_future_clock_source_authenticates_only_but_is_not_renewed(self):
        self.original(released=self.now + 3600, expires=self.now + 7200)
        authenticated = self.cli("authenticate", "--public-file", self.public, "--manifest", self.input,
                                  "--signature-file", self.signature)
        self.assertIn("status=not-yet-valid current_time_accepted=false", authenticated.stdout)
        self.cli("verify", "--public-file", self.public, "--manifest", self.input,
                 "--signature-file", self.signature, okay=False)
        self.renew(("--recover-expired",), okay=False)

    def test_either_corrupted_half_is_not_historical_authentication(self):
        self.original()
        original = self.signature.read_bytes()
        for offset in (44, 108):
            changed = bytearray(original)
            changed[offset] ^= 1
            self.signature.write_bytes(changed)
            self.cli("authenticate", "--public-file", self.public, "--manifest", self.input,
                     "--signature-file", self.signature, okay=False)
            self.renew(("--recover-expired",), okay=False)

    def test_signed_duplicates_and_overlong_intervals_fail_structural_authentication(self):
        for index, mutate in enumerate((lambda raw: raw.replace(b'{', b'{"format":2,', 1),
                                        lambda raw: raw.replace(str(self.now + 1000).encode(), str(self.now + 40 * refresh.DAY).encode()))):
            self.signature.unlink(missing_ok=True)
            self.original(mutate=mutate)
            self.cli("authenticate", "--public-file", self.public, "--manifest", self.input,
                     "--signature-file", self.signature, okay=False)

    def test_ledger_hash_validity_limit_and_existing_output_fail_closed(self):
        self.original()
        self.renew(("--expected-manifest-sha256", "00" * 32), okay=False)
        self.renew(("--valid-days", "31"), okay=False)
        output = self.renew()
        before = (output / "update.json").read_bytes()
        result = subprocess.run([sys.executable, str(ROOT / "release/refresh-update.py"),
            "--manifest", str(self.input), "--signature", str(self.signature),
            "--expected-manifest-sha256", refresh.digest(self.input.read_bytes()),
            "--binary-provenance", "fixture", "--public-key", str(self.public), "--sign-key", str(self.secret),
            "--signer", SIGNER, "--output", str(output)], capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(before, (output / "update.json").read_bytes())


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--signer", required=True)
    args, remainder = parser.parse_known_args()
    SIGNER = args.signer
    unittest.main(argv=[sys.argv[0], *remainder])
