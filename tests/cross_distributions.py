#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Inspect real frozen-source cross products and exercise the release producer.

No build, device, guest boot, signing key, network request or publication. Local
build-info records are explicitly local evidence, never GitHub workflow proof.
"""
import argparse
import hashlib
import io
import json
from pathlib import Path
import plistlib
import re
import runpy
import shutil
import subprocess
import tarfile
import tomllib
from urllib.parse import parse_qs, unquote, urlsplit
from cross_macho import verify_adhoc

PROGRAMS = ["nuschl", "nuschl-content-worker", "nuschl-crypto-worker",
            "nuschl-math-worker", "nuschl-media-worker", "nuschl-storage-worker",
            "nuschl-transfer-worker", "nuschl-update-verify-ed25519",
            "nuschl-update-verify-mldsa44"]


def run(arguments, **kwargs):
    result = subprocess.run(arguments, capture_output=True, text=True, **kwargs)
    if result.returncode:
        raise RuntimeError(f"{arguments[0]} failed: {result.stdout}\n{result.stderr}")
    return result.stdout


def sha(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def verify_snapshot(flake, source):
    """Bind every exported path, byte and executable bit to the exact flake Git tree."""
    url = urlsplit(flake)
    query = parse_qs(url.query, strict_parsing=True)
    if (url.scheme != "git+file" or url.netloc not in ("", "localhost") or url.fragment or
            set(query) != {"rev"} or len(query["rev"]) != 1 or
            not re.fullmatch(r"[0-9a-f]{40}", query["rev"][0])):
        raise ValueError("an exact local Git flake revision is required")
    revision = query["rev"][0]
    repository = Path(unquote(url.path)).resolve(strict=True)
    archive_bytes = subprocess.run(["git", "-C", str(repository), "archive", "--format=tar", revision],
                                   capture_output=True, check=True).stdout
    expected = set()
    with tarfile.open(fileobj=io.BytesIO(archive_bytes)) as archive:
        for member in archive:
            if member.isdir():
                continue
            expected.add(member.name)
            path = source / member.name
            if member.issym():
                assert path.is_symlink() and str(path.readlink()) == member.linkname, member.name
            else:
                assert member.isfile() and path.is_file() and not path.is_symlink(), member.name
                assert hashlib.sha256(archive.extractfile(member).read()).hexdigest() == sha(path), member.name
                assert bool(path.stat().st_mode & 0o111) == bool(member.mode & 0o111), member.name
    actual = {path.relative_to(source).as_posix() for path in source.rglob("*")
              if path.is_symlink() or not path.is_dir()}
    assert actual == expected, "snapshot has extra or missing exported paths"
    return {"revision": revision, "git_archive_sha256": hashlib.sha256(archive_bytes).hexdigest(),
            "exported_files": len(expected), "all_path_bytes_and_executable_bits_verified": True}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--flake", required=True, help="exact frozen Git revision flake ref")
    parser.add_argument("--source", required=True, type=Path, help="matching tracked-source snapshot")
    parser.add_argument("--output", required=True, type=Path, help="new disk-backed directory")
    args = parser.parse_args()
    match = re.search(r"[?&]rev=([0-9a-f]{40})(?:&|$)", args.flake)
    if not match:
        raise ValueError("an exact full Git revision is required")
    source = args.source.resolve(strict=True)
    source_evidence = verify_snapshot(args.flake, source)
    out = args.output.resolve()
    out.mkdir(parents=True, exist_ok=False)
    expression = '''let f=builtins.getFlake %s; p=f.packages.x86_64-linux;
      pkgs=import f.inputs.nixpkgs {system="x86_64-linux";};
      in {windows=p.windows.outPath; macos=p.macos-app.outPath;
        universal=p.macos-universal.outPath; mac_arm=p.macos-aarch64.outPath;
        mac_command=p.macos-universal.buildCommand;
        ed=p.windows-update-ed25519.outPath; ml=p.windows-update-mldsa44.outPath;
        mac_ed=p.macos-aarch64-update-ed25519.outPath;
        mac_ml=p.macos-aarch64-update-mldsa44.outPath;
        mac_x86_ed=p.macos-x86_64-update-ed25519.outPath;
        mac_x86_ml=p.macos-x86_64-update-mldsa44.outPath;
        windows_features={ed25519=p.windows-update-ed25519.crateFeatures;
                          mldsa44=p.windows-update-mldsa44.crateFeatures;};
        windows_providers={ed25519=map(x: x.pname or x.name)(p.windows-update-ed25519.dependencies);
                           mldsa44=map(x: x.pname or x.name)(p.windows-update-mldsa44.dependencies);};
        mac_commands={x86_ed=p.macos-x86_64-update-ed25519.buildPhase;
                      x86_ml=p.macos-x86_64-update-mldsa44.buildPhase;
                      arm_ed=p.macos-aarch64-update-ed25519.buildPhase;
                      arm_ml=p.macos-aarch64-update-mldsa44.buildPhase;};
        llvm="${pkgs.llvmPackages.llvm}/bin"; objdump="${pkgs.binutils}/bin/objdump";
        codesign="${pkgs.rcodesign}/bin/rcodesign"; zip="${pkgs.zip}/bin/zip"; }'''
    products = json.loads(run(["nix", "eval", "--impure", "--json", "--expr",
                               expression % json.dumps(args.flake)]))
    windows, macos = Path(products["windows"]), Path(products["macos"]) / "Nuschl.app"
    proof = {"source_revision": match[1], "flake": args.flake, "source_binding": source_evidence,
             "proof_scope": "local frozen-source production products; no CI or native macOS claim",
             "products": products, "files": {}}
    for algorithm, other, provider, excluded in (("ed25519", "mldsa44", "ed25519-dalek", "ml-dsa"),
                                                ("mldsa44", "ed25519", "ml-dsa", "ed25519-dalek")):
        features = products["windows_features"][algorithm]
        providers = " ".join(products["windows_providers"][algorithm])
        assert algorithm in features and not {other, "signer", "runtime-fixture"}.intersection(features)
        assert provider in providers and excluded not in providers
    for name, command in products["mac_commands"].items():
        algorithm = "ed25519" if name.endswith("ed") else "mldsa44"
        other = "mldsa44" if algorithm == "ed25519" else "ed25519"
        assert "--no-default-features" in command and f"--features nuschl-update-worker/{algorithm}" in command
        assert f"nuschl-update-worker/{other}" not in command and "signer" not in command and "runtime-fixture" not in command
    for root in (windows, macos):
        assert root.is_dir() and not root.is_symlink()
        assert not any(path.is_symlink() for path in root.rglob("*"))
    assert {p.name for p in (windows / "bin").iterdir()} == {p + ".exe" for p in PROGRAMS}
    assert {p.name for p in (macos / "Contents/MacOS").iterdir()} == set(PROGRAMS)
    version = tomllib.loads((source / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    plist = plistlib.loads((macos / "Contents/Info.plist").read_bytes())
    expected_plist = {"CFBundleExecutable": "nuschl", "CFBundleIdentifier": "org.nuschl.Nuschl",
                      "CFBundleName": "Nuschl", "CFBundleDisplayName": "Nuschl",
                      "CFBundlePackageType": "APPL", "CFBundleIconFile": "Nuschl",
                      "CFBundleShortVersionString": version, "CFBundleVersion": version,
                      "LSMinimumSystemVersion": "14.2", "NSHighResolutionCapable": True,
                      "NSMicrophoneUsageDescription": "Nuschl sends your voice to the people in your channel.",
                      "NSAudioCaptureUsageDescription": "Nuschl removes what your speakers play from your microphone, so others do not hear it. The sound stays on this Mac."}
    assert all(type(plist.get(key)) is type(value) and plist[key] == value
               for key, value in expected_plist.items())
    proof["macos_bundle_metadata"] = expected_plist
    for program in PROGRAMS:
        binary = windows / "bin" / (program + ".exe")
        report = run([products["objdump"], "-p", str(binary)])
        imports = [line.strip() for line in report.splitlines() if "DLL Name:" in line]
        assert imports and not any(re.search(r"DLL Name:.*" + name, report, re.I)
                                   for name in ("libgcc", "libstdc", "libwinpthread"))
        proof["files"]["windows/" + binary.name] = {"sha256": sha(binary), "imports": imports}
    for algorithm, key in (("ed25519", "ed"), ("mldsa44", "ml")):
        name = f"nuschl-update-verify-{algorithm}"
        assert sha(windows / "bin" / (name + ".exe")) == sha(Path(products[key]) / "bin" / (name + ".exe"))
    # Inspect BOTH actual slice products selected by the universal derivation.
    # This is trusted build-script structure, never archive-provided metadata.
    slices = re.search(r"llvm-lipo -create (/nix/store/[^ /]+)/(?:bin/\$program) (/nix/store/[^ /]+)/(?:bin/\$program)", products["mac_command"])
    assert slices, "universal build changed; update the actual-product inspector"
    slice_roots = {"x86_64": Path(slices[1]), "aarch64": Path(slices[2])}
    assert slice_roots["aarch64"] == Path(products["mac_arm"])
    for arch, root in slice_roots.items():
        assert {p.name for p in (root / "bin").iterdir()} == set(PROGRAMS)
        for algorithm, key in (("ed25519", "ed"), ("mldsa44", "ml")):
            product_key = "mac_" + ("x86_" if arch == "x86_64" else "") + key
            name = f"nuschl-update-verify-{algorithm}"
            assert sha(root / "bin" / name) == sha(Path(products[product_key]) / "bin" / name)
        for program in PROGRAMS:
            binary = root / "bin" / program
            run([products["llvm"] + "/llvm-lipo", str(binary), "-verify_arch",
                 "arm64" if arch == "aarch64" else arch])
            commands = run([products["llvm"] + "/llvm-otool", "-l", str(binary)])
            assert re.findall(r"^\s*minos (\S+)", commands, re.M) == ["14.2"]
            links = run([products["llvm"] + "/llvm-otool", "-L", str(binary)])
            assert not any("/nix/store/" in line for line in links.splitlines()[1:])
    # Universal bytes must come from these exact separate slices BEFORE signing.
    for algorithm in ("ed25519", "mldsa44"):
        name = f"nuschl-update-verify-{algorithm}"
        expected = out / (name + ".universal")
        run([products["llvm"] + "/llvm-lipo", "-create",
             *(str(root / "bin" / name) for root in slice_roots.values()), "-output", str(expected)])
        assert sha(expected) == sha(Path(products["universal"]) / "bin" / name)
    for program in PROGRAMS:
        binary = macos / "Contents/MacOS" / program
        run([products["llvm"] + "/llvm-lipo", str(binary), "-verify_arch", "x86_64", "arm64"])
        verification = subprocess.run([products["codesign"], "verify", "--config-file", "/dev/null", str(binary)],
                                      capture_output=True, text=True)
        (out / (program + "-signature.log")).write_text(verification.stdout + verification.stderr)
        expected_error = [f"@{index}: CMS error: missing further values (at position 0)" for index in range(2)]
        known_empty_cms_bug = (verification.returncode != 0 and
                               verification.stdout.strip().splitlines() == expected_error and
                               "verify command is known to be buggy" in verification.stderr)
        if verification.returncode and not known_empty_cms_bug:
            raise RuntimeError("Unexpected rcodesign failure: " + verification.stdout + verification.stderr)
        # An independently bounded test-only check supplements the known buggy
        # tool. Its failure and the native Apple gate stay explicit in evidence.
        directory = verify_adhoc(binary.read_bytes(), "org.nuschl.Nuschl" if program == "nuschl" else program,
            info=(macos / "Contents/Info.plist").read_bytes() if program == "nuschl" else None,
            resources=(macos / "Contents/_CodeSignature/CodeResources").read_bytes() if program == "nuschl" else None)
        info = run([products["codesign"], "print-signature-info", "--config-file", "/dev/null", str(binary)])
        flags = re.findall(r"flags: CodeSignatureFlags\(([^\n]+)\)", info)
        assert len(flags) == 2 and all("ADHOC" in flag for flag in flags)
        (out / (program + "-signature-info.log")).write_text(info)
        proof["files"]["macos/" + program] = {"sha256": sha(binary),
            "code_directory_integrity": directory, "rcodesign_verify_exit": verification.returncode,
            "rcodesign_known_empty_cms_failure": known_empty_cms_bug,
            "native_apple_signature_verification": "not run"}
    resources = plistlib.loads((macos / "Contents/_CodeSignature/CodeResources").read_bytes())
    checked_resources = set()
    for name, entry in resources.get("files2", {}).items():
        if isinstance(entry, dict) and "hash2" in entry:
            path = macos / "Contents" / name
            assert path.is_file() and hashlib.sha256(path.read_bytes()).digest() == entry["hash2"]
            checked_resources.add(name)
    assert all(path.relative_to(macos / "Contents").as_posix() in checked_resources
               for path in (macos / "Contents/Resources").rglob("*") if path.is_file())
    # Mirror the checked-in build.yml tree and tar steps exactly. These are real
    # production outputs, with local metadata clearly distinguished from CI.
    install = out / "install"
    (install / "windows/licenses").mkdir(parents=True)
    for path in (windows / "bin").glob("*.exe"):
        shutil.copy2(path, install / "windows" / path.name)
    shutil.copytree(windows / "share/licenses/nuschl", install / "windows/licenses", dirs_exist_ok=True)
    shutil.copytree(macos, install / "Nuschl.app")
    authority = runpy.run_path(str(source / "release/prepare-artifacts.py"))
    policy = authority["load"](source / "release/notice-roots.json")
    for platform, root_name, notices in (
        ("windows-x86_64", "windows", "licenses"),
        ("macos-universal", "Nuschl.app", "Contents/Resources/licenses"),
    ):
        tree = install / root_name
        authority["verify_notices"](tree / notices, policy, platform)
        files = [path for path in tree.rglob("*") if path.is_file()]
        assert 1 <= len(files) <= 256
        artifact = out / "artifacts" / platform
        artifact.mkdir(parents=True)
        run(["tar", "-C", str(install), "-cf", str(artifact / "install.tar"), root_name])
        info = {"repository": "ambiso/nuschl", "commit": match[1], "version": version,
                "platform": platform, "run_id": "local-cross-validation", "run_attempt": "1",
                "host": "x86_64-linux", "os_code_signed": False,
                "native_platform_runtime_tested_by_this_job": False,
                "macos_ad_hoc_signature": platform == "macos-universal", "local_validation_only": True}
        (artifact / "build-info.json").write_text(json.dumps(info, indent=2) + "\n")
        (artifact / "SHA256SUMS").write_text("".join(sha(artifact / name) + "  " + name + "\n"
                                                    for name in ("install.tar", "build-info.json")))
        authority["prepare"](artifact, platform, out / "checked-install",
            repository="ambiso/nuschl", commit=match[1], run_id="local-cross-validation",
            attempt=1, version=version, policy=policy)
        proof[platform] = {"file_count": len(files), "install_tar_sha256": sha(artifact / "install.tar"),
                           "notice_scope": platform, "notice_check": "passed"}
    run(["python3", str(source / "release/make-update.py"), "--version", version,
         "--base-url", "http://127.0.0.1:9/", "--platform", "windows-x86_64=" + str(install / "windows"),
         "--platform", "macos-universal=" + str(install / "Nuschl.app"), "--output", str(out / "update-assets")])
    # Actual release workflow zip layout, with no signing/publication operation.
    for platform, cwd, member in (("windows-x86_64", install / "windows", "."),
                                   ("macos-universal", install, "Nuschl.app")):
        target = out / f"nuschl-{version}-{platform}.zip"
        run([products["zip"], "-qr", str(target), member], cwd=cwd)
        proof[platform]["zip_sha256"] = sha(target)
    proof["result"] = "scoped_archive_checks_passed"
    proof["limits"] = ["local build evidence is not a GitHub CI run", "no native macOS execution",
                        "rcodesign0.29 generic verifier reports its known empty-CMS bug; raw failures retained",
                        "independent CodeDirectory integrity is not Apple codesign/Gatekeeper acceptance or publisher identity",
                        "final macOS signing changes signature bytes; exact helper copies verified in slices and pre-sign universal",
                        "unsigned update preparation exercises the real producer but grants no client acceptance",
                        "production trust remains unenrolled; GUI fixture is a separately recorded public-trust-only build"]
    (out / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
    print(json.dumps({"result": proof["result"], "proof": str(out / "proof.json")}))


if __name__ == "__main__":
    main()
