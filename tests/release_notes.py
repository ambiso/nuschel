#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Release notes: pinned git-cliff over owned synthetic repositories, frozen at prepare.

The real git-cliff from PATH (the pinned development shell) renders synthetic Git
history. Substitute executables check the fixed argv, empty environment and bounds;
publication reuse runs on the temporary-key fixtures of tests/local_release.py and
tests/release_command.py. No network, production keys or remote repositories: the
one remote-metadata control runs only inside an unprivileged network namespace.
"""
import argparse
import contextlib
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from unittest.mock import Mock, patch

ROOT = Path(__file__).resolve().parents[1]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


commands = module('release_command_fixture', ROOT / 'tests/release_command.py')
fixtures = commands.fixtures
local, front = fixtures.local, commands.front
SIGNER = None


def setUpModule():
    print('git-cliff under test:', *local.git_cliff(), file=sys.stderr)


class Repo:
    """Owned synthetic repository; signing is disabled in this repository only."""
    def __init__(self, path):
        self.path, self.count = path, 0
        path.mkdir()
        for args in (['init', '-q'], ['config', 'commit.gpgsign', 'false'], ['config', 'tag.gpgsign', 'false'],
                     ['config', 'user.name', 'fixture'], ['config', 'user.email', 'fixture@example.invalid']):
            self.git(*args)

    def git(self, *args):
        return subprocess.run(['git', '-C', str(self.path), *args], check=True,
                              capture_output=True, text=True).stdout.strip()

    def commit(self, message):
        self.count += 1
        (self.path / ('file-' + str(self.count))).write_text(message + '\n')
        self.git('add', '.')
        self.git('commit', '-qm', message)
        return self.git('rev-parse', 'HEAD')


class History(unittest.TestCase):
    def setUp(self):
        patcher = patch.dict(os.environ, {'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_SYSTEM': '/dev/null'})
        patcher.start()
        self.addCleanup(patcher.stop)
        temporary = tempfile.TemporaryDirectory(prefix='release-notes-test-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.repo = repo = Repo(self.root / 'repo')
        self.c = {1: repo.commit('feat: first feature')}
        repo.git('tag', '-a', 'v0.1.0', '-m', 'first')
        self.c[2] = repo.commit('fix(ui): repair before the previous release')
        self.c[3] = repo.commit('docs: internal notes are not release notes')
        repo.git('tag', '-a', 'v0.2.0-rc.1', '-m', 'candidate')
        repo.git('tag', 'v0.2.0')
        main = repo.git('rev-parse', '--abbrev-ref', 'HEAD')
        repo.git('switch', '-q', '-c', 'side', self.c[2])
        self.side = repo.commit('feat: side branch only')
        repo.git('tag', 'v0.2.1')  # Not an ancestor of the prepared commit.
        repo.git('switch', '-q', main)
        self.c[4] = repo.commit('feat(audio): new capture path')
        self.c[5] = repo.commit('fix: correct a regression')
        self.c[6] = repo.commit('test: hidden from release notes')
        self.c[7] = repo.commit('refactor!: drop the legacy protocol')
        repo.git('tag', 'unrelated')

    def stage(self, name):
        stage = self.root / name
        (stage / 'source').mkdir(parents=True)
        return stage

    def fake(self, body, name='fake-git-cliff'):
        path = self.root / name
        path.write_text('#!' + sys.executable + ' -I\n' + body)
        path.chmod(0o700)
        return path, 'git-cliff 9.9.9'

    def test_previous_tag_is_the_unique_newest_ancestor_version_tag(self):
        self.assertEqual(local.previous_tag(self.repo.path, self.c[7], 'v0.3.0'), ('v0.2.0', self.c[3]))
        self.assertEqual(local.previous_tag(self.repo.path, self.c[3], 'v0.2.0'), ('v0.1.0', self.c[1]))
        self.assertEqual(local.previous_tag(self.repo.path, self.c[1], 'v0.1.0'), (None, None))
        self.repo.git('tag', 'v0.3.0-rc.1', self.c[7])  # An alias on the prepared commit is not "previous".
        self.assertEqual(local.previous_tag(self.repo.path, self.c[7], 'v0.3.0'), ('v0.2.0', self.c[3]))

    def test_duplicate_diverged_or_shallow_history_refuses(self):
        self.repo.git('tag', 'v0.2.2', self.c[3])
        with self.assertRaisesRegex(ValueError, 'several version tags'):
            local.previous_tag(self.repo.path, self.c[7], 'v0.3.0')
        self.repo.git('tag', '-d', 'v0.2.2')
        self.repo.git('merge', '-q', '--no-ff', '-m', 'merge: side branch', 'side')
        with self.assertRaisesRegex(ValueError, 'diverge'):
            local.previous_tag(self.repo.path, self.repo.git('rev-parse', 'HEAD'), 'v0.3.0')
        shallow = self.root / 'shallow'
        subprocess.run(['git', 'clone', '-q', '--depth', '1', self.repo.path.as_uri(), str(shallow)],
                       check=True, capture_output=True)
        with self.assertRaisesRegex(ValueError, 'non-shallow'):
            local.previous_tag(shallow, self.c[7], 'v0.3.0')

    def test_real_git_cliff_freezes_the_exact_range_without_touching_the_source(self):
        git_dir = self.repo.path / '.git'
        before = local.tree(git_dir), self.repo.git('for-each-ref')
        stage = self.stage('stage')
        local.stage_notes(self.repo.path, stage, 'v0.3.0', self.c[7], '0.3.0')
        notes = stage / 'notes'
        body = (notes / 'RELEASE-NOTES.md').read_text()
        self.assertTrue(body.startswith('## Changes since v0.2.0\n'), body)
        for expected in ('### Breaking changes', 'Drop the legacy protocol (' + self.c[7][:12] + ')',
                         '### Features', 'audio: New capture path', '### Fixes', 'Correct a regression'):
            self.assertIn(expected, body)
        for absent in ('repair before', 'first feature', 'internal notes', 'hidden', 'side branch'):
            self.assertNotIn(absent, body)
        record = local.load(notes / 'record.json')
        config = (ROOT / 'cliff.toml').read_bytes()
        self.assertEqual({k: record[k] for k in ('tag', 'commit', 'previous_tag', 'previous_commit', 'range',
                                                 'commit_count', 'curated', 'config_sha256', 'generator_path')},
                         {'tag': 'v0.3.0', 'commit': self.c[7], 'previous_tag': 'v0.2.0',
                          'previous_commit': self.c[3], 'range': self.c[3] + '..' + self.c[7],
                          'commit_count': 4, 'curated': None,
                          'config_sha256': hashlib.sha256(config).hexdigest(),
                          'generator_path': str(local.git_cliff()[0])})
        self.assertEqual((notes / 'cliff.toml').read_bytes(), config)
        self.assertEqual(record['notes_sha256'], local.sha(notes / 'RELEASE-NOTES.md'))
        self.assertTrue(all((notes / n).stat().st_mode & 0o777 == 0o444 for n in
                            ('cliff.toml', 'git-cliff.md', 'RELEASE-NOTES.md', 'record.json')))
        self.assertEqual((local.tree(git_dir), self.repo.git('for-each-ref')), before)
        again = self.stage('again')
        # Relative operator paths must not let git-cliff fall back to its built-in template.
        with contextlib.chdir(self.root):
            local.stage_notes(Path('repo'), Path('again'), 'v0.3.0', self.c[7], '0.3.0')
        for name in ('git-cliff.md', 'RELEASE-NOTES.md'):
            self.assertEqual((again / 'notes' / name).read_bytes(), (notes / name).read_bytes())
        with self.assertRaisesRegex(ValueError, 'existing absolute paths'):
            local.run_git_cliff(local.git_cliff()[0], Path('cliff.toml'), self.root, self.c[7],
                                self.root / 'unused.md', self.root / 'unused.log')

    def test_first_release_spans_full_history_after_tagged_curated_notes(self):
        stage = self.stage('first')
        (stage / 'source/release').mkdir()
        curated = '# Nuschl 0.1.0\n\nCurated, tagged text.\n'
        (stage / 'source/release/notes-0.1.0.md').write_text(curated)
        local.stage_notes(self.repo.path, stage, 'v0.1.0', self.c[1], '0.1.0')
        body = (stage / 'notes/RELEASE-NOTES.md').read_text()
        self.assertTrue(body.startswith(curated + '\n## Changes\n'), body)
        self.assertIn('- First feature (' + self.c[1][:12] + ')', body)
        record = local.load(stage / 'notes/record.json')
        self.assertEqual((record['previous_tag'], record['range'], record['commit_count'], record['curated']),
                         (None, self.c[1], 1, 'release/notes-0.1.0.md'))

    def test_publication_check_reuses_bytes_and_refuses_changed_range_or_inputs(self):
        stage = self.stage('stage')
        local.stage_notes(self.repo.path, stage, 'v0.3.0', self.c[7], '0.3.0')
        local.write(stage / 'local.json', {'source_repository': str(self.repo.path)})
        ledger = {'tag': 'v0.3.0', 'commit': self.c[7], 'version': '0.3.0'}
        never = AssertionError('publication must never regenerate notes')
        with patch.object(local, 'run_git_cliff', side_effect=never), \
                patch.object(local, 'git_cliff', side_effect=never):
            path, text = local.check_notes(stage, ledger)
            self.assertEqual((path, text), (stage / 'notes/RELEASE-NOTES.md', path.read_text()))
            with patch.object(local, 'ROOT', self.root):  # A later publisher config is irrelevant.
                local.check_notes(stage, ledger)
            self.repo.git('tag', 'v0.2.5', self.c[5])
            with self.assertRaisesRegex(ValueError, 'range changed'):
                local.check_notes(stage, ledger)
            self.repo.git('tag', '-d', 'v0.2.5')
            for name in ('RELEASE-NOTES.md', 'git-cliff.md', 'cliff.toml'):
                with self.subTest(name=name):
                    target = stage / 'notes' / name
                    original = target.read_bytes()
                    target.chmod(0o644)
                    target.write_bytes(original.replace(b'\n', b'\n ', 1))
                    with self.assertRaisesRegex(ValueError, 'changed after preparation'):
                        local.check_notes(stage, ledger)
                    target.write_bytes(original)
            with self.assertRaisesRegex(ValueError, 'another source/tag'):
                local.check_notes(stage, {**ledger, 'commit': self.c[6]})
            target = stage / 'notes/RELEASE-NOTES.md'
            target.unlink()
            target.symlink_to(stage / 'notes/git-cliff.md')
            with self.assertRaises(OSError):
                local.check_notes(stage, ledger)

    def test_fixed_argv_empty_environment_and_private_history_view(self):
        record = self.root / 'invocation.json'
        fake = self.fake('import json, os, pathlib, sys\n'
                         'view = pathlib.Path(sys.argv[4])\n'
                         'refs = {str(p.relative_to(view)): p.read_text().strip()\n'
                         '        for p in (view / "refs").rglob("*") if p.is_file()}\n'
                         'env = dict(x.split("=", 1) for x in\n'
                         '           open("/proc/self/environ").read().split("\\0") if x)\n'
                         'json.dump({"argv": sys.argv[1:], "env": env, "cwd": os.getcwd(), "refs": refs,\n'
                         '           "alternates": (view / "objects/info/alternates").read_text()},\n'
                         '          open(' + repr(str(record)) + ', "w"))\n'
                         'print("## Changes\\n\\n- Substitute output")\n')
        polluted = {'GITHUB_TOKEN': 'synthetic-not-a-token', 'GIT_CLIFF__CHANGELOG__FOOTER': 'injected',
                    'GIT_CLIFF_CONFIG_URL': 'https://example.invalid/cliff.toml', 'RUST_LOG': 'trace'}
        stage = self.stage('stage')
        with patch.dict(os.environ, polluted), patch.object(local, 'git_cliff', return_value=fake):
            local.stage_notes(self.repo.path, stage, 'v0.3.0', self.c[7], '0.3.0')
        seen = json.loads(record.read_bytes())
        view = Path(seen['argv'][3])
        self.assertEqual(seen['argv'], ['--config', str(stage / 'notes/cliff.toml'), '--repository', str(view),
                                        '--offline', '--no-exec', '--strip', 'all', '--',
                                        self.c[3] + '..' + self.c[7]])
        self.assertEqual(seen['env'], {'HOME': seen['cwd'], 'TZ': 'UTC', 'LC_ALL': 'C'})
        self.assertEqual(seen['refs'], {'refs/heads/release': self.c[7], 'refs/tags/v0.3.0': self.c[7],
                                        'refs/tags/v0.2.0': self.c[3]})
        self.assertEqual(seen['alternates'], str((self.repo.path / '.git/objects').resolve()) + '\n')
        self.assertEqual(view.name, 'history.git')
        self.assertFalse(view.is_relative_to(self.repo.path) or view.exists())
        self.assertIn('- Substitute output', (stage / 'notes/RELEASE-NOTES.md').read_text())

    def test_output_time_and_range_bounds_fail_closed(self):
        cases = (('oversized-file', 'import sys\nsys.stdout.write("x" * (2 * 1024 * 1024))\n', 'failed or exceeded'),
                 ('oversized-notes', 'print("x" * 130000)\n', 'exceeds its release notes bound'),
                 ('control', 'print("- \\x1b[31mred")\n', 'control'),
                 ('failure', 'import sys\nsys.exit(3)\n', 'failed or exceeded'))
        for name, body, error in cases:
            with self.subTest(name=name), patch.object(local, 'git_cliff', return_value=self.fake(body, name)):
                stage = self.stage('bound-' + name)
                with self.assertRaisesRegex(ValueError, error):
                    local.stage_notes(self.repo.path, stage, 'v0.3.0', self.c[7], '0.3.0')
                self.assertLessEqual((stage / 'notes/git-cliff.md').stat().st_size, local.NOTES['file'])
                self.assertFalse((stage / 'notes/RELEASE-NOTES.md').exists())
        pid = self.root / 'sleeper.pid'
        sleeper = self.fake('import os, time\nopen(' + repr(str(pid)) + ', "w").write(str(os.getpid()))\n'
                            'time.sleep(60)\n', 'sleeper')
        started = time.monotonic()
        with patch.object(local, 'git_cliff', return_value=sleeper), patch.dict(local.NOTES, {'seconds': 1}):
            with self.assertRaisesRegex(ValueError, 'wall-time'):
                local.stage_notes(self.repo.path, self.stage('bound-time'), 'v0.3.0', self.c[7], '0.3.0')
        self.assertLess(time.monotonic() - started, 20)
        with self.assertRaises(ProcessLookupError):  # Killed AND reaped before the refusal.
            os.kill(int(pid.read_text()), 0)
        with patch.dict(local.NOTES, {'commits': 3}), \
                patch.object(local, 'git_cliff', side_effect=AssertionError('range bound comes first')):
            with self.assertRaisesRegex(ValueError, 'exceeds its bound'):
                local.stage_notes(self.repo.path, self.stage('bound-range'), 'v0.3.0', self.c[7], '0.3.0')

    def test_config_whitelist_refuses_commands_remotes_outputs_and_dynamic_templates(self):
        good = (ROOT / 'cliff.toml').read_text()
        local.cliff_config(good.encode())
        first = '## Changes since {{ previous.version }}\n'
        variants = {
            'preprocessor command': good.replace('commit_preprocessors = []',
                'commit_preprocessors = [{ pattern = ".*", replace_command = "sh" }]'),
            'postprocessor command': good.replace('trim = true',
                'trim = true\npostprocessors = [{ pattern = "x", replace_command = "sh" }]'),
            'remote': good + '\n[remote.github]\nowner = "fixture"\nrepo = "project"\n',
            'output': good.replace('trim = true', 'trim = true\noutput = "CHANGELOG.md"'),
            'header': good.replace('trim = true', 'trim = true\nheader = "x"'),
            'footer': good.replace('trim = true', 'trim = true\nfooter = "x"'),
            'bump': good + '\n[bump]\nfeatures_always_bump_minor = true\n',
            'submodules': good.replace('recurse_submodules = false', 'recurse_submodules = true'),
            'link parsers': good.replace('commit_preprocessors = []', 'commit_preprocessors = []\nlink_parsers = []'),
            'field parser': good.replace('commit_parsers = [',
                'commit_parsers = [\n    { field = "author.name", pattern = "x", skip = true },'),
            'environment': good.replace(first, '{{ get_env(name="HOME") }}\n'),
            'clock': good.replace(first, '{{ now() }}\n'),
            'remote template': good.replace(first, '{% for c in github.contributors %}{% endfor %}\n'),
            'local path': good.replace(first, '{{ repository }}\n'),
            'statistics': good.replace(first, '{{ statistics.commit_count }}\n'),
            'oversized': good + '#' * (64 * 1024),
        }
        for name, text in variants.items():
            with self.subTest(name=name):
                self.assertNotEqual(text, good)
                tomllib.loads(text)  # Valid TOML: the whitelist, not the parser, refuses it.
                with self.assertRaises(ValueError):
                    local.cliff_config(text.encode())
        publisher = self.root / 'publisher'
        publisher.mkdir()
        (publisher / 'cliff.toml').write_text(variants['preprocessor command'])
        stage = self.stage('refused-config')
        with patch.object(local, 'ROOT', publisher), \
                patch.object(local, 'git_cliff', side_effect=AssertionError('config is checked first')):
            with self.assertRaisesRegex(ValueError, 'reviewed git-cliff subset'):
                local.stage_notes(self.repo.path, stage, 'v0.3.0', self.c[7], '0.3.0')
        self.assertEqual(sorted(p.name for p in (stage / 'notes').iterdir()), [])

    def test_real_git_cliff_needs_no_exec_and_offline_flags(self):
        executable, _ = local.git_cliff()
        sentinel = self.root / 'command-ran'
        config = self.root / 'command.toml'
        config.write_text('[changelog]\nbody = "{% for commit in commits %}{{ commit.message }}{% endfor %}"\n'
                          '[git]\nconventional_commits = false\nfilter_unconventional = false\n'
                          'commit_preprocessors = [{ pattern = ".*", replace_command = ": > ' + str(sentinel) + '" }]\n')
        environment = {'HOME': str(self.root), 'PATH': os.environ['PATH']}
        for flags, ran in ((['--no-exec'], False), ([], True)):  # Discriminating control without the flag.
            subprocess.run([str(executable), '--config', str(config), '--repository', str(self.repo.path),
                            '--offline', *flags, '--', self.c[7]], check=True, capture_output=True,
                           env=environment, cwd=self.root, timeout=60)
            self.assertEqual(sentinel.exists(), ran)
        if shutil.which('unshare') is None or subprocess.run(['unshare', '-rn', 'true'],
                                                             capture_output=True).returncode:
            self.skipTest('no unprivileged network namespace; never run the remote control without one')
        remote = self.root / 'remote.toml'
        remote.write_text('[remote.github]\nowner = "fixture"\nrepo = "project"\n[changelog]\n'
                          'body = "{% for c in github.contributors %}{{ c.username }}{% endfor %}x"\n')
        outcomes = []
        for flags in ([], ['--offline']):
            outcomes.append(subprocess.run(
                ['unshare', '-rn', '--', str(executable), '--config', str(remote), '--repository',
                 str(self.repo.path), '--no-exec', *flags, '--', self.c[7]],
                capture_output=True, env=environment, cwd=self.root, timeout=60))
        self.assertNotEqual(outcomes[0].returncode, 0)
        self.assertIn(b'api.github.com', outcomes[0].stderr)  # Attempted, but the namespace has no network.
        self.assertEqual(outcomes[1].returncode, 0, outcomes[1].stderr)


class Publication(unittest.TestCase):
    def setUp(self):
        commands.SIGNER = fixtures.SIGNER = SIGNER
        self.fixture = fixtures.LocalRelease()
        self.fixture.setUp()
        self.addCleanup(self.fixture.doCleanups)
        repo = self.fixture.repo
        (repo / 'feature.txt').write_text('synthetic\n')
        for args in (['config', 'commit.gpgsign', 'false'], ['config', 'tag.gpgsign', 'false'], ['add', '.'],
                     ['-c', 'user.name=fixture', '-c', 'user.email=fixture@example.invalid',
                      'commit', '-qm', 'feat: synthetic release feature'], ['tag', '-f', 'v0.1.0']):
            subprocess.run(['git', '-C', str(repo), *args], check=True, capture_output=True)
        self.stage = self.fixture.stage = self.fixture.root / 'notes-stage'
        local.prepare(argparse.Namespace(source_repo=repo, tag='v0.1.0', repository='fixture/project',
                                         output=self.stage, build=False))
        self.fixture.sign()
        self.notes = self.stage / 'notes/RELEASE-NOTES.md'

    def test_publish_uploads_the_frozen_notes_and_never_runs_git_cliff(self):
        body = self.notes.read_text()
        self.assertTrue(body.startswith('Synthetic temporary-key fixture.\n\n## Changes\n'), body)
        self.assertIn('Synthetic release feature', body)
        never = AssertionError('publication must never regenerate notes')
        with patch.object(local, 'git_cliff', side_effect=never), \
                patch.object(local, 'run_git_cliff', side_effect=never), \
                patch.object(local, 'stage_notes', side_effect=never), \
                patch.object(local, 'ROOT', self.fixture.root):  # No publisher cliff.toml at all.
            calls, ok = self.fixture.publish()
        self.assertTrue(ok)
        create = next(c for c in calls if c[1:3] == ['release', 'create'])
        self.assertEqual(create[create.index('--notes-file') + 1], str(self.notes))
        self.assertEqual(self.notes.read_text(), body)
        self.assertEqual(calls[-1][1:3], ['release', 'edit'])

    def test_changed_notes_refuse_before_remote_calls_and_draft_body_mismatch_stays_draft(self):
        original = self.notes.read_bytes()
        self.notes.chmod(0o644)
        self.notes.write_bytes(original + b'\nUnreviewed addition.\n')
        calls, ok = self.fixture.publish()
        self.assertEqual((calls, ok), ([], False))
        self.notes.write_bytes(original)
        calls, ok = self.fixture.publish(body='A different remote body')
        self.assertFalse(ok)
        self.assertTrue(any(c[1:3] == ['release', 'create'] for c in calls))
        self.assertFalse(any(c[1:3] == ['release', 'edit'] for c in calls))


class Frontend(unittest.TestCase):
    def setUp(self):
        commands.SIGNER = fixtures.SIGNER = SIGNER
        self.command = commands.ReleaseCommand('test_prepare_signs_testable_artifacts_and_records_only_success')
        self.command.setUp()
        self.addCleanup(self.command.doCleanups)

    def test_publish_confirmation_shows_frozen_notes_and_tamper_refuses_first(self):
        stage = self.command.prepare()
        notes = stage / 'notes/RELEASE-NOTES.md'
        original = notes.read_bytes()
        notes.chmod(0o644)
        notes.write_bytes(original + b'tampered\n')
        confirmation = Mock(return_value=True)
        with self.assertRaisesRegex(ValueError, 'changed after preparation'):
            front.execute(True, confirm=confirmation)
        confirmation.assert_not_called()
        self.assertFalse(self.command.remote_calls)
        notes.write_bytes(original)
        details = []
        front.execute(True, confirm=lambda lines: details.extend(lines) or True)
        self.assertIn('Release notes (frozen at prepare): ' + str(notes), details)
        self.assertTrue(any(line.startswith('Notes range: first release (full history) .. ') for line in details))
        create = next(c for c in self.command.remote_calls if c[1:3] == ['release', 'create'])
        self.assertEqual(create[create.index('--notes-file') + 1], str(notes))


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--signer', required=True, help='existing signer; used ONLY with generated temporary keys')
    args, remaining = parser.parse_known_args()
    SIGNER = str(Path(args.signer).resolve(strict=True))
    cache = Path.home() / '.cache/nuschl-release-notes-tests'
    cache.mkdir(mode=0o700, parents=True, exist_ok=True)
    tempfile.tempdir = str(cache)  # Disk-backed owned scratch even with RAM-backed /tmp.
    unittest.main(argv=[__file__, *remaining])
