#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Validate same-run immutable build evidence before exposing signing secrets.

No network, execution of artifact programs, trust enrollment, or secret access.
Only regular files/directories are accepted; links and special tar members fail.
Missing runtime attribution is a publication blocker, not a legal assurance.
"""
import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import tarfile
import tomllib

ROOTS = {"windows-x86_64": "windows", "macos-universal": "Nuschl.app"}


def unique(pairs):
    out = {}
    for key, value in pairs:
        if key in out:
            raise ValueError("duplicate JSON field")
        out[key] = value
    return out


def load(path, limit=16384):
    if not path.is_file() or path.is_symlink() or path.stat().st_size > limit:
        raise ValueError("missing, linked or oversized JSON evidence")
    return json.loads(path.read_bytes(), object_pairs_hook=unique)


def digest(path):
    result = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            result.update(block)
    return result.hexdigest()


def checked_relative(value):
    path = PurePosixPath(value)
    if (not value or len(value) > 512 or "\\" in value or ":" in value or path.is_absolute() or
            any(part in {"", ".", ".."} for part in value.split("/"))):
        raise ValueError("unsafe artifact path")
    return path


def verify_notices(root, policy):
    report = load(root / "THIRD-PARTY.json", 8 * 1024 * 1024)
    if (report.get("format") != 1 or report.get("unresolved") != [] or
            report.get("roots_by_target") != policy):
        raise ValueError("missing/incomplete/mismatched runtime attribution scope")
    source = Path(__file__).resolve().parents[1]
    workspace = {tomllib.loads(path.read_text())["package"]["name"]
                 for directory in ("crates", "apps") for path in (source / directory).glob("*/Cargo.toml")}
    packages = report.get("packages")
    if not isinstance(packages, list) or not 1 <= len(packages) <= 8192:
        raise ValueError("missing bounded runtime package inventory")
    required = {(item["target"], name) for item in policy for name in item["packages"]}
    required.update((item["target"], name) for item in policy
                    if any("update-worker/" in f for f in item.get("features", []))
                    for name in ("ed25519-dalek", "ml-dsa"))
    found = set()
    references = set()
    def check_files(directory, files):
        if not isinstance(files, list) or len(files) > 4096:
            raise ValueError("invalid notice file list")
        for entry in files:
            relative = checked_relative(entry["path"])
            path = directory / relative
            if len(references) >= 16384 or path in references:
                raise ValueError("duplicate or excessive notice references")
            if not re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]) or digest(path) != entry["sha256"]:
                raise ValueError("bundled notice digest mismatch")
            references.add(path)
    for package in packages:
        name, version = package["name"], package["version"]
        if not re.fullmatch(r"[A-Za-z0-9_.+-]{1,128}", name + "-" + version):
            raise ValueError("invalid notice package identity")
        for target in package["targets"]:
            found.add((target, name))
        if type(package["first_party"]) is not bool or (package["first_party"] and
                (name not in workspace or package.get("license") != "EUPL-1.2" or package.get("source") != "project-path")):
            raise ValueError("self-asserted first-party license exemption")
        if not package["first_party"] and not package["files"]:
            raise ValueError("missing third-party notice text")
        check_files(root / "crates" / (name + "-" + version), package["files"])
    if not required <= found:
        raise ValueError("runtime roots or hybrid engines absent from attribution inventory")
    components = report.get("external_components", [])
    expected = {"rust-standard-library", "mingw-w64-runtime", "gcc-runtime",
                "newcm-math-font", "syntect-default-assets", "web-runtime-assets"}
    if not expected <= {item["name"] for item in components}:
        raise ValueError("required native/font/web runtime notice families absent")
    for item in components:
        checked_relative(item["name"])
        if not item.get("files"):
            raise ValueError("missing native/asset notice text")
        check_files(root / "external" / item["name"], item["files"])
    if not references:
        raise ValueError("no actual notice payload")


def prepare(directory, platform, output, *, repository, commit, run_id, attempt, version, policy):
    if platform not in ROOTS:
        raise ValueError("unsupported platform")
    info = load(directory / "build-info.json")
    expected = {"repository": repository, "commit": commit, "run_id": run_id,
                "platform": platform, "version": version, "host": "x86_64-linux",
                "os_code_signed": False, "native_platform_runtime_tested_by_this_job": False,
                "macos_ad_hoc_signature": platform == "macos-universal"}
    if any(info.get(key) != value or type(info.get(key)) is not type(value) for key, value in expected.items()):
        raise ValueError("artifact source/run/platform provenance mismatch")
    prior = info.get("run_attempt", "")
    if not isinstance(prior, str) or not re.fullmatch(r"[1-9][0-9]{0,8}", prior) or not 1 <= int(prior) <= attempt:
        raise ValueError("invalid/future artifact run attempt")
    sums_path = directory / "SHA256SUMS"
    if not sums_path.is_file() or sums_path.is_symlink() or sums_path.stat().st_size > 4096:
        raise ValueError("missing bounded artifact checksums")
    sums = {}
    for line in sums_path.read_text().splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  (install.tar|build-info.json)", line)
        if not match or match[2] in sums:
            raise ValueError("ambiguous artifact checksums")
        sums[match[2]] = match[1]
    if set(sums) != {"install.tar", "build-info.json"}:
        raise ValueError("incomplete artifact checksums")
    for name, sha in sums.items():
        path = directory / name
        if path.is_symlink() or not path.is_file() or path.stat().st_size > 4 * 1024**3 or digest(path) != sha:
            raise ValueError("artifact digest/size mismatch")
    # Preflight every member before extracting anything. A fixed root and
    # regular-only archive eliminate symlink/hardlink ordering and traversal.
    with tarfile.open(directory / "install.tar", "r:") as archive:
        members, names, total, file_count = [], set(), 0, 0
        for member in archive:
            if len(members) >= 16384:
                raise ValueError("too many archive members")
            relative = checked_relative(member.name.rstrip("/"))
            if relative.parts[0] != ROOTS[platform] or not (member.isfile() or member.isdir()):
                raise ValueError("foreign root, link or special archive member")
            if member.name.lower().rstrip("/") in names or member.size > 512 * 1024**2:
                raise ValueError("duplicate/oversized archive member")
            names.add(member.name.lower().rstrip("/"))
            if member.isfile():
                file_count += 1
                if file_count > 256:
                    raise ValueError("installation exceeds updater's 256-file bound; compact notices without dropping them")
            total += member.size
            if total > 4 * 1024**3:
                raise ValueError("oversized installation")
            members.append(member)
        destination = output / ROOTS[platform]
        if destination.exists():
            raise ValueError("installation destination already exists")
        output.mkdir(parents=True, exist_ok=True)
        archive.extractall(output, members=members, filter="data")
    root = output / ROOTS[platform]
    binaries = root if platform == "windows-x86_64" else root / "Contents/MacOS"
    suffix = ".exe" if platform == "windows-x86_64" else ""
    for name in ("nuschl", "nuschl-update-verify-ed25519", "nuschl-update-verify-mldsa44"):
        if not (binaries / (name + suffix)).is_file():
            raise ValueError("missing application or fixed hybrid verifier")
    notices = root / "licenses" if platform == "windows-x86_64" else root / "Contents/Resources/licenses"
    verify_notices(notices, policy)
    return info


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", action="append", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--attempt", type=int, required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--notice-roots", type=Path, default=Path(__file__).with_name("notice-roots.json"))
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.commit) or not re.fullmatch(r"[1-9][0-9]*", args.run_id) or args.attempt < 1:
        raise ValueError("invalid expected source/run")
    policy = load(args.notice_roots)
    seen = set()
    for item in args.artifact:
        platform, separator, path = item.partition("=")
        if not separator or platform in seen:
            raise ValueError("duplicate artifact")
        seen.add(platform)
        prepare(Path(path), platform, args.output, repository=args.repository, commit=args.commit,
                run_id=args.run_id, attempt=args.attempt, version=args.version, policy=policy)
    if seen != set(ROOTS):
        raise ValueError("both platform artifacts are mandatory")
    print("same-source build artifacts, hybrid workers and bounded notice payloads verified")


if __name__ == "__main__":
    main()
