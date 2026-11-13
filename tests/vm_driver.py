#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Run a built NixOS test driver outside the Nix daemon with all VM state on disk.

    python3 tests/vm_driver.py --root ~/.cache/nuschl-vm-r1 [--probe] [--env NUSCHL_X=VALUE] DRIVER

The pinned driver puts machine state and sockets below an inherited XDG_RUNTIME_DIR
(normally a tmpfs) and uses TMPDIR only when that is unset, so both point into a
fresh, owned mode-0700 ROOT inside the user's cache directory. Every name the
driver derives from a node (vm-state-machine, ...) is then private to this run.
tests/vm_driver_scratch.py runs inside the actual driver before the test script and
refuses to boot if the driver's real state paths or start-script sockets are
RAM-backed, outside ROOT/run or too long to bind. --probe runs only that guard;
no guest boots. Nothing is deleted afterwards; ROOT holds the evidence.
"""
import argparse
import datetime
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import threading
import time

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("vm_driver_scratch", HERE / "vm_driver_scratch.py")
scratch = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scratch)
ScratchError = scratch.ScratchError

STORE = r"/nix/store/[0-9a-z]{32}-"
# The complete wrapper the pinned runNixOSTest emits as bin/nixos-test-driver.
WRAPPER = re.compile(
    "#! " + STORE + r"bash-[^/\s]+/bin/bash -e\n"
    r'exec -a "\$0" "(?P<wrapped>' + STORE + r'[^/"\s]+/bin/\.nixos-test-driver-wrapped)"  '
    r"--config (?P<config>" + STORE + r"driverConfiguration\.json) --log-level info " + r'"\$@" ' + "\n")
CONFIG_KEYS = {"containers", "enable_ssh_backdoor", "global_timeout", "test_script", "vlans", "vms"}
NODE = re.compile(r"[A-Za-z_][A-Za-z0-9_-]{0,62}")
ROOT_PATH = re.compile(r"/[A-Za-z0-9._/-]+")  # no commas, spaces or quotes for QEMU options
EXTRA_ENV = re.compile(r"NUSCHL_[A-Z0-9_]+")
RESERVED_ENV = {"NUSCHL_VM_SCRATCH_ROOT"}
TEXT_MAX = 4 << 20
GIB = 1 << 30


def refuse(message):
    raise ScratchError(message)


def store_file(path, what):
    path = Path(path)
    if not re.fullmatch(STORE + r"[^/]+(/[^/]+)*", str(path)) or path.resolve() != path:
        refuse(f"{what} {path} is not a plain /nix/store path")
    info = path.lstat()
    if not stat.S_ISREG(info.st_mode) or info.st_size > TEXT_MAX:
        refuse(f"{what} {path} is not a bounded regular file")
    return path


def load_driver(argument):
    """The driver's wrapper, configuration and test script, all fixed-shape."""
    driver = Path(argument).resolve(strict=True)
    if not re.fullmatch(STORE + r"nixos-test-driver-[^/]+", str(driver)):
        refuse(f"{driver} is not a nixos-test-driver store path")
    wrapper = store_file(driver / "bin" / "nixos-test-driver", "driver wrapper")
    found = WRAPPER.fullmatch(wrapper.read_text())
    if found is None:
        refuse(f"{wrapper} is not the pinned driver wrapper shape; update tests/vm_driver.py deliberately")
    if found["wrapped"] != f"{driver}/bin/.nixos-test-driver-wrapped":
        refuse(f"{wrapper} executes another driver: {found['wrapped']}")
    config = check_config(json.loads(store_file(Path(found["config"]), "driver configuration").read_text()))
    for name, node in config["vms"].items():
        store_file(Path(node["start_script"]).resolve(), f"{name} start script")
    script = store_file(Path(config["test_script"]), "test script").read_text()
    return {"driver": str(driver), "wrapper": str(wrapper), "config_path": found["config"],
            "nodes": sorted(config["vms"]), "vlans": config["vlans"], "test_script": config["test_script"],
            "script": script}


