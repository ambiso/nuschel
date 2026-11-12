#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Native Apple STATIC validation of the same-run Linux cross-build artifact.

Never execute artifact programs, repair/re-sign code, load signing credentials or
assert Gatekeeper/publisher/notarisation acceptance. --prepare-only is LOCAL policy
inspection, explicitly rejected in GitHub Actions; it cannot emit native success.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import runpy
import selectors
import subprocess
import sys
import time
import tomllib

ROOT = Path(__file__).resolve().parents[1]
PROGRAMS = ("nuschl", "nuschl-content-worker", "nuschl-crypto-worker", "nuschl-math-worker",
            "nuschl-media-worker", "nuschl-storage-worker", "nuschl-transfer-worker",
            "nuschl-update-verify-ed25519", "nuschl-update-verify-mldsa44")
APPLE_TOOLS = {"/usr/bin/codesign", "/usr/bin/lipo", "/usr/bin/otool"}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def verify_checkout(source, commit):
    require(re.fullmatch(r"[0-9a-f]{40}", commit) is not None, "full source commit required")
    actual = subprocess.run(["git", "-C", str(source), "rev-parse", "HEAD"],
                            check=True, capture_output=True, text=True, timeout=15).stdout.strip()
    require(actual == commit, "checked-out source differs from expected artifact source")


