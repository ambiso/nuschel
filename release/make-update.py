#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Prepare exact-byte hybrid-v2 update assets; never enroll trust or create keys.

Unsigned preparation is allowed for review. Publication must provide --sign-key,
use the enrolled --public-key, and verify the result before uploading any asset.
"""
import argparse
import hashlib
import json
from pathlib import Path
import re
import shutil
import subprocess
import time
from urllib.parse import quote, urlsplit
import ipaddress

PLATFORMS = {"windows-x86_64", "macos-universal"}
PUBLIC_KEY = Path(__file__).resolve().parents[1] / "crates/update/update-trust.json"


def safe_path(path):
    if not path or len(path) > 240 or not re.fullmatch(r"[A-Za-z0-9._/ -]+", path):
        raise ValueError("unsupported installation path")
    for part in path.split("/"):
        base = part.split(".", 1)[0].upper()
        if (part in {"", ".", ".."} or part.endswith((".", " ")) or
                part.lower() in {".nuschl-update", ".nuschl-old"} or
                base in {"CON", "PRN", "AUX", "NUL"} or
                re.fullmatch(r"(?:COM|LPT)[1-9]", base)):
            raise ValueError("unsafe installation path")


def files_of(root):
    root = Path(root)
    if not root.is_dir():
        raise ValueError("installation must be a directory")
    found = []
    for path in sorted(root.rglob("*")):
        if path.is_symlink():
            raise ValueError("symlinked installations need explicit format support")
        if path.is_file():
            relative = path.relative_to(root).as_posix()
            safe_path(relative)
            found.append((relative, path))
    if not 1 <= len(found) <= 256:
        raise ValueError("installation must contain 1..256 files")
    return found


def validate_base(base):
    parsed = urlsplit(base)
    local = False
    try:
        local = ipaddress.ip_address(parsed.hostname or "").is_loopback
    except ValueError:
        pass
    if (len(base) > 1600 or any(ord(c) < 33 for c in base) or not parsed.hostname or
            parsed.username is not None or parsed.password is not None or parsed.fragment or parsed.query or
            not (parsed.scheme == "https" or parsed.scheme == "http" and local)):
        raise ValueError("HTTPS asset base required (literal-loopback HTTP only for fixtures)")
    return base if base.endswith("/") else base + "/"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--base-url", required=True)
    parser.add_argument("--platform", action="append", default=[], metavar="NAME=DIR")
    parser.add_argument("--notes")
    parser.add_argument("--released", type=int, default=int(time.time()))
    parser.add_argument("--expires", type=int, help="Unix seconds; default 30 days after release")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--sign-key", type=Path)
    parser.add_argument("--public-key", type=Path, default=PUBLIC_KEY)
    parser.add_argument("--signer", default="nuschl-update-sign")
    args = parser.parse_args()
    base = validate_base(args.base_url)
    expires = args.expires if args.expires is not None else args.released + 30 * 86400
    now = int(time.time())
    if (args.released < 0 or args.released > now + 300 or expires <= max(now, args.released) or
            expires - args.released > 30 * 86400):
        raise ValueError("invalid release/expiry interval")
    if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", args.version):
        raise ValueError("version must be semantic version syntax")
    if len(args.version) > 100 or args.notes and len(args.notes.encode()) > 8192:
        raise ValueError("oversized version/notes")
    if args.sign_key:
        # Pair matching precedes copying/signing; no secret appears in argv/logs.
        subprocess.run([args.signer, "key-status", "--secret-file", str(args.sign_key),
                        "--public-file", str(args.public_key)], check=True)
    args.output.mkdir(parents=True, exist_ok=True)
    platforms, assets = {}, set()
    for spec in args.platform:
        name, separator, directory = spec.partition("=")
        if not separator or name not in PLATFORMS or name in platforms:
            raise ValueError("unknown/duplicate platform")
        entries, paths = [], set()
        for relative, path in files_of(directory):
            asset = f"{name}--{relative.replace('/', '__')}"
            if relative.lower() in paths or asset.lower() in assets or len(asset) > 240:
                raise ValueError("colliding or oversized release path/asset")
            paths.add(relative.lower())
            assets.add(asset.lower())
            size = path.stat().st_size
            if size > 512 * 1024 * 1024:
                raise ValueError("installation file exceeds 512MiB")
            digest = hashlib.sha256()
            target = args.output / asset
            with path.open("rb") as source, target.open("xb") as out:
                while block := source.read(1024 * 1024):
                    digest.update(block)
                    out.write(block)
            if target.stat().st_size != size:
                raise ValueError("installation changed during packaging")
            entries.append({"path": relative, "url": base + quote(asset, safe=""),
                            "sha256": digest.hexdigest(), "size": size})
        platforms[name] = entries
    if not platforms:
        raise ValueError("at least one platform is required")
    manifest = {"format": 2, "algorithm": "ed25519+mldsa44", "version": args.version,
                "released": args.released, "expires": expires, "platforms": platforms}
    if args.notes:
        manifest["notes"] = args.notes
    raw = (json.dumps(manifest, indent=1) + "\n").encode()
    if len(raw) > 256 * 1024:
        raise ValueError("manifest exceeds 256KiB")
    target = args.output / "update.json"
    with target.open("xb") as out:
        out.write(raw)
    if args.sign_key:
        signed = args.output / "update.json.sig"
        subprocess.run([args.signer, "sign", "--secret-file", str(args.sign_key),
                        "--public-file", str(args.public_key), "--manifest", str(target),
                        "--signature-file", str(signed)], check=True)
        subprocess.run([args.signer, "verify", "--public-file", str(args.public_key),
                        "--manifest", str(target), "--signature-file", str(signed)], check=True)
    print(json.dumps({name: len(entries) for name, entries in platforms.items()}))


if __name__ == "__main__":
    main()
