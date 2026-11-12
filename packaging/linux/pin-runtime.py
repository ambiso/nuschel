#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Prepare a separate signed, distro-only integration-test userspace.

These programs/data are not bundled into the application or its .deb. Keeping
this lock separate avoids relinking application leaves when a test tool changes.
"""
import argparse
import importlib.util
import json
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
for name in ('release', 'signature', 'packages', 'keyring', 'output'):
    parser.add_argument('--' + name, required=True, type=Path)
parser.add_argument('--gpgv', required=True)
args = parser.parse_args()
spec = importlib.util.spec_from_file_location('pins', Path(__file__).with_name('pin-debian.py'))
pins = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pins)
pins.SEEDS = sorted(set(pins.SEEDS + ['pipewire-bin', 'xvfb', 'xauth', 'mesa-vulkan-drivers',
                                    'dbus-daemon', 'bubblewrap', 'util-linux', 'fontconfig']))
result = pins.prepare(args.release, args.signature, args.packages, args.keyring, args.gpgv)
result['scope'] = 'Distro-only integration-test userspace; not redistributed in the application package.'
args.output.write_text(json.dumps(result, indent=2, sort_keys=True) + '\n')
