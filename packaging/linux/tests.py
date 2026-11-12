#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Native packaging guards; synthetic executables are not product runtime proof."""
import argparse
import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[2]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    result = importlib.util.module_from_spec(spec); spec.loader.exec_module(result)
    return result


linker = module('linker', ROOT / 'packaging/linux/linker.py')
deb = module('deb', ROOT / 'packaging/debian/build-deb.py')


class Packaging(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(dir=ARGS.scratch)
        self.root = Path(self.temporary.name)
        self.addCleanup(self.temporary.cleanup)

    def test_nix_shared_search_paths_and_rpath_never_leak(self):
        arguments = ['-L/nix/store/fixture-fontconfig/lib', '-L', '/nix/store/fixture-gcc/lib',
                     '-Wl,--as-needed,-rpath,/nix/store/fixture-lib,--gc-sections',
                     '-Wl,--dynamic-linker=/nix/store/fixture-lib/ld.so', '-lfontconfig', 'owned.o']
        result = linker.filter_arguments(arguments, self.root)
        self.assertEqual(result, ['-Wl,--as-needed,--gc-sections', '-lfontconfig', 'owned.o'])

    def test_explicit_nix_native_objects_fail_closed(self):
        for path in ['/nix/store/fixture/lib/libbad.so', '/nix/store/fixture/lib/libbad.so.2',
                     '/nix/store/fixture/crti.o']:
            with self.assertRaises(ValueError): linker.filter_arguments([path], self.root)
        self.assertEqual(linker.filter_arguments(['/nix/store/fixture/libstd.rlib'], self.root),
                         ['/nix/store/fixture/libstd.rlib'])

    def test_salted_linker_environment_cannot_restore_nix_paths(self):
        env = linker.target_environment({'NIX_LDFLAGS': '-L/nix/store/a',
            'NIX_LDFLAGS_x86_64_unknown_linux_gnu': '-rpath /nix/store/b',
            'NIX_CFLAGS_LINK_x86_64_unknown_linux_gnu': '-L/nix/store/c',
            'LIBRARY_PATH': '/nix/store/d', 'LD_LIBRARY_PATH': '/nix/store/e',
            'OWNED_BUILD_SENTINEL': 'retained'})
        self.assertFalse(any(name.startswith(('NIX_LDFLAGS','NIX_CFLAGS_LINK')) for name in env))
        self.assertEqual(env['NIX_DONT_SET_RPATH_x86_64_unknown_linux_gnu'], '1')
        self.assertNotIn('LIBRARY_PATH', env); self.assertNotIn('LD_LIBRARY_PATH', env)
        self.assertEqual(env['OWNED_BUILD_SENTINEL'], 'retained')

    def test_link_trace_rejects_implicit_nix_shared_or_crt_fallback(self):
        sysroot = '/nix/store/fixture-target-sysroot'
        trace = [sysroot + '/usr/lib/libc.so.6', '/nix/store/fixture-rust/libstd.rlib(std.rcgu.o)']
        self.assertEqual(linker.validate_trace(trace, sysroot), ['/usr/lib/libc.so.6'])
        for value in ['/nix/store/fixture-host/libc.so.6', '/nix/store/fixture-host/crtbeginS.o']:
            with self.assertRaisesRegex(ValueError, 'native link fallback'):
                linker.validate_trace(trace + [value], sysroot)

    def test_malformed_elf_is_rejected_before_inspection(self):
        path = self.root / 'bad'; path.write_bytes(b'not an executable')
        with self.assertRaisesRegex(ValueError, 'expected real amd64'):
            deb.elf(path, 'amd64', (2, 41), ARGS.readelf)

    def test_real_elf_loader_runtime_path_and_abi_guards(self):
        source = self.root / 'fixture.rs'
        source.write_text('fn main() { println!("owned packaging fixture"); }\n')
        binary = self.root / 'fixture'
        subprocess.run([ARGS.rustc, '--edition=2024', str(source), '-o', str(binary)], check=True)
        with self.assertRaisesRegex(ValueError, 'target system loader'):
            deb.elf(binary, 'amd64', (2, 99), ARGS.readelf)
        subprocess.run([ARGS.patchelf, '--set-interpreter', '/lib64/ld-linux-x86-64.so.2', '--remove-rpath', str(binary)], check=True)
        value = deb.elf(binary, 'amd64', (2, 99), ARGS.readelf)
        self.assertEqual(len(value['sha256']), 64)
        with self.assertRaisesRegex(ValueError, 'needs glibc'):
            deb.elf(binary, 'amd64', (2, 2), ARGS.readelf)
        for rpath in ['/nix/store/fixture/lib', '$ORIGIN/lib', '/usr/lib/nuschl/../outside']:
            subprocess.run([ARGS.patchelf, '--set-rpath', rpath, str(binary)], check=True)
            with self.assertRaises(ValueError): deb.elf(binary, 'amd64', (2, 99), ARGS.readelf)

    def test_stage_link_cannot_escape_to_host_files(self):
        stage = self.root / 'stage'; (stage / 'usr').mkdir(parents=True)
        (stage / 'usr/escape').symlink_to('/etc/passwd')
        with self.assertRaisesRegex(ValueError, 'escaping or dangling'):
            deb.validate(stage, 'amd64', (2, 41), ARGS.readelf)

    def test_stage_privileged_modes_are_rejected(self):
        stage = self.root / 'stage'; (stage / 'usr').mkdir(parents=True)
        path = stage / 'usr/privileged'; path.write_text('fixture')
        # Nix correctly forbids creating a real SUID file. Inject only the
        # candidate's stat metadata; never weaken that OS restriction.
        real_stat = Path.stat
        def candidate_stat(candidate, *args, **kwargs):
            result = real_stat(candidate, *args, **kwargs)
            if candidate == path:
                result = os.stat_result((result.st_mode | 0o4000, *tuple(result)[1:]))
            return result
        with patch.object(Path, 'stat', candidate_stat):
            with self.assertRaisesRegex(ValueError, 'privileged mode'):
                deb.validate(stage, 'amd64', (2, 41), ARGS.readelf)

    def test_stage_cannot_supply_maintainer_scripts_or_special_paths(self):
        stage = self.root / 'stage'; (stage / 'DEBIAN').mkdir(parents=True)
        with self.assertRaisesRegex(ValueError, 'without package scripts'):
            deb.validate(stage, 'amd64', (2, 41), ARGS.readelf)
        (stage / 'DEBIAN').rmdir(); (stage / 'etc').mkdir()
        with self.assertRaisesRegex(ValueError, 'unexpected stage path'):
            deb.validate(stage, 'amd64', (2, 41), ARGS.readelf)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--scratch', type=Path, required=True)
    parser.add_argument('--rustc', required=True)
    parser.add_argument('--readelf', required=True)
    parser.add_argument('--patchelf', required=True)
    ARGS = parser.parse_args()
    unittest.main(argv=[__file__], verbosity=2)
