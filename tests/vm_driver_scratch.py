# SPDX-License-Identifier: EUPL-1.2
"""Pre-boot guard for the actual NixOS test driver's machine-state paths.

The pinned driver keeps machine state, sockets and the shared directory below an
inherited XDG_RUNTIME_DIR, and uses TMPDIR only when that is unset. Run this in the
driver's test-script namespace before any guest starts: tests/vm_driver.py runs it
ahead of the driver's own script, and fixtures may include it themselves.

It requires a fresh, owned mode-0700 NUSCHL_VM_SCRATCH_ROOT on disk with
TMPDIR=root/tmp and XDG_RUNTIME_DIR=root/run. It reads the paths the running driver
actually chose, and the Unix sockets each machine's start script will create, and
refuses RAM-backed storage, state outside root/run, reused state and socket paths
the platform cannot bind. Imported as a module it only defines the checks.
"""
# Fixtures concatenate this file before their own script; private aliases keep
# that script's imports from redefining unused names (ruff F811 in the driver build).
import json as _json
import os
from pathlib import Path as _Path
import re
import shutil as _shutil
import stat as _stat
import subprocess as _subprocess

# sockaddr_un.sun_path is 108 bytes on Linux; QEMU, virtiofsd and vde_switch need
# the path NUL-terminated inside it.
UNIX_PATH_MAX = 107
RAM_FILESYSTEMS = ("tmpfs", "ramfs")
START_SCRIPT_MAX = 1 << 20
_NAME = r"[A-Za-z0-9._-]+"
# The only Unix-socket forms a pinned qemu-vm.nix start script may contain. Any
# other socket-like token is refused rather than left unchecked.
VIRTIOFS_ASSIGNMENT = re.compile(r"^NIX_VIRTIOFS_DIR=\$\(mktemp -d\)$", re.MULTILINE)
VIRTIOFS_SOCKET = re.compile(r'--socket-path="\$NIX_VIRTIOFS_DIR"/"(' + _NAME + r')"')
VIRTIOFS_CHARDEV = re.compile(r"-chardev socket,id=" + _NAME + r",path=\$NIX_VIRTIOFS_DIR/(" + _NAME + r")(?=\s)")
SWTPM_ASSIGNMENT = re.compile(r'^\s*NIX_SWTPM_DIR=\$\(readlink -f "\$\{NIX_SWTPM_DIR:-(' + _NAME + r')\}"\)$', re.MULTILINE)
SWTPM_SOCKET = re.compile(r'type=unixio,path="\$NIX_SWTPM_DIR"/(' + _NAME + r')(?=\s)')
SWTPM_CHARDEV = re.compile(r'-chardev socket,id=' + _NAME + r',path="\$NIX_SWTPM_DIR"/(' + _NAME + r')(?=\s)')
SOCKET_TOKEN = re.compile(r"--socket-path=\S+|-chardev socket\S*|type=unixio\S*|unix:\S+")
COREUTILS_PATH = re.compile(r"^export PATH=(/nix/store/[^:$\s]+/bin)\$\{PATH:\+:\}\$PATH$", re.MULTILINE)


class ScratchError(Exception):
    """A VM scratch path that must not be used."""


def mount_of(path):
    """The longest (last-stacked) mount containing `path`: (mount point, type, source)."""
    path = _Path(path).resolve()
    best = None
    for line in _Path("/proc/self/mountinfo").read_text().splitlines():
        before, after = line.split(" - ", 1)
        point = _Path(re.sub(r"\\([0-7]{3})", lambda m: chr(int(m[1], 8)), before.split()[4]))
        if (point == path or point in path.parents) and (best is None or len(point.parts) >= len(best[0].parts)):
            fstype, source = after.split()[:2]
            best = (point, fstype, source)
    if best is None:
        raise ScratchError(f"no mount found for {path}")
    return best


def filesystem(path):
    return _subprocess.check_output(["stat", "-f", "-c", "%T", str(path)], text=True).strip()


def memory_backing(path):
    """Why `path` is RAM-backed (tmpfs/ramfs, or a zram/brd block device), or None."""
    kind = filesystem(path)
    _, fstype, source = mount_of(path)
    if kind in RAM_FILESYSTEMS or fstype in RAM_FILESYSTEMS:
        return kind if kind in RAM_FILESYSTEMS else fstype
    if re.fullmatch(r"/dev/(zram|ram)[0-9]+", source):
        return f"{fstype} on {source}"
    return None