def check_config(config):
    """The pinned DriverConfiguration JSON shape, VM nodes only."""
    if not isinstance(config, dict) or set(config) != CONFIG_KEYS:
        refuse(f"driver configuration keys {sorted(config) if isinstance(config, dict) else config!r} "
               f"are not {sorted(CONFIG_KEYS)}")
    if config["containers"]:
        refuse("container nodes are not supported by this runner")
    if not isinstance(config["vms"], dict) or not config["vms"]:
        refuse("the driver has no VM nodes")
    for name, node in config["vms"].items():
        if not NODE.fullmatch(name) or not isinstance(node, dict) or set(node) != {"name", "start_script"} \
                or node["name"] != name:
            refuse(f"unexpected VM node entry {name!r}")
        if not isinstance(node["start_script"], str):
            refuse(f"unexpected start script for {name!r}")
    if not isinstance(config["vlans"], list) or not all(type(v) is int and 0 <= v < 1024 for v in config["vlans"]):
        refuse(f"unexpected VLAN list {config['vlans']!r}")
    if type(config["enable_ssh_backdoor"]) is not bool or type(config["global_timeout"]) is not int \
            or not isinstance(config["test_script"], str):
        refuse("unexpected enable_ssh_backdoor, global_timeout or test_script value")
    return config


def cache_directory():
    return Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache").resolve(strict=True)


def check_new_root(root, cache):
    """A fresh scratch root that may be created: inside `cache`, below an owned disk directory."""
    root = Path(os.path.expanduser(root))
    if not root.is_absolute() or os.path.normpath(root) != str(root) or not ROOT_PATH.fullmatch(str(root)):
        refuse(f"scratch root {root} must be an absolute normalized path of [A-Za-z0-9._/-]")
    if os.path.lexists(root):
        refuse(f"scratch root {root} already exists; use a fresh path so no earlier state is reused")
    parent = root.parent
    if not parent.is_dir() or parent.resolve() != parent:
        refuse(f"parent {parent} must be an existing directory without symlinks")
    if cache != parent and cache not in parent.parents:
        refuse(f"scratch root {root} must be inside the cache directory {cache}")
    info = parent.lstat()
    if info.st_uid != os.getuid() or info.st_mode & 0o022:
        refuse(f"parent {parent} must be owned by uid {os.getuid()} and not group/other-writable")
    scratch.require_disk(parent, "scratch parent")
    return root


def create_root(root):
    root.mkdir(mode=0o700)
    root.chmod(0o700)
    for name in ("run", "tmp", "out", "home"):
        (root / name).mkdir(mode=0o700)
        (root / name).chmod(0o700)
    for name in ("run", "tmp", "out", "home"):
        scratch.require_owned_directory(root / name, name)
    scratch.require_owned_directory(root, "scratch root")
    scratch.require_disk(root, "scratch root")


def environment(root, extras):
    env = {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LANG": "C.UTF-8", "HOME": str(root / "home"),
           "TMPDIR": str(root / "tmp"), "XDG_RUNTIME_DIR": str(root / "run"), "NUSCHL_VM_SCRATCH_ROOT": str(root)}
    for item in extras:
        name, separator, value = item.partition("=")
        if not separator or not EXTRA_ENV.fullmatch(name) or name in RESERVED_ENV:
            refuse(f"--env {name!r}: only NUSCHL_* fixture inputs may be added; scratch, QEMU, Nix and "
                   "display variables are fixed by the runner")
        env[name] = value
    return env


def compose(root, guard_source, script):
    """ROOT/test-script.py: the guard in a copy of the driver namespace, then the driver's script."""
    guard = root / "vm_driver_scratch.py"
    guard.write_text(guard_source)
    composed = root / "test-script.py"
    composed.write_text(
        "# Composed by tests/vm_driver.py: the pre-boot scratch guard, then the driver's own script.\n"
        f"exec(compile(open({str(guard)!r}).read(), {str(guard)!r}, 'exec'), dict(globals()))\n"
        + (script if script.endswith("\n") or not script else script + "\n"))
    for path in (guard, composed):
        path.chmod(0o600)
    return composed


def free_bytes(path):
    info = os.statvfs(path)
    return info.f_bavail * info.f_frsize


