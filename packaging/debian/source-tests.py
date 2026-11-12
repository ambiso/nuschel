#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Independent source archive/checksum attacks and real changelog parsing."""
import argparse
import hashlib
import importlib.util
import io
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest

parser = argparse.ArgumentParser()
parser.add_argument('--dpkg-parsechangelog', required=True)
args, remaining = parser.parse_known_args()
spec = importlib.util.spec_from_file_location('validation', Path(__file__).with_name('validate-source.py'))
validation = importlib.util.module_from_spec(spec)
spec.loader.exec_module(validation)


class Source(unittest.TestCase):
    def test_archive_members(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'fixture.tar'
            cases = [('debian/rules', 0o755, 'file', True), ('../outside', 0o644, 'file', False),
                     ('debian/setid', 0o6755, 'file', False), ('debian/writable', 0o666, 'file', False),
                     ('debian/prebuilt', 0o755, 'elf', False), ('debian/device', 0o644, 'device', False),
                     ('debian/.git/config', 0o644, 'file', False), ('debian/link', 0o777, 'escape-link', False)]
            for name, mode, kind, accepted in cases:
                with self.subTest(name=name):
                    with tarfile.open(path, 'w') as stream:
                        member = tarfile.TarInfo(name)
                        member.mode = mode
                        data = b'\x7fELF' if kind == 'elf' else b'fixture'
                        member.size = len(data)
                        if kind == 'device':
                            member.type = tarfile.CHRTYPE
                            member.size = 0
                        if kind == 'escape-link':
                            member.type = tarfile.SYMTYPE
                            member.linkname = '../../outside'
                            member.size = 0
                        stream.addfile(member, io.BytesIO(data))
                    if accepted:
                        self.assertEqual(validation.archive(path, 'debian')['members'], 1)
                    else:
                        with self.assertRaises(ValueError):
                            validation.archive(path, 'debian')

    def test_checksums(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            package = root / 'source.tar.xz'
            package.write_bytes(b'public fixture')
            control = root / 'source.dsc'
            control.write_text('Source: fixture\nChecksums-Sha256:\n ' + hashlib.sha256(package.read_bytes()).hexdigest() +
                               f' {package.stat().st_size} {package.name}\n')
            self.assertEqual(validation.checksums(root, control, {package.name})['Source'], 'fixture')
            package.write_bytes(b'tampered bytes')  # Same size, so checksum must catch it.
            with self.assertRaises(ValueError):
                validation.checksums(root, control, {package.name})

    def test_preparation_and_real_changelog(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '.cargo').mkdir()
            (root / 'debian/source').mkdir(parents=True)
            (root / 'debian/control').write_text('Maintainer: @MAINTAINER@\n')
            command = [sys.executable, str(Path(__file__).with_name('prepare-source.py').resolve()),
                       '--revision', '0' * 40, '--version', '0.1.0+git10.20261112.g0123456789ab',
                       '--debian-revision', '1', '--distribution', 'unstable',
                       '--maintainer', 'Fixture <fixture@example.invalid>', '--epoch', '1794520000']
            subprocess.run(command + ['--phase', 'upstream'], cwd=root, check=True, capture_output=True)
            self.assertNotEqual(subprocess.run(command + ['--phase', 'upstream'], cwd=root, capture_output=True).returncode, 0)
            subprocess.run(command + ['--phase', 'debian'], cwd=root, check=True, capture_output=True)
            parsed = subprocess.run([args.dpkg_parsechangelog, '--file', str(root / 'debian/changelog'), '-S', 'Version'],
                                    capture_output=True, check=True, text=True)
            self.assertEqual(parsed.stdout.strip(), '0.1.0+git10.20261112.g0123456789ab-1')
            self.assertEqual(parsed.stderr, '')


if __name__ == '__main__':
    unittest.main(argv=[sys.argv[0], *remaining])
