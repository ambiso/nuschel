#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Real Git-export tampering regressions for the cross inspector's authority."""
import importlib.util
import io
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("cross_distributions", Path(__file__).with_name("cross_distributions.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class SourceBinding(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="cross-source-")
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.repository = root / "repository"
        self.repository.mkdir()
        for name, text in (("Cargo.toml", '[workspace]\nmembers=[]\n'),
                           ("release/notice-roots.json", '[]\n'),
                           ("vendor/example/LICENSE", 'actual attribution\n'),
                           ("tool.sh", '#!/bin/sh\nexit 0\n')):
            path = self.repository / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        (self.repository / "tool.sh").chmod(0o755)
        self.git("init", "-q")
        self.git("add", "--force", ".")
        self.git("-c", "user.name=Fixture", "-c", "user.email=fixture@invalid", "-c",
                 "commit.gpgsign=false", "commit", "-qm", "private fixture")
        self.revision = self.git("rev-parse", "HEAD").decode().strip()
        self.flake = "git+file://" + str(self.repository) + "?rev=" + self.revision
        self.source = root / "source"
        self.source.mkdir()
        with tarfile.open(fileobj=io.BytesIO(self.git("archive", "--format=tar", self.revision))) as archive:
            archive.extractall(self.source, filter="data")

    def git(self, *args):
        return subprocess.check_output(["git", "-C", str(self.repository), *args])

    def test_matching_export_is_bound_to_full_revision(self):
        result = module.verify_snapshot(self.flake, self.source)
        self.assertEqual(result["revision"], self.revision)
        self.assertEqual(result["exported_files"], 4)

    def test_policy_cargo_and_notice_bytes_cannot_be_substituted(self):
        for name in ("release/notice-roots.json", "Cargo.toml", "vendor/example/LICENSE"):
            with self.subTest(name=name):
                path = self.source / name
                original = path.read_bytes()
                path.write_bytes(original + b"changed\n")
                with self.assertRaises(AssertionError):
                    module.verify_snapshot(self.flake, self.source)
                path.write_bytes(original)

    def test_untracked_file_cannot_enter_source_authority(self):
        (self.source / "extra.json").write_text("{}")
        with self.assertRaises(AssertionError):
            module.verify_snapshot(self.flake, self.source)

    def test_missing_policy_is_rejected(self):
        (self.source / "release/notice-roots.json").unlink()
        with self.assertRaises(AssertionError):
            module.verify_snapshot(self.flake, self.source)

    def test_symlink_cannot_alias_a_regular_policy_file(self):
        policy = self.source / "release/notice-roots.json"
        policy.unlink()
        policy.symlink_to(self.repository / "release/notice-roots.json")
        with self.assertRaises(AssertionError):
            module.verify_snapshot(self.flake, self.source)

    def test_executable_bit_is_part_of_the_export(self):
        (self.source / "tool.sh").chmod(0o644)
        with self.assertRaises(AssertionError):
            module.verify_snapshot(self.flake, self.source)

    def test_branch_and_ambiguous_revision_are_rejected(self):
        for suffix in ("?rev=HEAD", "?rev=" + self.revision + "&rev=" + self.revision):
            with self.subTest(suffix=suffix), self.assertRaises(ValueError):
                module.verify_snapshot("git+file://" + str(self.repository) + suffix, self.source)


if __name__ == "__main__":
    unittest.main()
