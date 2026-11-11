#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Run actual browser SDK WASM voice framing on private Murmur, without audio devices."""
import argparse
from pathlib import Path
import subprocess
import tempfile
from local_murmur import Murmur, eventually, free_port


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--sdk", required=True, type=Path)
    parser.add_argument("--murmur", required=True, type=Path)
    parser.add_argument("--node", default="node")
    parser.add_argument("--site",type=Path,help="also test the exact packaged App UI with a confined synthetic-audio browser")
    parser.add_argument("--wrapper",type=Path,help="required with --site: owned confined Chromium wrapper")
    parser.add_argument("--output",type=Path,help="required with --site: disk-backed proof directory")
    args = parser.parse_args()
    if args.site and not (args.wrapper and args.output):
        parser.error("--site requires --wrapper and --output")
    with tempfile.TemporaryDirectory(prefix="voice-transport-") as temporary:
        root = Path(temporary)
        murmur = Murmur(args.murmur, root, free_port())
        murmur.seed()
        murmur.start()
        port = free_port()
        config = root / "relay.toml"
        config.write_text(f'listen = "127.0.0.1:{port}"\nports = ["1024-65535"]\nallow_networks = ["127.0.0.0/8"]\n')
        with (root / "relay.log").open("w") as log:
            relay = subprocess.Popen([str(args.binaries / "nuschl-web-relay"), "--config", str(config)], stdout=log, stderr=log)
            try:
                import socket
                def listening():
                    with socket.socket() as probe:
                        return probe.connect_ex(("127.0.0.1", port)) == 0
                eventually(listening, "relay startup failed")
                subprocess.run([args.node, str(Path(__file__).with_suffix(".mjs")), str(args.sdk), f"ws://127.0.0.1:{port}/relay", "127.0.0.1", str(murmur.port)], check=True, timeout=90)
                if args.site:
                    args.output.mkdir(parents=True,exist_ok=True)
                    subprocess.run([args.node,str(Path(__file__).with_name("web_voice_ui.mjs")),str(args.site),f"ws://127.0.0.1:{port}/relay",str(murmur.port),str(args.wrapper),str(args.output)],check=True,timeout=150)
                assert "voice-wasm-one" not in (root / "relay.log").read_text()
            finally:
                relay.terminate()
                relay.wait(timeout=10)
                murmur.stop()


if __name__ == "__main__":
    main()
