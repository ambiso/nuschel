#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Final-leaf linker adapter: cached Rust/static crate inputs, target system ABI.

Never silently fall back to a Nix shared library. No package/runtime behavior or
sandbox policy is changed here. Used only by the explicit native Nix builder.
"""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile


def native_directory(value, scratch):
    if not value.startswith('/nix/store/'):
        return value
    path = Path(value)
    # Native objects produced by reviewed Rust crates (ring/sqlite/FFI glue)
    # may be retained. Expose archive files only, never a fallback shared lib.
    if '-rust_' not in value or not path.is_dir():
        return None
    archives = list(path.glob('*.a'))
    if not archives:
        return None
    destination = scratch / str(len(list(scratch.iterdir())))
    destination.mkdir()
    for archive in archives:
        (destination / archive.name).symlink_to(archive.resolve())
    return str(destination)


def filter_arguments(arguments, scratch):
    result = []; index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument == '-L' or argument.startswith('-L'):
            if argument == '-L':
                index += 1
                if index == len(arguments): raise ValueError('missing -L path')
                value = arguments[index]
            else: value = argument[2:]
            directory = native_directory(value, scratch)
            if directory: result += ['-L', directory]
        elif argument.startswith('-Wl,'):
            items = argument[4:].split(','); kept = []; part = 0
            while part < len(items):
                item = items[part]
                if item in ('-rpath', '--rpath', '-rpath-link', '--rpath-link', '--dynamic-linker', '-dynamic-linker'):
                    part += 2; continue
                if any(item.startswith(prefix) for prefix in ('-rpath=', '--rpath=', '-rpath-link=', '--rpath-link=', '--dynamic-linker=')):
                    part += 1; continue
                kept.append(item); part += 1
            if kept: result.append('-Wl,' + ','.join(kept))
        elif argument.startswith('/nix/store/') and ('.so' in Path(argument).name or argument.endswith('.o')):
            # Explicit CRTs from the raw compiler are supplied via target -B;
            # Rust's own object/rlib inputs are kept, shared objects are not.
            raise ValueError('unexpected explicit Nix native object: ' + argument)
        else:
            result.append(argument)
        index += 1
    return result


def main():
    sysroot, compiler, *arguments = sys.argv[1:]
    root = Path(sysroot)
    triple = 'x86_64-linux-gnu'
    with tempfile.TemporaryDirectory(prefix='nuschl-static-', dir=os.environ['TMPDIR']) as temporary:
        scratch = Path(temporary)
        filtered = filter_arguments(arguments, scratch)
        command = [compiler, '--sysroot=' + sysroot,
                   '-B' + str(root / 'usr/lib' / triple) + '/',
                   '-B' + str(root / 'usr/lib/gcc' / triple / '14') + '/',
                   '-L' + str(root / 'usr/lib' / triple),
                   '-L' + str(root / 'usr/lib/gcc' / triple / '14'),
                   '-Wl,-rpath-link,' + str(root / 'usr/lib' / triple),
                   '-Wl,-rpath-link,' + str(root / 'usr/lib' / triple / 'pulseaudio'),
                   *filtered, '-Wl,--dynamic-linker=/lib64/ld-linux-x86-64.so.2']
        env = dict(os.environ)
        for name in ('NIX_LDFLAGS', 'NIX_CFLAGS_COMPILE', 'NIX_CFLAGS_LINK', 'LIBRARY_PATH', 'LD_LIBRARY_PATH'):
            env.pop(name, None)
        result = subprocess.run(command, env=env)
    raise SystemExit(result.returncode)


if __name__ == '__main__':
    main()
