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
from admin_bootstrap import Operator, encode, scalar
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
        if ARGS.high_admin_limit:
            self.ini.write_text(
                self.ini.read_text() + "messagelimit=100\nmessageburst=100\n"
            )
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
    return {"RUST_LOG": "nuschl=info,nuschl::layout=debug,nuschl::acl_editor=debug"}


def actions(args, root, shell, application, bot, murmur):
    shell.key("Shift_L")
    shell.place("Nuschl", 40, 40, 1100, 800)
    pin = bot.process.args[bot.process.args.index("--certificate-sha256") + 1]
    cases = []

    def mark(name):
        cases.append(name)
        (args.output / "cases.json").write_text(json.dumps(cases, indent=2) + "\n")

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

    def matching(fragment):
        return eventually(
            lambda: next(
                (
                    node
                    for node, role, name, _ in items(application)
                    if role == "push button" and fragment in name
                ),
                None,
            ),
            f"missing action containing {fragment}",
        )

    def click_matching(fragment):
        node = matching(fragment)
        assert node.call("Component", "GrabFocus")[0]
        time.sleep(0.15)
        shell.click(*point(node))

    def query_channel(admin, channel):
        admin.send(13, scalar(1, channel) + scalar(5, 1))
        return admin.wait(13)

    def local_rules(acl):
        return [fields(raw) for raw in acl.get(4, []) if not fields(raw).get(3, [1])[0]]

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
    mark("close_reopen_stale_save_refused")
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
    mark("native_root_group_and_offline_member_saved")
    shell.screenshot(args.output / "native-root-group.png")
    activate("Done")
    open_acl("Lobby")
    activate("@native-team · inherited")
    type_field("Registered group member", f"#{IDS['offline']}")
    activate("Exclude inherited member")
    click_matching("Inheritable by subchannels")
    activate("Add rule")
    type_field("ACL rule target", "native-team")
    activate("Apply target")
    activate("Speak: unset")
    click_matching("Apply to subchannels")
    activate("Add rule")
    activate("Registered user")
    type_field("ACL rule target", "Offline member")
    activate("Resolve account")
    eventually(
        lambda: text_contains(f"Resolved Offline member (#{IDS['offline']})"),
        "fresh rule account did not resolve",
    )
    activate("Apply target")
    activate("Speak: unset")
    activate("Speak: allow")
    activate("Move rule up")
    save()
    shell.screenshot(args.output / "native-child-rules.png")
    admin = operator(root, pin)
    try:
        child = query_channel(admin, 1)
        group = next(
            fields(raw) for raw in child[3] if fields(raw)[1] == [b"native-team"]
        )
        assert (
            group.get(2) == [1]
            and group.get(3, [1]) == [1]
            and group.get(4, [1]) == [0]
        ), group
        assert group.get(6) == [IDS["offline"]], group
        rules = local_rules(child)
        assert len(rules) == 2, rules
        assert rules[0].get(4) == [IDS["offline"]] and rules[0].get(7) == [8], rules
        assert rules[0].get(1, [1]) == [1] and rules[0].get(2, [1]) == [1], rules
        assert rules[1].get(5) == [b"native-team"] and rules[1].get(6) == [8], rules
        assert rules[1].get(1, [1]) == [1] and rules[1].get(2, [1]) == [0], rules
    finally:
        admin.close()
    mark("inherited_group_override_and_ordered_rules_saved")
    observer = Client(
        args.binaries, "127.0.0.1", SERVER.port, pin, "Offline member", root / "offline"
    )
    try:
        for channel in (1, 2):
            observer.send(kind="permissions", channel=channel)
            permission = observer.wait(
                lambda event, channel=channel: (
                    event["kind"] == "permissions" and event["channel"] == channel
                )
            )
            assert permission["permissions"] & 8 == 0, permission
        bot.send(kind="permissions", channel=1)
        control = bot.wait(
            lambda event: event["kind"] == "permissions" and event["channel"] == 1
        )
        assert control["permissions"] & 8 != 0, control
    finally:
        observer.close()
    mark("deny_and_subchannel_inheritance_with_unrelated_control")
    # A second operator changes the ACL while the native draft is dirty. Root
    # Write must still allow ACL recovery even when the child's Write is denied.
    activate("Add rule")
    admin = operator(root, pin)
    try:
        current = query_channel(admin, 1)
        current[4] = [
            raw for raw in current.get(4, []) if not fields(raw).get(3, [1])[0]
        ]
        current[4].append(
            encode({1: [1], 2: [1], 3: [0], 4: [IDS["reader"]], 6: [0], 7: [1]})
        )
        local_groups = []
        for raw in current.get(3, []):
            group = fields(raw)
            if (
                group.get(2, [1]) == [1]
                and group.get(3, [1]) == [1]
                and group.get(4, [1]) == [1]
                and not group.get(5)
                and not group.get(6)
            ):
                continue
            group.pop(7, None)
            local_groups.append(encode(group))
        current[3] = local_groups
        current[5] = [0]
        admin.send(13, encode(current))
        assert len(local_rules(query_channel(admin, 1))) == 3
    finally:
        admin.close()
    activate("Save permissions")
    eventually(
        lambda: text_contains("Permissions changed on the server"),
        "dirty ACL conflict was silently overwritten",
    )
    assert text_contains("Unsaved changes")
    activate("Reload and discard draft")
    eventually(
        lambda: (
            text_contains("Edit a local rule or group")
            and not text_contains("Unsaved changes")
        ),
        "explicit ACL reload did not discard the draft",
    )
    activate("Done")
    # The menu may not offer ordinary channel editing, but root administrators
    # retain the separate Edit permissions action and can remove the denial.
    open_acl("Lobby")
    activate(f"Welcome reader (#{IDS['reader']})")
    activate("Remove rule")
    save()
    admin = operator(root, pin)
    try:
        assert local_rules(query_channel(admin, 1)) == rules
    finally:
        admin.close()
    mark("dirty_conflict_retained_and_root_admin_recovers_denied_child_acl")
    activate("Done")
    open_acl()
    type_field("New group name", "unsaved-draft")
    activate("Create group")
    pending_save = wait("Save permissions", "push button")
    bot.close()
    SERVER.stop()
    eventually(
        lambda: text_contains("Unsaved draft retained"),
        "disconnect silently discarded the draft",
    )
    SERVER.start()
    eventually(
        lambda: any(
            role == "tree item" and name.startswith("Welcome reader")
            for _, role, name, _ in items(application)
        ),
        "native client did not reconnect",
    )
    eventually(
        lambda: named("@unsaved-draft", "push button"),
        "stale inspector lost its unsaved group",
    )
    try:
        accepted = pending_save.call("Action", "DoAction", "i", "0")[0]
    except RuntimeError:
        accepted = False
    assert accepted is False, "old Save became active on replacement connection"
    mark("dirty_draft_retained_and_stale_reconnect_save_refused")
    shell.screenshot(args.output / "native-stale-draft.png")
    admin = operator(root, pin)
    try:
        persisted = query_channel(admin, 1)
        assert local_rules(persisted) == rules, (
            "local rule order/permissions did not persist"
        )
        root_acl = admin.acl()
        assert all(fields(raw)[1] != [b"unsaved-draft"] for raw in root_acl[3]), (
            "unsent draft appeared on server"
        )
    finally:
        admin.close()
    mark("rules_persist_and_unsent_draft_absent_after_restart")
    if args.reconnect_settle:
        time.sleep(args.reconnect_settle)
    activate("Done")
    open_acl()
    save()
    mark("fresh_editor_after_reconnect")
    return {
        "fresh_offline_lookup": True,
        "unknown_account_refused": True,
        "close_reopen_stale_save_refused": True,
        "native_group_saved": True,
        "inherited_group_override_saved": True,
        "ordered_group_and_registered_user_rules_saved": True,
        "deny_applies_to_user_and_subchannel": True,
        "unrelated_control_can_speak": True,
        "dirty_conflict_retained": True,
        "root_admin_recovers_denied_child_acl": True,
        "dirty_draft_retained_read_only_after_disconnect": True,
        "stale_reconnect_save_refused": True,
        "persistence_after_restart": True,
        "fresh_editor_after_reconnect": True,
        "registered_ids": IDS.copy(),
        "server": args.server,
        "diagnostic_rate_override": args.high_admin_limit,
        "diagnostic_reconnect_settle_seconds": args.reconnect_settle,
    }


def checked_actions(*args):
    try:
        return actions(*args)
    except BaseException:
        options, _root, shell, application, _bot, _server = args
        for log in ("murmur.log", "nuschld.log"):
            source = _root / log
            if source.exists():
                (options.output / log).write_text(source.read_text())
        try:
            shell.screenshot(options.output / "failed-editor.png")
            (options.output / "failed-accessible.json").write_text(
                json.dumps(
                    [(role, name) for _, role, name, _ in items(application)],
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
    parser.add_argument(
        "--high-admin-limit",
        action="store_true",
        help="Diagnostic only: raise the owned Murmur control bucket",
    )
    parser.add_argument(
        "--reconnect-settle",
        type=float,
        default=0,
        help="Diagnostic only: wait before the final fresh editor",
    )
    ARGS = parser.parse_args()
    ARGS.document_links, ARGS.expect_markers = True, False
    fixture.WELCOME_EXTRA = ""
    fixture.seed_description = lambda _database: None
    fixture.configure_handler, fixture.run_actions = configure, checked_actions
    welcome_html.Murmur = OwnedMurmur if ARGS.server == "stock" else OwnedNuschld
    welcome_html.run(ARGS)


if __name__ == "__main__":
    main()
