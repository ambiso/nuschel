#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Prepare source metadata, never binaries, signatures, downloads or uploads."""
import argparse
import email.utils
import json
from pathlib import Path
import re

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--phase', required=True, choices=('upstream', 'debian'))
parser.add_argument('--revision')
parser.add_argument('--source-nar-hash')
parser.add_argument('--version', required=True)
parser.add_argument('--debian-revision', required=True)
parser.add_argument('--distribution', required=True)
parser.add_argument('--maintainer', required=True)
parser.add_argument('--epoch', required=True, type=int)
args = parser.parse_args()
if (args.revision is not None and not re.fullmatch('[0-9a-f]{40}', args.revision)) or args.epoch < 0:
    parser.error('invalid source identity/epoch')
if args.source_nar_hash is not None and not re.fullmatch(r'sha256-[A-Za-z0-9+/]{43}=', args.source_nar_hash):
    parser.error('invalid source NarHash')
if args.revision is None and args.source_nar_hash is None:
    parser.error('exact revision or source NarHash is required')
if not re.fullmatch('[0-9][A-Za-z0-9.+~]*', args.version) or not re.fullmatch('[0-9A-Za-z.+~]+', args.debian_revision):
    parser.error('invalid Debian version')
if not re.fullmatch('[a-z][a-z0-9-]*', args.distribution) or any(c in args.maintainer for c in '\n\r\0'):
    parser.error('invalid changelog metadata')
if args.phase == 'upstream':
    config = Path('.cargo/config.toml')
    if config.exists():
        raise ValueError('review existing Cargo configuration before replacing registry sources')
    config.write_text('[source.crates-io]\nreplace-with="offline-source"\n'
                      '[source.offline-source]\ndirectory=".cargo/registry-source"\n[net]\noffline=true\n')
    Path('SOURCE-PROVENANCE.json').write_text(json.dumps({
        'revision': args.revision, 'source_nar_hash': args.source_nar_hash,
        'scope': 'Genuine source build; external distro/Launchpad binaries have separate provenance from Nix final-leaf relinks.',
        'publication_gates': ['Review all SOURCE-NOTICES unresolved rows/embedded assets.',
                              'Supply/test Rust >=1.95 build dependencies for the chosen suite.',
                              'Perform a clean target source rebuild and sandbox/runtime tests.'],
    }, indent=2) + '\n')
else:
    # Explicit original icon/source-archive inputs to the quilt patch tar.
    # No compiled application or helper belongs in this list.
    binaries = []
    for path in Path('debian/prepared-desktop/share/icons/hicolor').glob('*/apps/org.nuschl.Nuschl.png'):
        if path.is_symlink() or path.parts[-3] not in {'16x16', '24x24', '32x32', '48x48', '64x64', '128x128', '256x256', '512x512'}:
            raise ValueError('unexpected prepared icon')
        binaries.append(str(path))
    for path in Path('debian/prepared-notices').glob('*-source.tar'):
        if path.is_symlink() or not path.is_file():
            raise ValueError('unexpected corresponding-source archive')
        binaries.append(str(path))
    Path('debian/source/include-binaries').write_text('\n'.join(sorted(binaries)) + '\n')
    control = Path('debian/control')
    control.write_text(control.read_text().replace('@MAINTAINER@', args.maintainer))
    date = email.utils.formatdate(args.epoch, usegmt=False).replace('-0000', '+0000')
    Path('debian/changelog').write_text(
        f'nuschl ({args.version}-{args.debian_revision}) {args.distribution}; urgency=medium\n\n'
        '  * Local unsigned offline source packaging validation.\n\n'
        f' -- {args.maintainer}  {date}\n')
