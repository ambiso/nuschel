#!/usr/bin/env python3
"""Run a test script against a freshly seeded loopback nuscheld.

Usage: with_local_nuscheld.py <binaries> [--e2ee] <script> [args...]; the literal
argument PORT is replaced by the server's port. The counterpart of
with_local_murmur.py: the same scripts compare stock Murmur and Nuschel. With
--e2ee the server is seeded in compatible mode (stock clients create the
channels), then runs the script in end-to-end mode (`security = "e2ee"`).
"""
import subprocess
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from local_nuscheld import Nuscheld, free_port  # noqa: E402


def main():
    binaries, *rest = sys.argv[1:]
    e2ee = rest[:1] == ["--e2ee"]
    script, *rest = rest[1:] if e2ee else rest
    with tempfile.TemporaryDirectory(prefix="nuschel-nuscheld-") as temporary:
        port = free_port()
        server = Nuscheld(Path(binaries), Path(temporary), port)
        server.seed()
        if e2ee:
            server.security = "e2ee"
            server.write_config(message_limit=1, message_burst=5)
        server.start()
        try:
            arguments = [argument.replace("PORT", str(port)) for argument in rest]
            status = subprocess.call([sys.executable, script, *arguments])
        finally:
            server.stop()
        if status != 0:
            print(Path(temporary, "nuscheld.log").read_text()[-4000:], file=sys.stderr)
        sys.exit(status)


if __name__ == "__main__":
    main()