def require_disk(path, what):
    reason = memory_backing(path)
    if reason is not None:
        raise ScratchError(f"{what} {path} is RAM-backed ({reason}); VM state must be on disk")


def require_owned_directory(path, what, mode=0o700):
    info = os.lstat(path)
    if not _stat.S_ISDIR(info.st_mode) or _Path(path).resolve() != _Path(path):
        raise ScratchError(f"{what} {path} must be a real directory without symlinks")
    if info.st_uid != os.getuid():
        raise ScratchError(f"{what} {path} is not owned by uid {os.getuid()}")
    if mode is not None and _stat.S_IMODE(info.st_mode) != mode:
        raise ScratchError(f"{what} {path} has mode {_stat.S_IMODE(info.st_mode):o}, expected {mode:o}")
    return info


def check_root(root, environ):
    """The scratch root and its runtime directory; returns (filesystem, mount source)."""
    root = _Path(root)
    if not root.is_absolute():
        raise ScratchError(f"scratch root {root} must be absolute")
    info = require_owned_directory(root, "scratch root")
    require_disk(root, "scratch root")
    runtime = root / "run"
    for name, expected in (("XDG_RUNTIME_DIR", runtime), ("TMPDIR", root / "tmp")):
        if environ.get(name) != str(expected):
            raise ScratchError(f"{name} must be {expected}, not {environ.get(name)!r}: the driver "
                               "prefers an inherited XDG_RUNTIME_DIR over TMPDIR")
    if require_owned_directory(runtime, "runtime directory").st_dev != info.st_dev:
        raise ScratchError(f"runtime directory {runtime} is on another filesystem than {root}")
    for name in ("QEMU_OPTS", "QEMU_NET_OPTS", "NIX_DISK_IMAGE", "NIX_SWTPM_DIR"):
        if environ.get(name):
            raise ScratchError(f"{name} is set; it can place images or sockets outside the scratch root")
    _, _, source = mount_of(root)
    return filesystem(root), source


def start_script_sockets(script, state_dir, environment):
    """Unix sockets a qemu-vm.nix start script creates, from its fixed-shape text."""
    script = _Path(script).resolve()
    if not script.is_file() or script.stat().st_size > START_SCRIPT_MAX:
        raise ScratchError(f"start script {script} is not a bounded regular file")
    text = script.read_text()
    sockets = []
    virtiofs = VIRTIOFS_SOCKET.findall(text) + VIRTIOFS_CHARDEV.findall(text)
    swtpm = SWTPM_SOCKET.findall(text) + SWTPM_CHARDEV.findall(text)
    if "NIX_VIRTIOFS_DIR" in text:
        assignments = VIRTIOFS_ASSIGNMENT.findall(text)
        if len(assignments) != 1 or text.count("$NIX_VIRTIOFS_DIR") != len(virtiofs):
            raise ScratchError(f"{script}: unrecognized NIX_VIRTIOFS_DIR use")
        # The same mktemp, in the same environment the driver gives the start script.
        found = COREUTILS_PATH.search(text)
        mktemp = _Path(found[1], "mktemp") if found else _shutil.which("mktemp", path=environment.get("PATH"))
        if mktemp is None or not _Path(mktemp).is_file():
            raise ScratchError(f"{script}: cannot locate the mktemp its virtiofs directory uses")
        directory = _Path(_subprocess.check_output([str(mktemp), "-u", "-d"], env=environment, text=True).strip())
        if directory.parent != state_dir:
            raise ScratchError(f"{script}: virtiofs sockets would be in {directory.parent}, not {state_dir}")
        sockets += [directory / tag for tag in sorted(set(virtiofs))]
    if "NIX_SWTPM_DIR" in text:
        assignments = SWTPM_ASSIGNMENT.findall(text)
        if len(assignments) != 1 or not swtpm:
            raise ScratchError(f"{script}: unrecognized NIX_SWTPM_DIR use")
        sockets += [state_dir / assignments[0] / name for name in sorted(set(swtpm))]
    for token in SOCKET_TOKEN.findall(text):
        if not (VIRTIOFS_SOCKET.match(token) or "$NIX_VIRTIOFS_DIR" in token or "$NIX_SWTPM_DIR" in token):
            raise ScratchError(f"{script}: unchecked Unix socket form {token[:120]!r}")
    return sockets


