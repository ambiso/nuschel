#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Hash local manifests exactly like flake.nix's crate2nixInputs expression.

Read the working tree directly: a Git-filtered flake omits new, untracked vendor
manifests even though crate2nix has already used them to generate the graph.
"""
import argparse
import hashlib
import os
from pathlib import Path
import sys


def fingerprint(root):
    root = Path(root)
    paths = [root / "Cargo.lock", root / "Cargo.toml"]
    for group in ("apps", "crates", "vendor"):
        # builtins.readDir reports symlinks as "symlink", including links to
        # directories. Only actual direct directories participate in Nix.
        with os.scandir(root / group) as entries:
            names = sorted((entry.name for entry in entries
                            if entry.is_dir(follow_symlinks=False)), key=os.fsencode)
        for name in names:
            manifest = root / group / name / "Cargo.toml"
            if manifest.exists():
                paths.append(manifest)
    result = hashlib.sha256()
    for path in paths:
        data = path.read_bytes()
        if b"\0" in data:
            raise ValueError(f"Nix cannot represent an input containing NUL: {path}")
        result.update(data)
    return result.hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[1])
    args = parser.parse_args()
    # Write only after every input has been read successfully. No flake source
    # copy, archive traversal or Cargo invocation is involved.
    sys.stdout.write(fingerprint(args.root))


if __name__ == "__main__":
    main()
