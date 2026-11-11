#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Build the signed update manifest and the flat release assets.

    make-update.py --version 0.2.0 --base-url https://github.com/ambiso/nuschel/releases/download/v0.2.0/ \
        --platform windows-x86_64=result-windows/bin --platform macos-universal=Nuschel.app \
        --output dist [--sign-key path/to/minisign.secret]

Every file of each installation becomes one release asset named
`<platform>--<path with / as __>`; `update.json` lists them with size and
SHA-256, and `update.json.minisig` signs it (rsign2/minisign format). The
updater in the app verifies the signature with the key in crates/update/update-key.pub.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys


def files_of(root):
    root = Path(root)
    if root.is_file():
        return [(root.name, root)]
    found = []
    for path in sorted(root.rglob("*")):
        if path.is_file() and not path.is_symlink():
            found.append((path.relative_to(root).as_posix(), path))
    return found


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--version", required=True)
    parser.add_argument("--base-url", required=True, help="where the assets will be downloadable")
    parser.add_argument("--platform", action="append", default=[], metavar="NAME=DIR")
    parser.add_argument("--notes")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sign-key", type=Path, help="minisign secret key (unencrypted)")
    parser.add_argument("--rsign", default=shutil.which("rsign") or "rsign")
    args = parser.parse_args()
    base = args.base_url if args.base_url.endswith("/") else args.base_url + "/"
    args.output.mkdir(parents=True, exist_ok=True)
    platforms = {}
    for spec in args.platform:
        name, _, directory = spec.partition("=")
        entries = []
        for relative, path in files_of(directory):
            asset = f"{name}--{relative.replace('/', '__')}"
            data = path.read_bytes()
            (args.output / asset).write_bytes(data)
            entries.append({"path": relative, "url": base + asset,
                            "sha256": hashlib.sha256(data).hexdigest(), "size": len(data)})
        if not entries:
            sys.exit(f"{name}: no files in {directory}")
        platforms[name] = entries
    manifest = {"version": args.version, "platforms": platforms}
    if args.notes:
        manifest["notes"] = args.notes
    target = args.output / "update.json"
    target.write_text(json.dumps(manifest, indent=1) + "\n")
    if args.sign_key:
        subprocess.run([args.rsign, "sign", "-W", "-s", str(args.sign_key), "-x", str(target) + ".minisig",
                        "-t", f"nuschel {args.version}", str(target)], check=True, stdout=subprocess.DEVNULL)
    print(json.dumps({name: len(entries) for name, entries in platforms.items()}))


if __name__ == "__main__":
    main()
