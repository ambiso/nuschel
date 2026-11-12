#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Release frontend: real temporary-key signing, synthetic products, no builds/network.

Reuse the audited backend's fixture substitutions for Nix and platform headers.
No production keys, host profiles, audio or remote repositories are accessed.
"""
import argparse
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import pty
import select
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


front = module('release_command', ROOT / 'release/release.py')
fixtures = module('local_release_fixture', ROOT / 'tests/local_release.py')
SIGNER = None


class ReleaseCommand(unittest.TestCase):
    def setUp(self):
        self.fixture = fixtures.LocalRelease()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        self.repo, self.area = self.fixture.repo, self.fixture.root
        self.local = fixtures.local
        self.home, self.cache = self.area / 'home', self.area / 'cache'
        self.home.mkdir()
        self.cache.mkdir()
        self.key = self.home / '.local/share/nuschl-release-keys/update-v2.secret.json'
        self.key.parent.mkdir(parents=True)
        shutil.copyfile(self.fixture.secret, self.key)
        self.key.chmod(0o600)
        self.patch(patch.dict(os.environ, {'HOME': str(self.home), 'XDG_CACHE_HOME': str(self.cache)}))
        self.patch(patch.object(front, 'ROOT', self.repo))
        self.patch(patch.object(front, 'backend', self.local))
        self.git('remote', 'add', 'origin', 'git@github.com:fixture/project.git')
        self.signer_product = self.area / 'store-signer'
        (self.signer_product / 'bin').mkdir(parents=True)
        (self.signer_product / 'bin/nuschl-update-sign').symlink_to(SIGNER)
        self.product = {'output': str(self.signer_product),
                        'drv': '/nix/store/' + 'a' * 32 + '-signer.drv',
                        'narHash': 'sha256-' + 'A' * 43 + '=', 'source_kind': 'local-nix'}
        self.patch(patch.object(self.local, 'select_product', side_effect=lambda flake, name:
                               self.product if name == 'update-signer' else self.fixture.products[name]))
        self.builds = []
        self.patch(patch.object(self.local, 'guarded_build', side_effect=self.fake_build))
        self.remote_calls, self.created = [], False
        real = self.local.run

        def command(args, **kwargs):
            args = list(map(str, args))
            if args[0] == 'git' and 'push' in args:
                self.remote_calls.append(args)
                return ''
            if args[0] != 'gh':
                return real(args, **kwargs)
            self.remote_calls.append(args)
            stage = Path(self.pointer()['stage'])
            ledger = self.local.load(stage / 'ledger.json')
            if args[1] == 'api':
                endpoint = next(x for x in args if x.startswith('repos/'))
                if '/git/ref/tags/' in endpoint:
                    return json.dumps({'object': {'type': 'commit', 'sha': ledger['commit']}})
                if endpoint.endswith('/releases?per_page=100'):
                    return json.dumps([[{'tag_name': 'v0.1.0', 'draft': True, 'id': 42}]] if self.created else [[]])
                if endpoint.endswith('/releases/42'):
                    assets = [{'name': p.name, 'size': p.stat().st_size,
                               'digest': 'sha256:' + self.local.sha(p), 'state': 'uploaded'}
                              for p in (stage / 'dist').iterdir()]
                    return json.dumps({'id': 42, 'draft': True, 'assets': assets})
            if args[1:3] == ['release', 'create']:
                self.created = True
            return ''
        self.patch(patch.object(self.local, 'run', side_effect=command))

    def patch(self, patcher):
        result = patcher.start()
        self.addCleanup(patcher.stop)
        return result

    def git(self, *args):
        return subprocess.run(['git', '-C', str(self.repo), *args], check=True,
                              capture_output=True, text=True).stdout.strip()

    def fake_build(self, args, log):
        self.builds.append(list(map(str, args)))
        Path(log).write_text('Synthetic build substitution; no Nix executed.\n')

    def pointer_path(self):
        return self.cache / 'nuschl-release/last-signed.json'

    def pointer(self):
        return json.loads(self.pointer_path().read_bytes())

    def prepare(self):
        return front.execute(confirm=lambda details: True)

    def test_inference_uses_origin_and_immutable_tag_not_newer_tool_head(self):
        old = self.git('rev-parse', 'v0.1.0')
        (self.repo / 'newer-tool.txt').write_text('synthetic newer publisher tooling\n')
        self.git('add', '.')
        self.git('-c', 'user.name=fixture', '-c', 'user.email=fixture@example.invalid',
                 'commit', '-qm', 'newer tooling')
        self.git('remote', 'add', 'andir', 'https://github.com/another/fork.git')
        selected = front.infer(self.repo)
        self.assertEqual(selected['repository'], 'fixture/project')
        self.assertEqual(selected['tag'], 'v0.1.0')
        self.assertEqual(selected['commit'], old)
        self.assertNotEqual(selected['head'], old)
        self.assertFalse(selected['create_tag'])
        self.assertEqual(selected['secret'], str(self.key))
        self.assertEqual(selected['key_id'], json.loads(self.fixture.public.read_bytes())['keys'][0]['key_id'])

    def test_ambiguous_origin_dirty_checkout_and_mismatched_push_origin_refuse(self):
        self.git('config', '--add', 'remote.origin.url', 'https://github.com/other/repo')
        with self.assertRaisesRegex(ValueError, 'exactly one origin'):
            front.infer(self.repo)
        self.git('config', '--unset-all', 'remote.origin.url')
        self.git('config', 'remote.origin.url', 'https://github.com/fixture/project.git')
        self.git('config', 'remote.origin.pushurl', 'https://github.com/other/repo.git')
        with self.assertRaisesRegex(ValueError, 'repositories differ'):
            front.infer(self.repo)
        self.git('config', '--unset', 'remote.origin.pushurl')
        (self.repo / 'uncommitted').write_text('not a release input')
        with self.assertRaisesRegex(ValueError, 'uncommitted changes'):
            front.infer(self.repo)

    def test_cancel_does_not_build_sign_tag_or_replace_last_success(self):
        with self.assertRaisesRegex(ValueError, 'Cancelled'):
            front.execute(confirm=lambda details: False)
        self.assertFalse(self.builds)
        self.assertFalse(self.pointer_path().exists())
        self.assertFalse(self.remote_calls)
        stage = self.prepare()
        previous, count = self.pointer_path().read_bytes(), len(self.builds)
        with self.assertRaisesRegex(ValueError, 'Cancelled'):
            front.execute(confirm=lambda details: False)
        self.assertEqual(self.pointer_path().read_bytes(), previous)
        self.assertEqual(len(self.builds), count)
        self.assertTrue((stage / 'dist/update.json.sig').is_file())

    def test_prepare_signs_testable_artifacts_and_records_only_success(self):
        stage = self.prepare()
        self.assertTrue((stage / 'install/windows/nuschl.exe').is_file())
        self.assertTrue((stage / 'install/Nuschl.app/Contents/MacOS/nuschl').is_file())
        self.local.verify_signature(stage, SIGNER)
        self.assertEqual(self.pointer()['identity'], front.signed_identity(stage))
        self.assertEqual(stage.parent.stat().st_mode & 0o777, 0o700)
        self.assertEqual(self.pointer_path().stat().st_mode & 0o777, 0o600)
        self.assertNotIn('/tmp/', str(stage))
        self.assertEqual(len(self.builds), 7)
        self.assertTrue(all('--cores' in args and '--max-jobs' in args for args in self.builds))
        self.assertIn('?rev=' + self.pointer()['commit'] + '#update-signer', self.builds[0][-1])
        self.assertFalse(self.remote_calls)
        old = self.pointer_path().read_bytes()
        with patch.object(self.local, 'prepare', side_effect=ValueError('fixture prep failed')):
            with self.assertRaisesRegex(ValueError, 'fixture prep failed'):
                self.prepare()
        self.assertEqual(self.pointer_path().read_bytes(), old)
        with patch.object(self.local, 'sign', side_effect=ValueError('fixture sign failed')):
            with self.assertRaisesRegex(ValueError, 'fixture sign failed'):
                self.prepare()
        self.assertEqual(self.pointer_path().read_bytes(), old)

    def test_publish_uses_same_signed_bytes_without_build_sign_or_private_key(self):
        stage = self.prepare()
        before = self.local.tree(stage / 'dist')
        count = len(self.builds)
        self.key.unlink()  # Publication needs no private key at all.
        with patch.object(self.local, 'sign', side_effect=AssertionError('publish must not sign')):
            self.assertEqual(front.execute(True, confirm=lambda details: True), stage)
        self.assertEqual(self.local.tree(stage / 'dist'), before)
        self.assertEqual(len(self.builds), count)
        self.assertEqual(self.remote_calls[-1][1:3], ['release', 'edit'])
        self.assertTrue(self.pointer()['published'])
        with self.assertRaisesRegex(ValueError, 'already published'):
            front.execute(True, confirm=lambda details: True)

    def test_publish_cancel_tamper_missing_pointer_and_unrelated_stage_are_local_refusals(self):
        with self.assertRaisesRegex(ValueError, 'No successfully signed'):
            front.execute(True, confirm=lambda details: True)
        stage = self.prepare()
        with self.assertRaisesRegex(ValueError, 'Cancelled'):
            front.execute(True, confirm=lambda details: False)
        self.assertFalse(self.remote_calls)
        asset = stage / 'dist/nuschl-0.1.0-windows-x86_64.zip'
        original = asset.read_bytes()
        asset.write_bytes(original + b'altered')
        confirmation = unittest.mock.Mock(return_value=True)
        with self.assertRaisesRegex(ValueError, 'payload changed'):
            front.execute(True, confirm=confirmation)
        confirmation.assert_not_called()
        self.assertFalse(self.remote_calls)
        asset.write_bytes(original)
        self.git('remote', 'set-url', 'origin', 'https://github.com/other/repo.git')
        with self.assertRaisesRegex(ValueError, 'another source/tag'):
            front.execute(True, confirm=lambda details: True)
        self.assertFalse(self.remote_calls)

    def test_new_tag_is_local_after_prepare_confirmation_and_pushed_only_after_publish_confirmation(self):
        self.git('tag', '-d', 'v0.1.0')
        with self.assertRaisesRegex(ValueError, 'Cancelled'):
            front.execute(confirm=lambda details: False)
        self.assertTrue(front.infer(self.repo)['create_tag'])
        self.assertFalse(self.remote_calls)
        self.prepare()
        self.assertTrue(self.pointer()['created_local_tag'])
        self.assertEqual(self.git('rev-parse', 'v0.1.0'), self.pointer()['commit'])
        self.assertFalse(self.remote_calls)
        with self.assertRaisesRegex(ValueError, 'Cancelled'):
            front.execute(True, confirm=lambda details: False)
        self.assertFalse(self.remote_calls)
        front.execute(True, confirm=lambda details: True)
        self.assertEqual(self.remote_calls[0], ['git', '-C', str(self.repo), 'push', 'origin',
                                             'refs/tags/v0.1.0:refs/tags/v0.1.0'])
        self.assertNotIn('--force', self.remote_calls[0])

    def test_tamper_during_publish_confirmation_refuses_before_new_tag_push(self):
        self.git('tag', '-d', 'v0.1.0')
        stage = self.prepare()
        def change_asset(details):
            (stage / 'dist/INSTALL-NIX.txt').write_text('tampered during confirmation')
            return True
        with self.assertRaisesRegex(ValueError, 'payload changed'):
            front.execute(True, confirm=change_asset)
        self.assertFalse(self.remote_calls)

    def test_cache_refuses_ram_or_symlink_staging_and_command_lock_is_exclusive(self):
        with patch.dict(os.environ, {'XDG_CACHE_HOME': '/tmp'}):
            with self.assertRaisesRegex(ValueError, 'disk-backed'):
                front.cache_root()
        directory = self.cache / 'nuschl-release'
        directory.symlink_to(self.home, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, '0700'):
            front.cache_root()
        directory.unlink()
        root = front.cache_root()
        with front.locked(root):
            with self.assertRaisesRegex(ValueError, 'Another just release'):
                with front.locked(root):
                    self.fail('Second operator acquired exclusive lock')


class TerminalAndArgv(unittest.TestCase):
    def test_noninteractive_confirmation_refuses(self):
        with patch.object(sys.stdin, 'isatty', return_value=False):
            with self.assertRaisesRegex(ValueError, 'interactive terminal'):
                front.confirm_modal(['fixture'])

    def test_actual_terminal_modal_defaults_no_and_requires_deliberate_yes(self):
        for keys, expected in ((b'\n', 'False'), (b'\t\n', 'True'), (b'\x1b', 'False')):
            with self.subTest(keys=keys):
                master, slave = pty.openpty()
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack('HHHH', 24, 80, 0, 0))
                code = ('import runpy; m=runpy.run_path(' + repr(str(ROOT / 'release/release.py')) + '); '
                        'print("RESULT="+str(m["confirm_modal"](["Synthetic release; no key or build."])))')
                child = subprocess.Popen([sys.executable, '-c', code], stdin=slave, stdout=slave,
                                         stderr=slave, env={**os.environ, 'TERM': 'xterm-256color'})
                os.close(slave)
                output, deadline = b'', time.monotonic() + 5
                try:
                    while b'Confirm Nuschl release' not in output and time.monotonic() < deadline:
                        if select.select([master], [], [], .2)[0]:
                            output += os.read(master, 65536)
                    self.assertIn(b'Confirm Nuschl release', output)
                    os.write(master, keys)
                    while child.poll() is None and time.monotonic() < deadline:
                        if select.select([master], [], [], .2)[0]:
                            try:
                                output += os.read(master, 65536)
                            except OSError:
                                break
                    child.wait(timeout=2)
                    self.assertEqual(child.returncode, 0, output.decode(errors='replace'))
                    self.assertIn(('RESULT=' + expected).encode(), output)
                finally:
                    if child.poll() is None:
                        child.kill()
                        child.wait()
                    os.close(master)

    def test_just_forwards_publish_and_literal_arguments_without_shell_expansion(self):
        with tempfile.TemporaryDirectory(prefix='release-argv-') as directory:
            area = Path(directory)
            record, sentinel = area / 'argv.json', area / 'should-not-exist'
            stub = area / 'python3'
            stub.write_text('#!' + sys.executable + '\nimport json,os,sys\n'
                            'open(os.environ["FIXTURE_ARGV"],"w").write(json.dumps(sys.argv[1:]))\n')
            stub.chmod(0o700)
            just = shutil.which('just')
            self.assertIsNotNone(just, 'Run this fixture in the pinned devshell (just required).')
            env = {**os.environ, 'PATH': str(area) + os.pathsep + os.environ['PATH'], 'FIXTURE_ARGV': str(record)}
            for arguments in (['--publish'], ['$(touch ' + str(sentinel) + ')', 'a b', '; false']):
                subprocess.run([just, '--justfile', str(ROOT / 'justfile'), 'release', *arguments],
                               check=True, capture_output=True, env=env)
                self.assertEqual(json.loads(record.read_bytes()), ['release/release.py', *arguments])
                self.assertFalse(sentinel.exists())


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--signer', required=True, help='existing signer; used ONLY with generated temporary keys')
    args, remaining = parser.parse_known_args()
    SIGNER = fixtures.SIGNER = str(Path(args.signer).resolve(strict=True))
    # Disk-backed owned scratch even when the caller has RAM-backed /tmp.
    cache = Path.home() / '.cache/nuschl-release-command-tests'
    cache.mkdir(mode=0o700, parents=True, exist_ok=True)
    tempfile.tempdir = str(cache)
    unittest.main(argv=[__file__, *remaining])
