#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Release notes: git-cliff over owned synthetic repositories, frozen and ledger-bound.

The real git-cliff of the pinned development shell renders synthetic Git history;
it substitutes only for the Nix devShell selection, which has its own argv test.
Substitute executables check the fixed argv, empty environment and bounds.
Publication reuse and the consistent-rewrite control use the temporary-key fixtures
of tests/local_release.py and tests/release_command.py. No network, production
keys or remote repositories: the one remote-metadata control runs only inside an
unprivileged network namespace.
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
SELECT_GIT_CLIFF = local.select_git_cliff  # The real selection, before any fixture substitutes it.
SIGNER = None
NEVER = AssertionError('publication must never regenerate notes')


def setUpModule():
    print('git-cliff under test:', fixtures.pinned_git_cliff()['output'], file=sys.stderr)


def sha256(data):
    return hashlib.sha256(data).hexdigest()


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

    def commit(self, message, config=None):
        self.count += 1
        (self.path / ('file-' + str(self.count))).write_text(message + '\n')
        if config is not None:
            (self.path / 'cliff.toml').write_text(config)
        self.git('add', '.')
        self.git('commit', '-qm', message)
        return self.git('rev-parse', 'HEAD')

    def blob(self, commit, name='cliff.toml'):
        return subprocess.run(['git', '-C', str(self.path), 'show', commit + ':' + name],
                              check=True, capture_output=True).stdout


