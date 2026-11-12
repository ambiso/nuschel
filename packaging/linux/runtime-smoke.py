#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Bounded worker-RPC/loader smoke in a distro-only, offline namespace.

This is not a GUI/audio-device/install test. The caller supplies an audited FHS
stage and its matching target userspace. No Nix store or host home is mounted.
"""
import argparse
import base64
import hashlib
import json
import math
from pathlib import Path
import struct
import subprocess
import tempfile
import zlib


def frame(payload):
    data = json.dumps({'version': 1, 'request_id': 1, 'payload': payload}).encode()
    return struct.pack('>I', len(data)) + data


def reply(data):
    assert len(data) >= 4, 'missing reply header'
    size = struct.unpack('>I', data[:4])[0]
    assert 0 < size <= 12 * 1024 * 1024 and len(data) == size + 4, 'invalid reply size'
    result = json.loads(data[4:])
    assert result['version'] == 1 and result['request_id'] == 1
    return result['payload']


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('stage', 'sysroot', 'bwrap', 'prlimit', 'scratch', 'proof'):
        parser.add_argument('--' + name, required=True, type=Path)
    args = parser.parse_args()
    args.scratch.mkdir(parents=True, exist_ok=True, mode=0o700)
    records = []

    def run(name, arguments=(), data=b'', expected=0, writable=False):
        binary = args.stage / 'usr/lib/nuschl' / name
        with tempfile.TemporaryDirectory(prefix=name + '-', dir=args.scratch) as own:
            own = Path(own)
            command = [str(args.prlimit), '--as=2147483648', '--cpu=15', '--fsize=16777216', '--',
                       str(args.bwrap), '--unshare-all', '--die-with-parent', '--new-session', '--clearenv',
                       '--ro-bind', str(args.sysroot / 'usr'), '/usr', '--symlink', 'usr/lib', '/lib',
                       '--symlink', 'usr/lib64', '/lib64', '--proc', '/proc', '--dev', '/dev',
                       '--tmpfs', '/tmp', '--chdir', '/tmp', '--ro-bind', str(binary), '/worker']
            if writable:
                (own / 'data').mkdir(mode=0o700)
                command += ['--bind', str(own / 'data'), '/data']
            command += ['--', '/worker', *arguments]
            with (own / 'stdout').open('wb') as out, (own / 'stderr').open('wb') as err:
                process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=out, stderr=err,
                                           env={'PATH': '/usr/bin:/bin'})
                try:
                    process.communicate(data, timeout=25)
                except BaseException:
                    process.kill()
                    process.wait()
                    raise
            stdout = (own / 'stdout').read_bytes()
            stderr = (own / 'stderr').read_bytes()
            assert process.returncode == expected, (name, process.returncode, stderr[:4096].decode(errors='replace'))
            records.append({'program': name, 'sha256': hashlib.sha256(binary.read_bytes()).hexdigest(),
                            'exit': process.returncode, 'stdout_bytes': len(stdout), 'stderr_bytes': len(stderr),
                            'reaped': True})
            return stdout, stderr

    # Canary runs the target distro Python, not a host interpreter in the namespace.
    canary = subprocess.run([str(args.bwrap), '--unshare-all', '--die-with-parent', '--clearenv',
                            '--ro-bind', str(args.sysroot / 'usr'), '/usr', '--symlink', 'usr/lib', '/lib',
                            '--symlink', 'usr/lib64', '/lib64', '--proc', '/proc', '--dev', '/dev',
                            '--tmpfs', '/tmp', '--', '/usr/bin/python3', '-c',
                            'import os,socket; assert not any(os.path.exists(p) for p in '
                            '("/nix/store","/home","/run/user","/dev/snd","/dev/dri")); '
                            'assert set(os.environ) <= {"LC_CTYPE","PWD"}; s=socket.socket(); s.settimeout(.1); '
                            'assert s.connect_ex(("192.0.2.1",9)) != 0; print("confined")'],
                           capture_output=True, timeout=5, env={})
    assert canary.returncode == 0, canary.stderr.decode(errors='replace')
    assert b'confined' in canary.stdout
    version, _ = run('nuschl', ['--version'])
    assert b'nuschl' in version.lower()

    rendered = reply(run('nuschl-content-worker', data=frame({'kind': 'render', 'text': '**Nuschl**\n```rust\nlet count = 1;\n```'}))[0])
    assert rendered['status'] == 'rendered' and 'Nuschl' in json.dumps(rendered)

    def chunk(kind, value):
        return struct.pack('>I', len(value)) + kind + value + struct.pack('>I', zlib.crc32(kind + value))
    png = b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', 1, 1, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(bytes([0, 200, 80, 40]))) + chunk(b'IEND', b'')
    image = reply(run('nuschl-content-worker', data=frame({'kind': 'avatar', 'image': base64.b64encode(png).decode()}))[0])
    assert image['status'] == 'rendered'
    # Exact synthetic RGBA must survive the real isolated image path.
    assert base64.b64encode(bytes([200, 80, 40, 255])).decode() in json.dumps(image)

    formula = reply(run('nuschl-math-worker', data=frame({'tex': '\\frac{1}{2}', 'display': True, 'size': 18}))[0])
    assert formula['status'] == 'rendered'
    mask = formula['value']
    pixels = mask['width'] * mask['height']
    coverage = base64.b64decode(mask['coverage'], validate=True)
    assert 0 < pixels <= 2 * 1024 * 1024 and len(coverage) == pixels and max(coverage) > 0

    samples = [0.1 * math.sin(2 * math.pi * 997 * n / 48000) for n in range(960)]
    packet = reply(run('nuschl-media-worker', data=frame({'kind': 'encode_opus', 'samples': samples}))[0])
    assert packet['status'] == 'packet' and 0 < len(packet['value']) <= 1275
    decoded = reply(run('nuschl-media-worker', data=frame({'kind': 'decode_opus', 'packet': packet['value']}))[0])
    assert decoded['status'] == 'samples' and len(decoded['value']) == 960
    assert all(math.isfinite(value) for value in decoded['value']) and any(decoded['value'])

    identity = reply(run('nuschl-crypto-worker', ['--data', '/data'], frame({'kind': 'identity'}), writable=True)[0])
    assert identity['status'] == 'identity'
    recent = reply(run('nuschl-storage-worker', ['--database', '/data/history.sqlite3'],
                       frame({'kind': 'recent', 'server': 'fixture', 'channel': '0', 'limit': 1}), writable=True)[0])
    assert recent == {'status': 'messages', 'value': []}, recent
    run('nuschl-transfer-worker')  # EOF only: no rendezvous or transfer claim.
    for name, tag, public_size, signature_size in [('nuschl-update-verify-ed25519', 1, 32, 64),
                                                   ('nuschl-update-verify-mldsa44', 2, 1312, 2420)]:
        # Correct wire shape with invalid public test material must reject.
        request = b'NUSUVR2\0' + bytes([tag]) + struct.pack('>I', 1) + bytes(public_size + signature_size) + b'x'
        _, error = run(name, data=request, expected=1)
        assert b'update verification rejected' in error
    proof = {'schema': 1, 'stage': str(args.stage), 'sysroot': str(args.sysroot),
             'stage_build_info_sha256': hashlib.sha256((args.stage / 'usr/share/nuschl/build-info.json').read_bytes()).hexdigest(),
             'namespace_canaries': 'passed', 'programs': records,
             'scope': 'Distro-only loader and synthetic worker RPC; no GUI rendering, device I/O, transfer interoperability or distro installation proof.'}
    args.proof.write_text(json.dumps(proof, indent=2) + '\n')
    print(json.dumps({'programs': len(set(row['program'] for row in records)), 'checks': len(records), 'proof': str(args.proof)}))


if __name__ == '__main__':
    main()
