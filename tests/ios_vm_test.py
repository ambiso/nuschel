#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Host-only boundary tests; they do not claim a working macOS/iOS Simulator."""
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import ios_vm as vm


class Boundaries(unittest.TestCase):
    def test_output_is_capped_and_process_reaped(self):
        with self.assertRaisesRegex(vm.CheckError, "output budget"):
            vm.bounded_run([sys.executable, "-c", "import os; os.write(1,b'x'*100000)"], limit=1024)

    def test_time_is_capped(self):
        start = time.monotonic()
        with self.assertRaisesRegex(vm.CheckError, "time budget"):
            vm.bounded_run([sys.executable, "-c", "import time; time.sleep(60)"], timeout=0.1)
        self.assertLess(time.monotonic() - start, 3)

    def test_return_code_and_output_survive(self):
        result = vm.bounded_run([sys.executable, "-c", "print('observed'); raise SystemExit(7)"])
        self.assertEqual(result, {"exit_code": 7, "output": "observed\n"})

    def test_ram_backed_and_broad_state_paths_are_refused(self):
        for path in [Path.home(), Path("/")]:
            with self.assertRaises(vm.CheckError):
                vm.disk_path(path)
        with patch.object(vm, "mount_type", return_value="tmpfs"):
            with self.assertRaisesRegex(vm.CheckError, "RAM-backed"):
                vm.disk_path(Path("/some/dedicated/vm"))
        with self.assertRaisesRegex(vm.CheckError, "delimiters"):
            vm.disk_path(Path("/some/vm,other-option"))

    def test_symlink_out_of_state_is_refused(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            state = root / "state"
            state.mkdir()
            (state / "macos.qcow2").symlink_to(root / "outside")
            with self.assertRaisesRegex(vm.CheckError, "dedicated state"):
                vm.media(state, "macos.qcow2")

    def test_mutable_or_partial_image_names_are_refused(self):
        for image in ["sickcodes/docker-osx:latest", "sha256:1234", "-evil", "image@sha256:no"]:
            with self.assertRaises(vm.CheckError):
                vm.pinned_image(image)
        vm.pinned_image(vm.IMAGE)
        vm.pinned_image("sha256:" + "a" * 64)

    def test_bad_registry_digest_is_refused(self):
        with self.assertRaisesRegex(vm.CheckError, "digest mismatch"):
            vm.verified_json(b'{}', "sha256:" + "0" * 64)

    def test_remote_engines_are_refused(self):
        with patch.dict(os.environ, {"DOCKER_HOST": "tcp://outside.example:2375"}):
            with self.assertRaisesRegex(vm.CheckError, "overrides"):
                vm.runtime("docker")

    def test_launch_plan_publishes_only_loopback_and_binds_two_media_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            plan = vm.launch_plan(state, "podman", 50922, 5999)
            command = plan["command"]
            self.assertFalse(plan["executed"])
            self.assertTrue(plan["review_required"])
            self.assertEqual(len(plan["blockers"]), 2)
            publishes = [command[i + 1] for i, value in enumerate(command) if value == "--publish"]
            self.assertEqual(publishes, ["127.0.0.1:50922:10022", "127.0.0.1:5999:5999"])
            mounts = [command[i + 1] for i, value in enumerate(command) if value == "--mount"]
            self.assertEqual(len(mounts), 2)
            self.assertTrue(all(f"src={state}/" in mount for mount in mounts))
            self.assertNotIn("--privileged", command)
            self.assertNotIn("/dev/snd", " ".join(command))
            self.assertIn("AUDIO_DRIVER=none", command)
            self.assertIn("accel=kvm", " ".join(command))
            self.assertIn(vm.IMAGE, command)
            self.assertEqual(command[-1], "exec ./Launch.sh")

    def test_status_detects_unsafe_existing_container(self):
        info = [{"State": {"Status": "running"}, "Image": "unverified",
                 "HostConfig": {"Privileged": True, "NetworkMode": "host", "CapAdd": ["SYS_ADMIN"],
                                "Devices": [{"PathOnHost": "/dev/snd"}],
                                "PortBindings": {"5999/tcp": [{"HostIp": "0.0.0.0", "HostPort": "5999"}]}},
                 "Mounts": [{"Type": "bind", "Source": str(Path.home())}]}]
        with patch.object(vm, "bounded_run", return_value={"exit_code": 0, "output": json.dumps(info)}):
            status = vm.status(Path("/some/vm"), "podman")
        self.assertFalse(status["guest_proven"])
        self.assertTrue(any("added capabilities" in problem for problem in status["boundary_problems"]))
        self.assertTrue(any("no-new-privileges" in problem for problem in status["boundary_problems"]))
        self.assertFalse(status["capability_profile"]["configuration_verified"])

    def test_expanded_drop_list_needs_compiled_empty_capabilities_not_a_default_allowlist(self):
        config = {"CapAdd": [], "CapDrop": ["CAP_CHOWN", "setuid"], "SecurityOpt": ["no-new-privileges"]}
        self.assertFalse(vm.capability_profile(config, None)["configuration_verified"])
        oci = {"process": {"capabilities": {}, "noNewPrivileges": True}}
        actual = vm.capability_profile(config, oci)
        self.assertTrue(actual["configuration_verified"])
        self.assertEqual(actual["inspect_cap_drop"], ["CAP_CHOWN", "CAP_SETUID"])
        self.assertFalse(actual["live_effective_verified"])
        for name in vm.CAPABILITY_SETS:
            changed = {"process": {"capabilities": {name:["CAP_SYS_ADMIN"]}, "noNewPrivileges":True}}
            self.assertFalse(vm.capability_profile(config, changed)["configuration_verified"])
        for malformed in (None, [], {"unknown": []}, {"effective": None}):
            changed = {"process": {"capabilities": malformed, "noNewPrivileges":True}}
            self.assertFalse(vm.capability_profile(config, changed)["configuration_verified"])

    def test_no_new_privileges_must_be_explicit_and_not_conflicting(self):
        for options in ([], ["no-new-privileges:false"], ["no-new-privileges", "no-new-privileges:false"]):
            profile = vm.capability_profile({"CapDrop":["ALL"],"SecurityOpt":options}, None)
            self.assertFalse(profile["configuration_verified"])
        config = {"CapDrop":["ALL"],"SecurityOpt":["no-new-privileges:true"]}
        self.assertTrue(vm.capability_profile(config,None)["configuration_verified"])
        self.assertFalse(vm.capability_profile(config,{"process":{"capabilities":{},"noNewPrivileges":False}})["configuration_verified"])
        self.assertFalse(vm.capability_profile(config|{"CapAdd":["SYS_ADMIN"]},None)["configuration_verified"])

    def test_stopped_status_never_claims_effective_kernel_proof(self):
        info = {"Id":"a"*64,"State":{"Status":"exited","Running":False,"Pid":0},
            "HostConfig":{"CapDrop":["CAP_CHOWN"],"SecurityOpt":["no-new-privileges"]}}
        oci = {"process":{"capabilities":{},"noNewPrivileges":True}}
        with patch.object(vm,"bounded_run",return_value={"exit_code":0,"output":json.dumps([info])}), patch.object(vm,"oci_configuration",return_value=(oci,"fixture")), patch.object(vm,"process_capabilities") as proc:
            result = vm.status(Path("/some/vm"),"podman")
        self.assertFalse(result["boundary_problems"])
        self.assertTrue(result["capability_profile"]["configuration_verified"])
        self.assertFalse(result["capability_profile"]["live_effective_verified"])
        proc.assert_not_called()

    def test_running_process_evidence_is_rejected_on_runtime_pid_change(self):
        info = {"Id":"a"*64,"State":{"Status":"running","Running":True,"Pid":123,"StartedAt":"fixed"},
            "HostConfig":{"CapDrop":["ALL"],"SecurityOpt":["no-new-privileges"]}}
        changed = info|{"State":info["State"]|{"Pid":456}}
        with patch.object(vm,"bounded_run",side_effect=[{"exit_code":0,"output":json.dumps([value])} for value in [info,changed]]), patch.object(vm,"oci_configuration",return_value=(None,"not available")), patch.object(vm,"process_capabilities",return_value={"verified":True}):
            result = vm.status(Path("/some/vm"),"podman")
        self.assertFalse(result["capability_profile"]["live_effective_verified"])
        self.assertTrue(result["boundary_problems"])

    def test_oci_inspection_never_opens_an_arbitrary_inspect_path(self):
        info={"Id":"a"*64,"OCIConfigPath":"/etc/shadow"}
        with patch.object(vm,"engine_info",return_value={"storage":"/some/runtime"}), patch.object(vm.os,"open") as opened:
            result,detail = vm.oci_configuration(info,"podman")
        self.assertIsNone(result)
        opened.assert_not_called()
        self.assertIn("outside",detail)


    def test_guest_ssh_uses_dedicated_identity_and_strict_local_host(self):
        with tempfile.TemporaryDirectory() as temporary:
            state = Path(temporary)
            for name in ["guest_ed25519", "known_hosts"]:
                (state / name).touch(mode=0o600)
            command = vm.ssh_command(state, "nuschl-test", 50922, ["/usr/bin/uname", "-m"])
            for option in ["IdentityAgent=none", "StrictHostKeyChecking=yes", "BatchMode=yes", "ForwardAgent=no"]:
                self.assertIn(option, command)
            self.assertIn("nuschl-test@127.0.0.1", command)
            with self.assertRaises(vm.CheckError):
                vm.ssh_command(state, "bad user;command", 50922, ["true"])
            (state / "guest_ed25519").chmod(0o644)
            with self.assertRaisesRegex(vm.CheckError, "0600"):
                vm.ssh_command(state, "nuschl-test", 50922, ["true"])

    def test_guest_inspection_never_claims_rendering_or_xctest(self):
        runtimes = json.dumps({"runtimes": [{"identifier": "com.apple.CoreSimulator.SimRuntime.iOS-18-2",
                                           "version": "18.2", "isAvailable": True}]})
        outputs = ["x86_64\n", "14.5\n", "Xcode 16.2\nBuild version placeholder\n", "18.2\n", runtimes, "{}", "{}"]
        with patch.object(vm, "ssh_command", return_value=["fixed-command"]), patch.object(
            vm, "bounded_run", side_effect=[{"exit_code": 0, "output": output} for output in outputs]
        ):
            report = vm.probe(Path("/some/vm"), "nuschl-test", 50922)
        self.assertTrue(report["macos_boot_observed"])
        self.assertTrue(report["pinned_versions_match"])
        self.assertFalse(report["simulator_boot_render_and_xctest_proven"])

    def test_sdk_version_alone_does_not_prove_a_simulator_runtime(self):
        outputs = ["x86_64\n", "14.5\n", "Xcode 16.2\n", "18.2\n", "{}", "{}", "{}"]
        with patch.object(vm, "ssh_command", return_value=["fixed-command"]), patch.object(
            vm, "bounded_run", side_effect=[{"exit_code": 0, "output": output} for output in outputs]
        ):
            report = vm.probe(Path("/some/vm"), "nuschl-test", 50922)
        self.assertFalse(report["pinned_versions_match"])
        self.assertFalse(report["pinned_simulator_runtime_available"])

    def test_container_probe_cleans_only_its_own_random_name(self):
        results = [{"exit_code": 0, "output": '[{"Architecture":"amd64"}]'},
                   {"exit_code": 0, "output": '{"kvm_api":12,"create_vm":true}'},
                   {"exit_code": 0, "output": ""}]
        with patch.object(vm, "bounded_run", side_effect=results) as run:
            vm.container_probe("podman", "sha256:" + "a" * 64)
        launch = run.call_args_list[1].args[0]
        cleanup = run.call_args_list[2].args[0]
        name = launch[launch.index("--name") + 1]
        self.assertTrue(name.startswith("nuschl-ios-kvm-probe-"))
        self.assertEqual(cleanup[-1], name)
        self.assertIn("--ignore", cleanup)


if __name__ == "__main__":
    unittest.main()
