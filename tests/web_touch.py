#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Touch behaviour of the packaged web client on an owned loopback Murmur (#202).

The voice toggles' pressed state and struck icons, the navigation drawer's
slide and the swipe gestures, in confined headless Chromium and Playwright's
Firefox at a phone viewport, with and without reduced motion. Touches go
through each engine's own touch input (Chromium DevTools touch events, the
Firefox Juggler touch command), not script-made DOM events. Text is synthetic.

Both browsers run in bubblewrap with only /nix/store and their owned scratch:
a fresh /dev without sound or camera devices, private /run, HOME and runtime,
and no host session sockets or profiles. Microphone input is the browsers'
synthetic fake stream (Chromium: an owned 997 Hz WAV), never host capture.

This proves rendering and input handling in these engines. It does not certify
iOS Safari, Android browsers, physical touch hardware or OS edge gestures.

Usage: web_touch.py --site SITE --relay RELAY_BIN --murmur MURMUR_BIN
       --chromium CHROMIUM_BIN --firefox PLAYWRIGHT_FIREFOX_BIN
       --playwright PLAYWRIGHT_CORE_INDEX_MJS --font-dir FONTS --output DIR
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
from xml.sax.saxutils import escape

from local_murmur import Murmur, eventually, free_port
from web_audio import prepare_browser

ALIASES = {"sans-serif": "DejaVu Sans", "ui-sans-serif": "DejaVu Sans", "system-ui": "DejaVu Sans",
           "Inter": "DejaVu Sans", "serif": "DejaVu Serif", "monospace": "DejaVu Sans Mono",
           "ui-monospace": "DejaVu Sans Mono", "math": "DejaVu Math TeX Gyre"}


def fontconfig(root, font_dir):
    aliases = "".join(f"<alias><family>{generic}</family><prefer><family>{family}</family></prefer></alias>"
                      for generic, family in ALIASES.items())
    (root / "fonts.conf").write_text(
        '<?xml version="1.0"?><!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">'
        f"<fontconfig><dir>{escape(str(font_dir))}</dir><cachedir>/b/font-cache</cachedir>{aliases}</fontconfig>")
    return hashlib.sha256((root / "fonts.conf").read_bytes()).hexdigest()