def mem_available():
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemAvailable:"):
            return int(line.split()[1]) * 1024
    return 0


def group_alive(pgid):
    try:
        os.killpg(pgid, 0)
    except ProcessLookupError:
        return False
    return True


def supervise(command, env, cwd, log, timeout, floor, watch=(), interval=2.0, grace=60.0, on_tick=None):
    """Run `command` in its own process group; stop that group on timeout or low resources.

    Only this group is signalled. The result is reported after the driver is reaped
    and the group is empty, never on the strength of a sent signal alone.
    """
    minimum = {str(path): None for path in watch} | {"MemAvailable": None}
    reason = None
    start = time.monotonic()
    with open(log, "wb") as sink:
        process = subprocess.Popen(command, env=env, cwd=cwd, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, start_new_session=True)
        pgid = process.pid

        def pump():
            for chunk in iter(lambda: process.stdout.read1(65536), b""):
                sink.write(chunk)
                sink.flush()
                sys.stdout.buffer.write(chunk)
                sys.stdout.buffer.flush()

        reader = threading.Thread(target=pump, daemon=True)
        reader.start()
        try:
            while process.poll() is None:
                samples = {str(path): free_bytes(path) for path in watch} | {"MemAvailable": mem_available()}
                for key, value in samples.items():
                    minimum[key] = value if minimum[key] is None else min(minimum[key], value)
                low = [key for key, value in samples.items() if value < floor]
                if low:
                    reason = f"below {floor} bytes free: {', '.join(low)}"
                elif time.monotonic() - start > timeout:
                    reason = f"timeout after {timeout} s"
                if reason:
                    break
                if on_tick:
                    on_tick()
                time.sleep(interval)
        except KeyboardInterrupt:
            reason = "interrupted"
        if reason and process.poll() is None:
            os.killpg(pgid, signal.SIGINT)
            try:
                process.wait(timeout=grace)
            except subprocess.TimeoutExpired:
                os.killpg(pgid, signal.SIGKILL)
        code = process.wait()
        deadline = time.monotonic() + 5
        while group_alive(pgid) and time.monotonic() < deadline:
            time.sleep(0.2)
        leftovers = group_alive(pgid)
        if leftovers:
            os.killpg(pgid, signal.SIGKILL)
            deadline = time.monotonic() + 30
            while group_alive(pgid) and time.monotonic() < deadline:
                time.sleep(0.2)
        reader.join(timeout=10)
        process.stdout.close()
    if on_tick:
        on_tick()
    return {"exit": code, "abort": reason, "seconds": round(time.monotonic() - start, 1),
            "minimum_free_bytes": minimum, "leftover_group_killed": leftovers,
            "group_empty": not group_alive(pgid)}


def socket_census(runtime, limit=20000):
    """Unix sockets actually present below `runtime` (bounded walk)."""
    found, seen = [], 0
    for directory, names, files in os.walk(runtime):
        for name in names + files:
            seen += 1
            path = Path(directory, name)
            try:
                if stat.S_ISSOCK(path.lstat().st_mode):
                    found.append(str(path))
            except FileNotFoundError:
                pass
            if seen >= limit:
                return found, True
    return found, False


def allocated_bytes(path):
    total = 0
    for directory, names, files in os.walk(path):
        for name in names + files:
            try:
                total += Path(directory, name).lstat().st_blocks * 512
            except FileNotFoundError:
                pass
    return total


def say(message):
    print(f"vm-driver: {message}", flush=True)


