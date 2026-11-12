#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Validate a prepared FHS runtime and create a local Debian binary package.

This does not turn Nix-linked binaries into portable ones, compile source,
change a sandbox, sign anything, or publish a package.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import runpy
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[2]
PROGRAMS = ('nuschl', 'nuschl-content-worker', 'nuschl-crypto-worker',
            'nuschl-math-worker', 'nuschl-media-worker', 'nuschl-storage-worker',
            'nuschl-transfer-worker', 'nuschl-update-verify-ed25519',
            'nuschl-update-verify-mldsa44')
APP_ID = 'org.nuschl.Nuschl'
# Includes dynamically opened graphical/audio libraries, not only DT_NEEDED.
DEPENDS = ('bubblewrap, util-linux, desktop-file-utils, hicolor-icon-theme, '
           'libgcc-s1, libstdc++6, libsqlite3-0, libfontconfig1, libfreetype6, libxkbcommon0, '
           'libxkbcommon-x11-0, libwayland-client0, libwayland-cursor0, '
           'libvulkan1, libgl1, libegl1, libx11-6, libxcursor1, libxi6, '
           'libxrandr2, libxcb1, libasound2t64 | libasound2, '
           'libpipewire-0.3-0t64 | libpipewire-0.3-0, libspa-0.2-modules, libpulse0')


def version(value):
    if not re.fullmatch(r'\d+\.\d+(?:\.\d+)?', value):
        raise ValueError('invalid glibc version')
    return tuple(map(int, value.split('.')))


def elf(path, arch, maximum, readelf):
    with path.open('rb') as stream:
        header = stream.read(64)
    machine = {'amd64': 62, 'arm64': 183}[arch]
    if len(header) < 64 or header[:6] != b'\x7fELF\x02\x01' or int.from_bytes(header[18:20], 'little') != machine:
        raise ValueError(f'{path.name}: expected real {arch} ELF64 executable')
    result = subprocess.run([readelf, '--program-headers', '--dynamic', '--version-info', '--wide', str(path)],
                            env={**os.environ, 'LC_ALL': 'C'}, capture_output=True, text=True, timeout=15)
    if result.returncode or len(result.stdout) > 2 * 1024 * 1024:
        raise ValueError(f'{path.name}: ELF inspection failed')
    text = result.stdout
    interpreter = re.search(r'Requesting program interpreter: ([^\]]+)\]', text)
    if not interpreter:
        raise ValueError(f'{path.name}: missing dynamic interpreter')
    loader = interpreter[1]
    standard = {'amd64': '/lib64/ld-linux-x86-64.so.2', 'arm64': '/lib/ld-linux-aarch64.so.1'}[arch]
    if loader != standard:
        raise ValueError(f'{path.name}: requires target system loader {standard}, found {loader}')
    for field, value in re.findall(r'\((RPATH|RUNPATH|NEEDED)\).*?\[([^\]]*)\]', text):
        if '/nix/store' in value or field == 'NEEDED' and '/' in value:
            raise ValueError(f'{path.name}: nonportable {field}')
        if field != 'NEEDED':
            for directory in value.split(':'):
                if not directory or not directory.startswith('/usr/lib/nuschl/') or '..' in PurePosixPath(directory).parts:
                    raise ValueError(f'{path.name}: unreviewed runtime path {directory!r}')
    symbols = set(re.findall(r'\bGLIBC_[A-Za-z0-9_.]+', text))
    for symbol in symbols:
        if symbol == 'GLIBC_ABI_DT_RELR' and maximum >= (2, 36):
            continue
        if not re.fullmatch(r'GLIBC_\d+\.\d+(?:\.\d+)?', symbol):
            raise ValueError(f'{path.name}: unreviewed glibc ABI requirement {symbol}')
    required = max((version(symbol[6:]) for symbol in symbols if re.fullmatch(r'GLIBC_\d+\.\d+(?:\.\d+)?', symbol)), default=(2, 17))
    if required > maximum:
        raise ValueError(f'{path.name}: needs glibc {".".join(map(str, required))}, target ceiling is {".".join(map(str, maximum))}')
    return {'sha256': hashlib.sha256(path.read_bytes()).hexdigest(), 'minimum_glibc': '.'.join(map(str, required))}


