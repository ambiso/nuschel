#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Generate a -bin recipe for exact Nix-built, audited FHS bytes; no upload."""
import argparse
import hashlib
from pathlib import Path
import re
import shlex
from urllib.parse import urlsplit

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--archive', required=True, type=Path)
parser.add_argument('--version', required=True)
parser.add_argument('--output', required=True, type=Path)
parser.add_argument('--source-url', help='Explicit immutable HTTPS release URL; absent means a local validation recipe only')
args = parser.parse_args()
if not re.fullmatch(r'[0-9][A-Za-z0-9._+]*', args.version):
    parser.error('invalid Arch pkgver')
if not re.fullmatch(r'nuschl-[A-Za-z0-9._+]+-x86_64\.tar\.xz', args.archive.name):
    parser.error('unexpected archive filename')
source = args.archive.name
if args.source_url:
    url = urlsplit(args.source_url)
    if url.scheme != 'https' or not url.hostname or url.username or url.password or url.fragment or url.query:
        parser.error('source URL must be a fixed HTTPS resource without credentials/query/fragment')
    source += '::' + args.source_url
args.output.mkdir(parents=True, exist_ok=False)
recipe = '''# Generated locally from the exact Nix FHS stage; no self-update or Nix runtime.
# Publication requires an immutable public source URL and clean Arch runtime proof.
pkgname=nuschl-bin
pkgver={version}
pkgrel=1
pkgdesc='Mumble-compatible voice and text chat with isolated helpers'
arch=('x86_64')
url='https://github.com/ambiso/nuschl'
license=('EUPL-1.2')
provides=("nuschl=$pkgver")
conflicts=('nuschl')
depends=('glibc>=2.41' 'gcc-libs' 'bubblewrap' 'util-linux' 'desktop-file-utils'
         'hicolor-icon-theme' 'sqlite' 'fontconfig' 'freetype2' 'libxkbcommon'
         'libxkbcommon-x11' 'wayland' 'vulkan-icd-loader' 'libglvnd' 'libx11'
         'libxcursor' 'libxi' 'libxrandr' 'libxcb' 'alsa-lib' 'pipewire' 'libpulse')
options=('!strip' '!debug')
source=({source})
sha256sums=('{checksum}')

package() {{
    cp -a -- "$srcdir/usr" "$pkgdir/usr"
}}
'''.format(version=shlex.quote(args.version), source=shlex.quote(source),
           checksum=hashlib.sha256(args.archive.read_bytes()).hexdigest())
(args.output / 'PKGBUILD').write_text(recipe)
(args.output / 'STATUS').write_text('immutable-source-url-configured; clean Arch proof still required\n' if args.source_url else
                                   'LOCAL VALIDATION ONLY: source archive is adjacent; no public release URL configured.\n')
