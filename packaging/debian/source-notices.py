#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Inventory every redistributed registry source, not just linked runtime crates.

Presence/provenance checks do not certify legal completeness or license
compatibility. Missing grants remain explicit publication gates.
"""
import argparse
import hashlib
import json
from pathlib import Path
import tomllib

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--source', required=True, type=Path)
parser.add_argument('--vendor', required=True, type=Path)
parser.add_argument('--output', required=True, type=Path)
args = parser.parse_args()
supplements = {}
for provenance in sorted((args.source / 'release/licenses').glob('*/provenance.json')):
    for row in json.loads(provenance.read_text()).get('packages', []):
        if row.get('status') != 'resolved':
            continue
        supplements.setdefault((row['name'], row['version']), []).append((provenance, row))


def original(path, root):
    resolved = path.resolve(strict=True)
    if not resolved.is_relative_to(root.resolve()) or not resolved.is_file():
        raise ValueError('license file escapes its source')
    return {'local': str(path.relative_to(args.source)),
            'sha256': hashlib.sha256(path.read_bytes()).hexdigest()}


rows = []
for directory in sorted(args.vendor.iterdir()):
    manifest = directory / 'Cargo.toml'
    if not manifest.is_file():
        continue
    package = tomllib.loads(manifest.read_text())['package']
    identity = (package['name'], package['version'])
    vcs = directory / '.cargo_vcs_info.json'
    revision = json.loads(vcs.read_text()).get('git', {}).get('sha1') if vcs.is_file() else None
    paths = [p for p in directory.iterdir() if p.is_file() and p.name.lower().startswith(
        ('license', 'licence', 'copying', 'notice', 'copyright', 'authors'))]
    if package.get('license-file'):
        paths.append(directory / package['license-file'])
    files = [original(p, directory) for p in sorted(set(paths))]
    evidence = []
    for provenance, candidate in supplements.get(identity, []):
        if candidate.get('declared_license') != package.get('license') or revision and candidate.get('revision') != revision:
            continue
        for file in candidate['files']:
            path = provenance.parent / file['local']
            item = original(path, provenance.parent)
            if item['sha256'] != file['sha256']:
                raise ValueError('curated notice hash mismatch')
            evidence.append(item)
    rows.append({'name': identity[0], 'version': identity[1], 'declared_license': package.get('license'),
                 'published_revision': revision, 'original_files': files, 'curated_files': evidence,
                 'status': 'source-evidence-present' if files or evidence else 'unresolved-source-grant'})
report = {'schema': 1, 'scope': 'Every redistributed registry source, including build/dev and unselected targets.',
          'limitations': ['File presence and provenance are not a legal compatibility/completeness assurance.',
                          'Embedded assets and first-party patched sources require their existing provenance too.',
                          'Unresolved rows block publication; this source package is a local unsigned validation artifact.'],
          'packages': rows, 'unresolved': [row for row in rows if row['status'] == 'unresolved-source-grant']}
args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + '\n')
print(json.dumps({'registry_source_packages': len(rows), 'unresolved_source_grants': len(report['unresolved'])}))
