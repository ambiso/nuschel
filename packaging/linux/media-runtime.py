#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Prove distro PipeWire public-data requirements using only virtual devices.

The outer namespace contains a private sound server. The tested worker is its
nested, offline, library-only child, so the server can inspect its own client's
PID as a normal sound server would. No host audio, GPU, profile or bus is exposed.
"""
import argparse
import hashlib
import json
import os
import select
import struct
from pathlib import Path
import subprocess
import sys
import time


def live_supervisor(binary, environment, log_path):
    """Hold input open through a real Status ACK; then shut down and reap."""
    def send(process, payload):
        data = json.dumps({'version': 1, 'request_id': 1, 'payload': payload}).encode()
        process.stdin.write(struct.pack('>I', len(data)) + data)
        process.stdin.flush()

    with log_path.open('wb') as log:
        process = subprocess.Popen([str(binary), 'media-sandbox'], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=log, env=environment)
        deadline = time.monotonic() + 8
        consumed = 0
        def take(size):
            nonlocal consumed
            result = bytearray()
            while len(result) < size:
                remaining = deadline - time.monotonic()
                assert remaining > 0 and select.select([process.stdout], [], [], remaining)[0], 'live ACK timeout'
                part = os.read(process.stdout.fileno(), size - len(result))
                assert part, 'worker EOF before Status'
                consumed += len(part)
                assert consumed <= 1024 * 1024, 'aggregate reply limit'
                result.extend(part)
            return result
        try:
            send(process, {'kind': 'set_state', 'epoch': 1, 'muted': True, 'deafened': False})
            while True:
                size = struct.unpack('>I', take(4))[0]
                assert 0 < size <= 1024 * 1024
                reply = json.loads(take(size))
                assert reply['version'] == 1 and reply['request_id'] == 0
                event = reply['payload']
                if event['kind'] == 'status':
                    break
            send(process, {'kind': 'shutdown'})
            process.stdin.close()
            process.wait(timeout=5)
            assert process.returncode == 0
            return {'ready': event['ready'], 'message': event['message'], 'exit': process.returncode, 'reaped': True}
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()


def inside(supervisor=False):
    own = Path('/evidence')
    runtime = own / 'run'
    runtime.mkdir(mode=0o700)
    config = own / 'config/pipewire/pipewire.conf.d'
    config.mkdir(parents=True)
    (config / '10-private.conf').write_text('''context.properties = { support.dbus = false }
context.objects = [
{ factory = adapter args = { factory.name = support.null-audio-sink node.name = speakers node.description = Speakers media.class = Audio/Sink audio.position = [ FL FR ] object.linger = true } }
{ factory = adapter args = { factory.name = support.null-audio-sink node.name = microphone node.description = Microphone media.class = Audio/Source/Virtual audio.position = [ MONO ] object.linger = true } }
]
''')
    environment = {'XDG_RUNTIME_DIR': str(runtime), 'XDG_CONFIG_HOME': str(own / 'config')}
    with (own / 'server.log').open('wb') as log:
        server = subprocess.Popen(['/usr/bin/pipewire'], stdout=log, stderr=log, env=environment)
        try:
            deadline = time.monotonic() + 5
            while not (runtime / 'pipewire-0').exists():
                assert server.poll() is None, 'private server exited'
                assert time.monotonic() < deadline, 'private socket timeout'
                time.sleep(.05)
            if supervisor:
                results = []
                for name, directory, expected in ([('baseline', Path('/baseline'), False)] if Path('/baseline').exists() else []) + [('fixed', Path('/stage'), True)]:
                    env = {**environment, 'PATH': '/usr/bin:/bin', 'NUSCHL_AUDIO_BACKEND': 'pipewire',
                           'NUSCHL_TEST_SECRET': 'must-not-be-forwarded'}
                    result = live_supervisor(directory / 'usr/lib/nuschl/nuschl', env, own / (name + '-supervisor.log'))
                    result.update({'case': name, 'desktop_sha256': hashlib.sha256((directory / 'usr/lib/nuschl/nuschl').read_bytes()).hexdigest(),
                                   'worker_sha256': hashlib.sha256((directory / 'usr/lib/nuschl/nuschl-media-worker').read_bytes()).hexdigest()})
                    results.append(result)
                    (own / 'proof.json').write_text(json.dumps({'production_supervisor': True, 'cases': results,
                        'scope': 'Actual desktop media-sandbox Status ACK with private virtual PipeWire; muted capture, no physical devices.'}, indent=2) + '\n')
                    assert result['ready'] == expected, result
                return
            worker = Path('/stage/usr/lib/nuschl/nuschl-media-worker')
            command = ['/usr/bin/bwrap', '--unshare-all', '--die-with-parent', '--new-session', '--clearenv',
                       '--ro-bind', '/usr/lib', '/usr/lib', '--ro-bind', '/usr/lib64', '/usr/lib64',
                       '--symlink', 'usr/lib', '/lib', '--symlink', 'usr/lib64', '/lib64',
                       '--proc', '/proc', '--dev', '/dev', '--tmpfs', '/tmp',
                       '--ro-bind', str(runtime / 'pipewire-0'), '/run/audio/pipewire-0',
                       '--setenv', 'XDG_RUNTIME_DIR', '/run/audio',
                       '--setenv', 'NUSCHL_AUDIO_BACKEND', 'pipewire',
                       '--setenv', 'PIPEWIRE_DEBUG', '3', '--ro-bind', str(worker), '/worker']
            results = []
            for name, extra in [('missing', []), ('public-config', ['--ro-bind', '/usr/share/pipewire', '/usr/share/pipewire'])]:
                result = subprocess.run(command + extra + ['--', '/worker', '--probe-route'],
                                        capture_output=True, timeout=15, env={})
                assert len(result.stdout) + len(result.stderr) < 1024 * 1024
                (own / (name + '.log')).write_bytes(result.stderr)
                results.append({'case': name, 'exit': result.returncode,
                                'route': json.loads(result.stdout) if result.returncode == 0 else None,
                                'stderr_tail': result.stderr.decode(errors='replace')[-2000:]})
            proof = {'worker_sha256': hashlib.sha256(worker.read_bytes()).hexdigest(), 'cases': results,
                     'scope': 'Private virtual PipeWire devices; namespace/data admission, not physical audio or capture-quality proof.'}
            (own / 'proof.json').write_text(json.dumps(proof, indent=2) + '\n')
            assert results[0]['exit'] != 0 and 'client.conf' in results[0]['stderr_tail']
            assert results[1]['exit'] == 0, results[1]
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()


def main():
    if sys.argv[1:] in (['--inside'], ['--inside', '--supervisor']):
        inside('--supervisor' in sys.argv[1:])
        return
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('root', 'stage', 'bwrap', 'scratch'):
        parser.add_argument('--' + name, required=True, type=Path)
    parser.add_argument('--supervisor', action='store_true')
    parser.add_argument('--baseline', type=Path)
    args = parser.parse_args()
    args.scratch.mkdir(mode=0o700)  # Refuse to overwrite a failed trial.
    command = [str(args.bwrap), '--unshare-all', '--die-with-parent', '--new-session', '--clearenv',
               '--ro-bind', str(args.root / 'usr'), '/usr', '--symlink', 'usr/lib', '/lib',
               '--symlink', 'usr/lib64', '/lib64', '--proc', '/proc', '--dev', '/dev', '--tmpfs', '/tmp',
               '--ro-bind', str(args.stage), '/stage', '--bind', str(args.scratch), '/evidence',
               '--ro-bind', str(Path(__file__).resolve()), '/fixture.py',
               ]
    if args.baseline:
        command += ['--ro-bind', str(args.baseline), '/baseline']
    command += ['--', '/usr/bin/python3', '/fixture.py', '--inside']
    if args.supervisor:
        command += ['--supervisor']
    subprocess.run(command, env={}, timeout=45, check=True)
    print(str(args.scratch / 'proof.json'))


if __name__ == '__main__':
    main()
