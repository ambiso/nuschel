#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Derive deterministic distro versions from an exact frozen source commit.

Official development updates must descend from the prior published source and
sort above its package version. SHA alone is an identity, not an ordering key.
"""
import argparse
from datetime import datetime, timezone
import json
from pathlib import Path
import re
import subprocess
import tomllib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--repo', required=True, type=Path)
parser.add_argument('--revision', required=True)
parser.add_argument('--mode', required=True, choices=('release', 'development'))
parser.add_argument('--previous-revision')
parser.add_argument('--previous-debian')
parser.add_argument('--previous-arch')
parser.add_argument('--dpkg', default='dpkg')
parser.add_argument('--vercmp', default='vercmp')
args = parser.parse_args()
for revision in (args.revision, args.previous_revision):
    if revision is not None and not re.fullmatch('[0-9a-f]{40}', revision):
        parser.error('expected exact 40-hex source revision')


def git(*arguments):
    return subprocess.check_output(['git', '--no-replace-objects', '-C', str(args.repo), *arguments], text=True, timeout=10).strip()


if git('rev-parse', '--is-shallow-repository') != 'false':
    parser.error('development revision counts require complete source history')
source = git('rev-parse', args.revision + '^{commit}')
assert source == args.revision
base = tomllib.loads(git('show', source + ':Cargo.toml'))['workspace']['package']['version']
if not re.fullmatch('[0-9]+\.[0-9]+\.[0-9]+', base):
    parser.error('only stable three-part workspace release versions are currently supported')
count = int(git('rev-list', '--count', source))
epoch = int(git('show', '-s', '--format=%ct', source))
stamp = datetime.fromtimestamp(epoch, timezone.utc).strftime('%Y%m%d%H%M%S')
if args.mode == 'release':
    if git('rev-parse', 'refs/tags/v' + base + '^{commit}') != source:
        parser.error('release mode requires the exact workspace-version tag target')
    debian, arch = base, base
else:
    debian = f'{base}+git{count}.{stamp}.g{source[:12]}'
    arch = f'{base}.r{count}.{stamp}.g{source[:12]}'
if args.previous_revision:
    subprocess.run(['git', '--no-replace-objects', '-C', str(args.repo), 'merge-base', '--is-ancestor', args.previous_revision, source],
                   check=True, timeout=10)
if args.previous_debian:
    subprocess.run([args.dpkg, '--compare-versions', debian + '-1', 'gt', args.previous_debian], check=True, timeout=5)
if args.previous_arch:
    order = subprocess.check_output([args.vercmp, arch + '-1', args.previous_arch], text=True, timeout=5).strip()
    if int(order) <= 0:
        parser.error('Arch update does not sort above the previously published version')
print(json.dumps({'source_revision': source, 'source_date_epoch': epoch, 'revision_count': count,
                  'mode': args.mode, 'debian_upstream': debian, 'debian_version': debian + '-1',
                  'arch_pkgver': arch, 'arch_version': arch + '-1'}, sort_keys=True))
