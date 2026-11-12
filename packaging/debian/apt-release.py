#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Create unsigned, reproducible APT metadata; never loads a key or uploads."""
import argparse
from datetime import datetime, timedelta, timezone
import email.utils
import hashlib
from pathlib import Path
import re

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--root', required=True, type=Path)
parser.add_argument('--suite', required=True)
parser.add_argument('--epoch', required=True, type=int)
args = parser.parse_args()
if not re.fullmatch('[a-z][a-z0-9.-]*', args.suite) or args.epoch < 0:
    parser.error('invalid suite or publication epoch')
release = args.root / 'dists' / args.suite
date = datetime.fromtimestamp(args.epoch, timezone.utc)
text = ('Origin: Nuschl\nLabel: Nuschl\nSuite: ' + args.suite + '\nCodename: ' + args.suite + '\n'
        'Architectures: amd64\nComponents: main\nDescription: Local Nuschl package validation repository\n'
        'Date: ' + email.utils.format_datetime(date) + '\nValid-Until: ' +
        email.utils.format_datetime(date + timedelta(days=7)) + '\n')
files = [release / 'main/binary-amd64' / name for name in ('Packages', 'Packages.gz')]
for algorithm in ('sha256', 'sha512'):
    text += algorithm.upper() + ':\n'
    for path in files:
        data = path.read_bytes()
        text += f' {hashlib.new(algorithm, data).hexdigest()} {len(data):16d} {path.relative_to(release)}\n'
(release / 'Release').write_text(text)
