#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Prepare/verify explicit native sysroot pins from a signed Debian release.

Metadata preparation only: no downloads, package installation or maintainer
scripts. The resulting fixed package set is extracted by Nix, not installed on
this host. This is not a general APT dependency solver.
"""
import argparse
from collections import deque
import hashlib
import json
import lzma
from pathlib import Path
import re
import subprocess
import tempfile

SEEDS = ('libc6-dev libgcc-14-dev libfontconfig-dev libfreetype-dev '
         'libxkbcommon-dev libxkbcommon-x11-dev libwayland-dev libxcb1-dev '
         'libx11-dev libxcursor-dev libxi-dev libxrandr-dev libvulkan-dev '
         'libgl-dev libegl-dev libasound2-dev libpipewire-0.3-dev libpulse-dev').split()
SIGNERS = {'04B54C3CDCA79751B16BC6B5225629DF75B188BD',
           '41587F7DB8C774BCCF131416762F67A0B2C39DE4'}


def paragraphs(text):
    for paragraph in text.split('\n\n'):
        fields = {}; key = None
        for line in paragraph.splitlines():
            if line.startswith(' ') and key:
                fields[key] += '\n' + line[1:]
            elif ':' in line and not line[0].isspace():
                key, value = line.split(':', 1); fields[key] = value.lstrip()
        if fields:
            yield fields


def prepare(release, signature, packages, keyring, gpgv):
    if release.stat().st_size > 1024*1024 or signature.stat().st_size > 65536 or packages.stat().st_size > 16*1024*1024:
        raise ValueError('metadata exceeds input bounds')
    with tempfile.TemporaryDirectory(prefix='debian-verify-', dir=release.parent) as home:
        check = subprocess.run([gpgv, '--homedir', home, '--keyring', str(keyring.resolve()), '--status-fd', '1',
                                str(signature.resolve()), str(release.resolve())], capture_output=True, text=True, timeout=20)
    if check.returncode:
        raise ValueError('archive signature validation failed: ' + check.stderr)
    seen = {line.split()[-1] for line in check.stdout.splitlines() if line.startswith('[GNUPG:] VALIDSIG ')}
    if not SIGNERS <= seen:
        raise ValueError('expected Debian 13 archive and stable signatures')
    metadata = next(paragraphs(release.read_text()))
    if metadata.get('Codename') != 'trixie' or 'amd64' not in metadata.get('Architectures', '').split():
        raise ValueError('wrong release or architecture')
    entries = [line.split() for line in metadata.get('SHA256', '').splitlines()]
    expected = next((row for row in entries if len(row) == 3 and row[2] == 'main/binary-amd64/Packages.xz'), None)
    digest = hashlib.sha256(packages.read_bytes()).hexdigest()
    if not expected or digest != expected[0] or packages.stat().st_size != int(expected[1]):
        raise ValueError('package index differs from signed release')
    with lzma.open(packages, 'rt') as stream:
        text = stream.read(150*1024*1024+1)
    if len(text) > 150*1024*1024:
        raise ValueError('expanded package index exceeds bound')
    table = {}; providers = {}
    for row in paragraphs(text):
        if 'Package' not in row: continue
        table[row['Package']] = row
        for name in row.get('Provides', '').split(', '):
            providers.setdefault(name.split(' ')[0], []).append(row['Package'])
    pending = deque(SEEDS); chosen = {}; selection = []
    while pending:
        name = pending.popleft()
        if name in chosen: continue
        row = table[name]; chosen[name] = row
        for field in ('Pre-Depends', 'Depends'):
            for dependency in row.get(field, '').split(', '):
                if not dependency: continue
                options = [re.split(r'[ (:\[<]', option.strip())[0] for option in dependency.split(' | ')]
                selected = next((option for option in options if option in table), None)
                if not selected:
                    selected = next((sorted(providers[option])[0] for option in options if option in providers), None)
                if not selected: raise ValueError(f'unresolved {name}: {dependency}')
                selection.append({'from': name, 'clause': dependency, 'selected': selected})
                pending.append(selected)
    result = []
    for name, row in sorted(chosen.items()):
        filename = row['Filename']
        if not filename.startswith('pool/') or '..' in Path(filename).parts or not re.fullmatch(r'[A-Za-z0-9_/+.:~%\-]+\.deb', filename):
            raise ValueError('unsafe archive filename')
        result.append({'name': name, 'version': row['Version'], 'source': row.get('Source', name),
                       'architecture': row['Architecture'], 'filename': filename, 'sha256': row['SHA256'],
                       'size': int(row['Size']), 'depends': row.get('Depends', '')})
    return {'schema': 1, 'distribution': 'debian', 'suite': 'trixie', 'architecture': 'amd64',
            'release_version': metadata['Version'], 'release_date': metadata['Date'],
            'release_sha256': hashlib.sha256(release.read_bytes()).hexdigest(),
            'signature_sha256': hashlib.sha256(signature.read_bytes()).hexdigest(),
            'valid_signers': sorted(seen), 'packages_sha256': digest,
            'packages_url': 'https://deb.debian.org/debian/dists/trixie/main/binary-amd64/by-hash/SHA256/' + digest,
            'seeds': SEEDS, 'packages': result, 'selection': selection,
            'download_bytes': sum(row['size'] for row in result),
            'scope': 'deterministic extraction/link sysroot; first concrete alternative/provider selected from this one signed release; not general APT resolution or install proof'}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('release', 'signature', 'packages', 'keyring', 'output'):
        parser.add_argument('--' + name, type=Path, required=True)
    parser.add_argument('--gpgv', default='gpgv')
    args = parser.parse_args()
    value = prepare(args.release, args.signature, args.packages, args.keyring, args.gpgv)
    args.output.write_text(json.dumps(value, indent=2) + '\n')
    print(json.dumps({'packages': len(value['packages']), 'download_bytes': value['download_bytes'], 'signers': value['valid_signers']}))


if __name__ == '__main__':
    main()
