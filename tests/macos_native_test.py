#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Local policy regressions using the REAL full Mac tar; no fake Apple success."""
import argparse
import copy
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import textwrap
import unittest

spec = importlib.util.spec_from_file_location("macos_native", Path(__file__).with_name("macos_native.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--artifact", type=Path, required=True)
parser.add_argument("--source", type=Path, required=True)
parser.add_argument("--commit", required=True)
args, remaining = parser.parse_known_args()


def digest(path):
    h = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            h.update(block)
    return h.hexdigest()


class ActualArchive(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="mac-native-policy-")
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.artifact = self.root / "artifact"
        self.artifact.mkdir()
        # A hardlink avoids copying the large real tar. Repack tests always
        # replace this link with a NEW file; they never mutate the original.
        os.link(args.artifact / "install.tar", self.artifact / "install.tar")
        self.info = json.loads((args.artifact / "build-info.json").read_bytes())
        self.info.update(repository="ambiso/nuschl", commit=args.commit, run_id="4242", run_attempt="1")
        self.info["local_test_fixture"] = True
        self.write_metadata()

    def write_metadata(self):
        (self.artifact / "build-info.json").write_text(json.dumps(self.info) + "\n")
        (self.artifact / "SHA256SUMS").write_text("".join(digest(self.artifact / name) + "  " + name + "\n"
                                                        for name in ("install.tar", "build-info.json")))

    def prepare(self, **overrides):
        options = dict(source=args.source, repository="ambiso/nuschl", commit=args.commit,
                       run_id="4242", attempt=1, artifact_name="nuschl-macos-universal-" + args.commit)
        options.update(overrides)
        return module.prepare_artifact(self.artifact, self.root / "checked", **options)

    def repack(self, change=None, extra=None):
        replacement = self.root / "changed.tar"
        with tarfile.open(self.artifact / "install.tar", "r:") as original, tarfile.open(replacement, "w:") as target:
            for member in original:
                body = original.extractfile(member) if member.isfile() else None
                if change and member.isfile() and member.name.endswith("/licenses/THIRD-PARTY.json"):
                    report = json.load(body)
                    report["bundle_scope"] = "windows-x86_64"
                    raw = json.dumps(report).encode()
                    member.size = len(raw)
                    body = io.BytesIO(raw)
                target.addfile(member, body)
            if extra:
                target.addfile(extra)
        os.replace(replacement, self.artifact / "install.tar")
        self.write_metadata()

    def test_real_full_archive_and_source_notice_bytes_prepare_without_native_claim(self):
        app, report = self.prepare()
        self.assertEqual(len(list((app / "Contents/MacOS").iterdir())), 9)
        self.assertEqual(report["notice_scope"], "macos-universal")
        self.assertNotIn("native_apple_static_verified", json.dumps(report))

    def test_repository_commit_run_and_future_attempt_are_rejected(self):
        original = copy.deepcopy(self.info)
        for key, wrong in (("repository", "foreign/repository"), ("commit", "0" * 40),
                           ("run_id", "4243"), ("run_attempt", "2")):
            with self.subTest(field=key):
                self.info = original | {key: wrong}
                self.write_metadata()
                with self.assertRaises(ValueError):
                    self.prepare()
                self.assertFalse((self.root / "checked/Nuschl.app").exists())

    def test_same_run_prior_success_attempt_can_be_reused(self):
        _, report = self.prepare(attempt=2)
        self.assertEqual(report["source"]["run_attempt"], "1")

    def test_artifact_name_cannot_select_a_different_source(self):
        with self.assertRaisesRegex(ValueError, "artifact name"):
            self.prepare(artifact_name="nuschl-macos-universal-" + "0" * 40)

    def test_changed_checksum_is_rejected_before_extraction(self):
        (self.artifact / "SHA256SUMS").write_text("0" * 64 + "  install.tar\n")
        with self.assertRaises(ValueError):
            self.prepare()
        self.assertFalse((self.root / "checked/Nuschl.app").exists())

    def test_real_notice_scope_cannot_be_relabelled(self):
        self.repack(change=True)
        with self.assertRaisesRegex(ValueError, "attribution scope"):
            self.prepare()

    def test_real_archive_with_traversal_never_extracts(self):
        self.repack(extra=tarfile.TarInfo("../outside"))
        with self.assertRaisesRegex(ValueError, "unsafe artifact path"):
            self.prepare()
        self.assertFalse((self.root / "outside").exists())
        self.assertFalse((self.root / "checked/Nuschl.app").exists())

    def test_real_archive_with_symlink_never_extracts(self):
        extra = tarfile.TarInfo("Nuschl.app/outside")
        extra.type, extra.linkname = tarfile.SYMTYPE, "/etc/passwd"
        self.repack(extra=extra)
        with self.assertRaisesRegex(ValueError, "link or special"):
            self.prepare()
        self.assertFalse((self.root / "checked/Nuschl.app").exists())

    def test_linked_tar_is_rejected(self):
        (self.artifact / "install.tar").unlink()
        (self.artifact / "install.tar").symlink_to((args.artifact / "install.tar").resolve())
        with self.assertRaises(ValueError):
            self.prepare()

    def test_non_mac_host_cannot_emit_native_success(self):
        if module.sys.platform == "darwin":
            self.skipTest("negative non-Mac host guard requires Linux")
        with self.assertRaisesRegex(ValueError, "real macOS"):
            module.NativeTools(self.root, {})

    def test_prepare_only_is_forbidden_in_github_actions(self):
        command = [module.sys.executable, str(Path(module.__file__)), "--prepare-only",
            "--artifact", str(self.artifact), "--artifact-name", "nuschl-macos-universal-" + args.commit,
            "--repository", "ambiso/nuschl", "--commit", args.commit, "--run-id", "4242", "--attempt", "1",
            "--output", str(self.root / "must-not-open")]
        result = subprocess.run(command, env=os.environ | {"GITHUB_ACTIONS": "true"}, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("prepare-only cannot be used", result.stderr)
        self.assertFalse((self.root / "must-not-open").exists())


class ActualWorkflowGuard(unittest.TestCase):
    def test_execute_the_actual_early_guard_for_dispatch_tag_retry_and_untrusted_contexts(self):
        # Execute the real inline Bash, rather than asserting source fragments
        # or reproducing its decision logic in a second implementation.
        workflow = (Path(__file__).resolve().parents[1] / ".github/workflows/build.yml").read_text()
        step = workflow.split("      - name: Reject unsupported contexts before checkout or artifact download\n", 1)[1]
        script = textwrap.dedent(step.split("        run: |\n", 1)[1].split("\n      - ", 1)[0])
        for event, ref, retention, name, success in (
            ("workflow_dispatch", "refs/heads/main", "7", "correct", True),
            ("push", "refs/tags/v0.1.0", "7", "correct", True),
            ("pull_request", "refs/pull/1/merge", "7", "correct", False),
            ("pull_request_target", "refs/heads/main", "7", "correct", False),
            ("workflow_run", "refs/heads/main", "7", "correct", False),
            ("push", "refs/heads/main", "7", "correct", False),
            ("workflow_dispatch", "refs/heads/main", "7", "foreign", False),
            ("workflow_dispatch", "refs/heads/main", "0", "correct", False),
        ):
            with self.subTest(event=event, ref=ref, name=name, retention=retention):
                with tempfile.TemporaryDirectory(prefix="mac-workflow-context-") as temporary:
                    environment = os.environ | {
                        "RUNNER_TEMP": temporary, "GITHUB_SHA": args.commit,
                        "GITHUB_EVENT_NAME": event, "GITHUB_REF": ref, "GITHUB_RUN_ATTEMPT": "2",
                        "EXPECTED_MAC_ARTIFACT": "nuschl-macos-universal-" + (args.commit if name == "correct" else "0" * 40),
                        "REQUESTED_RETENTION": retention,
                    }
                    result = subprocess.run(["bash", "-euo", "pipefail", "-c", script], env=environment,
                                            capture_output=True, text=True, timeout=10)
                    self.assertEqual(result.returncode == 0, success, result.stderr)
                    self.assertTrue((Path(temporary) / "macos-native-logs/context.txt").is_file())


if __name__ == "__main__":
    unittest.main(argv=[__file__, *remaining])
