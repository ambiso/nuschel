#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""The packaged web app's emoji through an owned loopback Murmur and relay.

Twemoji in messages (`™` stays text, `™️` is an image), the picker beside the
composer and what it inserts, in the confined Chromium of tests/web_math.py
(prepare it first with `web_math.py --prepare-browser`):

    python3 tests/web_math.py --prepare-browser --chromium CHROMIUM --font-dir FONTS --output SCRATCH
    python3 tests/web_emoji.py --site <web-client> --relay <nuschl-web-relay> --murmur <mumble-server> \\
        --wrapper SCRATCH/browser/<wrapper> --browser-tmp SCRATCH/browser/tmp --playwright <playwright-core> \\
        --output SCRATCH/emoji
"""
import argparse
from pathlib import Path
import os
import socket
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from local_murmur import Murmur, eventually, free_port  # noqa: E402


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for name in ("site", "relay", "murmur", "wrapper", "browser-tmp", "playwright", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
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
            subprocess.run([
                "node", str(Path(__file__).with_suffix(".browser.mjs")), str(args.site),
                f"ws://127.0.0.1:{port}/relay", str(murmur.port), str(args.wrapper),
                str(args.playwright), str(args.output),
            ], env={**os.environ, "TMPDIR": str(args.browser_tmp)}, timeout=180, check=True)
        finally:
            relay.terminate()
            relay.wait(timeout=10)
            murmur.stop()


if __name__ == "__main__":
    main()
