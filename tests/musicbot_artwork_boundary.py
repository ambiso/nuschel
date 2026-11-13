#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual content-sandbox thumbnail faults and bounded synthetic image admission."""
import argparse
import json
import os
from pathlib import Path
import struct
import subprocess
import tempfile
import zlib
from fake_subsonic import png


def chunk(kind, data):
    return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data))


def bomb():
    compressor = zlib.compressobj()
    parts = []
    row = bytes(8192 * 3 + 1)
    for _ in range(8192):
        part = compressor.compress(row)
        if part:
            parts.append(part)
    parts.append(compressor.flush())
    return (b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 8192, 8192, 8, 2, 0, 0, 0))
            + chunk(b'IDAT', b''.join(parts)) + chunk(b'IEND', b''))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('probe', type=Path)
    parser.add_argument('worker', type=Path)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    results = []

    def run(mode, *extra, env=None):
        process = subprocess.run([str(args.probe.resolve()), mode, *map(str, extra)], env=env,
                                 capture_output=True, text=True, timeout=12)
        assert process.returncode == 0, process.stderr
        result = json.loads(process.stdout)
        results.append(result)
        (args.output / 'attempt.json').write_text(json.dumps(results, indent=2) + '\n')
        return result

    assert run('valid')['kind'] == 'accepted'
    for mode in ['wrong-id', 'dimensions', 'bytes', 'trailing-bits', 'two', 'duplicate', 'crash']:
        assert run(mode)['kind'] == 'rejected', mode
    assert run('advertise')['kind'] == 'too_large'  # Before reading/allocating the body.
    assert run('oversize')['kind'] == 'rejected'
    assert run('missing')['kind'] == 'rejected'
    with tempfile.TemporaryDirectory(dir=args.output, prefix='tools-') as empty:
        env = {'PATH': empty}
        assert run('valid', env=env)['kind'] == 'missing_helper'
    for mode, minimum in [('linger', 1900), ('hang', 4900)]:
        result = run(mode)
        assert result['kind'] == 'timeout' and minimum <= result['elapsed_ms'] < 10000, result
    with tempfile.TemporaryDirectory(dir=args.output, prefix='images-') as temporary:
        root = Path(temporary)
        valid = root / 'rgb.png'
        valid.write_bytes(png(200, 80, 40))
        image = run('real', args.worker, valid)
        assert image['kind'] == 'accepted' and image['width'] == image['height'] == 1
        invalid = root / 'invalid.png'
        invalid.write_bytes(b'not a PNG or supported image')
        assert run('real', args.worker, invalid)['kind'] == 'rejected'
        compressed = root / 'oversized-dimensions.png'
        body = bomb()
        assert len(body) < 1024 * 1024  # 192 MiB logical RGB, admitted encoded bytes.
        compressed.write_bytes(body)
        assert run('real', args.worker, compressed)['kind'] == 'rejected'
    proof = {'cases': results, 'passed': len(results), 'worker': str(args.worker.resolve()),
             'scope': 'actual isolated Thumbnail supervisor and worker, synthetic PNG/faults only'}
    (args.output / 'proof.json').write_text(json.dumps(proof, indent=2) + '\n')
    print(json.dumps({'passed': len(results)}))


if __name__ == '__main__':
    main()
