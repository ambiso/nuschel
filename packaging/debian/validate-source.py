#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Check local unsigned source artifacts, checksums and bounded archive members.

This never authenticates a signature, compiles source, or closes legal/runtime
gates. Extract with dpkg-source and check offline Cargo metadata separately.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import posixpath
import tarfile

MAX_ARCHIVE = 768 * 1024 * 1024
MAX_EXPANDED = 4 * 1024**3
MAX_MEMBERS = 500_000
MAX_FILE = 64 * 1024 * 1024


def fields(path):
    if path.stat().st_size > 256 * 1024:
        raise ValueError('oversized source control metadata')
    rows = {}; key = None
    for line in path.read_text().splitlines():
        if line.startswith(' ') and key:
            rows[key] += '\n' + line[1:]
        elif ':' in line:
            key, value = line.split(':', 1)
            rows[key] = value.strip()
    return rows


def checksums(root, metadata, expected):
    rows = fields(metadata)
    observed = set()
    for line in rows['Checksums-Sha256'].splitlines():
        if not line.strip():
            continue
        digest, size, name = line.split()
        if name not in expected or name in observed:
            raise ValueError('unexpected/duplicate source checksum member')
        observed.add(name)
        path = root / name
        if path.is_symlink() or not path.is_file() or path.stat().st_size != int(size):
            raise ValueError('mismatched source artifact size/type')
        if int(size) > MAX_ARCHIVE:
            raise ValueError('source archive exceeds compressed bound')
        with path.open('rb') as stream:
            actual = hashlib.file_digest(stream, 'sha256').hexdigest()
        if digest != actual:
            raise ValueError('source checksum mismatch')
    if observed != expected:
        raise ValueError('missing source checksum member')
    return rows


def archive(path, prefix):
    count = total = 0
    with tarfile.open(path, 'r|*') as stream:
        for member in stream:
            count += 1
            total += member.size
            if member.size < 0 or count > MAX_MEMBERS or total > MAX_EXPANDED or member.size > MAX_FILE:
                raise ValueError('expanded source archive exceeds bounds')
            name = PurePosixPath(member.name)
            if name.is_absolute() or '..' in name.parts or not name.parts or name.parts[0] != prefix:
                raise ValueError('unsafe source archive path')
            if len(name.parts) > 1 and name.parts[1] in {'.git', '.jj', '.cache', '.direnv', 'target', 'node_modules', 'profiles', '__pycache__'}:
                raise ValueError('source archive contains repository/private/build state')
            if (member.isfile() or member.isdir()) and member.mode & 0o022:
                raise ValueError('group/world-writable source archive member')
            if member.mode & 0o6000 or not (member.isfile() or member.isdir() or member.issym() or member.islnk()):
                raise ValueError('privileged/special source archive member')
            if member.issym() or member.islnk():
                target = PurePosixPath(member.linkname)
                if target.is_absolute():
                    raise ValueError('absolute source archive link')
                relative = posixpath.normpath(posixpath.join(str(name.parent), str(target))) if member.issym() else posixpath.normpath(str(target))
                if relative != prefix and not relative.startswith(prefix + '/'):
                    raise ValueError('source archive link escapes root')
            if prefix == 'debian' and member.isfile():
                with stream.extractfile(member) as content:
                    if content.read(4) == b'\x7fELF':
                        raise ValueError('prebuilt ELF in Debian source patch archive')
    return {'members': count, 'expanded_bytes': total}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', required=True, type=Path)
    parser.add_argument('--version', required=True)
    parser.add_argument('--debian-revision', required=True)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    upstream = 'nuschl_' + args.version
    full = upstream + '-' + args.debian_revision
    orig, delta, dsc, changes = upstream + '.orig.tar.xz', full + '.debian.tar.xz', full + '.dsc', full + '_source.changes'
    required = {orig, delta, dsc, changes, 'SOURCE-NOTICES.json', 'SOURCE-PROVENANCE.json', 'source-notice-summary.json', 'STATUS', 'SHA256SUMS'}
    found = {p.name for p in args.root.iterdir()}
    if not required <= found or found - required - {'validation.json'}:
        raise ValueError('unexpected/missing source outputs: ' + str(found ^ required))
    if any(path.is_symlink() or not path.is_file() for path in args.root.iterdir()):
        raise ValueError('source output must contain only regular files')
    source = checksums(args.root, args.root / dsc, {orig, delta})
    upload = checksums(args.root, args.root / changes, {orig, delta, dsc})
    if source['Source'] != 'nuschl' or source['Version'] != args.version + '-' + args.debian_revision or upload['Version'] != source['Version']:
        raise ValueError('incorrect source identity')
    reports = {orig: archive(args.root / orig, 'nuschl-' + args.version), delta: archive(args.root / delta, 'debian')}
    if (args.root / 'SOURCE-NOTICES.json').stat().st_size > 16 * 1024 * 1024:
        raise ValueError('oversized source notice inventory')
    notices = json.loads((args.root / 'SOURCE-NOTICES.json').read_text())
    args.output.write_text(json.dumps({'unsigned_source_checksums': True, 'archives': reports,
                                      'registry_source_packages': len(notices['packages']),
                                      'unresolved_source_grants': len(notices['unresolved']),
                                      'publication_ready': False,
                                      'pending': ['dpkg-source extraction/offline metadata', 'clean distro source rebuild',
                                                  'source license/asset review', 'actual distro install/runtime proof']}, indent=2) + '\n')


if __name__ == '__main__':
    main()
