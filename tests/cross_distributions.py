#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Inspect real frozen-source cross products and exercise the release producer.

No build, device, guest boot, signing key, network request or publication. Local
build-info records are explicitly local evidence, never GitHub workflow proof.
"""
import argparse
import hashlib
import json
from pathlib import Path
import plistlib
import re
import runpy
import shutil
import subprocess
import tomllib

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
        llvm="${pkgs.llvmPackages.llvm}/bin"; objdump="${pkgs.binutils}/bin/objdump";
        codesign="${pkgs.rcodesign}/bin/rcodesign"; zip="${pkgs.zip}/bin/zip"; }'''
    products = json.loads(run(["nix", "eval", "--impure", "--json", "--expr",
                               expression % json.dumps(args.flake)]))
    windows, macos = Path(products["windows"]), Path(products["macos"]) / "Nuschl.app"
    proof = {"source_revision": match[1], "flake": args.flake,
             "proof_scope": "local frozen-source production products; no CI or native macOS claim",
             "products": products, "files": {}}
    for root in (windows, macos):
        assert root.is_dir() and not root.is_symlink()
        assert not any(path.is_symlink() for path in root.rglob("*"))
    assert {p.name for p in (windows / "bin").iterdir()} == {p + ".exe" for p in PROGRAMS}
    assert {p.name for p in (macos / "Contents/MacOS").iterdir()} == set(PROGRAMS)
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
        report = run([products["codesign"], "verify", "--config-file", "/dev/null", str(binary)])
        (out / (program + "-signature.log")).write_text(report)
        proof["files"]["macos/" + program] = {"sha256": sha(binary), "ad_hoc_signature_verified": True}
    resources = plistlib.loads((macos / "Contents/_CodeSignature/CodeResources").read_bytes())
    checked_resources = set()
    for name, entry in resources.get("files2", {}).items():
        if isinstance(entry, dict) and "hash2" in entry:
            path = macos / "Contents" / name
            assert path.is_file() and hashlib.sha256(path.read_bytes()).digest() == entry["hash2"]
            checked_resources.add(name)
    assert all(path.relative_to(macos / "Contents").as_posix() in checked_resources
               for path in (macos / "Contents/Resources/licenses").rglob("*") if path.is_file())
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
    version = tomllib.loads((source / "Cargo.toml").read_text())["workspace"]["package"]["version"]
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
    proof["result"] = "pass"
    proof["limits"] = ["local build evidence is not a GitHub CI run", "no native macOS execution",
                        "final macOS signing changes signature bytes; exact helper copies verified in slices and pre-sign universal",
                        "unsigned update preparation exercises the real producer but grants no client acceptance",
                        "production trust remains unenrolled; GUI fixture is a separately recorded public-trust-only build"]
    (out / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
    print(json.dumps({"result": "pass", "proof": str(out / "proof.json")}))


if __name__ == "__main__":
    main()