class History(unittest.TestCase):
    def setUp(self):
        patcher = patch.dict(os.environ, {'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_CONFIG_SYSTEM': '/dev/null'})
        patcher.start()
        self.addCleanup(patcher.stop)
        temporary = tempfile.TemporaryDirectory(prefix='release-notes-test-')
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.tool = fixtures.pinned_git_cliff()
        selection = patch.object(local, 'select_git_cliff', return_value=self.tool)
        self.selection = selection.start()
        self.addCleanup(selection.stop)
        self.config = (ROOT / 'cliff.toml').read_text()
        self.repo = repo = Repo(self.root / 'repo')
        self.c = {1: repo.commit('feat: first feature', self.config)}
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
        self.flake = 'git+file:///synthetic-selection-is-substituted?rev=' + self.c[7]

    def stage(self, name, commit=None, config=True):
        """A stage whose source/ holds the selected commit's cliff.toml, as export() would."""
        stage = self.root / name
        (stage / 'source').mkdir(parents=True)
        if config:
            (stage / 'source/cliff.toml').write_bytes(self.repo.blob(commit or self.c[7]))
        return stage

    def notes(self, stage, commit=None, tag='v0.3.0', version='0.3.0', **kwargs):
        return local.stage_notes(self.repo.path, stage, tag, commit or self.c[7], version, self.flake, **kwargs)

    def ledger(self, stage, entry, commit=None, tag='v0.3.0', version='0.3.0'):
        local.write(stage / 'local.json', {'source_repository': str(self.repo.path)})
        return {'tag': tag, 'commit': commit or self.c[7], 'version': version, 'release_notes': entry}

    def fake(self, body, name='fake-git-cliff'):
        """A substitute devShell output whose bin/git-cliff reports a version, then runs body."""
        output = self.root / name
        (output / 'bin').mkdir(parents=True)
        path = output / 'bin/git-cliff'
        path.write_text('#!' + sys.executable + ' -I\nimport sys\n'
                        'if sys.argv[1:] == ["--version"]:\n    print("git-cliff 9.9.9")\n    raise SystemExit\n'
                        + body)
        path.chmod(0o700)
        return {'drv': '/nix/store/' + 'b' * 32 + '-git-cliff-fake.drv', 'output': str(output)}

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
        # All scratch lives under the stage: a default TemporaryDirectory would fail here.
        with patch.object(tempfile, 'tempdir', str(self.root / 'no-default-temporary-directory')):
            entry = self.notes(stage)
        notes = stage / 'notes'
        body = (notes / 'RELEASE-NOTES.md').read_text()
        self.assertTrue(body.startswith('## Changes since v0.2.0\n'), body)
        for expected in ('### Breaking changes', 'Drop the legacy protocol (' + self.c[7][:12] + ')',
                         '### Features', 'audio: New capture path', '### Fixes', 'Correct a regression'):
            self.assertIn(expected, body)
        for absent in ('repair before', 'first feature', 'internal notes', 'hidden', 'side branch'):
            self.assertNotIn(absent, body)
        config = self.repo.blob(self.c[7])
        self.assertEqual(entry, {
            'format': 1, 'generator': entry['generator'], 'generator_path': self.tool['output'] + '/bin/git-cliff',
            'previous_tag': 'v0.2.0', 'previous_commit': self.c[3], 'range': self.c[3] + '..' + self.c[7],
            'commit_count': 4, 'config_sha256': sha256(config), 'curated': None, 'curated_sha256': None,
            'generated_sha256': sha256((notes / 'git-cliff.md').read_bytes()),
            'notes_sha256': sha256(body.encode())})
        self.assertRegex(entry['generator'], r'^git-cliff [0-9]+\.[0-9]+\.[0-9]+$')
        self.assertEqual((notes / 'cliff.toml').read_bytes(), config)
        self.assertEqual(sorted(p.name for p in notes.iterdir()),
                         ['RELEASE-NOTES.md', 'cliff.toml', 'git-cliff.log', 'git-cliff.md'])  # No tmp/, no record.
        self.assertTrue(all((notes / n).stat().st_mode & 0o777 == 0o444 for n in
                            ('cliff.toml', 'git-cliff.md', 'RELEASE-NOTES.md')))
        self.assertEqual((local.tree(git_dir), self.repo.git('for-each-ref')), before)
        again = self.stage('again')
        # Relative operator paths must not let git-cliff fall back to its built-in template.
        with contextlib.chdir(self.root):
            self.assertEqual(local.stage_notes(Path('repo'), Path('again'), 'v0.3.0', self.c[7], '0.3.0',
                                               self.flake), entry)
        for name in ('git-cliff.md', 'RELEASE-NOTES.md'):
            self.assertEqual((again / 'notes' / name).read_bytes(), (notes / name).read_bytes())
        with self.assertRaisesRegex(ValueError, 'existing absolute paths'):
            local.run_git_cliff(Path(self.tool['output']) / 'bin/git-cliff', Path('cliff.toml'), self.root,
                                self.c[7], self.root / 'unused.md', self.root / 'unused.log')

    def test_first_release_spans_full_history_after_tagged_curated_notes(self):
        stage = self.stage('first', self.c[1])
        (stage / 'source/release').mkdir()
        curated = '# Nuschl 0.1.0\n\nCurated, tagged text.\n'
        (stage / 'source/release/notes-0.1.0.md').write_text(curated)
        entry = self.notes(stage, self.c[1], 'v0.1.0', '0.1.0')
        body = (stage / 'notes/RELEASE-NOTES.md').read_text()
        self.assertTrue(body.startswith(curated + '\n## Changes\n'), body)
        self.assertIn('- First feature (' + self.c[1][:12] + ')', body)
        self.assertEqual((entry['previous_tag'], entry['range'], entry['commit_count'], entry['curated'],
                          entry['curated_sha256']),
                         (None, self.c[1], 1, 'release/notes-0.1.0.md', sha256(curated.encode())))

    def breaking_history(self, name, messages, config=None):
        """Tagged base, then messages; renders with the real pinned git-cliff and the tagged config."""
        repo = Repo(self.root / name)
        repo.commit('chore: start', config or self.config)
        repo.git('tag', 'v1.0.0')
        commits = [repo.commit(message) for message in messages]
        repo.git('tag', 'v1.1.0')
        stage = self.root / (name + '-stage')
        (stage / 'source').mkdir(parents=True)
        (stage / 'source/cliff.toml').write_bytes(repo.blob(commits[-1]))
        entry = local.stage_notes(repo.path, stage, 'v1.1.0', commits[-1], '1.1.0', self.flake)
        return commits, (stage / 'notes/RELEASE-NOTES.md').read_text(), entry

    def test_footer_only_breaking_changes_are_kept_on_any_type(self):
        cases = (('refactor: change synthetic protocol', 'BREAKING CHANGE', 'Change synthetic protocol'),
                 ('refactor(store): rename records', 'BREAKING-CHANGE', 'store: Rename records'),
                 ('chore: drop legacy store', 'BREAKING CHANGE', 'Drop legacy store'),
                 ('chore(deps): require newer runtime', 'BREAKING-CHANGE', 'deps: Require newer runtime'),
                 ('test: change fixture protocol', 'BREAKING CHANGE', 'Change fixture protocol'))
        for number, (subject, token, shown) in enumerate(cases):
            with self.subTest(subject=subject, token=token):  # Breaking-only release histories.
                commits, body, entry = self.breaking_history(
                    'breaking-' + str(number), [subject + '\n\n' + token + ': incompatible synthetic change'])
                self.assertEqual(body, '## Changes since v1.0.0\n\n### Breaking changes\n\n- ' + shown +
                                 ' (' + commits[0][:12] + ')\n')
                self.assertEqual((entry['previous_tag'], entry['commit_count']), ('v1.0.0', 1))
        # Discriminating control: the reviewed pre-fix config dropped exactly these commits.
        old = self.config.replace('    { footer = "^BREAKING[ -]CHANGE:", group = "<!-- 0 -->Breaking changes" },\n', '')
        self.assertNotEqual(old, self.config)
        with self.assertRaisesRegex(ValueError, 'git-cliff found no notable changes'):
            self.breaking_history('breaking-old-config', ['refactor: change synthetic protocol\n\n'
                                                          'BREAKING CHANGE: incompatible'], old)
        commits, body, _ = self.breaking_history('breaking-only-mixed-forms', [
            'refactor!: drop the v1 wire format', 'chore: remove old store\n\nBREAKING-CHANGE: unreadable',
            'test: harmless'])
        self.assertEqual(body, '## Changes since v1.0.0\n\n### Breaking changes\n\n'
                         '- Drop the v1 wire format (' + commits[0][:12] + ')\n'
                         '- Remove old store (' + commits[1][:12] + ')\n')

    def test_mixed_history_groups_breaking_footers_before_type_filters(self):
        commits, body, entry = self.breaking_history('mixed', [
            'feat: add capture meter',
            'refactor: rework storage\n\nBREAKING CHANGE: history databases are migrated',
            'docs: describe the meter',
            'fix: repair reconnect',
            'chore: plain chore\n\nRefs: #2',
            'refactor: harmless cleanup',
            'chore(deps): bump runtime\n\nBREAKING-CHANGE: needs the newer runtime',
            'fix(audio): change default device\n\nBREAKING CHANGE: device selection resets',
            'feat(api)!: remove the v1 endpoint'])
        c = {i + 1: commit[:12] for i, commit in enumerate(commits)}
        self.assertEqual(body, '## Changes since v1.0.0\n\n'
                         '### Breaking changes\n\n'
                         '- Rework storage (' + c[2] + ')\n'
                         '- deps: Bump runtime (' + c[7] + ')\n'
                         '- audio: Change default device (' + c[8] + ')\n'
                         '- api: Remove the v1 endpoint (' + c[9] + ')\n\n'
                         '### Features\n\n'
                         '- Add capture meter (' + c[1] + ')\n\n'
                         '### Fixes\n\n'
                         '- Repair reconnect (' + c[4] + ')\n')
        self.assertEqual(entry['commit_count'], 9)

    def test_only_the_selected_tags_config_can_render_notes(self):
        tool_checkout = self.root / 'tool-checkout'
        tool_checkout.mkdir()
        (tool_checkout / 'cliff.toml').write_text(self.config.replace('## Changes since', '## Tool checkout'))
        later = self.repo.commit('chore: later config', self.config.replace('## Changes since', '## Later HEAD'))
        stage = self.stage('selected', self.c[7])
        with patch.object(local, 'ROOT', tool_checkout):  # A different current tool config is ignored.
            entry = self.notes(stage)
        self.assertTrue((stage / 'notes/RELEASE-NOTES.md').read_text().startswith('## Changes since v0.2.0'))
        self.assertEqual(entry['config_sha256'], sha256(self.repo.blob(self.c[7])))
        self.assertNotEqual(self.repo.blob(later), self.repo.blob(self.c[7]))
        substituted = self.stage('substituted-export', self.c[7])
        (substituted / 'source/cliff.toml').write_bytes(self.repo.blob(later))  # Valid, but not the tag's.
        with self.assertRaisesRegex(ValueError, 'differs from the selected tagged source'):
            self.notes(substituted)
        with self.assertRaisesRegex(ValueError, 'no cliff.toml'):
            self.notes(self.stage('pre-git-cliff', config=False))
        unsafe = self.repo.commit('chore: unsafe config', self.config.replace(
            'commit_preprocessors = []', 'commit_preprocessors = [{ pattern = ".*", replace_command = "sh" }]'))
        self.repo.git('tag', 'v0.4.0')
        with patch.object(local, 'select_git_cliff', side_effect=AssertionError('config is checked first')):
            with self.assertRaisesRegex(ValueError, 'reviewed git-cliff subset'):
                self.notes(self.stage('unsafe', unsafe), unsafe, 'v0.4.0', '0.4.0')

    def test_publication_check_reuses_signed_entry_and_refuses_changed_range_or_inputs(self):
        stage = self.stage('stage')
        ledger = self.ledger(stage, self.notes(stage))
        with patch.object(local, 'run_git_cliff', side_effect=NEVER), \
                patch.object(local, 'git_cliff_version', side_effect=NEVER), \
                patch.object(local, 'stage_notes', side_effect=NEVER):
            path, text = local.check_notes(stage, ledger)
            self.assertEqual((path, text), (stage / 'notes/RELEASE-NOTES.md', path.read_text()))
            self.assertEqual(self.selection.call_args.args, ('git+' + self.repo.path.resolve().as_uri() +
                                                             '?rev=' + self.c[7],))
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
            # A different valid config with a coherently updated entry still differs from the tagged export.
            other = self.config.replace('## Changes since', '## Substituted').encode()
            (stage / 'notes/cliff.toml').write_bytes(other)
            with self.assertRaisesRegex(ValueError, 'changed after preparation'):
                local.check_notes(stage, {**ledger, 'release_notes': {**ledger['release_notes'],
                                                                      'config_sha256': sha256(other)}})
            (stage / 'notes/cliff.toml').write_bytes(self.repo.blob(self.c[7]))
            with self.assertRaisesRegex(ValueError, "not generated by the selected flake's git-cliff"):
                local.check_notes(stage, {**ledger, 'release_notes': {**ledger['release_notes'],
                                  'generator_path': '/nix/store/' + 'c' * 32 + '-git-cliff-0/bin/git-cliff'}})
            with self.assertRaisesRegex(ValueError, 'invalid release notes ledger entry'):
                local.check_notes(stage, {**ledger, 'release_notes': {**ledger['release_notes'], 'extra': 1}})
            target = stage / 'notes/RELEASE-NOTES.md'
            target.unlink()
            target.symlink_to(stage / 'notes/git-cliff.md')
            with self.assertRaises(OSError):
                local.check_notes(stage, ledger)

    def test_git_cliff_comes_only_from_the_selected_flake_devshell(self):
        good = [{'drv': '/nix/store/' + 'd' * 32 + '-git-cliff-2.14.2.drv',
                 'output': '/nix/store/' + 'e' * 32 + '-git-cliff-2.14.2'}]
        calls = []
        for reply, ok in ((good, True), ([], False), (good * 2, False),
                          ([{'drv': good[0]['drv'], 'output': '/tmp/git-cliff-2.14.2'}], False)):
            # The real selection with a substituted `nix eval` reply.
            with self.subTest(reply=reply), patch.object(local, 'run', side_effect=lambda args, **kw:
                                                         calls.append(list(map(str, args))) or json.dumps(reply)):
                if ok:
                    self.assertEqual(SELECT_GIT_CLIFF('git+file:///selected?rev=abc'), good[0])
                else:
                    with self.assertRaisesRegex(ValueError, 'exactly one git-cliff'):
                        SELECT_GIT_CLIFF('git+file:///selected?rev=abc')
        self.assertEqual(calls[0][:5], ['nix', 'eval', '--json',
                                        'git+file:///selected?rev=abc#devShells.x86_64-linux.default.nativeBuildInputs',
                                        '--apply'])
        self.assertIn('(p.pname or "") == "git-cliff"', calls[0][5])
        # PATH is never consulted: a decoy git-cliff first on PATH must not run.
        decoy, sentinel = self.root / 'decoy', self.root / 'decoy-ran'
        decoy.mkdir()
        (decoy / 'git-cliff').write_text('#!/bin/sh\n: > ' + str(sentinel) + '\n')
        (decoy / 'git-cliff').chmod(0o700)
        builds = []
        missing = {'drv': good[0]['drv'], 'output': str(self.root / 'not-built-git-cliff-2.14.2')}
        with patch.dict(os.environ, {'PATH': str(decoy) + os.pathsep + os.environ['PATH']}), \
                patch.object(local, 'select_git_cliff', return_value=missing), \
                patch.object(local, 'guarded_build', side_effect=lambda args, log: builds.append(args)):
            with self.assertRaisesRegex(ValueError, 'not built'):
                self.notes(self.stage('unbuilt'))
            self.assertEqual(builds, [])
            with self.assertRaisesRegex(ValueError, 'not built'):
                self.notes(self.stage('built'), build=True)  # Only that derivation, bounded like products.
            self.assertEqual(builds, [['nix', 'build', '--cores', '2', '--max-jobs', '2', '--no-link',
                                       good[0]['drv'] + '^out']])
        with patch.dict(os.environ, {'PATH': str(decoy) + os.pathsep + os.environ['PATH']}), \
                patch.object(local, 'select_git_cliff', return_value=self.tool):
            self.assertEqual(self.notes(self.stage('selected'))['generator_path'],
                             self.tool['output'] + '/bin/git-cliff')
        self.assertFalse(sentinel.exists())

    def test_fixed_argv_empty_environment_and_private_history_view(self):
        record = self.root / 'invocation.json'
        fake = self.fake('import json, os, pathlib\n'
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
        with patch.dict(os.environ, polluted), patch.object(local, 'select_git_cliff', return_value=fake):
            entry = self.notes(stage)
        seen = json.loads(record.read_bytes())
        view = Path(seen['argv'][3])
        self.assertEqual(seen['argv'], ['--config', str(stage / 'notes/cliff.toml'), '--repository', str(view),
                                        '--offline', '--no-exec', '--strip', 'all', '--',
                                        self.c[3] + '..' + self.c[7]])
        self.assertEqual(seen['env'], {'HOME': seen['cwd'], 'TZ': 'UTC', 'LC_ALL': 'C'})
        self.assertEqual(seen['refs'], {'refs/heads/release': self.c[7], 'refs/tags/v0.3.0': self.c[7],
                                        'refs/tags/v0.2.0': self.c[3]})
        self.assertEqual(seen['alternates'], str((self.repo.path / '.git/objects').resolve()) + '\n')
        self.assertEqual((view.name, view.parent.parent), ('history.git', stage / 'notes/tmp'))
        self.assertFalse(view.exists() or (stage / 'notes/tmp').exists())
        self.assertEqual(entry['generator'], 'git-cliff 9.9.9')
        self.assertIn('- Substitute output', (stage / 'notes/RELEASE-NOTES.md').read_text())

    def test_output_time_and_range_bounds_fail_closed(self):
        cases = (('oversized-file', 'sys.stdout.write("x" * (2 * 1024 * 1024))\n', 'failed or exceeded'),
                 ('oversized-notes', 'print("x" * 130000)\n', 'exceeds its release notes bound'),
                 ('control', 'print("- \\x1b[31mred")\n', 'control'),
                 ('failure', 'sys.exit(3)\n', 'failed or exceeded'))
        for name, body, error in cases:
            with self.subTest(name=name), patch.object(local, 'select_git_cliff', return_value=self.fake(body, name)):
                stage = self.stage('bound-' + name)
                with self.assertRaisesRegex(ValueError, error):
                    self.notes(stage)
                self.assertLessEqual((stage / 'notes/git-cliff.md').stat().st_size, local.NOTES['file'])
                self.assertFalse((stage / 'notes/RELEASE-NOTES.md').exists())
        pid = self.root / 'sleeper.pid'
        sleeper = self.fake('import os, time\nopen(' + repr(str(pid)) + ', "w").write(str(os.getpid()))\n'
                            'time.sleep(60)\n', 'sleeper')
        started = time.monotonic()
        with patch.object(local, 'select_git_cliff', return_value=sleeper), patch.dict(local.NOTES, {'seconds': 1}):
            with self.assertRaisesRegex(ValueError, 'wall-time'):
                self.notes(self.stage('bound-time'))
        self.assertLess(time.monotonic() - started, 20)
        with self.assertRaises(ProcessLookupError):  # Killed AND reaped before the refusal.
            os.kill(int(pid.read_text()), 0)
        with patch.dict(local.NOTES, {'commits': 3}), \
                patch.object(local, 'select_git_cliff', side_effect=AssertionError('range bound comes first')):
            with self.assertRaisesRegex(ValueError, 'exceeds its bound'):
                self.notes(self.stage('bound-range'))

    def test_config_whitelist_refuses_commands_remotes_outputs_and_dynamic_templates(self):
        good = self.config
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

    def test_real_git_cliff_needs_no_exec_and_offline_flags(self):
        executable = Path(self.tool['output']) / 'bin/git-cliff'
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

    def test_signed_public_ledger_binds_the_frozen_notes_reused_by_publish(self):
        body = self.notes.read_text()
        self.assertTrue(body.startswith('Synthetic temporary-key fixture.\n\n## Changes\n'), body)
        self.assertIn('Synthetic release feature', body)
        ledger = local.load(self.stage / 'ledger.json')
        self.assertEqual(ledger['format'], 2)
        self.assertEqual(ledger['release_notes']['notes_sha256'], local.sha(self.notes))
        self.assertEqual(local.load(self.stage / 'dist/release-ledger.json'), ledger)
        self.assertFalse((self.stage / 'notes/record.json').exists())
        with patch.object(local, 'git_cliff_version', side_effect=NEVER), \
                patch.object(local, 'run_git_cliff', side_effect=NEVER), \
                patch.object(local, 'stage_notes', side_effect=NEVER), \
                patch.object(local, 'ROOT', self.fixture.root):  # No current-tool cliff.toml at all.
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

    def forge(self, manifest):
        """Coherently replace the notes, their ledger hashes and the public ledger copy."""
        notes = self.stage / 'notes'
        forged = '## Changes\n\n- Forged entry nobody reviewed\n'
        curated = (self.stage / 'source/release/notes-0.1.0.md').read_text()
        for name, data in (('git-cliff.md', forged.encode()), ('RELEASE-NOTES.md', local.compose_notes(curated, forged))):
            (notes / name).chmod(0o644)
            (notes / name).write_bytes(data)
        path = self.stage / 'ledger.json'
        ledger = local.load(path)
        ledger['release_notes'].update(generated_sha256=local.sha(notes / 'git-cliff.md'),
                                       notes_sha256=local.sha(notes / 'RELEASE-NOTES.md'))
        path.write_text(json.dumps(ledger, indent=2, sort_keys=True) + '\n')
        shutil.copyfile(path, self.stage / 'dist/release-ledger.json')
        if manifest:
            update = self.stage / 'dist/update.json'
            raw = local.load(update)
            raw['notes'] = '\n'.join('ledger-sha256=' + local.sha(path) if line.startswith('ledger-sha256=') else line
                                     for line in raw['notes'].splitlines())
            update.write_text(json.dumps(raw, indent=1) + '\n')

    def test_consistent_notes_and_ledger_rewrite_is_refused_by_the_signed_binding(self):
        local.check_notes(self.stage, local.validate(self.stage))  # Positive control first.
        self.forge(manifest=False)
        with self.assertRaisesRegex(ValueError, 'signed source ledger binding absent'):
            local.validate(self.stage)
        calls, ok = self.fixture.publish()
        self.assertEqual((calls, ok), ([], False))
        self.forge(manifest=True)
        # Discriminating control: every local validator accepts this coherent forgery...
        self.assertIn('Forged entry', local.check_notes(self.stage, local.validate(self.stage))[1])
        remote = []
        real = local.run

        def no_remote(args, **kwargs):
            if str(args[0]) == 'fixture-gh':
                remote.append(args)
                raise AssertionError('remote call after a forged release body')
            return real(args, **kwargs)
        # ...and only the hybrid signature over update.json refuses it, before any remote call.
        with patch.object(local, 'run', side_effect=no_remote):
            with self.assertRaises(subprocess.CalledProcessError) as refused:
                local.publish(argparse.Namespace(stage=self.stage, signer=SIGNER, gh='fixture-gh'))
        self.assertEqual((refused.exception.cmd[:2], remote), ([SIGNER, 'verify'], []))


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
