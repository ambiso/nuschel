#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Local new-manifest regression and independent Nix aggregation comparison."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("crate2nix_inputs", ROOT / "nix/crate2nix-inputs.py")
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)


class ManifestFingerprint(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="crate2nix-inputs-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        for group in ("apps", "crates", "vendor"):
            (self.root / group).mkdir()
        (self.root / "Cargo.lock").write_bytes(b"lock\n")
        (self.root / "Cargo.toml").write_bytes(b"root\n")

    def manifest(self, relative, content):
        path = self.root / relative / "Cargo.toml"
        path.parent.mkdir(parents=True)
        path.write_bytes(content)
        return path

    def nix(self, root):
        # This is the independent production expression, not a Python-derived
        # input list. Supplying an absolute directory does not copy a flake.
        expression = """
        let
          root = builtins.toPath %s;
          manifests = dir: map (name: root + "/${dir}/${name}/Cargo.toml")
            (builtins.attrNames (builtins.removeAttrs
              (builtins.readDir (root + "/${dir}"))
              (builtins.filter (name:
                (builtins.readDir (root + "/${dir}")).${name} != "directory" ||
                !(builtins.pathExists (root + "/${dir}/${name}/Cargo.toml")))
                (builtins.attrNames (builtins.readDir (root + "/${dir}"))))));
        in builtins.hashString "sha256" (builtins.concatStringsSep ""
          (map builtins.readFile ([ (root + "/Cargo.lock") (root + "/Cargo.toml") ]
            ++ manifests "apps" ++ manifests "crates" ++ manifests "vendor")))
        """ % json.dumps(str(root))
        return subprocess.run(["nix", "eval", "--impure", "--raw", "--expr", expression],
                              check=True, capture_output=True, text=True).stdout

    def test_new_untracked_vendor_manifest_changes_the_recorded_fingerprint(self):
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        subprocess.run(["git", "-C", str(self.root), "add", "Cargo.lock", "Cargo.toml"], check=True)
        before = helper.fingerprint(self.root)
        self.manifest("vendor/new-codec", b"new untracked codec\n")
        tracked = subprocess.check_output(["git", "-C", str(self.root), "ls-files"], text=True)
        self.assertNotIn("vendor/new-codec/Cargo.toml", tracked)
        after = helper.fingerprint(self.root)
        self.assertNotEqual(before, after)
        self.assertEqual(after, hashlib.sha256(b"lock\nroot\nnew untracked codec\n").hexdigest())
        self.assertEqual(after, self.nix(self.root))
        (self.root / ".gitignore").write_text("ignored/\n")
        (self.root / "ignored").mkdir()
        (self.root / "ignored" / "archive.tar").write_bytes(b"unrelated archive")
        self.assertEqual(after, helper.fingerprint(self.root))

    def test_nix_order_directories_and_symlink_treatment_match(self):
        self.manifest("apps/z-last", b"app-z\r\n")
        self.manifest("apps/a-first", b"app-a\n")
        self.manifest("crates/core", b"crate\n")
        target = self.manifest("vendor/actual", b"vendor\n")
        (self.root / "vendor" / "linked-directory").symlink_to(target.parent, target_is_directory=True)
        (self.root / "vendor" / "not-a-directory").write_bytes(b"ignored")
        nested = self.manifest("vendor/actual/nested", b"not a direct manifest\n")
        empty = self.root / "vendor" / "empty"
        empty.mkdir()
        # A symlink file inside an actual directory is followed by Nix readFile.
        (empty / "Cargo.toml").symlink_to(target)
        self.assertEqual(helper.fingerprint(self.root), self.nix(self.root))
        before = helper.fingerprint(self.root)
        nested.write_bytes(b"changed nested manifest\n")
        self.assertEqual(before, helper.fingerprint(self.root))

    def test_missing_required_input_fails_instead_of_recording_a_partial_hash(self):
        (self.root / "Cargo.lock").unlink()
        with self.assertRaises(FileNotFoundError):
            helper.fingerprint(self.root)

    def test_nul_input_is_rejected_like_nix_read_file(self):
        self.manifest("crates/invalid", b"invalid\0manifest")
        with self.assertRaisesRegex(ValueError, "NUL"):
            helper.fingerprint(self.root)
        with self.assertRaises(subprocess.CalledProcessError):
            self.nix(self.root)

    def test_actual_tracked_source_matches_nix_and_recorded_fingerprint(self):
        actual = helper.fingerprint(ROOT)
        self.assertEqual(actual, self.nix(ROOT))
        self.assertEqual(actual, (ROOT / "nix/crate2nix/inputs.sha256").read_text().strip())


if __name__ == "__main__":
    unittest.main()
