#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Host-only tests of tests/vm_driver.py and its pre-boot guard; no VM boots.

Disk cases use NUSCHL_VM_TEST_DISK (default: the cache directory), RAM cases a
tmpfs from XDG_RUNTIME_DIR, /dev/shm or /tmp. Each test removes only the
directory it created.
"""
import os
from pathlib import Path
import shutil
import stat
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

import vm_driver as runner

scratch = runner.scratch
ScratchError = runner.ScratchError

# The socket-relevant lines of a pinned qemu-vm.nix start script.
VIRTIOFS_SCRIPT = """#! /bin/sh
NIX_VIRTIOFS_DIR=$(mktemp -d)
virtiofsd \\
  --socket-path="$NIX_VIRTIOFS_DIR"/"nix-store" \\
  --shared-dir="$(readlink -f "/nix/store")" \\
  &
virtiofsd \\
  --socket-path="$NIX_VIRTIOFS_DIR"/"shared" \\
  &
exec qemu-system-x86_64 \\
    -netdev vde,id=vlan1,sock="$QEMU_VDE_SOCKET_1" \\
    -chardev socket,id=nix-store,path=$NIX_VIRTIOFS_DIR/nix-store \\
    -device vhost-user-fs-pci,chardev=nix-store,tag=nix-store \\
    -chardev socket,id=shared,path=$NIX_VIRTIOFS_DIR/shared \\
    -device vhost-user-fs-pci,chardev=shared,tag=shared \\
    "$@"
"""
# virtualisation.tpm.enable, as qemu-vm.nix writes it.
SWTPM_SCRIPT = """#! /bin/sh
NIX_SWTPM_DIR=$(readlink -f "${NIX_SWTPM_DIR:-machine-swtpm}")
mkdir -p "$NIX_SWTPM_DIR"
swtpm \\
  socket \\
  --tpmstate dir="$NIX_SWTPM_DIR" \\
  --server type=unixio,path="$NIX_SWTPM_DIR"/socket \\
  --ctrl type=unixio,path="$NIX_SWTPM_DIR"/socket.ctrl \\
  --pid file="$NIX_SWTPM_DIR"/pid --daemon \\
  --log file="$NIX_SWTPM_DIR"/stdout,level=6
exec qemu-system-x86_64 \\
    -chardev socket,id=chrtpm,path="$NIX_SWTPM_DIR"/socket.ctrl \\
    "$@"
