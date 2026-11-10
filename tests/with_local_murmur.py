#!/usr/bin/env python3
"""Run a test script against a freshly seeded loopback stock server.

Usage: with_local_murmur.py <mumble-server|murmurd> <script> [args...]; the
literal argument PORT is replaced by the server's port. Used for older server
versions in the VM and for local development runs.
"""
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from local_murmur import Murmur, free_port  # noqa: E402


def main():
    binary, script, *rest = sys.argv[1:]
    with tempfile.TemporaryDirectory(prefix="nuschel-murmur-") as temporary:
        port = free_port()
        murmur = Murmur(Path(binary), Path(temporary), port)
        murmur.seed()
        murmur.start()
        try:
            arguments = [argument.replace("PORT", str(port)) for argument in rest]
            sys.exit(subprocess.call([sys.executable, script, *arguments]))
        finally:
            murmur.stop()


if __name__ == "__main__":
    main()
