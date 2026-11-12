#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Lossless compact notice packaging and real publication-gate regressions.
All package inventories here are synthetic; tests grant no release/legal approval.
"""
import importlib.util
import json
from pathlib import Path
import shutil
import unittest

ROOT = Path(__file__).resolve().parents[1]


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


compact = load("compact_notices", ROOT / "release/compact-notices.py")
fixture = load("update_release_fixture", ROOT / "tests/update_release.py")


class CompactNotices(unittest.TestCase):
    def setUp(self):
        self.fixture = fixture.ReleaseGates()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.directory, self.install, self.notices, self.repack = self.fixture.package()
        self.root = self.fixture.root

    def output(self):
        output = self.root / "compact"
        report = compact.compact(self.notices, output)
        return output, report

    def test_exact_unicode_license_bytes_and_original_paths_survive_compaction(self):
        extra = self.notices / "crates/example-1.0.0/NOTICE"
        extra.parent.mkdir()
        original = "Actual synthetic fixture bytes: café\n  preserve spaces\r\n".encode()
        extra.write_bytes(original)
        output, report = self.output()
        index = report["notice_files"]["crates/example-1.0.0/NOTICE"]
        with (output/"THIRD-PARTY.txt").open("rb") as stream:
            stream.seek(index["offset"])
            self.assertEqual(stream.read(index["size"]), original)
        self.assertEqual(report["unresolved"], [])
        fixture.prepare.verify_notices(output, self.fixture.policy)
        self.assertEqual(len(list(output.iterdir())), 2)

    def test_hundreds_of_notices_fit_the_existing_installation_budget(self):
        for index in range(600):
            (self.notices/f"NOTICE-{index}.txt").write_text(f"Synthetic unique notice {index}\n")
        output, report = self.output()
        self.assertGreater(len(report["notice_files"]), 600)
        shutil.rmtree(self.notices)
        shutil.copytree(output, self.notices)
        self.repack()
        self.fixture.accept(self.directory)
        self.assertLess(len(list(self.install.rglob("*"))), 256)

    def test_compaction_never_erases_unresolved_coverage(self):
        path = self.notices / "THIRD-PARTY.json"
        source = json.loads(path.read_bytes())
        source["unresolved"] = [{"name":"missing-upstream", "reason":"fixture"}]
        path.write_text(json.dumps(source))
        output, report = self.output()
        self.assertEqual(report["unresolved"], source["unresolved"])
        with self.assertRaises(ValueError): fixture.prepare.verify_notices(output, self.fixture.policy)

    def test_payload_tamper_cannot_be_hidden_by_only_updating_the_outer_hash(self):
        output, report = self.output()
        descriptor = next(iter(report["notice_files"].values()))
        path = output / "THIRD-PARTY.txt"
        raw = bytearray(path.read_bytes())
        raw[descriptor["offset"]] ^= 1
        path.write_bytes(raw)
        report["notice_payload"]["sha256"] = fixture.prepare.digest(path)
        (output/"THIRD-PARTY.json").write_text(json.dumps(report))
        with self.assertRaisesRegex(ValueError, "byte-range hash"):
            fixture.prepare.verify_notices(output, self.fixture.policy)

    def test_missing_overlapping_and_foreign_notice_references_fail_closed(self):
        output, report = self.output()
        original = json.loads(json.dumps(report))
        key = "crates/ml-dsa-0.1.0/LICENSE"
        for mode in ("missing", "overlap", "oversized", "traversal"):
            report = json.loads(json.dumps(original))
            if mode == "missing": report["notice_files"].pop(key)
            if mode == "overlap": report["notice_files"][key]["offset"] = 0
            if mode == "oversized": report["notice_files"][key]["size"] = 128 * 1024**2
            if mode == "traversal": report["notice_files"]["../escape"] = report["notice_files"].pop(key)
            (output/"THIRD-PARTY.json").write_text(json.dumps(report))
            with self.assertRaises(ValueError, msg=mode): fixture.prepare.verify_notices(output, self.fixture.policy)

    def test_inspected_file_hash_mismatch_and_symlink_are_not_packaged(self):
        (self.notices/"crates/ml-dsa-0.1.0/LICENSE").write_text("changed after inventory")
        with self.assertRaises(ValueError): self.output()
        shutil.rmtree(self.root/"compact")
        (self.notices/"unexpected-link").symlink_to(self.root)
        with self.assertRaises(ValueError): self.output()


if __name__ == "__main__":
    unittest.main()
