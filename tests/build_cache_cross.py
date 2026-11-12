#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Audit actual cross-product feature roots and optionally inspect built engines.

No compilation, guest boot, production trust input or network fixture is used.
--artifacts requires all six exported helper products already in the store and
checks PE imports / both Mach-O architectures, minimum OS and universal assembly.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


PROGRAMS = ["nuschl", "nuschl-content-worker", "nuschl-crypto-worker",
            "nuschl-math-worker", "nuschl-media-worker", "nuschl-storage-worker",
            "nuschl-transfer-worker", "nuschl-update-verify-ed25519",
            "nuschl-update-verify-mldsa44"]


def run(args, **kwargs):
    result = subprocess.run(args, capture_output=True, text=True, **kwargs)
    if result.returncode:
        raise RuntimeError(f"{args[0]} failed: {result.stderr}")
    return result.stdout


def definition(path):
    data = json.loads(run(["nix", "derivation", "show", path]))
    return next(iter(data.get("derivations", data).values()))["env"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--artifacts", action="store_true")
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    expression = '''let
      f=builtins.getFlake %s; p=f.packages.x86_64-linux;
      pkgs=import f.inputs.nixpkgs {system="x86_64-linux";};
      inspect = d: {drv=d.drvPath; out=d.outPath;
        features=d.crateFeatures or [];
        dependency_drv=if d ? cargoArtifacts then d.cargoArtifacts.drvPath else null;
        dependencies=map (x: x.pname or x.name) (d.dependencies or []);};
    in { products=builtins.mapAttrs (_: inspect) {
      windows-ed25519=p.windows-update-ed25519;
      windows-mldsa44=p.windows-update-mldsa44;
      macos-x86_64-ed25519=p.macos-x86_64-update-ed25519;
      macos-x86_64-mldsa44=p.macos-x86_64-update-mldsa44;
      macos-aarch64-ed25519=p.macos-aarch64-update-ed25519;
      macos-aarch64-mldsa44=p.macos-aarch64-update-mldsa44;
    }; windows=p.windows.drvPath; macos=p.macos-aarch64.drvPath;
       llvm="${pkgs.llvmPackages.llvm}/bin"; objdump="${pkgs.binutils}/bin/objdump";
    }'''
    data = json.loads(run(["nix", "eval", "--impure", "--json", "--expr",
                           expression % json.dumps("git+file://" + str(root))]))
    products = data["products"]
    for name, product in products.items():
        algorithm = name.rsplit("-", 1)[1]
        other = "mldsa44" if algorithm == "ed25519" else "ed25519"
        env = definition(product["drv"])
        if name.startswith("windows"):
            assert algorithm in product["features"] and other not in product["features"]
            assert "runtime-fixture" not in product["features"] and "signer" not in product["features"]
            dependencies = " ".join(product["dependencies"])
            assert ("ed25519-dalek" if algorithm == "ed25519" else "ml-dsa") in dependencies
            assert ("ml-dsa" if algorithm == "ed25519" else "ed25519-dalek") not in dependencies
        else:
            command = env["buildPhase"]
            assert "--no-default-features" in command
            assert f"--features nuschl-update-worker/{algorithm}" in command
            assert f"nuschl-update-worker/{other}" not in command
            assert "signer" not in command and "runtime-fixture" not in command
            assert env["MACOSX_DEPLOYMENT_TARGET"] == "14.2"
            deps = definition(product["dependency_drv"])
            assert not deps.get("postInstall")
            assert f"nuschl-update-worker/{algorithm}" in deps["buildPhase"]
        product["verified_algorithm"] = algorithm
    for platform in ("windows", "macos"):
        env = definition(data[platform])
        assert "-p nuschl-update-worker" not in env["buildPhase"]
        for program in PROGRAMS:
            assert program in env["postInstall"]
        for algorithm in ("ed25519", "mldsa44"):
            key = f"windows-{algorithm}" if platform == "windows" else f"macos-aarch64-{algorithm}"
            assert products[key]["out"] in env["postInstall"]
    args.output.mkdir(parents=True, exist_ok=False)
    if args.artifacts:
        for name, product in products.items():
            algorithm = product["verified_algorithm"]
            suffix = ".exe" if name.startswith("windows") else ""
            binary = Path(product["out"]) / "bin" / f"nuschl-update-verify-{algorithm}{suffix}"
            assert list(binary.parent.iterdir()) == [binary]
            product["sha256"] = hashlib.sha256(binary.read_bytes()).hexdigest()
            product["bytes"] = binary.stat().st_size
            if suffix:
                report = run([data["objdump"], "-p", str(binary)])
                imports = [s.strip() for s in report.splitlines() if "DLL Name:" in s]
                assert imports and not any(s in report.lower() for s in ("libgcc", "libstdc", "libwinpthread"))
                product["imports"] = imports
            else:
                report = run([data["llvm"] + "/llvm-otool", "-l", str(binary)])
                assert "minos 14.2" in report
                links = run([data["llvm"] + "/llvm-otool", "-L", str(binary)])
                assert not any("/nix/store/" in line for line in links.splitlines()[1:])
                arch = "arm64" if "aarch64" in name else "x86_64"
                run([data["llvm"] + "/llvm-lipo", str(binary), "-verify_arch", arch])
                product["minimum_os"] = "14.2"
        for algorithm in ("ed25519", "mldsa44"):
            paths = [str(Path(products[f"macos-{arch}-{algorithm}"]["out"]) / "bin" /
                         f"nuschl-update-verify-{algorithm}") for arch in ("x86_64", "aarch64")]
            universal = args.output / f"nuschl-update-verify-{algorithm}"
            run([data["llvm"] + "/llvm-lipo", "-create", *paths, "-output", str(universal)])
            run([data["llvm"] + "/llvm-lipo", str(universal), "-verify_arch", "x86_64", "arm64"])
    data["result"] = "pass"
    data["artifact_checks"] = args.artifacts
    (args.output / "proof.json").write_text(json.dumps(data, indent=2) + "\n")
    print(json.dumps({"result": "pass", "proof": str(args.output / "proof.json")}))


if __name__ == "__main__":
    main()
