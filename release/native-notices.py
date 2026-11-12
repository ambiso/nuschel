#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Retain actual pinned Rust/Windows-runtime notices, not generic SPDX text.
Only selected runtime license files are read from source archives; build tools,
profiling libraries and unrelated MinGW utility libraries are excluded.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import subprocess
import tarfile


def digest(data):
    return hashlib.sha256(data).hexdigest()


def write_component(output, name, version, source, files):
    target = output / name
    target.mkdir(parents=True)
    records = []
    with (target / 'NOTICES.txt').open('xb') as notices:
        notices.write((name + ' ' + version + '\nSource: ' + source + '\n').encode())
        for relative, data in files:
            if not data or len(data) > 4 * 1024**2 or b'\0' in data:
                raise ValueError('empty/binary/oversized runtime notice')
            data.decode('utf8')
            notices.write(('\n===== ' + relative + ' =====\n').encode())
            notices.write(data)
            notices.write(b'\n')
            records.append({'source_path': relative, 'bytes': len(data), 'sha256': digest(data)})
    (target / 'PROVENANCE.json').write_text(json.dumps({'component': name, 'version': version, 'source': source,
                                                      'files': records}, sort_keys=True, indent=2)+'\n')


def selected_archive(path, selected):
    found = {}
    with tarfile.open(path, 'r:*') as archive:
        for member in archive:
            parts = PurePosixPath(member.name).parts
            if len(parts) < 2:
                continue
            relative = '/'.join(parts[1:])
            if relative in selected:
                if not member.isfile() or member.size > 4 * 1024**2 or relative in found:
                    raise ValueError('unsafe runtime notice member')
                found[relative] = archive.extractfile(member).read()
    if set(found) != set(selected):
        raise ValueError('required pinned runtime notice missing')
    return sorted(found.items())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--rustc', type=Path, required=True)
    parser.add_argument('--rust-docs', type=Path, required=True)
    parser.add_argument('--gcc-source', type=Path, required=True)
    parser.add_argument('--mingw-source', type=Path, required=True)
    parser.add_argument('--gcc-version', required=True)
    parser.add_argument('--mingw-version', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    raw = subprocess.check_output([args.rustc, '-vV'], timeout=10, text=True)
    fields = dict(line.split(': ', 1) for line in raw.splitlines() if ': ' in line)
    revision = fields['commit-hash']
    if len(revision) != 40:
        raise ValueError('missing exact Rust compiler revision')
    rust = [('COPYRIGHT-library.html', (args.rust_docs/'COPYRIGHT-library.html').read_bytes())]
    rust += [('licenses/'+path.name, path.read_bytes()) for path in sorted((args.rust_docs/'licenses').iterdir()) if path.is_file()]
    if len(rust) < 5:
        raise ValueError('missing standard-library license set')
    write_component(args.output, 'rust-standard-library', fields['release'],
                    'https://github.com/rust-lang/rust/tree/'+revision+' (pinned toolchain COPYRIGHT-library.html and accompanying complete license set)', rust)
    (args.output/'rust-standard-library/RUSTC.txt').write_text(raw)
    gcc = selected_archive(args.gcc_source, {'COPYING.RUNTIME', 'COPYING3'})
    write_component(args.output, 'gcc-runtime', args.gcc_version,
                    'https://ftp.gnu.org/gnu/gcc/gcc-'+args.gcc_version+'/gcc-'+args.gcc_version+'.tar.xz', gcc)
    mingw = selected_archive(args.mingw_source, {'COPYING.MinGW-w64-runtime/COPYING.MinGW-w64-runtime.txt',
                                                'mingw-w64-libraries/winpthreads/COPYING'})
    write_component(args.output, 'mingw-w64-runtime', args.mingw_version,
                    'https://sourceforge.net/projects/mingw-w64/files/mingw-w64/mingw-w64-release/mingw-w64-v'+args.mingw_version+'.tar.bz2', mingw)
    for name, archive in [('gcc-runtime', args.gcc_source), ('mingw-w64-runtime', args.mingw_source)]:
        result = hashlib.sha256()
        with archive.open('rb') as stream:
            while block := stream.read(1024 * 1024): result.update(block)
        (args.output/name/'SOURCE-ARCHIVE-SHA256.txt').write_text(result.hexdigest()+'  '+archive.name+'\n')


if __name__ == '__main__':
    main()
