#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Exercise the exact IPC musicbot launcher against owned file/network canaries.

Compile musicbot_sandbox.rs with the packaged bot's nuschl-ipc rlib and its
normal dependency search paths; this script never opens host devices/profiles.
This proves the launch profile, not the whole decoder or systemd integration.
"""
import argparse
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("probe", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="sandbox-", dir=args.output) as temporary:
        canary = Path(temporary) / "owned-credential-canary"
        canary.write_text("public fixture sentinel\n")
        canary.chmod(0o600)
        with socket.socket() as listener:
            listener.bind(("127.0.0.1", 0))
            listener.listen(2)
            listener.settimeout(0.1)
            port = listener.getsockname()[1]
            env = {"PATH": os.environ.get("PATH", ""), "MUSICBOT_TEST_SECRET": "public-test-value"}
            result = subprocess.run([str(args.probe.resolve()), sys.executable, str(canary), str(port)],
                                    env=env, capture_output=True, text=True, timeout=10)
            (args.output / "stderr.txt").write_text(result.stderr)
            assert result.returncode == 0, result.stderr
            proof = json.loads(result.stdout)
            try:
                connection, _ = listener.accept()
                connection.close()
                accepted = 1
            except TimeoutError:
                accepted = 0
            assert proof["file_denied"] and proof["network_denied"]
            assert proof["credential_absent"] and proof["private_tmp"]
            assert proof["cwd"] == "/tmp" and accepted == 0
            proof["owned_listener_connections"] = accepted
            proof["scope"] = "exact musicbot launcher; owned canaries; no codec/service proof"
            (args.output / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
            print(json.dumps(proof))


if __name__ == "__main__":
    main()
