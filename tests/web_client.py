#!/usr/bin/env python3
"""The web client's SDK in WebAssembly against a loopback stock Mumble server,
through the web relay (docs/web.md): TLS runs in the WebAssembly module, the
relay forwards ciphertext. Needs `just wasm-client` and Node.js.

Usage: web_client.py <binaries> --murmur <mumble-server> [--sdk web/app/src/sdk]
Only loopback is used.
"""
import argparse
from pathlib import Path
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
from local_murmur import Murmur, eventually, free_port  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--sdk", type=Path, default=ROOT / "web/app/src/sdk")
    parser.add_argument("--node", default="node")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="nuschl-web-client-") as temporary:
        root = Path(temporary)
        murmur = Murmur(args.murmur, root, free_port())
        murmur.seed()
        murmur.start()
        relay_port = free_port()
        config = root / "relay.toml"
        # Loopback is refused by default; this test allows it, and any port.
        config.write_text(f'listen = "127.0.0.1:{relay_port}"\nports = ["1024-65535"]\n'
                          'allow_networks = ["127.0.0.0/8"]\n')
        relay = subprocess.Popen([str(args.binaries / "nuschl-web-relay"), "--config", str(config)],
                                 stdout=subprocess.DEVNULL, stderr=(root / "relay.log").open("w"))
        try:
            import socket

            def listening():
                with socket.socket() as probe:
                    return probe.connect_ex(("127.0.0.1", relay_port)) == 0
            eventually(listening, "the relay did not start")
            result = subprocess.run([args.node, str(ROOT / "tests/web_client.mjs"), str(args.sdk),
                                     f"ws://127.0.0.1:{relay_port}/relay", "127.0.0.1", str(murmur.port)],
                                    capture_output=True, text=True, timeout=60)
            print(result.stdout, end="")
            print(result.stderr, end="", file=sys.stderr)
            assert result.returncode == 0, "the web client did not connect"
            assert "connected channels=" in result.stdout and "web-sdk" in result.stdout
            # The relay logs no payloads: nothing of the login is in its log.
            assert "web-sdk" not in (root / "relay.log").read_text()
            print("web client: connected through the relay with TLS in WebAssembly")
        finally:
            relay.terminate()
            relay.wait(timeout=10)
            murmur.stop()


if __name__ == "__main__":
    main()
