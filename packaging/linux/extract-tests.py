#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Synthetic archive boundaries; no native executables or maintainer scripts."""
import io
from pathlib import Path
import subprocess
import sys
import tarfile
import tempfile
import unittest


class Extraction(unittest.TestCase):
    def test_unprivileged_modes_and_member_rejections(self):
        with tempfile.TemporaryDirectory() as temporary:
            own = Path(temporary)
            root = own / 'root'
            root.mkdir()
            transport = own / 'tar-stream'
            transport.write_text('#!/bin/sh\nexec cat "$2"\n')
            transport.chmod(0o755)
            for case in ('normal', 'escape', 'device', 'hardlink'):
                with self.subTest(case=case):
                    archive = own / (case + '.tar')
                    with tarfile.open(archive, 'w') as stream:
                        member = tarfile.TarInfo('../escape' if case == 'escape' else 'usr/bin/' + case)
                        member.mode = 0o6755
                        member.size = 1
                        if case == 'device':
                            member.type = tarfile.CHRTYPE
                            member.size = 0
                        if case == 'hardlink':
                            member.type = tarfile.LNKTYPE
                            member.linkname = '../../escape'
                            member.size = 0
                        stream.addfile(member, io.BytesIO(b'x'))
                    result = subprocess.run([sys.executable, str(Path(__file__).with_name('extract-deb.py')),
                                             '--dpkg-deb', str(transport), '--root', str(root), str(archive)],
                                            capture_output=True, timeout=5)
                    self.assertEqual(result.returncode == 0, case == 'normal', result.stderr)
            self.assertEqual((root / 'usr/bin/normal').stat().st_mode & 0o7777, 0o755)
            self.assertFalse((own / 'escape').exists())


if __name__ == '__main__':
    unittest.main()
