#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Lossless compact notice packaging and real publication-gate regressions.
All package inventories here are synthetic; tests grant no release/legal approval.
"""
import importlib.util
import copy
import hashlib
import io
import tarfile
import json
import os
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


class DeclaredLicensePolicy(unittest.TestCase):
    def setUp(self):
        self.module = load("license_evidence", ROOT / "release/license-evidence.py")
        self.family = ROOT / "release/licenses/crypto-upstreams"
        data = json.loads((self.family / "provenance.json").read_bytes())
        self.entries = [row for row in data["packages"] if row.get("evidence_kind") == "declared-license-supplement"]
        self.assertEqual(len(self.entries), 6)

    def source(self, row):
        if (row["name"], row["version"]) == ("hpke-rs-libcrux", "0.7.0"):
            return ROOT / "vendor/hpke-rs-libcrux"
        cache = Path(os.environ.get('NUSCHL_LICENSE_CRATE_SOURCES',
                                    str(Path(os.environ.get('CARGO_HOME', Path.home()/'.cargo'))/'registry/src')))
        name = row['name']+'-'+row['version']
        candidates = [cache/name, *cache.glob('*/'+name)]
        found = next((path for path in candidates if path.is_dir()), None)
        if found is None:
            raise RuntimeError('Curated tests require the cached exact published crates; set NUSCHL_LICENSE_CRATE_SOURCES')
        return found

    def test_all_six_real_pins_keep_exact_declarations_headers_and_source_access(self):
        for row in self.entries:
            decision = self.module.validate(row, self.family, self.source(row), ROOT)
            self.assertEqual(decision["declared_license"], "MPL-2.0")

    def test_other_versions_names_revisions_and_missing_links_are_rejected(self):
        original = self.entries[0]
        for mode in ('revision', 'version', 'name', 'missing-source', 'foreign-source', 'steward', 'missing-header'):
            row = copy.deepcopy(original)
            if mode == 'revision': row['revision'] = '0'*40
            if mode == 'version': row['version'] = '0.9.0'
            if mode == 'name': row['name'] = 'unreviewed-mpl-package'
            if mode == 'missing-source': row['source_access'].pop('published_crate_url')
            if mode == 'foreign-source': row['source_access']['repository_tree_url'] = 'https://example.invalid/'
            if mode == 'steward': next(x for x in row['files'] if x['role']=='license-steward')['sha256'] = '0'*64
            if mode == 'missing-header': row['files'] = [x for x in row['files'] if x['role']!='source-header-index']
            with self.assertRaises(ValueError, msg=mode): self.module.validate(row, self.family, self.source(original), ROOT)

    def test_patched_source_is_complete_and_verified_beyond_the_outer_archive_hash(self):
        import tempfile
        row = next(x for x in self.entries if (x['name'],x['version'])==('hpke-rs-libcrux','0.7.0'))
        expected = self.module.validate(row,self.family,self.source(row),ROOT)['source_access']['modified_source']
        with tempfile.TemporaryDirectory(prefix='curated-source-') as temporary:
            root = Path(temporary)
            policy = self.family/'declared-license-policy.json'
            shutil.copyfile(policy,root/'EVIDENCE-POLICY-hpke-mpl2.json')
            archive = root/expected['bundled_archive']
            def pack(tamper=False):
                with tarfile.open(archive,'w') as stream:
                    for index,(name,digest) in enumerate(sorted(expected['files'].items())):
                        raw = (ROOT/expected['project_path']/name).read_bytes()
                        if tamper and index == 0: raw += b'changed source bytes'
                        member = tarfile.TarInfo('hpke-rs-libcrux-0.7.0/'+name)
                        member.size = len(raw)
                        stream.addfile(member,io.BytesIO(raw))
                return {'package':'hpke-rs-libcrux','version':'0.7.0','path':archive.name,'size':archive.stat().st_size,
                        'sha256':self.module.sha(archive),'files':expected['files'],'project_path':expected['project_path']}
            report = {'packages':[{'name':row['name'],'version':row['version'],'corresponding_source':row['source_access']}],
                      'curated_evidence_policies':[{'id':self.module.POLICY_ID,'path':'EVIDENCE-POLICY-hpke-mpl2.json','sha256':self.module.sha(policy)}],
                      'source_archives':[pack()]}
            self.module.verify_bundled(report,root,self.module.sha,ROOT)
            report['source_archives'] = [pack(tamper=True)]
            with self.assertRaisesRegex(ValueError,'source file bytes differ'):
                self.module.verify_bundled(report,root,self.module.sha,ROOT)
            report['source_archives'] = []
            with self.assertRaisesRegex(ValueError,'missing/extra'):
                self.module.verify_bundled(report,root,self.module.sha,ROOT)


if __name__ == "__main__":
    unittest.main()
