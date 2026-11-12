#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""NixOS-VM-only inspection of the owned bot service's actual codec children.

No arbitrary process target or secret reads: inspect the named fixture service,
its child role metadata and absence of the public fixture credential path.
"""
import json
import sys
from pathlib import Path
import subprocess


def main():
    parent = int(subprocess.check_output(
        ["systemctl", "show", "nuschl-musicbot", "--property=MainPID", "--value"], text=True))
    assert parent > 1
    status = (Path("/proc") / str(parent) / "status").read_text().splitlines()
    fields = {line.split(":", 1)[0]: line.split(":", 1)[1].strip() for line in status if ":" in line}
    uid = int(fields["Uid"].split()[1])
    gid = int(fields["Gid"].split()[1])
    assert uid != 0 and int(fields["CapEff"], 16) == 0
    # Enter only the owned service's namespaces, then drop to its actual UID
    # with no capabilities. os.access is non-mutating; no sysctl/sysrq writes.
    check = r"""
import json,os,pathlib
paths=['/proc/sys/kernel/hostname','/proc/sys/kernel/domainname',
       '/proc/sys/vm/overcommit_memory','/proc/sysrq-trigger']
assert all(pathlib.Path(path).exists() for path in paths)
assert not any(os.access(path,os.W_OK) for path in paths)
assert os.access('/var/lib/nuschl-musicbot',os.W_OK)
status=pathlib.Path('/proc/self/status').read_text().splitlines()
assert int(next(line.split()[1] for line in status if line.startswith('CapEff:')),16)==0
print(json.dumps({'uid':os.geteuid(),'host_controls_unwritable':True,'effective_capabilities':0,'own_state_writable':True}))
"""
    parent_checks = json.loads(subprocess.check_output([
        "nsenter", "--target", str(parent), "--mount", "--uts", "--ipc", "--net", "--",
        "setpriv", "--reuid", str(uid), "--regid", str(gid), "--clear-groups",
        "--bounding-set=-all", "--inh-caps=-all", "--ambient-caps=-all", "--no-new-privs",
        sys.executable, "-c", check], text=True))
    assert parent_checks["uid"] == uid
    parent_network = (Path("/proc") / str(parent) / "ns/net").readlink()
    parent_pid_namespace = (Path("/proc") / str(parent) / "ns/pid").readlink()
    children = []
    for process in Path("/proc").iterdir():
        if not process.name.isdecimal():
            continue
        try:
            command = (process / "cmdline").read_bytes().split(b"\0")
            if command[:2] != [b"/worker", b"--codec-worker"]:
                continue
            assert command[2] in [b"decode", b"encode", b"duck"] and command[3:] == [b""]
            cgroup = (process / "cgroup").read_text()
            assert "nuschl-musicbot.service" in cgroup
            assert (process / "ns/net").readlink() != parent_network
            assert (process / "ns/pid").readlink() != parent_pid_namespace
            assert not (process / "root/etc/musicbot/key").exists()
            assert not (process / "root/var/lib/private/nuschl-musicbot").exists()
            assert not (process / "root/run/credentials").exists()
            environment = (process / "environ").read_bytes()
            assert not environment, "child inherited parent environment"
            assert {entry.name for entry in (process / "fd").iterdir()} <= {"0", "1", "2"}
            limits = (process / "limits").read_text()
            assert "1073741824" in next(line for line in limits.splitlines() if line.startswith("Max address space"))
            children.append({"pid": int(process.name), "role": command[2].decode(),
                             "network_namespace_separate": True, "pid_namespace_separate": True, "credentials_absent": True,
                             "stdio_only": True, "address_space_bytes": 1073741824})
        except FileNotFoundError:
            # Exited between enumeration and inspection; not a positive proof.
            continue
    assert any(child["role"] == "encode" for child in children), children
    print(json.dumps({"parent": parent, "parent_checks": parent_checks, "children": children}))


if __name__ == "__main__":
    main()