def validate(stage, arch, ceiling, readelf):
    if not stage.is_dir() or (stage / 'DEBIAN').exists():
        raise ValueError('stage must be an FHS directory without package scripts/control metadata')
    for path in stage.rglob('*'):
        relative = path.relative_to(stage)
        if relative.parts[0] != 'usr':
            raise ValueError(f'unexpected stage path {relative}')
        if path.is_symlink():
            target = os.readlink(path)
            resolved = stage / target.lstrip('/') if target.startswith('/') else path.parent / target
            if not resolved.resolve().is_relative_to(stage.resolve()) or not resolved.exists():
                raise ValueError(f'escaping or dangling stage link {relative}')
        elif not (path.is_file() or path.is_dir()):
            raise ValueError(f'unsupported special stage file {relative}')
        elif path.stat().st_mode & 0o6000:
            raise ValueError(f'privileged mode is forbidden in stage: {relative}')
    programs = {}
    for name in PROGRAMS:
        path = stage / 'usr/lib/nuschl' / name
        if path.is_symlink() or not path.is_file() or not path.stat().st_mode & 0o111:
            raise ValueError(f'missing regular executable {name}')
        programs[name] = elf(path, arch, ceiling, readelf)
    launch = stage / 'usr/bin/nuschl'
    if not launch.is_symlink() or os.readlink(launch) != '../lib/nuschl/nuschl':
        raise ValueError('expected /usr/bin/nuschl -> ../lib/nuschl/nuschl')
    desktop = stage / f'usr/share/applications/{APP_ID}.desktop'
    text = desktop.read_text()
    for line in ('Exec=nuschl %u', f'Icon={APP_ID}', 'MimeType=x-scheme-handler/mumble;x-scheme-handler/nuschl;'):
        if line not in text.splitlines():
            raise ValueError(f'missing desktop integration: {line}')
    if not (stage / f'usr/share/icons/hicolor/scalable/apps/{APP_ID}.svg').is_file():
        raise ValueError('missing original application icon')
    notices = stage / 'usr/share/licenses/nuschl'
    authority = runpy.run_path(str(ROOT / 'release/prepare-artifacts.py'))
    authority['verify_notices'](notices, authority['load'](ROOT / 'release/notice-roots.json'),
                                'linux-nix' if arch == 'amd64' else 'linux-aarch64-nix')
    return programs


def main():
    global ROOT
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--stage', type=Path, required=True)
    parser.add_argument('--project-root', type=Path, default=ROOT, help='matching source/policy tree for Nix script staging')
    parser.add_argument('--architecture', choices=['amd64', 'arm64'], required=True)
    parser.add_argument('--maximum-glibc', required=True, help='real target distro ABI ceiling, e.g. 2.41')
    parser.add_argument('--readelf', default='readelf')
    parser.add_argument('--validate-only', action='store_true')
    parser.add_argument('--version')
    parser.add_argument('--maintainer')
    parser.add_argument('--epoch', type=int)
    parser.add_argument('--output', type=Path)
    parser.add_argument('--dpkg-deb', default='dpkg-deb')
    args = parser.parse_args()
    ROOT = args.project_root.resolve(strict=True)
    stage = args.stage.resolve(strict=True)
    programs = validate(stage, args.architecture, version(args.maximum_glibc), args.readelf)
    report = {'programs': programs, 'architecture': args.architecture, 'target_glibc_ceiling': args.maximum_glibc,
              'scope': 'archive/layout/ELF/notice validation only; clean distro runtime proof is separate'}
    if args.validate_only:
        print(json.dumps(report, indent=2)); return
    if not args.output or args.epoch is None or args.epoch < 0 or not args.version or not args.maintainer:
        parser.error('building requires output, epoch, version and maintainer')
    if not re.fullmatch(r'[0-9][A-Za-z0-9.+:~\-]*', args.version) or any(c in args.maintainer for c in '\n\r\0'):
        parser.error('invalid version or maintainer field')
    output = args.output.resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    if output.exists():
        raise ValueError('refusing to replace an existing artifact')
    with tempfile.TemporaryDirectory(prefix='nuschl-deb-', dir=output.parent) as scratch:
        tree = Path(scratch) / 'root'
        shutil.copytree(stage, tree, symlinks=True)
        # Nix store directories/files are read-only. Their archive modes must
        # not make /usr read-only on installation, nor prevent control creation.
        tree.chmod(0o755)
        for path in tree.rglob('*'):
            if path.is_symlink():
                continue
            path.chmod(0o755 if path.is_dir() or path.stat().st_mode & 0o111 else 0o644)
        control = tree / 'DEBIAN'; control.mkdir()
        docs = tree / 'usr/share/doc/nuschl'; docs.mkdir(parents=True, exist_ok=True)
        own_license = ROOT / 'LICENSE'
        (docs / 'copyright').write_bytes(b'Upstream-Name: Nuschl\nSource: https://github.com/ambiso/nuschl\n\n' + own_license.read_bytes() + b'\n\nBundled third-party notices (full referenced files are in /usr/share/licenses/nuschl):\n\n' + (tree / 'usr/share/licenses/nuschl/THIRD-PARTY.txt').read_bytes())
        required = max((version(row['minimum_glibc']) for row in programs.values()))
        libc = '.'.join(map(str, required))
        size = sum(p.stat().st_size for p in tree.rglob('*') if p.is_file() and not p.is_symlink())
        (control / 'control').write_text(f'''Package: nuschl
Version: {args.version}
Section: net
Priority: optional
Architecture: {args.architecture}
Maintainer: {args.maintainer}
Depends: libc6 (>= {libc}), {DEPENDS}
Installed-Size: {(size + 1023) // 1024}
Homepage: https://github.com/ambiso/nuschl
Description: Voice and text chat for Mumble servers
 Desktop client with isolated content, cryptography, storage and media helpers.
 This package is updated by the system package manager.
''')
        env = {**os.environ, 'SOURCE_DATE_EPOCH': str(args.epoch), 'TZ': 'UTC', 'LC_ALL': 'C'}
        subprocess.run([args.dpkg_deb, '--root-owner-group', '--uniform-compression', '--threads-max=2', '--build', str(tree), str(output)], env=env, check=True)
    report['artifact'] = str(output)
    report['sha256'] = hashlib.sha256(output.read_bytes()).hexdigest()
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
