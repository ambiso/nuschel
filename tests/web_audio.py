#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Synthetic browser voice on a private Murmur, with physical audio inaccessible.

The Chromium wrapper mounts only /nix/store and its owned browser scratch. /dev
is freshly populated without sound/camera devices; host /run and personal files
are absent. ALSA defaults to null and Pulse/PipeWire use nonexistent private
sockets. Fake microphone input is an owned 997 Hz WAV, never host capture.
"""
import argparse
import array
import hashlib
import json
import math
import os
from pathlib import Path
import shlex
import shutil
import socket
import subprocess
import sys
import time
import wave
from xml.sax.saxutils import escape

from local_murmur import Murmur, eventually, free_port

ROOT = Path(__file__).resolve().parent.parent


def prepare_browser(root, chromium, bwrap, font_dir=None):
    root.mkdir(parents=True, exist_ok=True, mode=0o700)
    for name in ("home", "tmp", "runtime", "font-cache"):
        (root / name).mkdir(exist_ok=True, mode=0o700)
    if font_dir is not None:
        font_dir = font_dir.resolve(strict=True)
        if not font_dir.is_dir() or not font_dir.is_relative_to("/nix/store"):
            raise ValueError("Fixture fonts must be an explicit read-only Nix store directory.")
        (root / "fonts.conf").write_text(
            '<?xml version="1.0"?><!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">'
            '<fontconfig><dir>' + escape(str(font_dir)) + '</dir>'
            '<cachedir>/b/font-cache</cachedir></fontconfig>')
    (root / "asound.conf").write_text("pcm.!default { type null }\n")
    (root / "passwd").write_text(f"fixture:x:{os.getuid()}:{os.getgid()}:Browser fixture:/b/home:/bin/sh\n")
    (root / "group").write_text(f"fixture:x:{os.getgid()}:\n")
    wav = root / "synthetic-997hz.wav"
    samples = array.array("h", (round(32767 * (0.20 + 0.07 * math.sin(2 * math.pi * 5 * i / 48000))
                                     * math.sin(2 * math.pi * 997 * i / 48000)) for i in range(48000 * 30)))
    if sys.byteorder != "little":
        samples.byteswap()
    with wave.open(str(wav), "wb") as stream:
        stream.setnchannels(1)
        stream.setsampwidth(2)
        stream.setframerate(48000)
        stream.writeframes(samples.tobytes())
    bash = str(Path(shutil.which("bash")).resolve())
    path = ":".join(dict.fromkeys(str(Path(shutil.which(tool)).parent) for tool in ("bash", "env", "dirname")))
    common = [str(bwrap), "--unshare-user", "--unshare-ipc", "--unshare-pid", "--unshare-uts",
              "--uid", str(os.getuid()), "--gid", str(os.getgid()), "--die-with-parent", "--new-session",
              "--clearenv", "--ro-bind", "/nix/store", "/nix/store", "--bind", str(root), str(root),
              "--bind", str(root), "/b",
              "--dev", "/dev", "--proc", "/proc", "--tmpfs", "/run", "--tmpfs", "/tmp",
              "--dir", "/etc", "--ro-bind", str(root / "passwd"), "/etc/passwd",
              "--ro-bind", str(root / "group"), "/etc/group", "--dir", "/bin", "--symlink", bash, "/bin/sh",
              "--chdir", "/b/home"]
    for name, value in {
        "HOME": "/b/home", "TMPDIR": "/b/tmp", "XDG_RUNTIME_DIR": "/b/runtime",
        "PATH": path, "LANG": "C.UTF-8", "ALSA_CONFIG_PATH": "/b/asound.conf",
        "PULSE_SERVER": "unix:/b/runtime/no-pulse-socket",
        "PIPEWIRE_REMOTE": "nuschl-fixture-no-pipewire", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/no-session-bus",
    }.items():
        common.extend(["--setenv", name, value])
    if font_dir is not None:
        common.extend(["--setenv", "FONTCONFIG_FILE", "/b/fonts.conf",
                       "--setenv", "FONTCONFIG_PATH", "/b"])
    probe = subprocess.run(common + ["--", bash, "-c",
                           'test ! -e /dev/snd && test ! -e /dev/video0 && test ! -S "$XDG_RUNTIME_DIR/pulse/native" && '
                           'test ! -S "$XDG_RUNTIME_DIR/pipewire-0" && test ! -e /run/user && printf "audio-devices-absent\\n"'],
                           capture_output=True, text=True, timeout=15, check=True)
    assert probe.stdout == "audio-devices-absent\n"
    command = common + ["--", str(chromium), "--no-sandbox", "--disable-dev-shm-usage",
                        "--use-fake-device-for-media-stream", "--use-fake-ui-for-media-stream",
                        "--use-file-for-fake-audio-capture=/b/synthetic-997hz.wav"]
    wrapper = root / "chromium-isolated"
    wrapper.write_text(f"#!{bash}\nexec {shlex.join(command)} \"$@\"\n")
    wrapper.chmod(0o700)
    version = subprocess.run([str(wrapper), "--version"], capture_output=True, text=True, timeout=15, check=True)
    report = {"wrapper": str(wrapper), "browser_tmpdir": str(root / "tmp"), "chromium": str(chromium),
              "version": version.stdout.strip(), "command": command, "probe": probe.stdout.strip(),
              "font_dir": str(font_dir) if font_dir is not None else None,
              "synthetic_wav_sha256": hashlib.sha256(wav.read_bytes()).hexdigest(),
              "input": "owned mono 48 kHz 997 Hz WAV; browser fake device only",
              "output": "ALSA null/private nonexistent Pulse+PipeWire sockets; actual test requests AudioContext null sink",
              "isolation": "fresh /dev lacks /dev/snd and camera; private /run, HOME, runtime; no host session/personal profile bind"}
    (root / "confinement.json").write_text(json.dumps(report, indent=2) + "\n")
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--chromium", required=True, type=Path)
    parser.add_argument("--bwrap", type=Path, default=shutil.which("bwrap"))
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--font-dir", type=Path, help="Explicit read-only Nix store fonts for realistic browser text layout.")
    parser.add_argument("--murmur", type=Path)
    parser.add_argument("--relay", type=Path)
    parser.add_argument("--sdk", type=Path)
    parser.add_argument("--codec", type=Path)
    parser.add_argument("--sdk-source", type=Path, default=ROOT / "web/app/src")
    parser.add_argument("--playwright", type=Path)
    parser.add_argument("--opus-library", type=Path)
    parser.add_argument("--node", default="node")
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    confinement = prepare_browser(args.output / "browser", args.chromium, args.bwrap, args.font_dir)
    if args.prepare_only:
        print(json.dumps(confinement))
        return
    for field in ("murmur", "relay", "sdk", "codec", "playwright"):
        if getattr(args, field) is None:
            parser.error(f"--{field} is required unless --prepare-only")
    server_root = args.output / "server"
    server_root.mkdir(exist_ok=True)
    murmur = Murmur(args.murmur, server_root, free_port())
    murmur.seed()
    murmur.start()
    relay_port = free_port()
    config = args.output / "relay.toml"
    config.write_text(f'listen="127.0.0.1:{relay_port}"\nports=["1024-65535"]\nallow_networks=["127.0.0.0/8"]\n')
    with (args.output / "relay.log").open("w") as log:
        relay = subprocess.Popen([str(args.relay), "--config", str(config)], stdout=log, stderr=log)
        try:
            def listening():
                with socket.socket() as client:
                    return client.connect_ex(("127.0.0.1", relay_port)) == 0
            eventually(listening, "private voice relay startup failed")
            live = {"murmur_port": murmur.port, "relay": f"ws://127.0.0.1:{relay_port}/relay", **confinement}
            (args.output / "fixture-live.json").write_text(json.dumps(live, indent=2) + "\n")
            env = {**os.environ, "TMPDIR": confinement["browser_tmpdir"]}
            command = [args.node, str(ROOT / "tests/web_audio.browser.mjs"), str(ROOT), str(args.sdk_source),
                       str(args.sdk), str(args.codec), live["relay"], str(murmur.port), confinement["wrapper"],
                       str(args.playwright), str(args.output)]
            subprocess.run(command, env=env, check=True, timeout=120)
            if args.opus_library:
                from mumble_client import ReferenceOpus
                packets = json.loads((args.output / "wire-opus.json").read_text())
                pcm = ReferenceOpus(args.opus_library).decode(packets)
                stable = pcm[1920:]
                def power(frequency):
                    real = sum(v * math.cos(2 * math.pi * frequency * i / 48000) for i, v in enumerate(stable))
                    imaginary = sum(v * math.sin(2 * math.pi * frequency * i / 48000) for i, v in enumerate(stable))
                    return real * real + imaginary * imaginary
                carrier = power(997)
                assert carrier > 0
                for wrong in (400, 440, 800, 1200):
                    assert carrier > power(wrong) * 5, f"fake WAV carrier was lost relative to {wrong} Hz"
                report = {"frames": len(packets), "samples": len(pcm), "carrier_hz": 997,
                          "reference_decoder": str(args.opus_library), "finite": all(math.isfinite(v) for v in pcm)}
                assert report["finite"]
                (args.output / "reference-proof.json").write_text(json.dumps(report, indent=2) + "\n")
        finally:
            relay.terminate()
            relay.wait(timeout=10)
            murmur.stop()


if __name__ == "__main__":
    main()