def interrupted(*_):
    raise KeyboardInterrupt


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--root", required=True, help="fresh scratch root inside the cache (keep it short)")
    parser.add_argument("--probe", action="store_true", help="run only the pre-boot guard; boot no guest")
    parser.add_argument("--env", action="append", default=[], metavar="NUSCHL_NAME=VALUE",
                        help="add a fixture input variable (NUSCHL_* only)")
    parser.add_argument("--timeout", type=int, default=1800, help="wall-clock limit in seconds")
    parser.add_argument("--min-free-gib", type=int, default=8,
                        help="stop below this much free /tmp, scratch filesystem or MemAvailable")
    parser.add_argument("driver", help="a built nixos-test-driver store path (the test's .driver)")
    args = parser.parse_args(argv)
    try:
        root = check_new_root(args.root, cache_directory())
        loaded = load_driver(args.driver)
        env = environment(root, args.env)
        create_root(root)
    except (ScratchError, OSError, ValueError, TypeError) as error:
        print(f"vm-driver: refusing to start: {error}", file=sys.stderr)
        return 2
    guard_source = (HERE / "vm_driver_scratch.py").read_text()
    composed = compose(root, guard_source, "" if args.probe else loaded["script"])
    command = [loaded["wrapper"], "--no-interactive", "-o", str(root / "out"), "--junit-xml", "junit.xml",
               "--test-script", str(composed)]
    say(f"root {root} ({scratch.filesystem(root)} on {scratch.mount_of(root)[2]}, mode 0700)")
    say(f"TMPDIR={env['TMPDIR']} XDG_RUNTIME_DIR={env['XDG_RUNTIME_DIR']}")
    probe = " (probe: no guest boots)" if args.probe else ""
    say(f"driver {loaded['driver']} nodes {', '.join(loaded['nodes'])}{probe}")
    proof_path = root / "scratch-proof.json"
    observed = {"proof": None, "sockets": set(), "truncated": False}

    def tick():
        if observed["proof"] is None and proof_path.exists():
            try:
                observed["proof"] = json.loads(proof_path.read_text())
            except ValueError:
                return
            for name, path in observed["proof"]["state_paths"].items():
                say(f"actual driver state {name}: {path} ({observed['proof']['filesystem']})")
        sockets, truncated = socket_census(root / "run")
        observed["sockets"].update(sockets)
        observed["truncated"] |= truncated

    signal.signal(signal.SIGTERM, interrupted)
    started = datetime.datetime.now().astimezone().isoformat(timespec="seconds")
    result = supervise(command, env, root, root / "driver.log", args.timeout, args.min_free_gib * GIB,
                       watch=(Path("/tmp"), root), on_tick=tick)
    sockets = sorted(observed["sockets"], key=lambda path: (-len(os.fsencode(path)), path))
    proof = observed["proof"]
    problems = []
    if proof is None:
        problems.append("the pre-boot guard did not report; the run is not disk-proven")
    elif proof.get("root") != str(root) or any(Path(path).parent != root / "run"
                                               for path in proof.get("state_paths", {}).values()):
        problems.append("the guard reported state outside ROOT/run")
    if any(len(os.fsencode(path)) > scratch.UNIX_PATH_MAX for path in sockets):
        problems.append("an observed Unix socket path is over the limit")
    if not result["group_empty"]:
        problems.append("processes of the driver's group remained after SIGKILL")
    status = {
        "started": started, "ended": datetime.datetime.now().astimezone().isoformat(timespec="seconds"),
        "command": command, "env": env, "driver": loaded["driver"], "config": loaded["config_path"],
        "test_script": loaded["test_script"], "probe": args.probe, **result, "proof": proof,
        "observed_sockets": {"count": len(sockets), "longest": sockets[0] if sockets else None,
                             "bytes": len(os.fsencode(sockets[0])) if sockets else 0,
                             "truncated": observed["truncated"]},
        "run_allocated_bytes": allocated_bytes(root / "run"), "problems": problems,
    }
    (root / "status.json").write_text(json.dumps(status, indent=2) + "\n")
    stopped = f", stopped: {result['abort']}" if result["abort"] else ""
    say(f"exit {result['exit']} after {result['seconds']} s{stopped}")
    say(f"ROOT/run holds {status['run_allocated_bytes']} allocated bytes; longest observed socket "
        f"{status['observed_sockets']['bytes']} bytes; status {root / 'status.json'}")
    for problem in problems:
        say(f"FAILED: {problem}")
    if result["exit"] != 0:
        return result["exit"] if result["exit"] > 0 else 1
    return 1 if problems or result["abort"] else 0


if __name__ == "__main__":
    raise SystemExit(main())
