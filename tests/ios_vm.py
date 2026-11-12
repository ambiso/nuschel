#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Bounded Linux-hosted macOS/iOS feasibility checks; see docs/ios-vm.md.

No command pulls an image, installs software, starts macOS, or accepts passwords.
`plan` prints a command for review; `probe` reads an already configured test guest.
"""
from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
import platform
import re
import selectors
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import time
import urllib.parse
import urllib.request
import uuid
from pathlib import Path

GIB = 1024**3
UPSTREAM = "aa05a2c9a06aabfc8542c1c373ad1ad0af74ede3"
IMAGE_DIGEST = "sha256:3a3c82c79bc4e73531f819ccdfa4053b3084efd7c1f645678dbf8b4b3a24369c"
IMAGE = "docker.io/sickcodes/docker-osx@" + IMAGE_DIGEST
PROFILE = {"macos": "14.5", "xcode": "16.2", "ios_sdk": "18.2",
           "simulator_runtime": "18.2", "rust_target": "x86_64-apple-ios"}
NAME = "nuschl-ios-feasibility"
MEMORY_FS = {"tmpfs", "ramfs", "hugetlbfs"}
KVM_SCRIPT = ("import fcntl,json,os; f=os.open('/dev/kvm',os.O_RDWR); "
              "a=fcntl.ioctl(f,0xAE00,0); v=fcntl.ioctl(f,0xAE01,0); "
              "print(json.dumps({'kvm_api':a,'create_vm':True})); os.close(v); os.close(f)")


class CheckError(Exception):
    pass


def bounded_run(command: list[str], timeout: float = 15, limit: int = 256 * 1024) -> dict:
    """Cap output and wall time; kill our entire process group on either limit."""
    process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, start_new_session=True)
    output = bytearray()
    deadline = time.monotonic() + timeout
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            while selector.get_map():
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise CheckError("command exceeded its time budget")
                for key, _ in selector.select(min(remaining, 0.25)):
                    chunk = os.read(key.fd, 65536)
                    if not chunk:
                        selector.unregister(key.fileobj)
                    elif len(output) + len(chunk) > limit:
                        raise CheckError("command exceeded its output budget")
                    else:
                        output.extend(chunk)
            code = process.wait(timeout=max(0.001, deadline - time.monotonic()))
        return {"exit_code": code, "output": output.decode("utf-8", errors="replace")}
    except BaseException:
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()
        raise
    finally:
        process.stdout.close()


def mount_type(path: Path) -> str:
    """Resolve through symlinks before selecting the longest Linux mount point."""
    path = path.resolve()
    best = (-1, "unknown")
    for line in Path("/proc/self/mountinfo").read_text().splitlines():
        before, after = line.split(" - ", 1)
        mount = Path(re.sub(r"\\([0-7]{3})", lambda m: chr(int(m[1], 8)), before.split()[4]))
        if path == mount or mount in path.parents:
            if len(mount.parts) > best[0]:
                best = len(mount.parts), after.split()[0]
    return best[1]


def disk_path(path: Path) -> Path:
    path = path.expanduser().resolve()
    if path == Path.home().resolve() or path == Path("/") or len(path.parts) < 3:
        raise CheckError("use a dedicated VM state directory, not a home or filesystem root")
    if any(c in str(path) for c in [",", "\n", "\r", "\x00"]):
        raise CheckError("VM state path contains mount-option delimiters")
    if mount_type(path) in MEMORY_FS:
        raise CheckError("VM state must be disk-backed; RAM-backed filesystems are refused")
    return path


def existing_parent(path: Path) -> Path:
    while not path.exists():
        path = path.parent
    return path


def runtime(explicit: str | None) -> str:
    engine = explicit or shutil.which("podman") or shutil.which("docker")
    if not engine:
        raise CheckError("no local Podman or Docker executable; nothing was installed")
    if Path(engine).name not in {"podman", "docker"}:
        raise CheckError("runtime must be a local podman or docker executable")
    if os.environ.get("CONTAINER_HOST") or os.environ.get("DOCKER_HOST") or os.environ.get("CONTAINER_CONNECTION"):
        raise CheckError("remote/container endpoint overrides are refused; use a local runtime")
    return engine


def engine_command(engine: str, *arguments: str) -> list[str]:
    if Path(engine).name == "podman":
        endpoint = ["--remote=false"]
    else:
        # Docker contexts can select a remote host even without DOCKER_HOST.
        socket = Path(f"/run/user/{os.getuid()}/docker.sock")
        endpoint = ["--host", "unix://" + str(socket if socket.exists() else Path("/var/run/docker.sock"))]
    return [engine, *endpoint, *arguments]


def engine_info(engine: str) -> dict:
    result = bounded_run(engine_command(engine, "info", "--format", "json"))
    if result["exit_code"]:
        raise CheckError("local container runtime is unavailable: " + result["output"][:1000])
    info = json.loads(result["output"])
    if Path(engine).name == "podman":
        if info.get("host", {}).get("serviceIsRemote"):
            raise CheckError("remote Podman is refused")
        store = info.get("store", {}).get("graphRoot")
        temp = info.get("store", {}).get("imageCopyTmpDir")
        rootless = info.get("host", {}).get("security", {}).get("rootless")
    else:
        store, temp, rootless = info.get("DockerRootDir"), None, "unverified"
    if not store or mount_type(Path(store)) in MEMORY_FS:
        raise CheckError("container image storage is absent or RAM-backed")
    if temp and mount_type(Path(temp)) in MEMORY_FS:
        raise CheckError("container image-copy temporary storage is RAM-backed")
    return {"executable": engine, "rootless": rootless, "storage": store,
            "storage_filesystem": mount_type(Path(store)), "image_copy_tmp": temp}


def preflight(state: Path, engine: str | None, budget: int, reserve: int) -> dict:
    blockers = []
    report = {"platform": platform.system(), "architecture": platform.machine(),
              "state": str(state), "filesystem": mount_type(state), "profile": PROFILE,
              "image": IMAGE, "upstream_revision": UPSTREAM,
              "state_budget_bytes": budget * GIB, "host_reserve_bytes": reserve * GIB}
    if platform.system() != "Linux" or platform.machine() != "x86_64":
        blockers.append("this experiment requires a Linux x86_64 KVM host")
    available = shutil.disk_usage(existing_parent(state)).free
    report["free_bytes"] = available
    if available < (budget + reserve) * GIB:
        blockers.append("free disk is below VM/image budget plus host reserve")
    meminfo = Path("/proc/meminfo").read_text()
    match = re.search(r"^MemAvailable:\s+(\d+) kB$", meminfo, re.MULTILINE)
    report["available_memory_bytes"] = int(match[1]) * 1024 if match else None
    if not match or int(match[1]) * 1024 < 16 * GIB:
        blockers.append("less than 16 GiB currently available for the bounded guest experiment")
    try:
        fd = os.open("/dev/kvm", os.O_RDWR | os.O_CLOEXEC)
        try:
            api = fcntl.ioctl(fd, 0xAE00, 0)
            vm = fcntl.ioctl(fd, 0xAE01, 0)
            os.close(vm)
            report["kvm"] = {"api": api, "create_vm": True}
            if api != 12:
                blockers.append("unexpected KVM API version")
        finally:
            os.close(fd)
    except OSError as error:
        blockers.append("KVM could not create a VM: " + str(error))
    try:
        report["container"] = engine_info(runtime(engine))
    except (CheckError, OSError, ValueError) as error:
        blockers.append(str(error))
    report["blockers"] = blockers
    report["host_preflight_passed"] = not blockers
    report["macos_simulator_swiftui_proven"] = False
    return report


def http_bytes(url: str, headers: dict | None = None) -> bytes:
    request = urllib.request.Request(url, headers={"User-Agent": "nuschl-ios-vm-feasibility", **(headers or {})})
    with urllib.request.urlopen(request, timeout=15) as response:
        data = response.read(1024 * 1024 + 1)
    if len(data) > 1024 * 1024:
        raise CheckError("registry metadata exceeded 1 MiB")
    return data


def verified_json(data: bytes, digest: str) -> dict:
    if "sha256:" + hashlib.sha256(data).hexdigest() != digest:
        raise CheckError("registry metadata digest mismatch")
    return json.loads(data)


def metadata() -> dict:
    """Fetch only digest-checked manifest/config metadata, never image layers."""
    auth = json.loads(http_bytes("https://auth.docker.io/token?" + urllib.parse.urlencode({
        "service": "registry.docker.io", "scope": "repository:sickcodes/docker-osx:pull"})))
    headers = {"Authorization": "Bearer " + auth["token"],
               "Accept": "application/vnd.docker.distribution.manifest.v2+json"}
    base = "https://registry-1.docker.io/v2/sickcodes/docker-osx/"
    manifest = verified_json(http_bytes(base + "manifests/" + IMAGE_DIGEST, headers), IMAGE_DIGEST)
    digest = manifest["config"]["digest"]
    if not re.fullmatch(r"sha256:[0-9a-f]{64}", digest):
        raise CheckError("invalid config digest")
    config = verified_json(http_bytes(base + "blobs/" + digest, headers), digest)
    compressed = sum(layer["size"] for layer in manifest["layers"])
    if config.get("architecture") != "amd64" or compressed > 2 * GIB:
        raise CheckError("image architecture or compressed size differs from the bounded profile")
    return {"image": IMAGE, "config_digest": digest, "architecture": config["architecture"],
            "created": config.get("created"), "compressed_layer_bytes": compressed,
            "layers_downloaded": False, "source_revision": UPSTREAM,
            "source_url": f"https://github.com/sickcodes/Docker-OSX/tree/{UPSTREAM}",
            "source_to_image_provenance_verified": False}


def pinned_image(image: str) -> None:
    if not re.fullmatch(r"(?:sha256:[0-9a-f]{64}|[a-zA-Z0-9./_-]+@sha256:[0-9a-f]{64})", image):
        raise CheckError("use a full local image ID or immutable repository@sha256 digest")


def container_probe(engine: str, image: str) -> dict:
    pinned_image(image)
    inspected = bounded_run(engine_command(engine, "image", "inspect", image))
    if inspected["exit_code"]:
        raise CheckError("probe image is not cached; no image was pulled")
    info = json.loads(inspected["output"])[0]
    if info.get("Architecture") != "amd64":
        raise CheckError("cached probe image is not amd64")
    name = "nuschl-ios-kvm-probe-" + uuid.uuid4().hex
    command = engine_command(engine, "run", "--name", name, "--rm", "--pull", "never", "--network", "none",
                             "--cap-drop", "ALL", "--security-opt", "no-new-privileges",
                             "--read-only", "--device", "/dev/kvm", "--memory", "128m",
                             "--cpus", "1", "--pids-limit", "32", "--user", f"{os.getuid()}:{os.getgid()}")
    if Path(engine).name == "podman":
        command += ["--userns", "keep-id", "--http-proxy=false"]
    command += ["--entrypoint", "python3", image, "-c", KVM_SCRIPT]
    try:
        result = bounded_run(command, timeout=30, limit=16 * 1024)
    finally:
        # Killing a client does not necessarily stop a daemon-managed container.
        # Only this invocation's random name is eligible for cleanup.
        cleanup = engine_command(engine, "rm", "--force")
        if Path(engine).name == "podman":
            cleanup.append("--ignore")
        cleaned = bounded_run([*cleanup, name], timeout=5, limit=4096)
        if cleaned["exit_code"] and "no such container" not in cleaned["output"].lower():
            raise CheckError(f"probe cleanup failed; inspect only container {name}")
    if result["exit_code"]:
        raise CheckError("container KVM probe failed: " + result["output"][:1000])
    evidence = json.loads(result["output"])
    if evidence != {"kvm_api": 12, "create_vm": True}:
        raise CheckError("unexpected container probe output")
    return {"image": image, "container_kvm": evidence, "network": "none", "mounts": [],
            "macos_simulator_swiftui_proven": False}


def media(state: Path, name: str) -> Path:
    path = (state / name).resolve()
    if path.parent != state:
        raise CheckError("VM media must stay in the dedicated state directory")
    return path


def launch_plan(state: Path, engine: str, ssh_port: int, vnc_port: int) -> dict:
    disk, recovery = media(state, "macos.qcow2"), media(state, "BaseSystem.qcow2")
    blockers = [f"missing local media: {p}" for p in (disk, recovery) if not p.is_file()]
    for path in (disk, recovery):
        if path.is_file() and path.stat().st_size > 200 * GIB:
            blockers.append(f"media exceeds 200 GiB per-file ceiling: {path}")
    command = engine_command(engine, "run", "--name", NAME, "--pull", "never", "-d", "-i",
                             "--device", "/dev/kvm", "--cap-drop", "ALL",
                             "--security-opt", "no-new-privileges", "--memory", "14g",
                             "--cpus", "6", "--pids-limit", "512", "--shm-size", "256m",
                             "--user", f"{os.getuid()}:{os.getgid()}")
    if Path(engine).name == "podman":
        command += ["--userns", "keep-id", "--http-proxy=false"]
    command += ["--publish", f"127.0.0.1:{ssh_port}:10022",
                "--publish", f"127.0.0.1:{vnc_port}:5999",
                "--mount", f"type=bind,src={disk},dst=/image",
                "--mount", f"type=bind,src={recovery},dst=/home/arch/OSX-KVM/BaseSystem.img,readonly"]
    for variable in ["RAM=12", "SMP=6", "CORES=6", "KVM=accel=kvm", "AUDIO_DRIVER=none",
                     "IMAGE_PATH=/image", "IMAGE_FORMAT=qcow2", "BASESYSTEM_FORMAT=qcow2",
                     "DISPLAY=:99", "EXTRA=-display none -vnc 0.0.0.0:99",
                     "SSH_AUTH_SOCK=", "HTTP_PROXY=", "HTTPS_PROXY=", "ALL_PROXY=",
                     "http_proxy=", "https_proxy=", "all_proxy="]:
        command += ["--env", variable]
    # Bypass upstream's automatic downloads, sudo/chown and serial generation.
    # Launch.sh itself still needs inspection from the pinned image before use.
    command += ["--entrypoint", "/bin/bash", IMAGE, "-c", "exec ./Launch.sh"]
    return {"executed": False, "review_required": True, "blockers": blockers,
            "command": command, "shell_command": shlex.join(command), "profile": PROFILE,
            "media_digests_verified": False, "network": "private container network with outbound access; host publishes loopback only",
            "next": "inspect the pinned image Launch.sh; record and verify Apple media hashes before execution"}


CAPABILITY_SETS = {"bounding", "effective", "inheritable", "permitted", "ambient"}


def capability_profile(config: dict, oci: dict | None) -> dict:
    """Normalize inspect intent against compiled OCI configuration, not a fixed
    runtime-default CapDrop list. A stopped OCI bundle is never live proof.
    """
    drops = config.get("CapDrop") or []
    added = config.get("CapAdd") or []
    options = config.get("SecurityOpt") or []
    valid_drop = isinstance(drops, list) and all(isinstance(cap, str) for cap in drops)
    normalized = sorted({cap.upper() if cap.upper() == "ALL" or cap.upper().startswith("CAP_")
                         else "CAP_" + cap.upper() for cap in drops}) if valid_drop else []
    nnp_true = {"no-new-privileges", "no-new-privileges:true"}
    requested_nnp = (isinstance(options, list) and all(isinstance(option, str) for option in options)
        and any(option in nnp_true for option in options)
        and not any(option.startswith("no-new-privileges") and option not in nnp_true for option in options))
    process = oci.get("process") if isinstance(oci, dict) else None
    capabilities = process.get("capabilities") if isinstance(process, dict) else None
    compiled_empty = (isinstance(capabilities, dict) and not (set(capabilities)-CAPABILITY_SETS)
        and all(isinstance(values, list) and not values for values in capabilities.values()))
    compiled_nnp = isinstance(process, dict) and process.get("noNewPrivileges") is True
    problems = []
    if not isinstance(added, list) or added:
        problems.append("container has added capabilities")
    if not valid_drop or not ("ALL" in normalized or compiled_empty):
        problems.append("all-capability drop is not established by inspect or empty compiled OCI sets")
    if oci is not None and not compiled_empty:
        problems.append("compiled OCI capabilities are missing, malformed or nonempty")
    if not requested_nnp or (oci is not None and not compiled_nnp):
        problems.append("no-new-privileges is not explicitly configured")
    return {"inspect_cap_drop": normalized, "oci_observed": oci is not None,
        "oci_all_sets_empty": compiled_empty, "oci_no_new_privileges": compiled_nnp,
        "inspect_no_new_privileges": requested_nnp, "configuration_verified": not problems,
        "live_effective_verified": False, "problems": problems}


def oci_configuration(info: dict, engine: str) -> tuple[dict | None, str]:
    """Read only this local Podman bundle's bounded config under runtime storage.
    Do not follow an arbitrary inspect-supplied path into the user's filesystem.
    """
    if Path(engine).name != "podman" or not info.get("OCIConfigPath"):
        return None, "compiled OCI path is unavailable for this runtime"
    identity = info.get("Id", "")
    if not re.fullmatch(r"[a-f0-9]{64}", identity):
        return None, "invalid container identity for OCI path"
    try:
        storage = Path(engine_info(engine)["storage"]).resolve()
        expected = storage / "overlay-containers" / identity / "userdata" / "config.json"
        supplied = Path(info["OCIConfigPath"])
        if supplied != expected or supplied.resolve() != expected:
            return None, "OCI path is outside this container's supported storage layout"
        fd = os.open(supplied, os.O_RDONLY | os.O_NOFOLLOW | os.O_CLOEXEC)
        with os.fdopen(fd, "rb") as file:
            metadata = os.fstat(file.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_uid != os.getuid() or metadata.st_size > 1024*1024:
                return None, "OCI config ownership/type/size differs from the bounded rootless profile"
            data = file.read(1024*1024+1)
        if len(data) > 1024*1024:
            return None, "OCI config exceeded 1MiB"
        parsed = json.loads(data)
        if not isinstance(parsed, dict):
            return None, "OCI config is not an object"
        return parsed, "local retained OCI config sha256:" + hashlib.sha256(data).hexdigest()
    except (OSError, ValueError, CheckError) as error:
        return None, "compiled OCI inspection unavailable: " + str(error)


def process_capabilities(pid: int) -> dict:
    """Observe only the running container init's kernel fields; no exec/start.
    PID stability is checked both here and by a second runtime inspection.
    """
    if not isinstance(pid, int) or not 0 < pid < 2**31:
        return {"verified": False, "detail": "running container has no valid host PID"}
    try:
        folder = Path("/proc") / str(pid)
        before = (folder / "stat").read_text().rsplit(")", 1)[1].split()[19]
        fields = {}
        for line in (folder / "status").read_text().splitlines():
            key, _, value = line.partition(":")
            if key in {"CapEff", "CapBnd", "CapPrm", "CapInh", "CapAmb", "NoNewPrivs"}:
                if key in fields:
                    raise ValueError("duplicate kernel capability field")
                fields[key] = value.strip()
        after = (folder / "stat").read_text().rsplit(")", 1)[1].split()[19]
        masks = {name: int(fields[name], 16) for name in ["CapEff", "CapBnd", "CapPrm", "CapInh", "CapAmb"]}
        verified = before == after and fields["NoNewPrivs"] == "1" and not any(masks.values())
        return {"verified": verified, "pid": pid, "start_ticks": before,
            "capability_masks": masks, "no_new_privileges": fields["NoNewPrivs"] == "1"}
    except (OSError, ValueError, KeyError, IndexError) as error:
        return {"verified": False, "pid": pid, "detail": "kernel fields unavailable: " + str(error)}


def status(state: Path, engine: str) -> dict:
    result = bounded_run(engine_command(engine, "container", "inspect", NAME))
    if result["exit_code"]:
        return {"container": NAME, "state": "absent_or_inaccessible", "guest_proven": False,
                "detail": result["output"][:1000]}
    info = json.loads(result["output"])[0]
    bindings = info.get("HostConfig", {}).get("PortBindings") or {}
    problems = []
    for ports in bindings.values():
        for port in ports or []:
            if port.get("HostIp") not in {"127.0.0.1", "::1"}:
                problems.append("container publishes a non-loopback port")
    allowed = {str(media(state, "macos.qcow2")), str(media(state, "BaseSystem.qcow2"))}
    mounts = info.get("Mounts", [])
    for mount in mounts:
        if mount.get("Type") == "bind" and str(Path(mount.get("Source", "/")).resolve()) not in allowed:
            problems.append("container has an unexpected host bind mount")
    devices = info.get("HostConfig", {}).get("Devices") or []
    if any(device.get("PathOnHost") != "/dev/kvm" for device in devices):
        problems.append("container has an unexpected host device")
    if info.get("HostConfig", {}).get("Privileged") or info.get("HostConfig", {}).get("NetworkMode") == "host":
        problems.append("privileged/host-network containers are outside this profile")
    config = info.get("HostConfig", {})
    oci, oci_detail = oci_configuration(info, engine)
    capabilities = capability_profile(config, oci)
    problems.extend(capabilities.pop("problems"))
    capabilities["oci_detail"] = oci_detail
    state_info = info.get("State", {})
    live = {"verified": False, "detail": "container is stopped; effective kernel capabilities were not observed"}
    if state_info.get("Running") is True:
        live = process_capabilities(state_info.get("Pid"))
        second = bounded_run(engine_command(engine, "container", "inspect", NAME))
        stable = False
        if second["exit_code"] == 0:
            again = json.loads(second["output"])[0]
            stable = (bool(re.fullmatch(r"[a-f0-9]{64}", info.get("Id", "")))
                and bool(state_info.get("StartedAt")) and again.get("Id") == info.get("Id") and again.get("State", {}).get("Running") is True
                and all(again.get("State", {}).get(key) == state_info.get(key) for key in ["Pid", "StartedAt"]))
        live["runtime_identity_stable"] = stable
        capabilities["live_effective_verified"] = bool(live["verified"] and stable)
        if not capabilities["live_effective_verified"]:
            problems.append("running container's dropped capabilities/no-new-privileges could not be verified")
    return {"container": NAME, "state": info.get("State", {}).get("Status"),
            "image_id": info.get("Image"), "port_bindings": bindings,
            "capability_profile": capabilities, "live_init_process": live,
            "boundary_problems": problems, "guest_proven": False}


def private_file(state: Path, name: str) -> Path:
    path = media(state, name)
    if not path.is_file():
        raise CheckError(f"missing dedicated guest SSH file: {path}")
    mode = path.stat()
    if mode.st_uid != os.getuid() or mode.st_mode & 0o077:
        raise CheckError(f"guest SSH file must be owned by this user and mode 0600: {path}")
    return path


def ssh_command(state: Path, user: str, port: int, command: list[str]) -> list[str]:
    if not re.fullmatch(r"[a-zA-Z_][a-zA-Z0-9_-]{0,31}", user):
        raise CheckError("invalid dedicated guest account name")
    key, known = private_file(state, "guest_ed25519"), private_file(state, "known_hosts")
    return ["ssh", "-F", "/dev/null", "-o", "BatchMode=yes", "-o", "IdentitiesOnly=yes",
            "-o", "IdentityAgent=none", "-o", "PasswordAuthentication=no",
            "-o", "KbdInteractiveAuthentication=no", "-o", "StrictHostKeyChecking=yes",
            "-o", "ForwardAgent=no", "-o", "ClearAllForwardings=yes", "-o", "ConnectTimeout=5",
            "-o", f"UserKnownHostsFile={known}", "-o", "GlobalKnownHostsFile=/dev/null",
            "-i", str(key), "-p", str(port), f"{user}@127.0.0.1", shlex.join(command)]


def probe(state: Path, user: str, port: int) -> dict:
    checks = {}
    commands = {
        "architecture": ["/usr/bin/uname", "-m"],
        "macos": ["/usr/bin/sw_vers", "-productVersion"],
        "xcode": ["/usr/bin/xcodebuild", "-version"],
        "ios_sdk": ["/usr/bin/xcrun", "--sdk", "iphonesimulator", "--show-sdk-version"],
        "runtimes": ["/usr/bin/xcrun", "simctl", "list", "runtimes", "--json"],
        "devices": ["/usr/bin/xcrun", "simctl", "list", "devices", "--json"],
        "graphics": ["/usr/sbin/system_profiler", "SPDisplaysDataType", "-json"],
    }
    for name, command in commands.items():
        checks[name] = bounded_run(ssh_command(state, user, port, command), timeout=30)
        if checks[name]["exit_code"] == 255:
            raise CheckError("guest SSH unavailable or host identity not verified: " + checks[name]["output"][:1000])
    runtimes = json.loads(checks["runtimes"]["output"]) if checks["runtimes"]["exit_code"] == 0 else {}
    if not isinstance(runtimes, dict) or not isinstance(runtimes.get("runtimes", []), list):
        raise CheckError("unexpected guest simulator-runtime metadata")
    if len(runtimes.get("runtimes", [])) > 4096 or any(
        not isinstance(item, dict) for item in runtimes.get("runtimes", [])
    ):
        raise CheckError("unexpected guest simulator-runtime entries")
    available = any(item.get("identifier") == "com.apple.CoreSimulator.SimRuntime.iOS-18-2"
                    and item.get("version") == PROFILE["simulator_runtime"]
                    and item.get("isAvailable") is True for item in runtimes.get("runtimes", []))
    return {"profile": PROFILE, "checks": checks, "macos_boot_observed":
            checks["architecture"]["exit_code"] == 0 and checks["architecture"]["output"].strip() == "x86_64"
            and checks["macos"]["exit_code"] == 0,
            "pinned_versions_match": available and all(checks[key]["exit_code"] == 0 for key in
            ["architecture", "macos", "xcode", "ios_sdk"])
            and checks["architecture"]["output"].strip() == "x86_64"
            and checks["macos"]["output"].strip() == PROFILE["macos"]
            and checks["xcode"]["output"].startswith("Xcode " + PROFILE["xcode"] + "\n")
            and checks["ios_sdk"]["output"].strip() == PROFILE["ios_sdk"],
            "pinned_simulator_runtime_available": available,
            "simulator_boot_render_and_xctest_proven": False,
            "next": "runtime/device listings and graphics metadata do not prove SwiftUI or XCTest; complete docs/ios-vm.md evidence gates"}


def port(value: str) -> int:
    parsed = int(value)
    if not 1024 <= parsed <= 65535:
        raise argparse.ArgumentTypeError("use an unprivileged port (1024–65535)")
    return parsed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--state", type=Path, default=Path(os.environ.get("NUSCHL_IOS_VM_DIR", Path.home() / "vms/nuschl-ios")))
    parser.add_argument("--runtime", help="local podman or docker executable")
    commands = parser.add_subparsers(dest="command", required=True)
    pre = commands.add_parser("preflight")
    pre.add_argument("--budget-gib", type=int, choices=range(128, 513), default=256, metavar="128..512")
    pre.add_argument("--reserve-gib", type=int, choices=range(32, 257), default=64, metavar="32..256")
    commands.add_parser("metadata")
    host = commands.add_parser("container-probe")
    host.add_argument("--image", required=True, help="already cached amd64 image with python3, full ID or digest")
    plan = commands.add_parser("plan")
    plan.add_argument("--ssh-port", type=port, default=50922)
    plan.add_argument("--vnc-port", type=port, default=5999)
    commands.add_parser("status")
    guest = commands.add_parser("probe")
    guest.add_argument("--user", default="nuschl-test")
    guest.add_argument("--ssh-port", type=port, default=50922)
    args = parser.parse_args()
    try:
        state = disk_path(args.state)
        if args.command == "preflight":
            result = preflight(state, args.runtime, args.budget_gib, args.reserve_gib)
        elif args.command == "metadata":
            result = metadata()
        elif args.command == "container-probe":
            result = container_probe(runtime(args.runtime), args.image)
        elif args.command == "plan":
            if args.ssh_port == args.vnc_port:
                raise CheckError("SSH and VNC must use different loopback ports")
            result = launch_plan(state, runtime(args.runtime), args.ssh_port, args.vnc_port)
        elif args.command == "status":
            result = status(state, runtime(args.runtime))
        else:
            result = probe(state, args.user, args.ssh_port)
        print(json.dumps(result, indent=2, sort_keys=True))
        return 2 if result.get("blockers") or result.get("boundary_problems") else 0
    except (CheckError, OSError, ValueError, KeyError, subprocess.TimeoutExpired) as error:
        print(json.dumps({"error": str(error), "feasibility_proven": False}, indent=2))
        return 2


if __name__ == "__main__":
    sys.exit(main())
