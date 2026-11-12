#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

parser = argparse.ArgumentParser()
parser.add_argument('--dpkg', required=True)
parser.add_argument('--vercmp', required=True)
args = parser.parse_args()
with tempfile.TemporaryDirectory(prefix='distro-version-') as temporary:
    root = Path(temporary)
    environment = {'PATH': os.environ['PATH'], 'HOME': temporary, 'GIT_CONFIG_NOSYSTEM': '1',
                   'GIT_CONFIG_GLOBAL': '/dev/null', 'GIT_AUTHOR_NAME': 'Fixture',
                   'GIT_AUTHOR_EMAIL': 'fixture@example.invalid', 'GIT_COMMITTER_NAME': 'Fixture',
                   'GIT_COMMITTER_EMAIL': 'fixture@example.invalid'}
    def git(*arguments, epoch=1730000010):
        return subprocess.check_output(['git', '-C', temporary, *arguments],
                                       env={**environment, 'GIT_AUTHOR_DATE': f'@{epoch} +0000',
                                            'GIT_COMMITTER_DATE': f'@{epoch} +0000'},
                                       text=True, stderr=subprocess.PIPE).strip()
    git('init', '--quiet')
    (root / 'Cargo.toml').write_text('[workspace.package]\nversion="0.1.0"\n')
    git('add', 'Cargo.toml')
    git('commit', '--quiet', '-m', 'release fixture')
    first = git('rev-parse', 'HEAD')
    git('tag', 'v0.1.0')
    def version(revision, *extra):
        return subprocess.run([sys.executable, str(Path(__file__).with_name('version.py')),
                               '--repo', temporary, '--revision', revision, '--dpkg', args.dpkg,
                               '--vercmp', args.vercmp, *extra], env=environment, capture_output=True, text=True)
    release = version(first, '--mode', 'release')
    assert release.returncode == 0, release.stderr
    assert json.loads(release.stdout)['debian_version'] == '0.1.0-1'
    first_dev = json.loads(version(first, '--mode', 'development').stdout)
    (root / 'marker').write_text('next source')
    git('add', 'marker')
    git('commit', '--quiet', '-m', 'earlier timestamp, later descendant', epoch=1730000000)
    second = git('rev-parse', 'HEAD')
    later = version(second, '--mode', 'development', '--previous-revision', first,
                    '--previous-debian', first_dev['debian_version'], '--previous-arch', first_dev['arch_version'])
    assert later.returncode == 0, later.stderr
    second_dev = json.loads(later.stdout)
    assert second_dev['revision_count'] == 2 and second_dev['source_date_epoch'] < first_dev['source_date_epoch']
    assert version(second, '--mode', 'release').returncode != 0  # No relabeling an untagged build.
    assert version(first, '--mode', 'development', '--previous-revision', second).returncode != 0
    assert version(first, '--mode', 'development', '--previous-debian', second_dev['debian_version']).returncode != 0
    assert version(first, '--mode', 'development', '--previous-arch', second_dev['arch_version']).returncode != 0
    (root / 'Cargo.toml').write_text('[workspace.package]\nversion="0.1.1"\n')
    git('add', 'Cargo.toml')
    git('commit', '--quiet', '-m', 'next tagged release')
    third = git('rev-parse', 'HEAD')
    git('tag', 'v0.1.1')
    assert version(third, '--mode', 'release', '--previous-debian', second_dev['debian_version'],
                   '--previous-arch', second_dev['arch_version']).returncode == 0
print('8 version/tag/ancestry/actual dpkg+vercmp ordering checks passed')