def prepare_artifact(artifact, output, *, source, repository, commit, run_id, attempt, artifact_name):
    """The CLI always supplies its own checkout; explicit source allows local tests."""
    require(re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository) is not None,
            "expected repository is invalid")
    require(re.fullmatch(r"[0-9a-f]{40}", commit) is not None and
            re.fullmatch(r"[1-9][0-9]*", run_id) is not None and type(attempt) is int and attempt >= 1,
            "invalid expected commit/run/attempt")
    require(artifact_name == "nuschl-macos-universal-" + commit, "foreign artifact name")
    require(artifact.is_dir() and not artifact.is_symlink() and
            {p.name for p in artifact.iterdir()} == {"install.tar", "SHA256SUMS", "build-info.json"},
            "unexpected artifact container")
    authority = runpy.run_path(str(source / "release/prepare-artifacts.py"))
    version = tomllib.loads((source / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    info = authority["prepare"](artifact, "macos-universal", output / "install",
        repository=repository, commit=commit, run_id=run_id, attempt=attempt, version=version,
        policy=authority["load"](source / "release/notice-roots.json"))
    app = output / "install/Nuschl.app"
    require({p.name for p in (app / "Contents/MacOS").iterdir()} == set(PROGRAMS),
            "application must contain all nine programs and no signer/fixture")
    metadata = plistlib.loads((app / "Contents/Info.plist").read_bytes())
    expected = {"CFBundleExecutable": "nuschl", "CFBundleIdentifier": "org.nuschl.Nuschl",
                "CFBundlePackageType": "APPL", "CFBundleShortVersionString": version,
                "CFBundleVersion": version, "LSMinimumSystemVersion": "14.2"}
    require(all(type(metadata.get(k)) is type(v) and metadata[k] == v for k, v in expected.items()),
            "bundle metadata differs from source version/app policy")
    for program in PROGRAMS:
        binary = app / "Contents/MacOS" / program
        require(binary.is_file() and not binary.is_symlink() and os.access(binary, os.X_OK),
                "program missing, linked or not executable")
    return app, {"source": info, "artifact_name": artifact_name, "bundle_metadata": expected,
                 "archive_sha256": authority["digest"](artifact / "install.tar"),
                 "notice_scope": "macos-universal", "all_nine_programs": True}


class NativeTools:
    def __init__(self, output, report):
        require(sys.platform == "darwin", "native Apple tools require a real macOS host")
        self.output, self.report = output, report
        self.deadline = time.monotonic() + 300
        self.remaining_output = 4 * 1024**2
        report["commands"] = []

    def run(self, *arguments):
        require(arguments[0] in APPLE_TOOLS and Path(arguments[0]).is_file(), "fixed Apple tool missing")
        remaining = min(30, self.deadline - time.monotonic())
        require(remaining > 0, "native verification deadline exceeded")
        deadline = time.monotonic() + remaining
        process = subprocess.Popen(arguments, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, bufsize=0)
        captured = bytearray()
        failure = None
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            try:
                while selector.get_map():
                    if time.monotonic() >= deadline:
                        raise TimeoutError("Apple tool deadline exceeded")
                    for key, _ in selector.select(min(.2, max(0, deadline - time.monotonic()))):
                        block = os.read(key.fileobj.fileno(), 16384)
                        if not block:
                            selector.unregister(key.fileobj)
                            continue
                        self.remaining_output -= len(block)
                        if len(captured) + len(block) > 128 * 1024 or self.remaining_output < 0:
                            raise ValueError("Apple tool diagnostic output limit exceeded")
                        captured.extend(block)
                code = process.wait(timeout=max(.1, deadline - time.monotonic()))
                require(code == 0, "Apple tool rejected artifact (exit " + str(code) + ")")
            except Exception as error:
                failure = error
                if process.poll() is None:
                    process.kill()
                process.wait(timeout=5)
            finally:
                process.stdout.close()
        text = captured.decode("utf-8", errors="replace")
        index = len(self.report["commands"])
        (self.output / f"apple-{index:02}.log").write_text(text)
        self.report["commands"].append({"argv": list(arguments), "exit_code": process.returncode,
                                        "output_bytes": len(captured), "failure": str(failure) if failure else None})
        (self.output / "commands.json").write_text(json.dumps(self.report["commands"], indent=2) + "\n")
        if failure:
            raise failure
        return text


def verify_native(app, output, report):
    tools = NativeTools(output, report)
    require(os.environ.get("RUNNER_OS") == "macOS" and os.environ.get("RUNNER_ARCH") == "ARM64",
            "expected the explicitly selected hosted ARM64 macOS runner")
    report["host"] = {"platform": sys.platform, "machine": platform.machine(),
                       "macos": platform.mac_ver()[0], "runner_os": os.environ["RUNNER_OS"],
                       "runner_arch": os.environ["RUNNER_ARCH"], "python": sys.version.split()[0]}
    tools.run("/usr/bin/codesign", "--verify", "--strict", "--deep", "--all-architectures", "--verbose=4", str(app))
    for program in PROGRAMS:
        binary = app / "Contents/MacOS" / program
        arches = tools.run("/usr/bin/lipo", "-archs", str(binary)).split()
        require(len(arches) == 2 and set(arches) == {"x86_64", "arm64"}, "universal architectures differ")
        tools.run("/usr/bin/codesign", "--verify", "--strict", "--all-architectures", "--verbose=4", str(binary))
        for arch in ("x86_64", "arm64"):
            tools.run("/usr/bin/codesign", "--verify", "--strict", "--arch", arch, "--verbose=4", str(binary))
            display = tools.run("/usr/bin/codesign", "--display", "--verbose=4", "--arch", arch, str(binary))
            require("Signature=adhoc" in display and not re.search(r"^Authority=", display, re.M),
                    "expected ad-hoc code, not publisher identity")
            expected_id = "org.nuschl.Nuschl" if program == "nuschl" else program
            require(re.search(r"^Identifier=" + re.escape(expected_id) + r"$", display, re.M) is not None,
                    "signature identifier differs")
            loads = tools.run("/usr/bin/otool", "-arch", arch, "-l", str(binary))
            require(re.findall(r"^\s*minos (\S+)", loads, re.M) == ["14.2"], "unexpected minimum OS")
            links = tools.run("/usr/bin/otool", "-arch", arch, "-L", str(binary))
            require(not any("/nix/store/" in line for line in links.splitlines()[1:]), "nonportable dylib import")
    report["status"] = "native_apple_static_verified"
    report["artifact_binaries_executed"] = False
    report["limits"] = ["No app startup, Seatbelt/updater/media or Intel execution proof",
                        "Ad-hoc integrity is not Developer ID, notarisation or Gatekeeper publisher acceptance"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--artifact", type=Path, required=True)
    parser.add_argument("--artifact-name", required=True)
    parser.add_argument("--repository", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--attempt", type=int, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--prepare-only", action="store_true")
    args = parser.parse_args()
    require(sys.version_info >= (3, 12), "Python3.12+ required for safe tar extraction")
    require(not args.prepare_only or os.environ.get("GITHUB_ACTIONS") != "true",
            "prepare-only cannot be used as GitHub native verification")
    args.output.mkdir(parents=True, exist_ok=False)
    report = {"status": "failed", "artifact_binaries_executed": False}
    try:
        verify_checkout(ROOT, args.commit)
        app, evidence = prepare_artifact(args.artifact.resolve(strict=True), args.output, source=ROOT,
            repository=args.repository, commit=args.commit, run_id=args.run_id, attempt=args.attempt,
            artifact_name=args.artifact_name)
        report.update(evidence)
        if args.prepare_only:
            report["status"] = "prepared_only_not_native_verified"
        else:
            verify_native(app, args.output, report)
    except Exception as error:
        report["error"] = str(error)[:2048]
        raise
    finally:
        (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(report["status"])


if __name__ == "__main__":
    main()
