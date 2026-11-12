#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Bind supplied assets to canonical helpers evaluated from the actual source."""
import argparse
import hashlib
import json
from pathlib import Path
import re

parser = argparse.ArgumentParser(description=__doc__)
for name in ('source', 'notices', 'expected-notices', 'desktop', 'expected-desktop', 'output'):
    parser.add_argument('--' + name, required=True, type=Path)
parser.add_argument('--appstream', type=Path)
parser.add_argument('--source-nar-hash', required=True)
parser.add_argument('--revision')
args = parser.parse_args()
if not re.fullmatch(r'sha256-[A-Za-z0-9+/]{43}=', args.source_nar_hash):
    parser.error('invalid source NarHash')
if args.revision is not None and not re.fullmatch('[0-9a-f]{40}', args.revision):
    parser.error('invalid source revision')


def tree(root):
    files = {}
    total = 0
    for path in sorted(root.rglob('*')):
        if path.is_symlink() or not (path.is_dir() or path.is_file()):
            raise ValueError('asset tree contains links or special files')
        if path.is_file():
            total += path.stat().st_size
            if len(files) >= 8192 or path.stat().st_size > 64 * 1024 * 1024 or total > 128 * 1024 * 1024:
                raise ValueError('asset tree exceeds bounds')
            with path.open('rb') as stream:
                files[str(path.relative_to(root))] = hashlib.file_digest(stream, 'sha256').hexdigest()
    if not files:
        raise ValueError('empty asset tree')
    return files


notices = tree(args.notices / 'share/licenses/nuschl')
expected = tree(args.expected_notices / 'share/licenses/nuschl')
if notices != expected:
    raise ValueError('notice payload does not match canonical inventory from this source')
if hashlib.sha256((args.source / 'Cargo.lock').read_bytes()).hexdigest() != notices['Cargo.lock']:
    raise ValueError('notice Cargo.lock differs from actual application source')
desktop = tree(args.desktop / 'share')
if desktop != tree(args.expected_desktop / 'share'):
    raise ValueError('desktop assets differ from canonical source helper')
appstream = None
if args.appstream:
    expected_xml = args.source / 'packaging/flatpak/org.nuschl.Nuschl.metainfo.xml'
    if args.appstream.stat().st_size > 1024 * 1024 or args.appstream.read_bytes() != expected_xml.read_bytes():
        raise ValueError('AppStream bytes do not match the actual source')
    appstream = hashlib.sha256(args.appstream.read_bytes()).hexdigest()
args.output.write_text(json.dumps({'schema': 1, 'source_nar_hash': args.source_nar_hash,
                                  'source_revision': args.revision,
                                  'canonical_source_helpers_compared': True,
                                  'native_notices': notices, 'desktop_assets': desktop,
                                  'appstream_sha256': appstream,
                                  'scope': 'Source/asset byte binding; not legal completeness or platform/runtime certification.'},
                                 indent=2, sort_keys=True) + '\n')
