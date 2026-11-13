#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Touch behaviour of the packaged web client on an owned loopback Murmur (#202).

The voice toggles' pressed state and struck icons, the navigation drawer's
slide, the swipe gestures and the browser's back swipe, in confined headless
Chromium (web_audio.prepare_browser) at a phone viewport, with and without
reduced motion. Touches are DevTools touch events through Chromium's own input
pipeline, not script-made DOM events. Text and microphone input are synthetic.

Firefox is not run: Playwright's public API can only tap there, and a swipe
would need its private Juggler session. This does not certify Firefox, iOS
Safari, Android browsers, physical touch hardware or OS edge gestures.

Usage: web_touch.py --site SITE --relay RELAY_BIN --murmur MURMUR_BIN
       --chromium CHROMIUM_BIN --playwright PLAYWRIGHT_CORE_INDEX_MJS
       --font-dir FONTS --output DIR
"""
import argparse
import os
from pathlib import Path
import shutil
import socket
import subprocess

from local_murmur import Murmur, eventually, free_port
from web_audio import prepare_browser


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    for name in ("site", "relay", "murmur", "chromium", "playwright", "font-dir", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    browser = prepare_browser(output / "browser", args.chromium, shutil.which("bwrap"), args.font_dir)
    root = output / "server"
    root.mkdir(exist_ok=True)
    murmur = Murmur(args.murmur, root, free_port())
    murmur.seed()
    murmur.start()
    port = free_port()
    config = output / "relay.toml"
    config.write_text(f'listen="127.0.0.1:{port}"\nports=["1024-65535"]\nallow_networks=["127.0.0.0/8"]\n')
    with (output / "relay.log").open("w") as log:
        relay = subprocess.Popen([str(args.relay), "--config", str(config)], stdout=log, stderr=log)
        try:
            def listening():
                with socket.socket() as probe:
                    return probe.connect_ex(("127.0.0.1", port)) == 0
            eventually(listening, "private relay failed to start")
            subprocess.run(["node", str(Path(__file__).with_suffix(".browser.mjs")), str(args.site),
                            f"ws://127.0.0.1:{port}/relay", str(murmur.port), browser["wrapper"],
                            str(args.playwright), str(output / "chromium")],
                           env={**os.environ, "TMPDIR": browser["browser_tmpdir"]}, timeout=300, check=True)
        finally:
            relay.terminate()
            relay.wait(timeout=10)
            murmur.stop()


if __name__ == "__main__":
    main()
