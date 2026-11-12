#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Offline distro GUI smoke with private Xvfb/DBus and CPU-only Vulkan.

Only an owned new profile is used. No host store, GPU/audio devices, display,
bus, network or user configuration is exposed. The retained XWD proves rendering;
this does not prove interaction, distro installation or physical-platform support.
"""
import argparse
import os
from pathlib import Path
import subprocess
import sys

def inside():
    import ctypes, json, pathlib, subprocess, time, shutil, hashlib
    own = pathlib.Path('/evidence')
    (own / 'home').mkdir(mode=448)
    (own / 'run').mkdir(mode=448)
    (own / 'fb').mkdir()
    (own / 'bus.conf').write_text('<busconfig><type>session</type><listen>unix:path=/evidence/run/bus</listen><auth>EXTERNAL</auth><policy context="default"><allow send_destination="*"/><allow receive_sender="*"/><allow own="*"/></policy></busconfig>')
    env = {'PATH': '/usr/bin:/bin', 'HOME': '/evidence/home', 'XDG_CONFIG_HOME': '/evidence/home/config', 'XDG_DATA_HOME': '/evidence/home/data', 'XDG_CACHE_HOME': '/evidence/home/cache', 'XDG_RUNTIME_DIR': '/evidence/run', 'DISPLAY': ':90', 'DBUS_SESSION_BUS_ADDRESS': 'unix:path=/evidence/run/bus', 'VK_ICD_FILENAMES': '/usr/share/vulkan/icd.d/lvp_icd.json', 'LIBGL_ALWAYS_SOFTWARE': '1', 'LP_NUM_THREADS': '2', 'NUSCHL_AUDIO_BACKEND': 'pulse', 'PULSE_SERVER': 'unix:/evidence/run/no-pulse'}
    processes = []
    logs = []

    def spawn(name, cmd):
        log = (own / (name + '.log')).open('wb')
        logs.append(log)
        p = subprocess.Popen(cmd, stdout=log, stderr=log, env=env)
        processes.append(p)
        return p
    try:
        pathlib.Path('/tmp/.X11-unix').mkdir(mode=1023)
        x = spawn('xvfb', ['/usr/bin/Xvfb', ':90', '-screen', '0', '1100x760x24', '-nolisten', 'tcp', '-noreset', '-fbdir', '/evidence/fb'])
        bus = spawn('dbus', ['/usr/bin/dbus-daemon', '--nofork', '--config-file=/evidence/bus.conf'])
        deadline = time.monotonic() + 5
        while not pathlib.Path('/tmp/.X11-unix/X90').exists():
            assert x.poll() is None
            assert time.monotonic() < deadline
            time.sleep(0.05)
        app = spawn('app', ['/stage/usr/lib/nuschl/nuschl', '--profile-dir', '/evidence/home/profile'])
        lib = ctypes.CDLL('libX11.so.6')
        lib.XOpenDisplay.argtypes = [ctypes.c_char_p]
        lib.XOpenDisplay.restype = ctypes.c_void_p
        d = lib.XOpenDisplay(b':90')
        assert d
        lib.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
        lib.XDefaultRootWindow.restype = ctypes.c_ulong
        root = lib.XDefaultRootWindow(d)
        lib.XQueryTree.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.POINTER(ctypes.c_ulong)), ctypes.POINTER(ctypes.c_uint)]
        lib.XFetchName.argtypes = [ctypes.c_void_p, ctypes.c_ulong, ctypes.POINTER(ctypes.c_char_p)]
        lib.XFree.argtypes = [ctypes.c_void_p]
        titles = []
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            assert app.poll() is None, 'desktop exited'
            r = ctypes.c_ulong()
            p = ctypes.c_ulong()
            children = ctypes.POINTER(ctypes.c_ulong)()
            count = ctypes.c_uint()
            lib.XQueryTree(d, root, ctypes.byref(r), ctypes.byref(p), ctypes.byref(children), ctypes.byref(count))
            titles = []
            for i in range(count.value):
                title = ctypes.c_char_p()
                if lib.XFetchName(d, children[i], ctypes.byref(title)) and title.value:
                    titles.append(title.value.decode(errors='replace'))
                    lib.XFree(title)
            if children:
                lib.XFree(children)
            if any(('Nuschl' in title for title in titles)):
                break
            time.sleep(0.1)
        assert any(('Nuschl' in title for title in titles)), titles
        time.sleep(2)
        shutil.copyfile(own / 'fb/Xvfb_screen0', own / 'frame.xwd')
        (own / 'proof.json').write_text(json.dumps({'desktop_sha256': hashlib.sha256(pathlib.Path('/stage/usr/lib/nuschl/nuschl').read_bytes()).hexdigest(), 'titles': titles, 'software_vulkan': env['VK_ICD_FILENAMES'], 'host_gpu_exposed': False, 'nix_store_exposed': pathlib.Path('/nix/store').exists(), 'scope': 'Private Xvfb/software Vulkan render; no user interaction, audio, packaging install or physical-GPU proof.'}, indent=2) + '\n')
    finally:
        for p in reversed(processes):
            if p.poll() is None:
                p.terminate()
            try:
                p.wait(timeout=5)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait()
        for log in logs:
            log.close()

def main():
    if sys.argv[1:] == ['--inside']:
        inside()
        return
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('root', 'stage', 'bwrap', 'scratch'):
        parser.add_argument('--' + name, required=True, type=Path)
    args = parser.parse_args()
    args.scratch.mkdir(mode=448)
    (args.scratch / 'passwd').write_text(f'fixture:x:{os.getuid()}:{os.getgid()}::/evidence/home:/bin/sh\n')
    (args.scratch / 'group').write_text(f'fixture:x:{os.getgid()}:\n')
    command = [str(args.bwrap), '--unshare-all', '--die-with-parent', '--new-session', '--clearenv', '--ro-bind', str(args.root / 'usr'), '/usr', '--symlink', 'usr/lib', '/lib', '--symlink', 'usr/lib64', '/lib64', '--symlink', 'usr/bin', '/bin', '--symlink', 'usr/sbin', '/sbin', '--proc', '/proc', '--dev', '/dev', '--tmpfs', '/tmp', '--ro-bind', str(args.root / 'etc/fonts'), '/etc/fonts', '--ro-bind', str(args.scratch / 'passwd'), '/etc/passwd', '--ro-bind', str(args.scratch / 'group'), '/etc/group', '--ro-bind', str(args.stage), '/stage', '--bind', str(args.scratch), '/evidence', '--ro-bind', str(Path(__file__).resolve()), '/fixture.py', '--', '/usr/bin/python3', '/fixture.py', '--inside']
    subprocess.run(command, env={}, timeout=35, check=True)
    print(str(args.scratch / 'proof.json'))
if __name__ == '__main__':
    main()
