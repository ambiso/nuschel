#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Synthetic, disk-only release gates and actual offline hybrid signing CLI.
No production keys, network, release uploads or platform execution.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import sys
import tarfile
import tempfile
import time
import unittest

REPO = Path(__file__).resolve().parents[1]


def module(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    loaded = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(loaded)
    return loaded


prepare = module("prepare_artifacts", REPO / "release/prepare-artifacts.py")
make = module("make_update", REPO / "release/make-update.py")
SIGNER = None


class ReleaseGates(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="hybrid-release-test-")
        self.root = Path(self.temp.name)
        self.addCleanup(self.temp.cleanup)
        self.policy = prepare.load(REPO / "release/notice-roots.json")

    def cli(self, *args, ok=True):
        result = subprocess.run([SIGNER, *map(str, args)], capture_output=True, text=True, timeout=15)
        self.assertEqual(result.returncode == 0, ok, result.stderr)
        return result

    def keys(self):
        secret, public = self.root / "secret.json", self.root / "public.json"
        self.cli("keygen", "--secret-file", secret, "--public-file", public)
        return secret, public

    def package(self, platform="windows-x86_64", attempt="1", notice_scope=None):
        install = self.root / "source" / prepare.ROOTS[platform]
        binaries = install if platform == "windows-x86_64" else install / "Contents/MacOS"
        binaries.mkdir(parents=True)
        suffix = ".exe" if platform == "windows-x86_64" else ""
        for name in ("nuschl", "nuschl-update-verify-ed25519", "nuschl-update-verify-mldsa44"):
            (binaries / (name + suffix)).write_bytes(b"synthetic binary; never executed")
        notices = install / "licenses" if platform == "windows-x86_64" else install / "Contents/Resources/licenses"
        notices.mkdir(parents=True)
        for bundled, original in (("EUPL-1.2", "LICENSE"), ("Mumble-BSD", "crates/protocol/proto/LICENSE.mumble"),
                                  ("NewCMMath-GUST-Font-License", "crates/math/fonts/GUST-FONT-LICENSE.txt")):
            shutil.copyfile(REPO / original, notices / bundled)
        # Synthetic inventory: validates gate mechanics only, never attribution
        # completeness for the real app or any upstream license choice.
        scope = notice_scope or platform
        selected, external_names = prepare.notice_scope(self.policy, scope)
        targets = {}
        for item in selected:
            for name in item["packages"]:
                targets.setdefault(name, []).append(item["target"])
        native = [item["target"] for item in selected if item["features"]]
        packages = [{"name": name, "version": "0.1.0", "license": "EUPL-1.2", "source": "project-path",
                     "first_party": True, "targets": triples, "files": []} for name, triples in targets.items()]
        for name in (("ed25519-dalek", "ml-dsa") if native else ()):
            path = notices / "crates" / (name + "-0.1.0") / "LICENSE"
            path.parent.mkdir(parents=True)
            path.write_text("Synthetic gate fixture notice. No real license assurance.\n")
            packages.append({"name": name, "version": "0.1.0", "first_party": False, "targets": native,
                             "files": [{"path": "LICENSE", "sha256": prepare.digest(path)}]})
        external = []
        for name in sorted(external_names):
            path = notices / "external" / name / "NOTICE"
            path.parent.mkdir(parents=True)
            path.write_text("Synthetic gate fixture only.\n")
            external.append({"name": name, "files": [{"path": "NOTICE", "sha256": prepare.digest(path)}]})
        (notices / "THIRD-PARTY.json").write_text(json.dumps({"format": 1, "bundle_scope": scope, "unresolved": [], "roots_by_target": selected,
                                                              "packages": packages, "external_components": external}))
        directory = self.root / "artifact"
        directory.mkdir()
        info = {"repository": "fixture/project", "commit": "a"*40, "version": "0.1.0", "platform": platform,
                "run_id": "123", "run_attempt": attempt, "host": "x86_64-linux", "os_code_signed": False,
                "native_platform_runtime_tested_by_this_job": False, "macos_ad_hoc_signature": platform == "macos-universal"}
        (directory / "build-info.json").write_text(json.dumps(info))
        def repack():
            with tarfile.open(directory / "install.tar", "w") as archive:
                archive.add(install, arcname=install.name)
            self.checksums(directory)
        repack()
        return directory, install, notices, repack

    def checksums(self, directory):
        (directory / "SHA256SUMS").write_text("".join(prepare.digest(directory/name) + "  " + name + "\n" for name in ("install.tar", "build-info.json")))

    def accept(self, directory, platform="windows-x86_64"):
        return prepare.prepare(directory, platform, self.root / "extracted", repository="fixture/project", commit="a"*40,
                               run_id="123", attempt=2, version="0.1.0", policy=self.policy)

    def test_prior_success_same_run_and_exact_source_is_usable(self):
        directory, *_ = self.package(attempt="1")
        self.assertEqual(self.accept(directory)["run_attempt"], "1")

    def test_mac_root_and_helper_discovery(self):
        directory, *_ = self.package(platform="macos-universal", attempt="2")
        self.assertTrue(self.accept(directory, "macos-universal")["macos_ad_hoc_signature"])

    def test_wrong_run_source_platform_and_future_attempt_rejected(self):
        directory, *_ = self.package()
        original = json.loads((directory/"build-info.json").read_text())
        for key, value in (("repository", "other/project"), ("commit", "b"*40), ("run_id", "122"),
                           ("run_attempt", "3"), ("run_attempt", "0"), ("platform", "macos-universal"),
                           ("os_code_signed", 0)):
            (directory/"build-info.json").write_text(json.dumps(original | {key:value}))
            self.checksums(directory)
            with self.assertRaises(ValueError, msg=key): self.accept(directory)

    def test_digest_and_duplicate_checksum_fail_before_extraction(self):
        directory, *_ = self.package()
        with (directory/"install.tar").open("ab") as out: out.write(b"tampered")
        with self.assertRaises(ValueError): self.accept(directory)
        self.checksums(directory)
        text = (directory/"SHA256SUMS").read_text()
        (directory/"SHA256SUMS").write_text(text + text)
        with self.assertRaises(ValueError): self.accept(directory)
        self.assertFalse((self.root/"extracted").exists())

    def test_traversal_symlink_hardlink_and_duplicate_member_rejected(self):
        directory, *_ = self.package()
        for kind in ("traversal", "symlink", "hardlink", "duplicate"):
            with tarfile.open(directory/"install.tar", "w") as archive:
                item = tarfile.TarInfo("../escape" if kind == "traversal" else "windows/entry")
                if kind in ("symlink", "hardlink"):
                    item.type = tarfile.SYMTYPE if kind == "symlink" else tarfile.LNKTYPE
                    item.linkname = "../../escape"
                archive.addfile(item)
                if kind == "duplicate": archive.addfile(item)
            self.checksums(directory)
            with self.assertRaises(ValueError, msg=kind): self.accept(directory)
        self.assertFalse((self.root/"escape").exists())

    def test_missing_notices_and_self_asserted_scope_fail_closed(self):
        directory, install, notices, repack = self.package()
        report_path = notices/"THIRD-PARTY.json"
        original = json.loads(report_path.read_text())
        for change in ({"unresolved":[{"name":"missing"}]}, {"roots_by_target":[]},
                       {"packages":[]}, {"external_components":[]}):
            report_path.write_text(json.dumps(original | change)); repack()
            with self.assertRaises(ValueError): self.accept(directory)
            shutil.rmtree(self.root/"extracted")
        report_path.unlink(); repack()
        with self.assertRaises(ValueError): self.accept(directory)

    def test_artifact_cannot_choose_another_scope_or_omit_a_mac_slice(self):
        directory, _, notices, repack = self.package(platform="macos-universal")
        path = notices/"THIRD-PARTY.json"
        original = json.loads(path.read_bytes())
        for change in ({"bundle_scope":"web"}, {"roots_by_target":original["roots_by_target"][:1]},
                       {"bundle_scope":"all", "roots_by_target":self.policy}):
            path.write_text(json.dumps(original | change)); repack()
            with self.assertRaises(ValueError): self.accept(directory, "macos-universal")
            shutil.rmtree(self.root/"extracted")

    def test_missing_verifier_and_notice_bytes_fail(self):
        directory, install, notices, repack = self.package()
        (install/"nuschl-update-verify-mldsa44.exe").unlink(); repack()
        with self.assertRaises(ValueError): self.accept(directory)
        shutil.rmtree(self.root/"extracted")
        (install/"nuschl-update-verify-mldsa44.exe").write_bytes(b"synthetic")
        (notices/"crates/ml-dsa-0.1.0/LICENSE").write_text("tampered"); repack()
        with self.assertRaises(ValueError): self.accept(directory)

    def test_attribution_cannot_silently_expand_the_installation_file_budget(self):
        directory, install, _, repack = self.package()
        for index in range(257):
            (install / f"notice-{index}.txt").write_text("Synthetic notice entry")
        repack()
        with self.assertRaisesRegex(ValueError, "256-file"):
            self.accept(directory)
        self.assertFalse((self.root/"extracted").exists())

    def linux_package(self):
        directory, _, notices, _ = self.package(notice_scope="web")
        site = self.root/"static-site"
        (site/"app").mkdir(parents=True)
        (site/"index.html").write_text("synthetic landing")
        (site/"app/index.html").write_text("synthetic client")
        shutil.copytree(notices, site/"licenses/nuschl")
        with tarfile.open(directory/"web-site.tar", "w") as archive:
            archive.add(site, arcname=".")
        info = json.loads((directory/"build-info.json").read_text())
        info["platform"] = "linux-nix"
        (directory/"build-info.json").write_text(json.dumps(info))
        (directory/"nix-paths.json").write_text(json.dumps({"/nix/store/" + "a"*32 + "-fixture-" + str(i): {"narHash":"sha256-"+"A"*43+"="} for i in range(4)}))
        (directory/"INSTALL-NIX.txt").write_text("Synthetic Nix-only evidence, not a portable Linux binary")
        shutil.copyfile(REPO/"flake.lock", directory/"flake.lock")
        def sums():
            (directory/"SHA256SUMS").write_text("".join(prepare.digest(directory/name)+"  "+name+"\n" for name in ("web-site.tar", "nix-paths.json", "INSTALL-NIX.txt", "flake.lock", "build-info.json")))
        sums()
        return directory, site, sums

    def accept_linux(self, directory):
        return prepare.prepare_linux(directory, self.root/"extracted", repository="fixture/project", commit="a"*40,
                                     run_id="123", attempt=2, version="0.1.0", policy=self.policy)

    def test_linux_web_bundle_and_nix_references_keep_source_scope(self):
        directory, _, _ = self.linux_package()
        self.assertEqual(self.accept_linux(directory)["platform"], "linux-nix")
        self.assertTrue((self.root/"extracted/web-site/app/index.html").is_file())

    def test_linux_foreign_lock_and_missing_wasm_notices_rejected(self):
        directory, site, sums = self.linux_package()
        original = (directory/"flake.lock").read_bytes()
        (directory/"flake.lock").write_bytes(b"foreign source"); sums()
        with self.assertRaises(ValueError): self.accept_linux(directory)
        (directory/"flake.lock").write_bytes(original)
        (site/"licenses/nuschl/THIRD-PARTY.json").unlink()
        with tarfile.open(directory/"web-site.tar", "w") as archive: archive.add(site,arcname=".")
        sums()
        with self.assertRaises(ValueError): self.accept_linux(directory)

    def test_ci_notice_gate_runs_the_same_source_policy_before_payload_upload(self):
        _, _, notices, _ = self.package()
        command = [sys.executable, str(REPO/"release/check-notices.py"), "--scope", "windows-x86_64", str(notices)]
        good = subprocess.run(command, capture_output=True, text=True, timeout=10)
        self.assertEqual(good.returncode, 0, good.stderr)
        (notices/"THIRD-PARTY.json").unlink()
        self.assertNotEqual(subprocess.run(command, capture_output=True, timeout=10).returncode, 0)

    def test_paths_and_https_policy(self):
        for value in ("../escape", "a//b", "CON.txt", "dir/nul", "a.", "a/../b", "A\\b", ".NUSCHL-OLD/file"):
            with self.assertRaises(ValueError, msg=value): make.safe_path(value)
        for value in ("http://example.invalid/", "https://user:password@example.invalid/", "https://example.invalid/#fragment"):
            with self.assertRaises(ValueError, msg=value): make.validate_base(value)
        self.assertEqual(make.validate_base("http://127.0.0.1:1"), "http://127.0.0.1:1/")

    def test_real_signer_release_helper_exact_raw_bytes_and_no_overwrite(self):
        secret, public = self.keys()
        install = self.root/"install"; install.mkdir(); (install/"nuschl.exe").write_bytes(b"fixture app")
        output = self.root/"dist"
        command = [sys.executable, str(REPO/"release/make-update.py"), "--version", "9.1.0", "--base-url", "http://127.0.0.1:1/",
                   "--platform", "windows-x86_64="+str(install), "--output", str(output), "--sign-key", str(secret),
                   "--public-key", str(public), "--signer", SIGNER]
        result = subprocess.run(command, capture_output=True, text=True, timeout=20)
        self.assertEqual(result.returncode, 0, result.stderr)
        raw, signature = output/"update.json", output/"update.json.sig"
        self.assertEqual(len(signature.read_bytes()), 2528)
        self.cli("verify", "--public-file", public, "--manifest", raw, "--signature-file", signature)
        raw.write_bytes(raw.read_bytes()+b" ")
        self.cli("verify", "--public-file", public, "--manifest", raw, "--signature-file", signature, ok=False)
        self.assertNotEqual(subprocess.run(command, capture_output=True, timeout=20).returncode, 0)

    def test_real_signed_expired_duplicate_and_wrong_public_pair_rejected(self):
        secret, public = self.keys()
        now = int(time.time())
        payload = {"format":2,"algorithm":"ed25519+mldsa44","version":"1.0.0","released":now-10,"expires":now-1,
                   "platforms":{"windows-x86_64":[{"path":"nuschl.exe","url":"https://example.invalid/a","sha256":"00"*32,"size":1}]}}
        for index, data in enumerate((json.dumps(payload).encode(), b'{"format":2,' + json.dumps(payload | {"expires":now+60}).encode()[1:])):
            raw, sig = self.root/f"manifest{index}", self.root/f"sig{index}"
            raw.write_bytes(data)
            self.cli("sign", "--secret-file", secret, "--public-file", public, "--manifest", raw, "--signature-file", sig)
            self.cli("verify", "--public-file", public, "--manifest", raw, "--signature-file", sig, ok=False)
        other_secret, other_public = self.root/"other-secret", self.root/"other-public"
        self.cli("keygen", "--secret-file", other_secret, "--public-file", other_public)
        self.cli("key-status", "--secret-file", secret, "--public-file", other_public, ok=False)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--signer", required=True)
    args, rest = parser.parse_known_args()
    SIGNER = str(Path(args.signer).resolve())
    unittest.main(argv=[sys.argv[0], *rest])
