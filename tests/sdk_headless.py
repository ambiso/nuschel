#!/usr/bin/env python3
"""The client SDK without a user interface, against nuscheld: three people
connect, register and meet in a channel; one sends a message the other
receives, edits it and the other sees the edit; the third, connecting
afterwards, loads the channel's history from the server with the edit in it
(crates/client/examples/headless.rs).

    cargo build -p nuschel-client --example headless
    python3 tests/with_local_nuscheld.py <binaries> tests/sdk_headless.py <binaries> --port PORT

The example runs from a copy beside the workers in <binaries>, as the
desktop does, so messages are rendered and kept by the real workers.
"""

import argparse
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    parser.add_argument("--example", type=Path, help="default: <binaries>/examples/headless")
    args = parser.parse_args()
    example = args.example or args.binaries / "examples" / "headless"
    with tempfile.TemporaryDirectory(prefix="nuschel-sdk-headless-") as temporary:
        # Workers are found beside the executable.
        run = Path(temporary)
        shutil.copy2(example, run / "headless")
        for worker in args.binaries.glob("nuschel-*-worker"):
            (run / worker.name).symlink_to(worker.resolve())
        result = subprocess.run([run / "headless", args.host, str(args.port)], timeout=300)
    sys.exit(result.returncode)


if __name__ == "__main__":
    main()