def check_sockets(paths, runtime):
    longest = max(paths, key=lambda path: len(os.fsencode(path)))
    for path in paths:
        if runtime not in _Path(path).parents:
            raise ScratchError(f"Unix socket {path} is outside {runtime}")
        size = len(os.fsencode(path))
        if size > UNIX_PATH_MAX:
            raise ScratchError(f"Unix socket path {path} is {size} bytes, over {UNIX_PATH_MAX}; "
                               "use a shorter scratch root")
    return {"limit": UNIX_PATH_MAX, "longest": str(longest), "bytes": len(os.fsencode(longest)),
            "checked": len(paths)}


def guard(machines, vlans, driver, environ=os.environ):
    """Check the running driver's actual paths before boot and record them."""
    if not environ.get("NUSCHL_VM_SCRATCH_ROOT"):
        raise ScratchError("NUSCHL_VM_SCRATCH_ROOT is not set")
    root = _Path(environ["NUSCHL_VM_SCRATCH_ROOT"])
    kind, source = check_root(root, environ)
    runtime = root / "run"
    device = os.stat(root).st_dev
    machines = list(machines)
    if not machines:
        raise ScratchError("the driver has no machines")
    states, sockets = {}, []
    for machine in machines:
        for name in ("qmp_path", "monitor_path", "shell_path", "efi_vars_path", "start_command"):
            if not hasattr(machine, name):
                raise ScratchError(f"{machine.name}: only QEMU machines are checked")
        if machine.booted:
            raise ScratchError(f"{machine.name} already booted; run this guard before any start()")
        state = _Path(machine.state_dir)
        if _Path(machine.tmp_dir) != runtime or state.parent != runtime or state.resolve() != state:
            raise ScratchError(f"{machine.name}: driver state {state} (tmp {machine.tmp_dir}) is not "
                               f"directly inside {runtime}")
        if state in states.values():
            raise ScratchError(f"{machine.name}: state directory {state} is shared")
        if require_owned_directory(state, "machine state").st_dev != device:
            raise ScratchError(f"{machine.name}: {state} is on another filesystem")
        if any(state.iterdir()):
            raise ScratchError(f"{machine.name}: state directory {state} is not empty (reused state)")
        if _Path(machine.shared_dir).parent != runtime:
            raise ScratchError(f"{machine.name}: shared directory {machine.shared_dir} escapes {runtime}")
        if _Path(machine.efi_vars_path).parent != state:
            raise ScratchError(f"{machine.name}: EFI variables {machine.efi_vars_path} escape {state}")
        environment = machine.start_command.build_environment(state, _Path(machine.shared_dir))
        if environment.get("TMPDIR") != str(state) or environment.get("SHARED_DIR") != str(machine.shared_dir):
            raise ScratchError(f"{machine.name}: the start script would not receive its state directory")
        sockets += [_Path(machine.qmp_path), _Path(machine.monitor_path), _Path(machine.shell_path)]
        sockets += start_script_sockets(driver.config.vms[machine.name].start_script, state, environment)
        states[machine.name] = state
    vlan_dirs = []
    for vlan in vlans:
        directory = _Path(vlan.socket_dir)
        if directory.parent != runtime:
            raise ScratchError(f"VLAN socket directory {directory} escapes {runtime}")
        vlan_dirs.append(str(directory))
        sockets += [entry for entry in directory.iterdir() if _stat.S_ISSOCK(entry.lstat().st_mode)]
    vsock = getattr(driver, "vhost_vsock", None)
    if vsock is not None:
        sockets += [path for pair in vsock.sockets.values() for path in (pair.host, pair.guest)]
    report = check_sockets(sockets, runtime)
    proof = {
        "kind": "actual-driver-pre-boot-path-probe", "guest_booted": False,
        "filesystem": kind, "mount_source": source, "root": str(root),
        "state_paths": {name: str(path) for name, path in states.items()},
        "shared_dirs": sorted({str(machine.shared_dir) for machine in machines}),
        "vlan_dirs": vlan_dirs, "sockets": report,
    }
    (root / "scratch-proof.json").write_text(_json.dumps(proof, indent=2) + "\n")
    for name, path in states.items():
        print(f"vm scratch: {name} state {path} ({kind} on {source})")
    print(f"vm scratch: longest Unix socket {report['bytes']}/{UNIX_PATH_MAX} bytes: {report['longest']}")
    print("All actual driver machine-state paths are isolated under owned disk runtime")
    return proof


# `machines`, `vlans` and `driver` exist only in the actual driver's test namespace.
if "machines" in globals() and "driver" in globals():
    guard(machines, vlans, driver)  # noqa: F821
