#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""GUI harness payload gates and optional actual Windows PID-ownership proof.

--guest runs a synthetic managed stand-in, never the desktop or an updater.
"""
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
import uuid

import windows_update as harness

GUEST = "--guest" in sys.argv
if GUEST:
    sys.argv.remove("--guest")


class Payload(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="windows-update-harness-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.public = self.root / "fixture-public.json"
        self.public.write_text(json.dumps({"format": 2, "algorithm": "ed25519+mldsa44", "keys": [{"fixture": True}]}))
        self.build = self.root / "build"
        (self.build / "bin").mkdir(parents=True)
        self.notices = self.build / "share/licenses/nuschl"
        self.notices.mkdir(parents=True)
        (self.notices / "THIRD-PARTY.json").write_text('{"synthetic": true}')
        for name in harness.REQUIRED_PROGRAMS:
            (self.build / "bin" / name).write_bytes(b"synthetic executable")
        (self.build / "bin/nuschl.exe").write_bytes(b"synthetic desktop trust constant" + self.public.read_bytes())

    def test_complete_helper_payload_and_packaged_notices_are_selected(self):
        programs, notices = harness.payload(self.build, self.public)
        self.assertEqual({p.name for p in programs}, harness.REQUIRED_PROGRAMS)
        self.assertEqual(notices, self.notices)

    def test_every_missing_helper_including_either_verifier_is_rejected(self):
        for name in sorted(harness.REQUIRED_PROGRAMS):
            path = self.build / "bin" / name
            body = path.read_bytes()
            path.unlink()
            with self.assertRaises(ValueError, msg=name):
                harness.payload(self.build, self.public)
            path.write_bytes(body)

    def test_signer_or_fixture_tools_cannot_enter_the_desktop_payload(self):
        (self.build / "bin/nuschl-update-sign.exe").write_bytes(b"must not ship")
        with self.assertRaises(ValueError):
            harness.payload(self.build, self.public)

    def test_unenrolled_or_wrong_public_document_cannot_masquerade_as_fixture(self):
        self.public.write_text('{"format":2,"keys":[]}')
        with self.assertRaises(ValueError):
            harness.payload(self.build, self.public)
        self.public.write_text('{"format":2,"keys":[{"different":true}]}')
        with self.assertRaises(ValueError):
            harness.payload(self.build, self.public)

    def test_missing_or_linked_notices_are_rejected(self):
        path = self.notices / "THIRD-PARTY.json"
        path.unlink()
        with self.assertRaises(ValueError):
            harness.payload(self.build, self.public)
        path.symlink_to(self.public)
        with self.assertRaises(ValueError):
            harness.payload(self.build, self.public)

    def test_read_only_package_notices_remain_intact_and_staging_accepts_its_marker(self):
        self.notices.chmod(0o555)
        destination = self.root / "staged-notices"
        harness.copy_notices(self.notices, destination)
        marker = destination / "UPDATE-TEST"
        marker.write_text("synthetic delivery only\n")
        self.assertTrue(marker.is_file())
        self.assertEqual((destination / "THIRD-PARTY.json").read_bytes(),
                         (self.notices / "THIRD-PARTY.json").read_bytes())
        self.assertEqual(self.notices.stat().st_mode & 0o777, 0o555)


@unittest.skipUnless(GUEST, "use --guest for actual private Windows ownership proof")
class GuestOwnership(unittest.TestCase):
    def test_same_named_unowned_process_is_preserved(self):
        self.assertIn("NUSCHL_WINDOWS_VM_DIR", os.environ)
        run = uuid.uuid4().hex
        # These two managed stand-ins have identical process names but distinct
        # paths. No application build, key, audio, desktop UI or network is used.
        script = r'''
$mine = "$env:LOCALAPPDATA\Programs\NuschlUpdateTest\__RUN__"
$other = "$env:TEMP\nuschl-unowned-sentinel-__RUN__"
New-Item -ItemType Directory -Path $mine -Force | Out-Null
New-Item -ItemType Directory -Path $other | Out-Null
$first = $null; $second = $null
try {
  Add-Type -TypeDefinition 'public class FixtureHold__RUN__ { public static void Main() { System.Threading.Thread.Sleep(60000); } }' -OutputAssembly "$mine\nuschl.exe" -OutputType ConsoleApplication
  Copy-Item -LiteralPath "$mine\nuschl.exe" -Destination "$other\nuschl.exe"
  $first = Start-Process "$mine\nuschl.exe" -WindowStyle Hidden -PassThru
  $second = Start-Process "$other\nuschl.exe" -WindowStyle Hidden -PassThru
  Start-Sleep -Milliseconds 500
  if ($first.HasExited -or $second.HasExited) { throw 'stand-in did not stay running' }
  __STOP_OWNED__
  if (-not $first.WaitForExit(5000)) { throw 'owned process survived cleanup' }
  $second.Refresh()
  if ($second.HasExited) { throw 'unowned same-name process was stopped' }
  'PASS: owned process stopped; same-name foreign-path sentinel preserved'
} finally {
  if ($null -ne $first) { Stop-Process -Id $first.Id -Force -ErrorAction SilentlyContinue }
  if ($null -ne $second) { Stop-Process -Id $second.Id -Force -ErrorAction SilentlyContinue }
  Remove-Item -LiteralPath $mine,$other -Recurse -Force -ErrorAction SilentlyContinue
}
'''.replace("__RUN__", run).replace("__STOP_OWNED__", harness.stop_owned(run))
        output = harness.guest(script, timeout=60)
        self.assertIn("PASS: owned process stopped; same-name foreign-path sentinel preserved", output)


if __name__ == "__main__":
    unittest.main()
