#!/usr/bin/env python3
"""Evaluate real production derivations; never compile or copy ignored files.

Use an exact committed revision containing the cache fix. All snapshots/logs go
under --output on disk. Nix may realise its existing evaluation/vendor helpers.
"""
import argparse
import io
import json
from pathlib import Path
import subprocess
import tarfile


def run(args, **kwargs):
    return subprocess.run(args, check=True, text=True, capture_output=True, **kwargs).stdout


def snapshot(repository, revision, destination):
    data = subprocess.run(
        ["git", "archive", "--format=tar", revision], cwd=repository,
        check=True, capture_output=True,
    ).stdout
    destination.mkdir()
    with tarfile.open(fileobj=io.BytesIO(data)) as archive:
        archive.extractall(destination, filter="data")


def evaluate(source):
    # Inspect the actual exported packages, not a reconstructed test builder.
    expression = '''let
      f = builtins.getFlake %s;
      p = f.packages.x86_64-linux;
      inspect = x: {
        final = x.drvPath;
        deps = x.cargoArtifacts.drvPath;
        dependency_hook = x.cargoArtifacts.postInstall or "";
        final_hook = x.postInstall;
        flags = x.cargoArtifacts.CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUSTFLAGS or "";
      };
    in { windows = inspect p.windows; macos = inspect p.macos-aarch64; }'''
    return json.loads(run([
        "nix", "eval", "--impure", "--json", "--expr",
        expression % json.dumps("path:" + str(source)),
    ]))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--revision", required=True, help="exact committed Git revision")
    parser.add_argument("--output", type=Path, required=True, help="new disk-backed output directory")
    args = parser.parse_args()
    repository = Path(__file__).resolve().parents[1]
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    revision = run(["git", "rev-parse", args.revision], cwd=repository).strip()
    baseline = output / "baseline"
    snapshot(repository, revision, baseline)
    base = evaluate(baseline)
    results = {"revision": revision, "baseline": base}
    # A real source edit changes the final source AND source-bound notices.
    application = baseline / "apps/desktop/src/main.rs"
    original = application.read_bytes()
    application.write_bytes(original + b"\n// Dependency-cache source-only regression fixture.\n")
    edited = evaluate(baseline)
    results["source_only"] = edited
    for platform in base:
        assert edited[platform]["deps"] == base[platform]["deps"], platform
        assert edited[platform]["final"] != base[platform]["final"], platform
        assert not edited[platform]["dependency_hook"], platform
        assert edited[platform]["final_hook"], platform
    # A real linker/target flag remains a dependency input. No builder override
    # is used: change the actual production argument in this private snapshot.
    flake = baseline / "flake.nix"
    source = flake.read_text()
    needle = '"-C target-feature=+crt-static -L native=${mingw.windows.pthreads}/lib"'
    assert source.count(needle) == 1, "update the regression for changed production flags"
    flake.write_text(source.replace(needle, needle[:-1] + ' -C debuginfo=1"'))
    flags = evaluate(baseline)
    results["target_flag"] = flags
    assert flags["windows"]["deps"] != edited["windows"]["deps"]
    assert flags["macos"]["deps"] == edited["macos"]["deps"]
    # Native dependency passes must not acquire final notice closure inputs.
    for platform, item in edited.items():
        drv = json.loads(run(["nix", "derivation", "show", item["deps"]]))
        definitions = drv.get("derivations", drv)
        definition = next(iter(definitions.values()))
        assert not definition["env"].get("postInstall"), platform
        assert "notice" not in definition["env"].get("installPhase", ""), platform
    results["result"] = "pass"
    (output / "proof.json").write_text(json.dumps(results, indent=2) + "\n")
    print(json.dumps({"result": "pass", "proof": str(output / "proof.json")}))


if __name__ == "__main__":
    main()
