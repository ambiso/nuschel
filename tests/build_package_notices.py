#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Check real standalone inventories/ranges and unchanged main scope policy."""
import argparse
import hashlib
import json
from pathlib import Path
import runpy
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rtc-notices", type=Path, required=True)
    parser.add_argument("--baseline-revision", default="12231aca8c376de50363d6f801f570f6f895ee2e")
    parser.add_argument("--main-notices", action="append", default=[], help="trusted scope=real directory")
    parser.add_argument("--musicbot-notices", type=Path)
    parser.add_argument("--musicbot-web", type=Path)
    args = parser.parse_args()
    authority = runpy.run_path(str(ROOT / "release/prepare-artifacts.py"))
    policy = json.loads((ROOT / "release/notice-roots.json").read_bytes())
    scopes = json.loads((ROOT / "release/notice-scopes.json").read_bytes())
    old = json.loads(subprocess.check_output(["git", "-C", str(ROOT), "show",
                                             args.baseline_revision + ":release/notice-roots.json"]))
    for scope in ("linux-nix", "linux-aarch64-nix", "windows-x86_64", "macos-universal", "web"):
        selected, _ = authority["notice_scope"](policy, scope)
        expected = [row for row in old if row["target"] in scopes[scope]["targets"]]
        assert selected == expected, "main bundle root/features semantics changed: " + scope
    union, _ = authority["notice_scope"](policy, "all")
    for row in union:
        original = next(item for item in old if item["target"] == row["target"])
        assert row["packages"][:len(original["packages"])] == original["packages"]
        assert row["features"] == original["features"]
        groups = next(item for item in policy if item["target"] == row["target"]).get("standalone", {})
        for group, package in (("rtc-probe", "nuschl-rtc-probe"), ("musicbot", "nuschl-musicbot")):
            assert (package in row["packages"]) == (group in groups)
    standalone = [scope for scope in scopes if scope.startswith("rtc-probe-")]
    assert len(standalone) == 4
    for scope in standalone:
        selected, external = authority["notice_scope"](policy, scope)
        assert selected == [{"target": scopes[scope]["targets"][0], "packages": ["nuschl-rtc-probe"], "features": []}]
        assert external == {"rust-standard-library"}
    bot_scopes = [scope for scope in scopes if scope.startswith("musicbot-")]
    assert len(bot_scopes) == 4
    for scope in bot_scopes:
        selected, external = authority["notice_scope"](policy, scope)
        assert selected == [{"target": scopes[scope]["targets"][0], "packages": ["nuschl-musicbot", "nuschl-musicbot-codec-worker", "nuschl-content-worker"], "features": []}]
        assert external == {"rust-standard-library", "musicbot-web-runtime-assets", "syntect-default-assets"}
    if args.musicbot_notices:
        bot_report = json.loads((args.musicbot_notices / "THIRD-PARTY.json").read_bytes())
        bot_scope = bot_report["bundle_scope"]
        assert bot_scope in bot_scopes
        authority["verify_notices"](args.musicbot_notices, policy, bot_scope)
        try:
            authority["verify_notices"](args.musicbot_notices, policy, "linux-nix")
        except ValueError:
            pass
        else:
            raise AssertionError("standalone musicbot bundle accepted as main product")
    if args.musicbot_web:
        collector = runpy.run_path(str(ROOT / "release/web-notices.py"))
        lock = ROOT / "apps/musicbot/web/package-lock.json"
        collector["collect_npm"](args.musicbot_web, lock)
        with tempfile.TemporaryDirectory(prefix="musicbot-notice-boundaries-") as temporary:
            app = Path(temporary) / "app"
            shutil.copytree(args.musicbot_web, app)
            app.chmod(0o700)
            for directory in app.rglob("*"):
                if directory.is_dir():
                    directory.chmod(0o700)
            inventory = app / "licenses/npm/BUNDLED-NPM.json"
            inventory.chmod(0o600)
            original = json.loads(inventory.read_bytes())
            first = original["packages"][0]["files"][0]
            missing = app / first["path"]
            missing.unlink()
            try:
                collector["collect_npm"](app, lock)
            except ValueError:
                pass
            else:
                raise AssertionError("missing real emitted npm notice accepted")
            shutil.copyfile(args.musicbot_web / first["path"], missing)
            changed = json.loads(json.dumps(original))
            changed["packages"][0]["files"][0]["path"] = "licenses/runtime/EUPL-1.2"
            inventory.write_text(json.dumps(changed))
            try:
                collector["collect_npm"](app, lock)
            except ValueError:
                pass
            else:
                raise AssertionError("foreign owner's text accepted as npm attribution")
            inventory.write_text(json.dumps(original | {"packaged_wasm_build_roots": ["nuschl-client-wasm"]}))
            result = subprocess.run(["python3", str(ROOT / "release/web-notices.py"),
                "--profile", "musicbot", "--app", str(app), "--lock", str(lock),
                "--own-license", str(ROOT / "LICENSE"), "--output", str(Path(temporary) / "output")],
                capture_output=True, text=True)
            assert result.returncode != 0 and "unshipped WASM roots" in result.stderr
    root = args.rtc_notices
    report = json.loads((root / "THIRD-PARTY.json").read_bytes())
    scope = report["bundle_scope"]
    assert scope in standalone and report["unresolved"] == []
    assert report["roots_by_target"] == authority["notice_scope"](policy, scope)[0]
    authority["verify_notices"](root, policy, scope)
    names = {p["name"] for p in report["packages"]}
    assert {"nuschl-rtc-probe", "nuschl-rtc", "str0m", "dimpl", "str0m-rust-crypto"} <= names
    assert not any(name in names for name in ("aws-lc-rs", "aws-lc-sys", "ring"))
    evidence = ROOT / "release/licenses/rtc-upstreams"
    provenance = json.loads((evidence / "provenance.json").read_bytes())
    assert {(p["name"], p["version"]) for p in provenance["packages"]} == {
        ("is", "0.12.0"), ("str0m-proto", "0.8.0"), ("str0m-rust-crypto", "0.7.0")}
    payload = (root / report["notice_payload"]["path"]).read_bytes()
    for package in provenance["packages"]:
        assert package["revision"] == "5f2183ce7c67117cd1960dfeb11bed11fc188b66"
        assert package["selected_license"] == "MIT"
        row = next(p for p in report["packages"] if (p["name"], p["version"]) == (package["name"], package["version"]))
        for file in package["files"]:
            actual = next(p for p in row["files"] if p.get("upstream_path") == file["upstream_path"])
            assert actual["sha256"] == file["sha256"] and actual["revision"] == package["revision"]
            key = "crates/" + package["name"] + "-" + package["version"] + "/" + actual["path"]
            index = report["notice_files"][key]
            body = payload[index["offset"]:index["offset"] + index["size"]]
            assert body == (evidence / file["local"]).read_bytes()
            assert hashlib.sha256(body).hexdigest() == file["sha256"]
    # Tamper actual compact bytes; never mutate an immutable source/store output.
    with tempfile.TemporaryDirectory(prefix="rtc-notice-tamper-") as temporary:
        changed = Path(temporary) / "notices"
        shutil.copytree(root, changed)
        path = changed / report["notice_payload"]["path"]
        path.chmod(0o600)
        bad = bytearray(path.read_bytes())
        bad[len(bad) // 2] ^= 1
        path.write_bytes(bad)
        try:
            authority["verify_notices"](changed, policy, scope)
        except ValueError:
            pass
        else:
            raise AssertionError("actual compact payload tampering was accepted")
    for spec in args.main_notices:
        scope, separator, path = spec.partition("=")
        assert separator
        authority["verify_notices"](Path(path), policy, scope)
    boundaries = "/musicbot text boundaries" if args.musicbot_web else ""
    print("real standalone inventories" + boundaries + "/byte ranges/tampering and unchanged main roots PASS")


if __name__ == "__main__":
    main()
