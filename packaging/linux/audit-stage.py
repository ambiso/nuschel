#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Bind a native staging tree to source, ELF ABI checks and real link receipts."""
import argparse
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--stage', type=Path, required=True)
    parser.add_argument('--project-root', type=Path, required=True)
    parser.add_argument('--packaging-root', type=Path, required=True, help='actual builder scripts/target pins, separately bound from application source')
    parser.add_argument('--revision')
    parser.add_argument('--source-nar-hash')
    parser.add_argument('--version', required=True)
    parser.add_argument('--readelf', required=True)
    parser.add_argument('--rust-version', required=True)
    args = parser.parse_args()
    if args.revision is not None and not re.fullmatch(r'[0-9a-f]{40}', args.revision):
        raise ValueError('invalid source revision')
    if args.source_nar_hash is not None and not re.fullmatch(r'sha256-[A-Za-z0-9+/]{43}=', args.source_nar_hash):
        raise ValueError('invalid source NarHash')
    if args.revision is None and args.source_nar_hash is None:
        raise ValueError('exact source revision or NarHash is required')
    root = args.project_root
    packaging = args.packaging_root
    spec = importlib.util.spec_from_file_location('deb', packaging/'packaging/debian/build-deb.py')
    deb = importlib.util.module_from_spec(spec); spec.loader.exec_module(deb)
    deb.ROOT = root
    programs = deb.validate(args.stage, 'amd64', (2, 41), args.readelf)
    receipts = args.stage/'usr/share/nuschl/link-receipts'
    for name, metadata in programs.items():
        receipt = json.loads((receipts/(name+'.json')).read_text())
        if receipt['sha256'] != metadata['sha256'] or receipt['nix_native_fallbacks'] != 0:
            raise ValueError('mismatched native link receipt: ' + name)
        # Retain exact imported symbol names, not merely the highest version.
        output = subprocess.check_output([args.readelf, '--dyn-syms', '--wide', str(args.stage/'usr/lib/nuschl'/name)], text=True)
        metadata['imports'] = sorted({line.split()[-2] if line.rstrip().endswith(')') else line.split()[-1]
                                      for line in output.splitlines() if ' UND ' in line})
        metadata['shared_link_inputs'] = receipt['shared_link_inputs']
    binding = args.stage / 'usr/share/nuschl/SOURCE-ASSETS.json'
    binding_hash = None
    if binding.exists():
        if binding.stat().st_size > 1024 * 1024:
            raise ValueError('oversized source asset binding')
        attestation = json.loads(binding.read_text())
        if attestation['source_revision'] != args.revision or attestation['source_nar_hash'] != args.source_nar_hash:
            raise ValueError('source asset identity mismatch')
        for section, directory in [('native_notices', args.stage/'usr/share/licenses/nuschl'), ('desktop_assets', args.stage/'usr/share')]:
            if len(attestation[section]) > 8192:
                raise ValueError('asset count exceeds bound')
            for name, expected in attestation[section].items():
                relative = Path(name)
                if relative.is_absolute() or '..' in relative.parts or not re.fullmatch('[0-9a-f]{64}', expected):
                    raise ValueError('invalid source asset path/hash')
                with (directory/relative).open('rb') as stream:
                    if hashlib.file_digest(stream, 'sha256').hexdigest() != expected:
                        raise ValueError('staged source asset mismatch')
        if attestation['appstream_sha256'] is not None:
            path = args.stage/'usr/share/metainfo/org.nuschl.Nuschl.metainfo.xml'
            if hashlib.sha256(path.read_bytes()).hexdigest() != attestation['appstream_sha256']:
                raise ValueError('staged AppStream mismatch')
        binding_hash = hashlib.sha256(binding.read_bytes()).hexdigest()
    pins = json.loads((packaging/'packaging/linux/debian/sysroot-lock.json').read_text())
    value = {'schema': 3, 'source_revision': args.revision, 'source_nar_hash': args.source_nar_hash, 'version': args.version,
             'rust_version': args.rust_version, 'programs': programs, 'source_asset_binding_sha256': binding_hash,
             'source_hashes': {str(path): hashlib.sha256((root/path).read_bytes()).hexdigest()
                               for path in map(Path, ['Cargo.lock','Cargo.toml','flake.lock'])},
             'packaging_hashes': {str(path): hashlib.sha256((packaging/path).read_bytes()).hexdigest()
                                  for path in map(Path, ['nix/portable-linux.nix','packaging/linux/default.nix','packaging/linux/version.nix','packaging/linux/bind-assets.py','packaging/linux/linker.py','packaging/linux/audit-stage.py','packaging/debian/build-deb.py','packaging/linux/pin-debian.py','packaging/linux/debian/sysroot-lock.json'])},
             'sysroot_index_sha256': pins['packages_sha256'], 'sysroot_release_sha256': pins['release_sha256'],
             'runtime_native_libraries': 'provided by target distribution; not bundled',
             'updates': 'package-manager-owned; existing Linux in-app updater disabled',
             'scope': 'native link/layout/notice validation; per-distro install/runtime proof remains separate'}
    directory = args.stage/'usr/share/nuschl'; directory.mkdir(parents=True, exist_ok=True)
    (directory/'build-info.json').write_text(json.dumps(value, sort_keys=True, indent=2)+'\n')


if __name__ == '__main__':
    main()
