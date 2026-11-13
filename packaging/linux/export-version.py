#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Create an external public schema1 sidecar for a verified frozen source export.

Git/Nix are used only in this operator-side preparation tool, never by the pure
version function or artifact derivations. The consumer needs no Git repository.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import posixpath
import re
import resource
import stat
import subprocess
import sys

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--repo', required=True, type=Path)
parser.add_argument('--revision', required=True)
parser.add_argument('--exported-source', required=True, type=Path, help='immutable sourceInfo.outPath in the Nix store')
parser.add_argument('--output', required=True, type=Path, help='new file OUTSIDE the source tree')
parser.add_argument('--mode', required=True, choices=('release', 'development'))
parser.add_argument('--package-revision', type=int, default=1)
parser.add_argument('--source-nar-hash', help='optional expected sourceInfo.narHash, independently checked')
parser.add_argument('--previous-revision')
parser.add_argument('--previous-debian')
parser.add_argument('--previous-arch')
parser.add_argument('--dpkg', default='dpkg')
parser.add_argument('--vercmp', default='vercmp')
parser.add_argument('--nix', default='nix')
args = parser.parse_args()
source = args.exported_source.resolve(strict=True)
if not source.is_relative_to('/nix/store') or source.stat().st_mode & 0o222:
    parser.error('use an immutable Nix source export, not a mutable checkout')
if args.output.resolve().is_relative_to(source):
    parser.error('metadata must be external to its source tree')
if args.output.exists():
    parser.error('refusing to replace a frozen metadata checkpoint')
if not re.fullmatch('[0-9a-f]{40}', args.revision):
    parser.error('expected exact source revision')
command = [sys.executable, str(Path(__file__).with_name('version.py')), '--repo', str(args.repo),
           '--revision', args.revision, '--mode', args.mode, '--package-revision', str(args.package_revision),
           '--dpkg', args.dpkg, '--vercmp', args.vercmp]
for name in ('previous_revision', 'previous_debian', 'previous_arch'):
    if (value := getattr(args, name)) is not None:
        command += ['--' + name.replace('_', '-'), value]
versions = json.loads(subprocess.check_output(command, timeout=30))
# Verify complete tracked membership before hashing any unexpected file content.
listing = subprocess.check_output(['git', '--no-replace-objects', '-C', str(args.repo),
                                   'ls-tree', '-rlz', args.revision], timeout=15)
if len(listing) > 16 * 1024 * 1024:
    raise ValueError('source tree exceeds manifest bound')
expected = {}; directories = {''}; size = 0
for item in listing.split(b'\0'):
    if not item:
        continue
    header, name = item.split(b'\t', 1)
    mode, kind, digest, length = header.split()
    if kind != b'blob' or mode not in (b'100644', b'100755', b'120000'):
        raise ValueError('explicit submodule/materialization review required')
    name = os.fsdecode(name)
    path = PurePosixPath(name)
    if path.is_absolute() or '..' in path.parts:
        raise ValueError('invalid tracked source path')
    length = int(length); size += length
    if length > 64 * 1024 * 1024 or size > 512 * 1024 * 1024 or len(expected) >= 100000:
        raise ValueError('source export exceeds bounds')
    expected[name] = (mode, digest.decode(), length)
    directories.update('' if str(parent) == '.' else str(parent) for parent in path.parents)
observed = set()
for parent, dirs, files in os.walk(source, followlinks=False):
    relative = Path(parent).relative_to(source)
    for name in list(dirs):
        path = Path(parent) / name
        key = str(relative / name)
        if path.is_symlink():
            dirs.remove(name); files.append(name)
        elif key not in directories:
            raise ValueError('source export contains untracked directories')
    for name in files:
        path = Path(parent) / name; key = str(relative / name)
        if key not in expected:
            raise ValueError('source export contains untracked files')
        mode, digest, length = expected[key]
        status = path.lstat()
        if mode == b'120000':
            if not stat.S_ISLNK(status.st_mode):
                raise ValueError('tracked link type mismatch')
            data = os.fsencode(os.readlink(path))
            target = posixpath.normpath(posixpath.join(str(PurePosixPath(key).parent), os.fsdecode(data)))
            if data.startswith(b'/') or target == '..' or target.startswith('../'):
                raise ValueError('escaping tracked source link')
            actual = hashlib.sha1(b'blob ' + str(len(data)).encode() + b'\0' + data).hexdigest()
        else:
            if not stat.S_ISREG(status.st_mode) or status.st_size != length or status.st_mode & 0o222:
                raise ValueError('tracked immutable file type/size mismatch')
            if bool(status.st_mode & 0o111) != (mode == b'100755'):
                raise ValueError('tracked executable mode mismatch')
            h = hashlib.sha1(b'blob ' + str(length).encode() + b'\0')
            with path.open('rb') as stream:
                actual = hashlib.file_digest(stream, lambda: h).hexdigest()
        if actual != digest:
            raise ValueError('export bytes do not match the exact Git tree')
        observed.add(key)
if observed != set(expected):
    raise ValueError('source export is missing tracked files')
environment = {**os.environ, 'NIX_USER_CONF_FILES': '/dev/null', 'NIX_REMOTE': 'dummy://'}
resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
actual_nar = subprocess.check_output([args.nix, '--extra-experimental-features', 'nix-command',
                                    '--store', 'dummy://', 'hash', 'path', '--type', 'sha256', '--sri', str(source)],
                                   env=environment, text=True, timeout=60).strip()
if not re.fullmatch(r'sha256-[A-Za-z0-9+/]{43}=', actual_nar) or (args.source_nar_hash is not None and args.source_nar_hash != actual_nar):
    raise ValueError('export/sourceInfo NarHash mismatch')
metadata = {'schema': 1, 'kind': args.mode, 'base_version': versions['base_version'],
            'source_nar_hash': actual_nar, 'source_revision': args.revision,
            'sequence': versions['revision_count'], 'commit_epoch': versions['source_date_epoch'],
            'commit_stamp': versions['commit_stamp'], 'tag': 'v' + versions['base_version'] if args.mode == 'release' else None,
            'package_revision': args.package_revision}
with args.output.open('x') as output:
    output.write(json.dumps(metadata, indent=2, sort_keys=True) + '\n')
print(json.dumps({'metadata': str(args.output), 'source_files_verified': len(observed),
                  'source_bytes_verified': size, 'source_nar_hash': actual_nar}))
