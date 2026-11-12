#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Native SuperUser promotion on an owned server that starts/stays E2EE-only."""

import argparse
import json
import os
import signal
import subprocess
import tempfile
import time
import uuid
from pathlib import Path

from accessibility import items, start_registry
from gnome_session import GnomeSession
from local_murmur import eventually, free_port
from local_nuschld import Nuschld
from mumble_client import Client, certificate_pin


def stop(process):
    if process and process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


def run(args):
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschl-e2ee-admin-") as temporary:
        root = Path(temporary)
        server = Nuschld(args.binaries, root, free_port())
        server.security = "e2ee"
        server.write_config(1, 5)
        original_config = server.config.read_text()
        server.start()
        shell = GnomeSession(
            args.gnome_shell, 1440, 900, log=(args.output / "gnome.log").open("w")
        )
        helpers = []
        app = alice = None
        application = None
        try:
            pin = certificate_pin(
                args.binaries, "127.0.0.1", server.port, root / "probe"
            )
            alice = Client(
                args.binaries,
                "127.0.0.1",
                server.port,
                pin,
                "Offline Alice",
                root / "alice",
                ["--require-nuschl"],
            )
            alice.send(kind="register")
            registered = alice.wait(
                lambda event: (
                    event["kind"] == "user"
                    and event["user"]["session"] == alice.session
                    and event["user"]["user_id"] is not None
                )
            )
            account = registered["user"]["user_id"]
            alice.send(kind="permissions", channel=0)
            before = alice.wait(
                lambda event: event["kind"] == "permissions" and event["channel"] == 0
            )
            assert before["permissions"] & 1 == 0, before
            alice.close()
            alice = None
            shell.start()

            def spawn(command, env, log):
                process = subprocess.Popen(
                    command,
                    env=env,
                    stdout=(args.output / log).open("w"),
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
                helpers.append(process)
                return process

            registry = start_registry(args.at_spi, shell.app_environment(), spawn)
            pulse = shell.runtime / "pulse"
            pulse.mkdir(mode=0o700, exist_ok=True)
            assert not (pulse / "native").exists()
            script = pulse / "default.pa"
            script.write_text(
                f"load-module module-native-protocol-unix socket={pulse / 'native'} auth-anonymous=1\nload-module module-null-sink sink_name=speakers rate=48000\nload-module module-null-sink sink_name=microphone rate=48000\nset-default-sink speakers\nset-default-source microphone.monitor\n"
            )
            spawn(
                [
                    str(args.pulse_bin / "pulseaudio"),
                    "--daemonize=no",
                    "--use-pid-file=no",
                    "--exit-idle-time=-1",
                    "-n",
                    f"--file={script}",
                ],
                shell.environment()
                | {
                    "PULSE_RUNTIME_PATH": str(pulse),
                    "PULSE_STATE_PATH": str(pulse / "state"),
                    "DBUS_SESSION_BUS_ADDRESS": f"unix:path={root}/no-pulse-bus",
                },
                "pulse.log",
            )
            eventually(
                lambda: (pulse / "native").exists(), "private null audio missing"
            )
            profile = root / "native"
            (profile / "config").mkdir(parents=True)
            (profile / "config/servers.json").write_text(
                json.dumps(
                    [
                        {
                            "id": str(uuid.uuid4()),
                            "name": "E2EE admin bootstrap",
                            "host": "127.0.0.1",
                            "port": server.port,
                            "username": "SuperUser",
                            "certificate_sha256": pin,
                        }
                    ]
                )
            )
            (profile / "config/settings.json").write_text(
                json.dumps(
                    {
                        "appearance": {"theme": "dark"},
                        "audio": {"transmit": "push_to_talk"},
                    }
                )
            )
            env = shell.app_environment() | {
                "RUST_LOG": "nuschl=info,nuschl::acl_editor=debug",
                "PULSE_SERVER": f"unix:{pulse}/native",
                "NUSCHL_AUDIO_BACKEND": "pulseaudio",
                "PULSE_SINK": "speakers",
                "PULSE_SOURCE": "microphone.monitor",
                "PIPEWIRE_REMOTE": str(root / "no-pipewire"),
            }
            env.pop("NO_AT_BRIDGE", None)
            app = subprocess.Popen(
                [
                    str(args.binaries / "nuschl"),
                    "--profile-dir",
                    str(profile),
                    f"mumble://SuperUser@127.0.0.1:{server.port}/?policy=nuschl",
                ],
                env=env,
                stdout=(args.output / "desktop.log").open("w"),
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            eventually(lambda: shell.window("Nuschl"), "native window missing")
            shell.place("Nuschl", 40, 40, 1100, 800)
            application = eventually(
                lambda: next(
                    (
                        node
                        for node in registry.children()
                        if node.name.lower() == "nuschl"
                    ),
                    None,
                ),
                "native accessibility application missing",
            )

            def named(name, role=None):
                return next(
                    (
                        node
                        for node, kind, label, _ in items(application)
                        if label == name and (role is None or kind == role)
                    ),
                    None,
                )

            def wait(name, role=None):
                return eventually(
                    lambda: named(name, role), f"missing native admin control {name}"
                )

            def text_contains(text):
                return any(text in name for _, _, name, _ in items(application))

            def type_field(name, text):
                field = wait(name)
                assert field.call("Component", "GrabFocus")[0]
                time.sleep(0.1)
                shell.chord("Control_L", "a")
                shell.type(text)

            # Login refusal opens the existing retry form; no password in argv,
            # stored bookmark or screenshot. The native UI submits the secret.
            type_field("Server password", server.SUPERUSER_PASSWORD)
            wait("Save and connect", "push button").press()
            row = eventually(
                lambda: next(
                    (
                        node
                        for node, role, name, _ in items(application)
                        if role == "tree item" and name.startswith("Root")
                    ),
                    None,
                ),
                "E2EE root channel missing after native login",
            )
            assert row.call("Component", "GrabFocus")[0]
            time.sleep(0.2)
            origin = json.loads(
                shell.eval(
                    "JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return {x:b.x,y:b.y};})())"
                )
            )
            x, y, width, height = row.extents()
            shell.click(
                origin["x"] + x + width // 2, origin["y"] + y + height // 2, button=3
            )
            wait("Edit permissions…", "menu item").press()
            eventually(
                lambda: text_contains("Edit a local rule or group"),
                "E2EE native ACL query did not load",
            )
            wait("@admin", "push button").press()
            type_field("Registered group member", "Offline Alice")
            wait("Resolve member", "push button").press()
            eventually(
                lambda: text_contains(f"Resolved Offline Alice (#{account})"),
                "E2EE native offline lookup did not resolve",
            )
            wait("Add member", "push button").press()
            wait("Save permissions", "push button").press()
            eventually(
                lambda: text_contains("Permissions saved and confirmed"),
                "E2EE native promotion was not confirmed",
            )
            shell.screenshot(args.output / "native-e2ee-admin-promoted.png")
            stop(app)
            app = None

            def verify():
                user = Client(
                    args.binaries,
                    "127.0.0.1",
                    server.port,
                    pin,
                    "Offline Alice",
                    root / "alice",
                    ["--require-nuschl"],
                )
                try:
                    user.send(kind="permissions", channel=0)
                    granted = user.wait(
                        lambda event: (
                            event["kind"] == "permissions" and event["channel"] == 0
                        )
                    )
                    assert granted["permissions"] & 1, granted
                    user.send(kind="acl_draft", channel=0, request=77)
                    reply = user.wait(
                        lambda event: (
                            event["kind"] == "acl_reply" and event["request"] == 77
                        )
                    )
                    group = next(
                        group
                        for group in reply["acl"]["groups"]
                        if group["name"] == "admin"
                    )
                    assert account in group["add"], group
                    return granted, group
                finally:
                    user.close()

            granted, group = verify()
            assert server.config.read_text() == original_config, (
                "security/config changed during bootstrap"
            )
            server.stop()
            server.start()
            persisted, persisted_group = verify()
            proof = {
                "passed": True,
                "security_from_first_start": "e2ee",
                "security_unchanged": True,
                "raw_legacy_operator_used": False,
                "native_password_login": True,
                "native_admin_group_editor": True,
                "fresh_offline_name_resolution": True,
                "registered_account": account,
                "before": before,
                "after": granted,
                "admin_group": group,
                "after_restart": persisted,
                "persisted_admin_group": persisted_group,
                "public_servers_or_host_devices": False,
            }
            (args.output / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
            print(json.dumps(proof))
        except BaseException:
            if application:
                (args.output / "failed-accessible.json").write_text(
                    json.dumps(
                        [(role, name) for _, role, name, _ in items(application)],
                        indent=2,
                    )
                )
                shell.screenshot(args.output / "failed-native.png")
            raise
        finally:
            stop(app)
            for process in helpers:
                stop(process)
            if alice:
                alice.close()
            shell.stop()
            server.stop()
            (args.output / "server.log").write_text(server.log.read_text())


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    run(parser.parse_args())
