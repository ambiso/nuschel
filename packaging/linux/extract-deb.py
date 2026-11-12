#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Extract pinned test-userland packages without scripts or privileged modes."""
import argparse
import os
from pathlib import Path, PurePosixPath
import subprocess
import tarfile

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--dpkg-deb', required=True)
parser.add_argument('--root', required=True, type=Path)
parser.add_argument('archives', nargs='+', type=Path)
args = parser.parse_args()
root = args.root.resolve(strict=True)


def checked(member, destination):
    name = PurePosixPath(member.name)
    if name.is_absolute() or '..' in name.parts:
        raise ValueError('unsafe archive member')
    if not (member.isfile() or member.isdir() or member.issym() or member.islnk()):
        raise ValueError('special archive member')
    if member.issym() and member.linkname.startswith('/'):
        member = member.replace(linkname=os.path.relpath(root / member.linkname.lstrip('/'),
                                                        (root / str(name)).parent))
    member = member.replace(mode=member.mode & 0o777, uid=None, gid=None, uname=None, gname=None)
    # Also checks traversal through existing filesystem links, and hardlink targets.
    return tarfile.data_filter(member, destination)


for archive in args.archives:
    process = subprocess.Popen([args.dpkg_deb, '--fsys-tarfile', str(archive)], stdout=subprocess.PIPE)
    try:
        with tarfile.open(fileobj=process.stdout, mode='r|') as stream:
            stream.extractall(root, filter=checked)
        process.stdout.close()
        if process.wait(timeout=30) != 0:
            raise ValueError('Debian data extraction failed')
    except BaseException:
        process.kill()
        process.wait()
        raise
