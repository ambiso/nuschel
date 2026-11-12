#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Owned stock/nuschld native ACL editor + fresh offline-account lookup proof."""

import argparse
import json
import subprocess
import time
from pathlib import Path

import document_link_regression as fixture
import welcome_html
from accessibility import items
from admin_bootstrap import Operator
from hash_document_wire import fields
from local_murmur import Murmur, eventually
from local_nuschld import Nuschld
from mumble_client import Client

ARGS = None
SERVER = None
IDS = {}


class OwnedMurmur(Murmur):
    def __init__(self, *args):
        global SERVER
        super().__init__(*args)
        SERVER = self


class OwnedNuschld(Nuschld):
    def __init__(self, _binary, root, port):
        global SERVER
        super().__init__(ARGS.binaries, root, port)
        self.ini = root / "welcome-bridge.ini"
        self.ini.write_text(
            'welcometext="Welcome to the <b>Nuschl</b> screenshot server."'
        )
        self.database = root / "unused-database-path"
        SERVER = self

    def seed(self):
        super().seed()
        self.write_config(message_limit=100, message_burst=100)
        welcome = self.ini.read_text().split('welcometext="', 1)[1].rsplit('"', 1)[0]
        config = self.config.read_text().splitlines()
        self.config.write_text(
            "\n".join(
                "welcome_text = " + json.dumps(welcome)
                if line.startswith("welcome_text =")
                else line
                for line in config
            )
            + "\n"
        )


def operator(root, pin):
    certificate, key = root / "operator.pem", root / "operator-key.pem"
    if not certificate.exists():
        subprocess.run(
            [
                "openssl",
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-config",
                "/dev/null",
                "-subj",
                "/CN=Owned ACL fixture",
                "-days",
                "1",
                "-keyout",
                str(key),
                "-out",
                str(certificate),
            ],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=30,
        )
    return Operator(SERVER.port, pin, SERVER.SUPERUSER_PASSWORD, certificate, key)


def configure(_shell, root, output):
    bookmark = json.loads((root / "profile/config/servers.json").read_text())[0]
    pin = bookmark["certificate_sha256"]
    reader = Client(
        ARGS.binaries, "127.0.0.1", SERVER.port, pin, "Welcome reader", root / "profile"
    )
    offline = None
    try:
        reader.send(kind="register")
        registered = reader.wait(
            lambda event: (
                event["kind"] == "user"
                and event["user"]["session"] == reader.session
                and event["user"]["user_id"] is not None
            )
        )
        IDS["reader"] = registered["user"]["user_id"]
        offline = Client(
            ARGS.binaries,
            "127.0.0.1",
            SERVER.port,
            pin,
            "Offline member",
            root / "offline",
        )
        offline.send(kind="register")
        registered = offline.wait(
            lambda event: (
                event["kind"] == "user"
                and event["user"]["session"] == offline.session
                and event["user"]["user_id"] is not None
            )
        )
        IDS["offline"] = registered["user"]["user_id"]
        offline.close()
        offline = None
        admin = operator(root, pin)
        try:
            admin.promote(IDS["reader"])
        finally:
            admin.close()
        reader.send(kind="acl_draft", channel=0, request=100)
        reader.wait(
            lambda event: (
                event["kind"] == "acl_reply"
                and event["request"] == 100
                and "acl" in event
            )
        )
        reader.send(
            kind="lookup_account", channel=0, request=101, name="Offline member"
        )
        resolved = reader.wait(
            lambda event: event["kind"] == "account_reply" and event["request"] == 101
        )
        assert resolved["account"]["id"] == IDS["offline"], resolved
        reader.send(
            kind="lookup_account",
            channel=0,
            request=102,
            name="No such private fixture account",
        )
        missing = reader.wait(
            lambda event: event["kind"] == "account_reply" and event["request"] == 102
        )
        assert "error" in missing, missing
        (output / "fresh-wire-lookup.json").write_text(
            json.dumps({"offline": resolved, "unknown": missing}, indent=2) + "\n"
        )
    finally:
        if offline:
            offline.close()
        reader.close()
    return {"RUST_LOG": "nuschl=info,nuschl::layout=debug"}


