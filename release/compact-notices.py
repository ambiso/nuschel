#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Losslessly combine inspected notice bytes; never resolve a missing license.
The input inventory remains separately inspectable. This format carries byte
ranges and original-file hashes so consumers verify every notice, not a summary.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import shutil

MAX_PAYLOAD = 128 * 1024 * 1024
MAX_NOTICE = 4 * 1024 * 1024


def relative(value):
    path = PurePosixPath(value)
    if not value or path.is_absolute() or "\\" in value or any(p in ("", ".", "..") for p in value.split("/")):
        raise ValueError("unsafe notice path")
    return path


def compact(source, destination):
    if destination.exists():
        raise ValueError("notice output must be new")
    report_path = source / "THIRD-PARTY.json"
    if report_path.stat().st_size > 8 * 1024 * 1024:
        raise ValueError("oversized source inventory")
    report = json.loads(report_path.read_bytes())
    if report.get("format") != 1:
        raise ValueError("expected original inspectable inventory")
    destination.mkdir(parents=True)
    index, folded = {}, set()
    payload_path = destination / "THIRD-PARTY.txt"
    with payload_path.open("xb") as payload:
        payload.write(b"Nuschl third-party notices\nExact source-file bytes follow each header.\nSee THIRD-PARTY.json for provenance, scope and byte-range/hash verification.\n")
        for path in sorted(source.rglob("*")):
            if path.is_symlink():
                raise ValueError("symlink in notice inventory")
            if not path.is_file() or path.relative_to(source).as_posix() in {"THIRD-PARTY.json", "Cargo.lock", "README.txt"}:
                continue
            name = path.relative_to(source).as_posix()
            relative(name)
            if name.casefold() in folded or len(index) >= 16384 or path.stat().st_size > MAX_NOTICE:
                raise ValueError("colliding/excessive/oversized notice")
            folded.add(name.casefold())
            data = path.read_bytes()
            if not data or b"\0" in data:
                raise ValueError("empty or binary notice")
            data.decode("utf-8")
            payload.write(("\n===== " + name + " =====\n").encode())
            index[name] = {"offset": payload.tell(), "size": len(data), "sha256": hashlib.sha256(data).hexdigest()}
            payload.write(data)
            payload.write(b"\n===== end notice =====\n")
            if payload.tell() > MAX_PAYLOAD:
                raise ValueError("combined notices exceed 128MiB")
    def reference(name, sha):
        item = index.get(str(relative(name)))
        if item is None or item["sha256"] != sha:
            raise ValueError("source notice reference/hash is missing or inconsistent")
    for package in report["packages"]:
        directory = "crates/" + package["name"] + "-" + package["version"] + "/"
        for file in package["files"]:
            reference(directory + file["path"], file["sha256"])
    for component in report["external_components"]:
        directory = "external/" + component["name"] + "/"
        for file in component["files"]:
            reference(directory + file["path"], file["sha256"])
    if not index:
        raise ValueError("no real notice bytes")
    report["format"] = 2
    report["notice_payload"] = {"path": "THIRD-PARTY.txt", "size": payload_path.stat().st_size,
                                "sha256": hashlib.sha256(payload_path.read_bytes()).hexdigest()}
    report["notice_files"] = index
    raw = (json.dumps(report, sort_keys=True, indent=2) + "\n").encode()
    if len(raw) > 8 * 1024 * 1024:
        raise ValueError("compact provenance exceeds 8MiB")
    (destination / "THIRD-PARTY.json").write_bytes(raw)
    for name in ("Cargo.lock", "README.txt"):
        path = source / name
        if path.is_file():
            shutil.copyfile(path, destination / name)
    # No unresolved entry is erased or recategorized by compaction.
    return report


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args()
    compact(args.source, args.destination)
