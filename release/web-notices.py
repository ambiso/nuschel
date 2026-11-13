#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Collect emitted npm/runtime text and exact generated WASM build identities.
No network, parsing of formula/message input, device access or key material.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath


def sha(data):
    return hashlib.sha256(data).hexdigest()


def read(path, limit=4*1024**2):
    if path.is_symlink() or not path.is_file() or not 0 < path.stat().st_size <= limit:
        raise ValueError('missing/linked/oversized web notice input')
    return path.read_bytes()


def relative(value):
    path = PurePosixPath(value)
    if not value or path.is_absolute() or '\\' in value or any(part in ('', '.', '..') for part in value.split('/')):
        raise ValueError('unsafe web notice path')
    return path


def collect_npm(app, lock):
    """Validate emitted owners against the source lock and retain actual text."""
    inventory = read(app/'licenses/npm/BUNDLED-NPM.json')
    report = json.loads(inventory)
    if report.get('format') != 1 or not 1 <= len(report.get('packages', [])) <= 128:
        raise ValueError('missing actual emitted npm owner evidence')
    locked = json.loads(read(lock))['packages']
    payloads, owners = [], set()
    for package in report['packages']:
        identity = (package['name'], package['version'])
        if identity in owners or not package.get('modules') or not package.get('phases'):
            raise ValueError('duplicate/empty npm owner evidence')
        owners.add(identity)
        matches = [item for name,item in locked.items() if
                   (name == 'node_modules/'+identity[0] or name.endswith('/node_modules/'+identity[0])) and
                   item.get('version') == identity[1] and
                   item.get('resolved') == package['source']['url'] and item.get('integrity') == package['source']['integrity']]
        if not matches or not package['files']:
            raise ValueError('npm owner differs from pinned source lock or lacks real notice text')
        for item in package['files']:
            name = relative(item['path'])
            if not str(name).startswith('licenses/npm/'+identity[0]+'/'+identity[1]+'/'):
                raise ValueError('foreign npm notice path')
            raw = read(app/name)
            raw.decode('utf8')
            if sha(raw) != item['sha256']:
                raise ValueError('actual emitted npm notice bytes differ')
            payloads.append((str(name),raw))
    if not any(name == 'svelte' for name,_ in owners) or not any(name == 'vite' for name,_ in owners):
        raise ValueError('real application must account for runtime Svelte and generated Vite helpers')
    return inventory, report, payloads, owners


def write_notices(output, inventory, lock, payloads):
    output.mkdir(parents=True)
    with (output/'NOTICES.txt').open('xb') as target:
        target.write(b'Actual bundled web runtime notices\nComplete upstream notice files are preserved; they can mention more than the rendered helper subset.\n')
        for name, raw in payloads:
            target.write(('\n===== '+name+' =====\n').encode())
            target.write(raw)
            target.write(b'\n')
        if target.tell() > 4 * 1024**2:
            raise ValueError('web notice aggregate exceeds 4MiB')
    (output/'BUNDLED-NPM.json').write_bytes(inventory)
    (output/'PACKAGE-LOCK.json').write_bytes(read(lock))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--app', type=Path, required=True)
    parser.add_argument('--lock', type=Path, required=True)
    parser.add_argument('--profile', choices=['main', 'musicbot'], default='main')
    parser.add_argument('--own-license', type=Path)
    parser.add_argument('--cargo-lock', type=Path)
    parser.add_argument('--temml-source', type=Path)
    parser.add_argument('--sdk', type=Path)
    parser.add_argument('--audio', type=Path)
    parser.add_argument('--content', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    inventory, report, payloads, owners = collect_npm(args.app, args.lock)
    if args.profile == 'musicbot':
        if args.own_license is None or any(value is not None for value in
                [args.cargo_lock, args.temml_source, args.sdk, args.audio, args.content]):
            raise ValueError('musicbot notices require own text and no unrelated WASM/math inputs')
        if report.get('generated_wasm_cargo_roots') != [] or report.get('packaged_wasm_build_roots') != []:
            raise ValueError('musicbot inventory claims unshipped WASM roots')
        own = read(args.own_license)
        own.decode('utf8')
        payloads.insert(0, ('EUPL-1.2', own))
        write_notices(args.output, inventory, args.lock, payloads)
        (args.output/'EUPL-1.2').write_bytes(own)
        return
    if any(value is None for value in [args.cargo_lock, args.temml_source, args.sdk, args.audio, args.content]):
        raise ValueError('main web notices require actual WASM and Temml source inputs')
    if not any(name == 'temml' for name,_ in owners):
        raise ValueError('real math worker Temml owner is absent')
    # Preserve the already reviewed exact Temml/KaTeX/React ancestry notices.
    ancestry = json.loads(read(args.temml_source/'temml-provenance.json'))
    if (ancestry.get('package'), ancestry.get('version')) not in owners or ancestry.get('package') != 'temml':
        raise ValueError('Temml ancestry record does not match emitted package version')
    if len(ancestry.get('files', [])) != 4:
        raise ValueError('missing reviewed Temml ancestry notice set')
    for entry in ancestry['files']:
        if sha(read(args.temml_source/relative(entry['file']))) != entry['sha256']:
            raise ValueError('reviewed Temml ancestry source hash mismatch')
    for path in sorted(args.temml_source.rglob('*')):
        if not path.is_file() or not path.name.startswith('temml-'): continue
        name = path.relative_to(args.temml_source).as_posix()
        source = read(path)
        actual = read(args.app/'licenses'/name)
        if source != actual:
            raise ValueError('existing Temml ancestry notice was lost or changed')
        payloads.append(('licenses/'+name,actual))
    write_notices(args.output, inventory, args.lock, payloads)
    lock_bytes = read(args.cargo_lock)
    wasm = []
    emitted = set(report.get('generated_wasm_cargo_roots', []))
    for root,module,directory in [('nuschl-client-wasm','nuschl_client_wasm',args.sdk),
                                  ('nuschl-audio-wasm','nuschl_audio_wasm',args.audio),
                                  ('nuschl-content-wasm','nuschl_content_wasm',args.content)]:
        raw = read(directory/(module+'_bg.wasm'),32*1024**2)
        if not raw.startswith(b'\0asm'): raise ValueError('not the expected generated wasm artifact')
        bindings = read(directory/(module+'.js'))
        matches = [path.name for path in (args.app/'assets').glob('*.wasm') if sha(read(path,32*1024**2)) == sha(raw)]
        if root in emitted and not matches:
            raise ValueError('rendered WASM/glue has no matching byte-identical emitted WASM asset')
        wasm.append({'cargo_root':root,'module':module,'wasm_sha256':sha(raw),'prebundled_bindings_sha256':sha(bindings),
                     'cargo_lock_sha256':sha(lock_bytes),'observed_in_rendered_chunks':root in emitted,
                     'emitted_wasm_files':matches})
    if not emitted <= {item['cargo_root'] for item in wasm}:
        raise ValueError('unowned generated WASM module')
    (args.output/'WASM-PROVENANCE.json').write_text(json.dumps({'format':1,'artifacts':wasm,
        'limits':['Built artifact identities, not a statement that every exported codec has an active application consumer.',
                  'Unreferenced build outputs can be tree-shaken; exact rendered evidence is explicit.',
                  'Corresponding Rust terms are in the source-scoped Cargo notice inventory.']},indent=2)+'\n')


if __name__ == '__main__':
    main()
