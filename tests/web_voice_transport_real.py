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
    args = parser.parse_args()
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
                assert "voice-wasm-one" not in (root / "relay.log").read_text()
            finally:
                relay.terminate()
                relay.wait(timeout=10)
                murmur.stop()


if __name__ == "__main__":
    main()
