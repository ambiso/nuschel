#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""The packaged web app's voice bitrate on an owned loopback Murmur.

A stock Murmur (its own default budget, a private D-Bus for live setConf),
the real nuschl-web-relay, and a bwrap-confined Chromium with a synthetic
microphone and no host audio devices (web_audio.prepare_browser) run
`bitrate_web.browser.mjs`. Nothing else is contacted.

    python3 tests/bitrate_web.py --site <web-app> --relay <nuschl-web-relay> \\
        --murmur <mumble-server> --chromium <chromium> --font-dir <fonts> \\
        --playwright <playwright-core> --output <directory>
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from bitrate_matrix import Bus  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from web_audio import prepare_browser  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for name in ("site", "relay", "murmur", "chromium", "font-dir", "playwright", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    browser = prepare_browser(output / "browser", args.chromium, shutil.which("bwrap"), args.font_dir)
    root = output / "server"
    root.mkdir(exist_ok=True)
    bus = Bus(root)
    murmur = Murmur(args.murmur, root, free_port())
    murmur.ini.write_text(murmur.ini.read_text().replace("bandwidth=72000\n", "dbus=session\n"))
    os.environ["DBUS_SESSION_BUS_ADDRESS"] = bus.address
    murmur.seed()
    murmur.start()
    port = free_port()
    config = output / "relay.toml"
    config.write_text(f'listen="127.0.0.1:{port}"\nports=["1024-65535"]\nallow_networks=["127.0.0.0/8"]\n')
    try:
        with (output / "relay.log").open("w") as log:
            relay = subprocess.Popen([str(args.relay), "--config", str(config)], stdout=log, stderr=log)
            try:
                def listening():
                    with socket.socket() as probe:
                        return probe.connect_ex(("127.0.0.1", port)) == 0
                eventually(listening, "private relay failed to start")
                subprocess.run([
                    "node", str(Path(__file__).with_suffix(".browser.mjs")), str(args.site),
                    f"ws://127.0.0.1:{port}/relay", str(murmur.port), browser["wrapper"], str(output),
                    str(args.playwright / "index.mjs"), bus.address,
                ], env={**os.environ, "TMPDIR": browser["browser_tmpdir"]}, timeout=300, check=True)
            finally:
                relay.terminate()
                relay.wait(timeout=10)
    finally:
        murmur.stop()
        bus.stop()
    (output / "browser-confinement.json").write_text(json.dumps(browser, indent=2))


if __name__ == "__main__":
    main()