"""
H = "0" * 32
WRAPPER = (f"#! /nix/store/{H}-bash-5.3p20/bin/bash -e\n"
           f'exec -a "$0" "/nix/store/{H}-nixos-test-driver-x/bin/.nixos-test-driver-wrapped"  '
           f'--config /nix/store/{H}-driverConfiguration.json --log-level info "$@" \n')


def tmpfs_directory():
    for candidate in (os.environ.get("XDG_RUNTIME_DIR"), "/dev/shm", "/tmp"):
        if candidate and os.path.isdir(candidate) and os.access(candidate, os.W_OK) \
                and scratch.filesystem(candidate) == "tmpfs":
            return candidate
    return None


def disk_directory():
    return Path(os.environ.get("NUSCHL_VM_TEST_DISK") or runner.cache_directory()).resolve()


class Scratch(unittest.TestCase):
    def setUp(self):
        disk = disk_directory()
        if scratch.memory_backing(disk):
            self.skipTest(f"{disk} is not disk-backed; set NUSCHL_VM_TEST_DISK")
        self.base = Path(tempfile.mkdtemp(prefix="t", dir=disk))
        self.addCleanup(shutil.rmtree, self.base)

    def tmpfs(self):
        directory = tmpfs_directory()
        if directory is None:
            self.skipTest("no writable tmpfs to test against")
        path = Path(tempfile.mkdtemp(prefix="vm-driver-test-", dir=directory)).resolve()
        self.addCleanup(shutil.rmtree, path)
        return path

    def root(self, name="r"):
        root = runner.check_new_root(self.base / name, self.base)
        runner.create_root(root)
        return root

    def env(self, root):
        return {"NUSCHL_VM_SCRATCH_ROOT": str(root), "TMPDIR": str(root / "tmp"),
                "XDG_RUNTIME_DIR": str(root / "run"), "PATH": os.environ.get("PATH", "")}

    def fake_driver(self, root, script=VIRTIOFS_SCRIPT, names=("machine",)):
        """Objects with the attributes the pinned driver exposes before boot."""
        runtime = root / "run"
        path = self.base / "run-vm"
        path.write_text(script)
        shared = runtime / "shared-xchg"
        shared.mkdir(mode=0o700, exist_ok=True)

        def build_environment(state_dir, shared_dir, efi_vars_path=None):
            return dict(os.environ, TMPDIR=str(state_dir), SHARED_DIR=str(shared_dir), USE_TMPDIR="1")

        machines = []
        for name in names:
            state = runtime / f"vm-state-{name}"
            state.mkdir(mode=0o700)
            machines.append(SimpleNamespace(
                name=name, tmp_dir=runtime, state_dir=state, shared_dir=shared, booted=False,
                qmp_path=state / "qmp", monitor_path=state / "monitor", shell_path=state / "shell",
                efi_vars_path=state / f"{name}-efi-vars.fd",
                start_command=SimpleNamespace(build_environment=build_environment)))
        vlan = runtime / "vde1.ctl"
        vlan.mkdir(mode=0o700)
        driver = SimpleNamespace(config=SimpleNamespace(vms={name: SimpleNamespace(start_script=path)
                                                             for name in names}), vhost_vsock=None)
        return machines, [SimpleNamespace(socket_dir=vlan)], driver


class Roots(Scratch):
    def test_tmpfs_root_is_refused(self):
        parent = self.tmpfs()
        self.assertEqual(scratch.memory_backing(parent), "tmpfs")
        with self.assertRaisesRegex(ScratchError, "RAM-backed"):
            runner.check_new_root(parent / "r", parent)
        self.assertFalse((parent / "r").exists())

    def test_cli_refuses_tmpfs_before_creating_anything(self):
        parent = self.tmpfs()
        with patch.dict(os.environ, {"XDG_CACHE_HOME": str(parent)}):
            self.assertEqual(runner.main(["--root", str(parent / "r"), "/nix/store/missing"]), 2)
        self.assertEqual(list(parent.iterdir()), [])

    def test_disk_root_is_accepted(self):
        root = self.root()
        for path in (root, root / "run", root / "tmp", root / "out", root / "home"):
            self.assertEqual(stat.S_IMODE(path.stat().st_mode), 0o700)
        env = runner.environment(root, ["NUSCHL_PKI_FIXTURE_DIRECTORY=/fixture"])
        self.assertEqual((env["TMPDIR"], env["XDG_RUNTIME_DIR"]), (str(root / "tmp"), str(root / "run")))
        kind, _ = scratch.check_root(root, env)
        self.assertNotIn(kind, scratch.RAM_FILESYSTEMS)

    def test_reused_root_is_refused(self):
        (self.base / "r").mkdir()
        with self.assertRaisesRegex(ScratchError, "already exists"):
            runner.check_new_root(self.base / "r", self.base)

    def test_root_outside_cache_is_refused(self):
        with self.assertRaisesRegex(ScratchError, "inside the cache"):
            runner.check_new_root(self.base / "r", self.base / "cache")

    def test_symlinked_or_shared_parent_is_refused(self):
        (self.base / "real").mkdir()
        (self.base / "link").symlink_to(self.base / "real")
        with self.assertRaisesRegex(ScratchError, "without symlinks"):
            runner.check_new_root(self.base / "link" / "r", self.base)
        (self.base / "open").mkdir()
        (self.base / "open").chmod(0o770)
        with self.assertRaisesRegex(ScratchError, "group/other-writable"):
            runner.check_new_root(self.base / "open" / "r", self.base)

    def test_paths_qemu_options_cannot_carry_are_refused(self):
        for name in ("a,b", "a b", "../r"):
            with self.assertRaisesRegex(ScratchError, "normalized path"):
                runner.check_new_root(Path(f"{self.base}/{name}"), self.base)

    def test_only_fixture_inputs_may_be_added(self):
        root = self.base / "r"
        for item in ("TMPDIR=/tmp", "XDG_RUNTIME_DIR=/run/user/1", "NIX_DISK_IMAGE=/tmp/x", "QEMU_OPTS=-x",
                     "DISPLAY=:0", "NUSCHL_VM_SCRATCH_ROOT=/tmp", "NUSCHL_X"):
            with self.assertRaises(ScratchError, msg=item):
                runner.environment(root, [item])


class Wrapper(unittest.TestCase):
    def test_pinned_shape_is_parsed(self):
        found = runner.WRAPPER.fullmatch(WRAPPER)
        self.assertEqual(found["config"], f"/nix/store/{H}-driverConfiguration.json")

    def test_other_shapes_fail_closed(self):
        for text in (WRAPPER.replace("--log-level info", "--log-level info --keep-machine-state"),
                     WRAPPER.replace(f"/nix/store/{H}-driverConfiguration.json", "/tmp/c.json"),
                     WRAPPER + "echo extra\n", WRAPPER.replace("--config", "-c")):
            self.assertIsNone(runner.WRAPPER.fullmatch(text))

    def test_configuration_shape_fails_closed(self):
        good = {"containers": {}, "enable_ssh_backdoor": False, "global_timeout": 3600,
                "test_script": f"/nix/store/{H}-test-script", "vlans": [1],
                "vms": {"machine": {"name": "machine", "start_script": f"/nix/store/{H}-run-machine-vm"}}}
        self.assertEqual(runner.check_config(good), good)
        node = good["vms"]["machine"]
        for bad in (good | {"extra": 1}, good | {"containers": {"c": node}}, good | {"vms": {}},
                    good | {"vms": {"a/b": node | {"name": "a/b"}}}, good | {"vms": {"other": node}},
                    good | {"vms": {"machine": node | {"vnc": None}}}, good | {"vlans": ["1"]},
                    good | {"enable_ssh_backdoor": 1}, good | {"test_script": None}, []):
            with self.assertRaises(ScratchError, msg=bad):
                runner.check_config(bad)

    @unittest.skipUnless(os.environ.get("NUSCHL_VM_TEST_DRIVER"), "set NUSCHL_VM_TEST_DRIVER to a built driver")
    def test_built_driver_is_parsed(self):
        loaded = runner.load_driver(os.environ["NUSCHL_VM_TEST_DRIVER"])
        self.assertTrue(loaded["nodes"])
        self.assertTrue(loaded["script"])


class Guard(Scratch):
    def test_actual_shape_is_accepted_and_recorded(self):
        root = self.root()
        machines, vlans, driver = self.fake_driver(root, names=("a", "b"))
        proof = scratch.guard(machines, vlans, driver, self.env(root))
        self.assertEqual(proof["state_paths"], {name: str(root / "run" / f"vm-state-{name}") for name in "ab"})
        self.assertTrue(proof["sockets"]["longest"].endswith("/nix-store"))
        self.assertEqual(proof["sockets"]["checked"], 2 * 5)
        self.assertTrue((root / "scratch-proof.json").exists())

    def test_too_long_socket_path_is_refused(self):
        root = self.root("r" * (scratch.UNIX_PATH_MAX - len(str(self.base)) - 20))
        machines, vlans, driver = self.fake_driver(root)
        with self.assertRaisesRegex(ScratchError, f"over {scratch.UNIX_PATH_MAX}"):
            scratch.guard(machines, vlans, driver, self.env(root))
        self.assertFalse((root / "scratch-proof.json").exists())

    def test_swtpm_sockets_are_counted(self):
        root = self.root()
        state = root / "run" / "vm-state-machine"
        sockets = scratch.start_script_sockets(self.fake_driver(root, SWTPM_SCRIPT)[2].config.vms["machine"]
                                               .start_script, state, dict(os.environ, TMPDIR=str(state)))
        self.assertEqual(sockets, [state / "machine-swtpm" / "socket", state / "machine-swtpm" / "socket.ctrl"])

    def test_unrecognized_socket_forms_fail_closed(self):
        scripts = (VIRTIOFS_SCRIPT.replace('"$@"', "-chardev socket,id=x,path=/tmp/x.sock \\\n    \"$@\""),
                   VIRTIOFS_SCRIPT + 'rm -rf "$NIX_VIRTIOFS_DIR"\n',
                   VIRTIOFS_SCRIPT.replace("$(mktemp -d)", "$(mktemp -d -p /tmp)"))
        for index, script in enumerate(scripts):
            root = self.root(f"r{index}")
            machines, vlans, driver = self.fake_driver(root, script)
            with self.assertRaises(ScratchError, msg=script):
                scratch.guard(machines, vlans, driver, self.env(root))

    def test_inherited_runtime_directory_is_refused(self):
        root = self.root()
        machines, vlans, driver = self.fake_driver(root)
        with self.assertRaisesRegex(ScratchError, "XDG_RUNTIME_DIR must be"):
            scratch.guard(machines, vlans, driver, self.env(root) | {"XDG_RUNTIME_DIR": "/run/user/1000"})
        with self.assertRaisesRegex(ScratchError, "QEMU_OPTS"):
            scratch.guard(machines, vlans, driver, self.env(root) | {"QEMU_OPTS": "-chardev x"})

    def test_escaped_shared_reused_or_booted_state_is_refused(self):
        root = self.root()
        machines, vlans, driver = self.fake_driver(root)
        machine = machines[0]
        cases = [("state_dir", root / "tmp" / "vm-state-machine", "not directly inside"),
                 ("tmp_dir", Path("/run/user/1000"), "not directly inside"),
                 ("booted", True, "already booted")]
        for name, value, message in cases:
            with patch.object(machine, name, value), self.assertRaisesRegex(ScratchError, message):
                scratch.guard(machines, vlans, driver, self.env(root))
        (machine.state_dir / "machine.qcow2").write_bytes(b"")
        with self.assertRaisesRegex(ScratchError, "reused state"):
            scratch.guard(machines, vlans, driver, self.env(root))

    def test_guard_refuses_a_tmpfs_root(self):
        root = self.tmpfs()
        for name in ("run", "tmp"):
            (root / name).mkdir(mode=0o700)
        root.chmod(0o700)
        machines, vlans, driver = self.fake_driver(root)
        with self.assertRaisesRegex(ScratchError, "RAM-backed"):
            scratch.guard(machines, vlans, driver, self.env(root))


class Supervision(Scratch):
    def run_group(self, code, **limits):
        return runner.supervise([sys.executable, "-c", code], dict(os.environ), self.base, self.base / "log",
                                interval=0.2, grace=1, **limits)

    def test_timeout_stops_and_reaps_the_whole_group(self):
        ignore = "import signal,time; signal.signal(signal.SIGINT, signal.SIG_IGN); "
        result = self.run_group(ignore + "import subprocess,sys; subprocess.Popen([sys.executable,'-c',"
                                f"{ignore + 'time.sleep(60)'!r}]); time.sleep(60)", timeout=1, floor=0)
        self.assertIn("timeout", result["abort"])
        self.assertEqual(result["exit"], -9)
        self.assertTrue(result["group_empty"])
        self.assertLess(result["seconds"], 15)

    def test_orphaned_group_member_is_killed_after_exit(self):
        result = self.run_group("import subprocess,sys; subprocess.Popen([sys.executable,'-c',"
                                "'import time; time.sleep(60)'])", timeout=60, floor=0)
        self.assertEqual((result["exit"], result["abort"]), (0, None))
        self.assertTrue(result["leftover_group_killed"])
        self.assertTrue(result["group_empty"])

    def test_resource_floor_stops_the_group(self):
        result = self.run_group("import time; time.sleep(60)", timeout=60, floor=1 << 62, watch=(self.base,))
        self.assertIn("below", result["abort"])
        self.assertTrue(result["group_empty"])


if __name__ == "__main__":
    unittest.main()