def prepare_firefox(root, firefox, bwrap, font_dir):
    """Playwright's Firefox (Juggler) under the same confinement as prepare_browser."""
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    for name in ("home", "tmp", "runtime", "font-cache"):
        (root / name).mkdir(exist_ok=True, mode=0o700)
    (root / "asound.conf").write_text("pcm.!default { type null }\n")
    (root / "passwd").write_text(f"fixture:x:{os.getuid()}:{os.getgid()}:Browser fixture:/b/home:/bin/sh\n")
    (root / "group").write_text(f"fixture:x:{os.getgid()}:\n")
    fonts = fontconfig(root, font_dir)
    bash = str(Path(shutil.which("bash")).resolve())
    path = ":".join(dict.fromkeys(str(Path(shutil.which(tool)).parent) for tool in ("bash", "env", "dirname")))
    common = [str(bwrap), "--unshare-user", "--unshare-ipc", "--unshare-pid", "--unshare-uts",
              "--uid", str(os.getuid()), "--gid", str(os.getgid()), "--die-with-parent", "--new-session",
              "--clearenv", "--ro-bind", "/nix/store", "/nix/store", "--bind", str(root), str(root),
              "--bind", str(root), "/b", "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/run", "--tmpfs", "/tmp",
              "--dir", "/etc", "--ro-bind", str(root / "passwd"), "/etc/passwd",
              "--ro-bind", str(root / "group"), "/etc/group", "--dir", "/bin", "--symlink", bash, "/bin/sh",
              "--chdir", "/b/home"]
    for name, value in {
        "HOME": "/b/home", "TMPDIR": "/b/tmp", "XDG_RUNTIME_DIR": "/b/runtime", "PATH": path,
        "LANG": "C.UTF-8", "ALSA_CONFIG_PATH": "/b/asound.conf", "PULSE_SERVER": "unix:/b/runtime/no-pulse-socket",
        "PIPEWIRE_REMOTE": "nuschl-fixture-no-pipewire", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/no-session-bus",
        "FONTCONFIG_FILE": "/b/fonts.conf", "FONTCONFIG_PATH": "/b", "MOZ_CRASHREPORTER_DISABLE": "1",
    }.items():
        common.extend(["--setenv", name, value])
    probe = subprocess.run(common + ["--", bash, "-c",
                           'test ! -e /dev/snd && test ! -e /dev/video0 && test ! -e /run/user && printf "devices-absent\\n"'],
                           capture_output=True, text=True, timeout=15, check=True)
    assert probe.stdout == "devices-absent\n"
    wrapper = root / "firefox-isolated"
    wrapper.write_text(f"#!{bash}\nexec {shlex.join(common + ['--', str(firefox)])} \"$@\"\n")
    wrapper.chmod(0o700)
    version = subprocess.run([str(wrapper), "--version"], capture_output=True, text=True, timeout=30, check=True)
    report = {"wrapper": str(wrapper), "browser_tmpdir": str(root / "tmp"), "firefox": str(firefox),
              "version": version.stdout.strip(), "command": common, "probe": probe.stdout.strip(),
              "font_dir": str(font_dir), "fontconfig_sha256": fonts,
              "input": "Firefox fake media stream (media.navigator.streams.fake); no host capture",
              "isolation": "fresh /dev lacks /dev/snd and camera; private /run, HOME, runtime; no host session/personal profile bind"}
    (root / "confinement.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for name in ("site", "relay", "murmur", "chromium", "firefox", "playwright", "font-dir"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--engines", default="chromium,firefox")
    parser.add_argument("--node", default="node")
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    font_dir = args.font_dir.resolve(strict=True)
    if not font_dir.is_relative_to("/nix/store"):
        parser.error("--font-dir must be a read-only Nix store directory")
    bwrap = shutil.which("bwrap")
    engines = args.engines.split(",")
    wrappers = {}
    if "chromium" in engines:
        report = prepare_browser(args.output / "browser-chromium", args.chromium, bwrap, font_dir)
        report["fontconfig_sha256"] = fontconfig(args.output / "browser-chromium", font_dir)
        (args.output / "browser-chromium/confinement.json").write_text(json.dumps(report, indent=2) + "\n")
        wrappers["chromium"] = report
    if "firefox" in engines:
        wrappers["firefox"] = prepare_firefox(args.output / "browser-firefox", args.firefox, bwrap, font_dir)
    root = args.output / "server"
    root.mkdir(exist_ok=True)
    murmur = Murmur(args.murmur, root, free_port())
    murmur.seed()
    murmur.start()
    port = free_port()
    config = args.output / "relay.toml"
    config.write_text(f'listen="127.0.0.1:{port}"\nports=["1024-65535"]\nallow_networks=["127.0.0.0/8"]\n')
    with (args.output / "relay.log").open("w") as log:
        relay = subprocess.Popen([str(args.relay), "--config", str(config)], stdout=log, stderr=log)
        try:
            def listening():
                with socket.socket() as probe:
                    return probe.connect_ex(("127.0.0.1", port)) == 0
            eventually(listening, "private relay failed to start")
            for engine, report in wrappers.items():
                subprocess.run([args.node, str(Path(__file__).with_suffix(".browser.mjs")), engine, str(args.site),
                                f"ws://127.0.0.1:{port}/relay", str(murmur.port), report["wrapper"],
                                str(args.playwright), str(args.output / engine)],
                               env={**os.environ, "TMPDIR": report["browser_tmpdir"]}, timeout=300, check=True)
        finally:
            relay.terminate()
            relay.wait(timeout=10)
            murmur.stop()


if __name__ == "__main__":
    main()
