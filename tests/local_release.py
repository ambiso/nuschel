#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Disk-only local publisher boundaries, synthetic products, real TEMP-key signer.
No Nix compilation, platform execution, production keys or network calls.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location('local_release', ROOT / 'release/local-release.py')
local = importlib.util.module_from_spec(spec)
spec.loader.exec_module(local)
SIGNER = None


class LocalRelease(unittest.TestCase):
    def setUp(self):
        self.addCleanup(patch.stopall)
        patch.dict(os.environ, {'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_SYSTEM': '/dev/null'}).start()
        self.tmp = tempfile.TemporaryDirectory(prefix='local-release-test-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.repo = self.root / 'repo'
        self.repo.mkdir()
        self.secret, self.public = self.root / 'fixture.secret.json', self.root / 'public.json'
        subprocess.run([SIGNER, 'keygen', '--secret-file', self.secret, '--public-file', self.public],
                       check=True, capture_output=True)
        for name in ('release/make-update.py', 'release/prepare-artifacts.py',
                     'release/notice-roots.json', 'release/notice-scopes.json'):
            target = self.repo / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, target)
        (self.repo / 'Cargo.toml').write_text('[workspace.package]\nversion="0.1.0"\n')
        (self.repo / 'flake.lock').write_text('{}\n')
        (self.repo / 'release/notes-0.1.0.md').write_text('Synthetic temporary-key fixture.\n')
        (self.repo / 'crates/update').mkdir(parents=True)
        shutil.copyfile(self.public, self.repo / 'crates/update/update-trust.json')
        for args in (['init', '-q'], ['add', '.'], ['-c', 'user.name=fixture', '-c',
                     'user.email=fixture@example.invalid', 'commit', '-qm', 'fixture'], ['tag', 'v0.1.0']):
            subprocess.run(['git', '-C', self.repo, *args], check=True, capture_output=True)
        self.products = {}
        for name in local.PRODUCTS:
            out = self.root / ('store-' + name)
            out.mkdir()
            self.products[name] = {'output': str(out), 'drv': '/nix/store/' + 'a'*32 + '-' + name + '.drv',
                                   'narHash': 'sha256-' + 'A'*43 + '=', 'source_kind': 'local-nix'}
        win = Path(self.products['windows']['output'])
        (win / 'bin').mkdir()
        for name in local.PROGRAMS:
            (win / 'bin' / (name + '.exe')).write_bytes(b'MZ synthetic never executed ' + name.encode())
        (win / 'share/licenses/nuschl').mkdir(parents=True)
        (win / 'share/licenses/nuschl/NOTICE').write_text('Synthetic gate fixture only.\n')
        mac = Path(self.products['macos-app']['output']) / 'Nuschl.app/Contents'
        (mac / 'MacOS').mkdir(parents=True)
        for name in local.PROGRAMS:
            p = mac / 'MacOS' / name
            p.write_bytes(b'synthetic never executed ' + name.encode())
            p.chmod(0o755)
        (mac / 'Info.plist').write_bytes(plistlib.dumps({'fixture': True}))
        web = Path(self.products['web-site']['output'])
        (web / 'app').mkdir()
        (web / 'index.html').write_text('synthetic')
        (web / 'app/index.html').write_text('synthetic')
        self.stage = self.root / 'stage'
        # Only notice/platform-header validation and Nix selection are substitutes.
        # All source/tag, product/tree/archive, signer and upload-boundary code is real.
        self.addCleanup(patch.stopall)
        patch.object(local, 'select_product', side_effect=lambda flake, name: self.products[name]).start()
        patch.object(local, 'check_installs').start()
        auth = local.module(self.repo / 'release/prepare-artifacts.py')
        auth.verify_notices = lambda *a, **kw: None
        patch.object(local, 'authority', return_value=auth).start()
        local.prepare(argparse.Namespace(source_repo=self.repo, tag='v0.1.0', repository='fixture/project',
                                         output=self.stage, build=False))

    def mac_fixture(self):
        # Injected synthetic gate metadata tests post-gate orchestration only;
        # this is deliberately NOT a native Mac/tool PASS.
        d = self.stage / 'macos-native'
        d.mkdir()
        ledger = local.load(self.stage / 'ledger.json')
        local.write(d / 'report.json', {'status': 'native_apple_static_verified',
            'artifact_binaries_executed': False, 'source': 'local-native-operator-controlled',
            'commit': ledger['commit'], 'ledger_sha256': local.sha(self.stage / 'ledger.json'),
            'archive_sha256': ledger['macos_archive_sha256'],
            'host': {'platform': 'darwin'}, 'commands': [{'fixture': 'not actual Apple proof'}]})

    def sign(self):
        self.mac_fixture()
        local.sign(argparse.Namespace(stage=self.stage, signer=SIGNER, sign_key=self.secret))

    def test_real_fixture_hybrid_signatures_bind_ledger_and_exact_assets(self):
        self.sign()
        local.validate(self.stage)
        local.verify_signature(self.stage, SIGNER)
        manifest = self.stage / 'dist/update.json'
        manifest.write_bytes(manifest.read_bytes() + b' ')
        with self.assertRaises(subprocess.CalledProcessError):
            local.verify_signature(self.stage, SIGNER)

    def test_missing_native_proof_blocks_before_secret_command(self):
        with patch.object(local, 'run', side_effect=AssertionError('no signer allowed')):
            # Native proof check itself is strict; no injected native success.
            with self.assertRaises(ValueError):
                local.mac_gate(self.stage, local.load(self.stage / 'ledger.json'))

    @unittest.skipUnless(os.uname().sysname == 'Linux', 'Linux refusal control')
    def test_linux_cannot_mint_native_apple_pass(self):
        with self.assertRaises(ValueError):
            local.verify_macos(argparse.Namespace(stage=self.stage))
        self.assertNotEqual(local.load(self.stage / 'macos-native/report.json')['status'],
                            'native_apple_static_verified')

    def test_rehashed_install_tampering_cannot_substitute_for_nix_output(self):
        path = self.stage / 'install/windows/nuschl.exe'
        path.write_bytes(b'changed but ledger rehashed')
        ledger = local.load(self.stage / 'ledger.json')
        ledger['installations'] = local.tree(self.stage / 'install')
        with self.assertRaisesRegex(ValueError, 'selected Nix product'):
            local.check_products(self.stage, ledger, self.repo)

    def test_rehashed_derivation_or_nar_receipt_is_not_authority(self):
        ledger = local.load(self.stage / 'ledger.json')
        ledger['products']['windows']['narHash'] = 'sha256-' + 'B'*43 + '='
        with self.assertRaisesRegex(ValueError, 'derivation/output/NAR'):
            local.check_products(self.stage, ledger, self.repo)

    def test_source_export_and_tag_movement_are_refused(self):
        (self.stage / 'source/flake.lock').write_text('changed')
        with self.assertRaisesRegex(ValueError, 'source bytes'):
            local.validate(self.stage)
        shutil.copyfile(self.repo / 'flake.lock', self.stage / 'source/flake.lock')
        (self.repo / 'new').write_text('new')
        subprocess.run(['git', '-C', self.repo, 'add', '.'], check=True)
        subprocess.run(['git', '-C', self.repo, '-c', 'user.name=f', '-c', 'user.email=f@x',
                        'commit', '-qm', 'new'], check=True)
        subprocess.run(['git', '-C', self.repo, 'tag', '-f', 'v0.1.0'], check=True, capture_output=True)
        with self.assertRaisesRegex(ValueError, 'tag moved'):
            local.validate(self.stage)

    def test_zip_and_web_archive_bytes_are_checked_against_product(self):
        ledger = local.load(self.stage / 'ledger.json')
        path = self.stage / 'dist/nuschl-0.1.0-web-site.tar'
        path.unlink()
        bad = self.root / 'bad-web'
        shutil.copytree(Path(self.products['web-site']['output']), bad)
        (bad / 'app/index.html').write_text('different')
        local.canonical_tar(bad, path)
        with self.assertRaisesRegex(ValueError, 'archive differs'):
            local.check_products(self.stage, ledger, self.repo)

    def test_ci_foreign_commit_and_failed_native_job_cannot_supply_proof(self):
        ledger = local.load(self.stage / 'ledger.json')
        good = {'head_sha': ledger['commit'], 'status': 'completed', 'conclusion': 'success',
                'event': 'workflow_dispatch', 'head_repository': {'full_name': 'fixture/project'},
                'path': '.github/workflows/build.yml', 'run_attempt': 1}
        for wrong_source in (True, False):
            record = dict(good)
            if wrong_source:
                record['head_sha'] = 'b' * 40
            calls = []
            def command(arguments, **kwargs):
                calls.append(arguments)
                self.assertEqual(arguments[1], 'api', 'must refuse before downloading any artifact')
                if '/jobs?' in arguments[2]:
                    return json.dumps({'jobs': [{'name': 'macos-native-verify',
                        'head_sha': ledger['commit'], 'conclusion': 'failure'}]})
                return json.dumps(record)
            with patch.object(local, 'run', side_effect=command):
                with self.assertRaises(ValueError):
                    local.ci_evidence(self.stage, ledger, 'fixture-gh', '123', self.root / 'unused')
            self.assertEqual(len(calls), 1 if wrong_source else 2)

    def test_links_and_directory_only_fanout_are_bounded(self):
        root = self.root / 'bounded'
        root.mkdir()
        for n in range(5):
            (root / str(n)).mkdir()
        with self.assertRaisesRegex(ValueError, 'entry limit'):
            local.tree(root, files=1)
        shutil.rmtree(root)
        root.mkdir()
        (root / 'link').symlink_to(self.public)
        with self.assertRaisesRegex(ValueError, 'links'):
            local.tree(root)

    def publish(self, bad_remote=False):
        calls, real, created = [], local.run, False
        ledger = local.load(self.stage / 'ledger.json')
        def command(args, **kwargs):
            nonlocal created
            if str(args[0]) != 'fixture-gh':
                return real(args, **kwargs)
            args = list(map(str, args)); calls.append(args)
            if args[1] == 'api':
                endpoint = next(x for x in args if x.startswith('repos/'))
                if '/git/ref/tags/' in endpoint:
                    return json.dumps({'object': {'type': 'commit', 'sha': ledger['commit']}})
                if endpoint.endswith('/releases?per_page=100'):
                    return json.dumps([[{'tag_name': 'v0.1.0', 'draft': True, 'id': 42}]] if created else [[]])
                if endpoint.endswith('/releases/42'):
                    assets = [{'name': p.name, 'size': p.stat().st_size, 'digest': 'sha256:' + local.sha(p), 'state': 'uploaded'}
                              for p in (self.stage / 'dist').iterdir()]
                    return json.dumps({'id': 42, 'draft': True, 'assets': assets[:-1] if bad_remote else assets})
            if args[1:3] == ['release', 'create']:
                created = True
            return ''
        with patch.object(local, 'run', side_effect=command):
            try:
                local.publish(argparse.Namespace(stage=self.stage, signer=SIGNER, gh='fixture-gh'))
            except ValueError as error:
                print('Expected-or-diagnostic publication refusal:', error)
                return calls, False
        return calls, True

    def test_draft_then_verify_all_digests_before_finalize(self):
        self.sign()
        calls, ok = self.publish()
        self.assertTrue(ok)
        self.assertIn('--verify-tag', next(c for c in calls if c[1:3] == ['release', 'create']))
        self.assertEqual(calls[-1][1:3], ['release', 'edit'])
        self.assertTrue(all('--clobber' not in c for c in calls))

    def test_missing_uploaded_asset_keeps_release_unpublished(self):
        self.sign()
        calls, ok = self.publish(bad_remote=True)
        self.assertFalse(ok)
        self.assertFalse(any(c[1:3] == ['release', 'edit'] for c in calls))

    def test_bad_checksum_file_stops_before_any_remote_call(self):
        self.sign()
        (self.stage / 'dist/SHA256SUMS').write_text('wrong\n')
        calls, ok = self.publish()
        self.assertFalse(ok)
        self.assertEqual(calls, [])


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--signer', required=True)
    args, remaining = parser.parse_known_args()
    SIGNER = args.signer
    unittest.main(argv=[__file__, *remaining])
