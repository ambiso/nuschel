#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""NixOS-VM-only inspection of the owned bot service's actual codec children.

No arbitrary process target or secret reads: inspect the named fixture service,
its child role metadata and absence of the public fixture credential path.
"""
import json
from pathlib import Path
import subprocess


def main():
    parent = int(subprocess.check_output(
        ["systemctl", "show", "nuschl-musicbot", "--property=MainPID", "--value"], text=True))
    assert parent > 1
    parent_network = (Path("/proc") / str(parent) / "ns/net").readlink()
    children = []
    for process in Path("/proc").iterdir():
        if not process.name.isdecimal():
            continue
        try:
            command = (process / "cmdline").read_bytes().split(b"\0")
            if command[:2] != [b"/worker", b"--codec-worker"]:
                continue
            cgroup = (process / "cgroup").read_text()
            assert "nuschl-musicbot.service" in cgroup
            assert (process / "ns/net").readlink() != parent_network
            assert not (process / "root/etc/musicbot/key").exists()
            assert not (process / "root/var/lib/private/nuschl-musicbot").exists()
            assert not (process / "root/run/credentials").exists()
            environment = (process / "environ").read_bytes()
            assert not environment, "child inherited parent environment"
            assert {entry.name for entry in (process / "fd").iterdir()} <= {"0", "1", "2"}
            limits = (process / "limits").read_text()
            assert "1073741824" in next(line for line in limits.splitlines() if line.startswith("Max address space"))
            children.append({"pid": int(process.name), "role": command[2].decode(),
                             "network_namespace_separate": True, "credentials_absent": True,
                             "stdio_only": True, "address_space_bytes": 1073741824})
        except FileNotFoundError:
            # Exited between enumeration and inspection; not a positive proof.
            continue
    assert any(child["role"] == "encode" for child in children), children
    print(json.dumps({"parent": parent, "children": children}))


if __name__ == "__main__":
    main()
