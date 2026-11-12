"""Run with nixos-test-driver --test-script before booting any guest.

Requires a fresh, owned NUSCHL_VM_SCRATCH_ROOT on disk. This is a path-selection
probe, not a guest functional test. Set TMPDIR=root/tmp and XDG_RUNTIME_DIR=root/run.
"""
import json
import os
from pathlib import Path
import stat
import subprocess

root = Path(os.environ["NUSCHL_VM_SCRATCH_ROOT"])
assert root.is_absolute() and root.resolve() == root
assert root.stat().st_uid == os.getuid()
assert stat.S_IMODE(root.stat().st_mode) == 0o700
filesystem = subprocess.check_output(["stat", "-f", "-c", "%T", root], text=True).strip()
assert filesystem not in ("tmpfs", "ramfs"), filesystem
runtime = root / "run"
assert os.environ["XDG_RUNTIME_DIR"] == str(runtime)
assert os.environ["TMPDIR"] == str(root / "tmp")
assert runtime.resolve() == runtime
assert stat.S_IMODE(runtime.stat().st_mode) == 0o700
assert runtime.stat().st_dev == root.stat().st_dev
paths = {}
# `machines` is provided by the actual pinned NixOS test driver.
for machine in machines:  # noqa: F821
    expected = runtime / ("vm-state-" + machine.name)
    assert machine.tmp_dir == runtime, machine.tmp_dir
    assert machine.state_dir == expected, machine.state_dir
    assert expected.resolve() == expected
    assert expected.stat().st_uid == os.getuid()
    assert expected.stat().st_dev == root.stat().st_dev
    paths[machine.name] = str(expected)
assert paths
(root / "scratch-proof.json").write_text(json.dumps({
    "kind": "actual-driver-pre-boot-path-probe", "guest_booted": False,
    "filesystem": filesystem, "root": str(root), "state_paths": paths,
}, indent=2) + "\n")
print("All actual driver machine-state paths are isolated under owned disk runtime")