def actions(args, root, shell, application, bot, murmur):
    shell.key("Shift_L")
    shell.place("Nuschl", 40, 40, 1100, 800)
    pin = bot.process.args[bot.process.args.index("--certificate-sha256") + 1]

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
        return eventually(lambda: named(name, role), f"missing ACL UI {name}")

    def point(node):
        origin = json.loads(
            shell.eval(
                "JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return {x:b.x,y:b.y};})())"
            )
        )
        x, y, width, height = node.extents()
        return origin["x"] + x + width // 2, origin["y"] + y + height // 2

    def activate(name):
        node = wait(name, "push button")
        assert node.call("Component", "GrabFocus")[0]
        time.sleep(0.15)
        shell.click(*point(node))
        return node

    def type_field(label, value):
        node = eventually(
            lambda: next(
                (
                    node
                    for node, role, name, _ in items(application)
                    if name == label and role in ("entry", "text")
                ),
                None,
            ),
            f"missing input {label}",
        )
        assert node.call("Component", "GrabFocus")[0]
        time.sleep(0.15)
        shell.click(*point(node))
        shell.chord("Control_L", "a")
        shell.type(value)
        time.sleep(0.2)

    def text_contains(text):
        return any(text in label for _, _, label, _ in items(application))

    def open_acl(channel="Root"):
        row = eventually(
            lambda: next(
                (
                    node
                    for node, role, name, _ in items(application)
                    if role == "tree item" and name.startswith(channel)
                ),
                None,
            ),
            f"missing channel {channel}",
        )
        assert row.call("Component", "GrabFocus")[0]
        time.sleep(0.2)
        shell.click(*point(row), button=3)
        wait("Edit permissions…", "menu item").press()
        eventually(
            lambda: text_contains("Edit a local rule or group"),
            "ACL draft did not load",
        )

    def save():
        activate("Save permissions")
        eventually(
            lambda: text_contains("Permissions saved and confirmed"),
            "ACL save was not verified",
        )

    open_acl()
    stale_save = wait("Save permissions", "push button")
    activate("Done")
    open_acl()
    try:
        accepted = stale_save.call("Action", "DoAction", "i", "0")[0]
    except RuntimeError:
        accepted = False
    assert accepted is False, "closed/reopened editor reused an old Save action"
    type_field("New group name", "native-team")
    activate("Create group")
    type_field("Registered group member", "Offline member")
    activate("Resolve member")
    eventually(
        lambda: text_contains(f"Resolved Offline member (#{IDS['offline']})"),
        "fresh member lookup was not pinned",
    )
    activate("Add member")
    save()
    admin = operator(root, pin)
    try:
        acl = admin.acl()
        group = next(
            fields(raw) for raw in acl[3] if fields(raw)[1] == [b"native-team"]
        )
        assert group.get(5) == [IDS["offline"]], group
        assert group.get(3, [1]) == [1] and group.get(4, [1]) == [1], group
    finally:
        admin.close()
    shell.screenshot(args.output / "native-root-group.png")
    return {
        "fresh_offline_lookup": True,
        "unknown_account_refused": True,
        "close_reopen_stale_save_refused": True,
        "native_group_saved": True,
        "registered_ids": IDS.copy(),
        "server": args.server,
    }


def checked_actions(*args):
    try:
        return actions(*args)
    except BaseException:
        options, _root, shell, application, _bot, _server = args
        try:
            shell.screenshot(options.output / "failed-editor.png")
            (options.output / "failed-accessible.json").write_text(
                json.dumps(
                    [
                        (role, name, node.extents())
                        for node, role, name, _ in items(application)
                    ],
                    indent=2,
                )
                + "\n"
            )
        except (RuntimeError, subprocess.SubprocessError, OSError) as error:
            print(f"Could not capture failing editor: {error}")
        raise


def main():
    global ARGS
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--server", choices=("stock", "nuschld"), default="stock")
    ARGS = parser.parse_args()
    ARGS.document_links, ARGS.expect_markers = True, False
    fixture.WELCOME_EXTRA = ""
    fixture.seed_description = lambda _database: None
    fixture.configure_handler, fixture.run_actions = configure, checked_actions
    welcome_html.Murmur = OwnedMurmur if ARGS.server == "stock" else OwnedNuschld
    welcome_html.run(ARGS)


if __name__ == "__main__":
    main()
