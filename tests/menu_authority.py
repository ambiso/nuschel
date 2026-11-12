#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Original-target/focus regressions using owned stock Murmur and private GNOME."""

import argparse
import collections
import hashlib
import json
import socket
import ssl
import struct
import threading
import time
from pathlib import Path

import context_menus
import document_link_regression as fixture
import welcome_html
from accessibility import ROLES, Node, items
from hash_document_wire import fields, varint
from local_murmur import eventually
from mumble_client import Client

TARGET = "Authority target"
ALL = 0x1F0FFF


def scalar(tag, value):
    return varint(tag << 3) + varint(value)


def blob(tag, value):
    return varint((tag << 3) | 2) + varint(len(value)) + value


class Administrator:
    """Pinned TEST-ONLY loopback SuperUser; changes only the disposable ACL."""

    def __init__(self, port, pin):
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
        self.stream = context.wrap_socket(
            socket.create_connection(("127.0.0.1", port), timeout=5),
            server_hostname="localhost",
        )
        assert (
            hashlib.sha256(self.stream.getpeercert(binary_form=True)).hexdigest() == pin
        )
        self.stream.settimeout(None)
        self.frames = collections.deque(maxlen=200)
        self.closed = False
        self.reader = threading.Thread(target=self.read, daemon=True)
        self.reader.start()
        self.send(0, scalar(1, 0x010500))
        self.send(2, blob(1, b"SuperUser") + blob(2, b"nuschl-super") + scalar(5, 1))
        eventually(
            lambda: any(kind == 5 for kind, _ in self.frames),
            "private SuperUser login missing",
        )

    def send(self, kind, body):
        self.stream.sendall(struct.pack(">HI", kind, len(body)) + body)

    def read(self):
        def exact(size):
            data = bytearray()
            while len(data) < size:
                part = self.stream.recv(size - len(data))
                if not part:
                    raise EOFError
                data.extend(part)
            return bytes(data)

        try:
            while not self.closed:
                kind, size = struct.unpack(">HI", exact(6))
                assert size <= 256 * 1024
                self.frames.append((kind, fields(exact(size))))
        except (OSError, EOFError):
            pass

    def permissions(self, grant):
        acl = scalar(1, 1) + scalar(2, 1) + blob(5, b"all") + scalar(6, grant)
        self.send(13, scalar(1, 0) + scalar(2, 1) + blob(4, acl) + scalar(5, 0))

    def close(self):
        self.closed = True
        try:
            self.stream.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.stream.close()
        self.reader.join(timeout=3)


def seed(database):
    context_menus.seed(database)
    ini = database.parent / "murmur.ini"
    ini.write_text(ini.read_text().replace("users=50", "users=4"))


def actions(args, root, shell, application, bot, murmur):
    pin = bot.process.args[bot.process.args.index("--certificate-sha256") + 1]
    target = Client(
        args.binaries, "127.0.0.1", murmur.port, pin, TARGET, root / "authority-target"
    )
    admin = Administrator(murmur.port, pin)
    replacement = observer = None
    records = {}

    def nodes(parent=None):
        if parent is None:
            return items(application)
        data = application.bus(
            "call",
            application.destination,
            "/org/a11y/atspi/cache",
            "org.a11y.atspi.Cache",
            "GetItems",
        )[0]
        parents = {tuple(row[0]): tuple(row[2]) for row in data}
        wanted = (parent.destination, parent.path)
        result = []
        for row in data:
            current = tuple(row[0])
            for _ in range(64):
                if current == wanted:
                    result.append(
                        (
                            Node(application.bus, *row[0]),
                            ROLES.get(row[7], f"role {row[7]}"),
                            row[6],
                            row[9][0],
                        )
                    )
                    break
                current = parents.get(current)
                if current is None:
                    break
        return result

    def named(name, role=None, parent=None):
        return next(
            (
                node
                for node, kind, label, _ in nodes(parent)
                if label == name and (role is None or kind == role)
            ),
            None,
        )

    def menu():
        return next(
            (node for node, role, _, _ in items(application) if role == "menu"), None
        )

    def point(node):
        origin = json.loads(
            shell.eval(
                "JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return {x:b.x,y:b.y};})())"
            )
        )
        x, y, width, height = node.extents()
        return origin["x"] + x + width // 2, origin["y"] + y + height // 2

    def open_menu(name):
        row = eventually(
            lambda: next(
                (
                    node
                    for node, role, label, _ in items(application)
                    if role == "tree item" and label.startswith(name)
                ),
                None,
            ),
            f"row missing: {name}",
        )
        row.call("Component", "GrabFocus")
        time.sleep(0.2)
        shell.click(*point(row), button=3)
        return eventually(menu, f"menu missing: {name}")

    def action(name, current=None):
        current = current or menu()
        return eventually(
            lambda: named(name, "menu item", current), f"menu item missing: {name}"
        )

    def dismiss():
        if menu():
            shell.key("Escape")
            eventually(lambda: menu() is None, "menu remained after Escape")

    def dialog():
        return named("Input", "dialog")

    def cancel():
        current = eventually(dialog, "prompt missing")
        eventually(
            lambda: named("Cancel", "push button", current), "Cancel missing"
        ).press()
        eventually(lambda: dialog() is None, "prompt not dismissed")

    def open_create(name):
        current = open_menu("Root")
        action("Create sub-channel…", current).press()
        current = eventually(dialog, "create prompt missing")
        entry = eventually(
            lambda: next(
                (
                    node
                    for node, role, label, _ in nodes(current)
                    if role in ("entry", "text") and label == "Create a channel"
                ),
                None,
            ),
            "create input missing",
        )
        shell.click(*point(entry))
        shell.type(name)
        return current

    def close_details():
        current = eventually(lambda: named("Details", "dialog"), "profile did not open")
        eventually(
            lambda: named("Done", "push button", current), "profile Done missing"
        ).press()
        eventually(lambda: named("Details", "dialog") is None, "profile did not close")

    try:
        shell.key("Shift_L")
        shell.place("Nuschl", 40, 40, 1280, 800)
        # A real permission response removes an earlier action; retained keyboard
        # focus must remain Kick rather than becoming Ban at its previous index.
        current = open_menu(TARGET)
        register = action(f"Register {TARGET}", current)
        kick = action("Kick…", current)
        kick.call("Component", "GrabFocus")
        eventually(lambda: kick.focused, "Kick did not receive focus")
        before = [name for _, role, name, _ in nodes(current) if role == "menu item"]
        assert register is not None
        admin.permissions(ALL & ~0x1 & ~0x40000)
        eventually(
            lambda: named(f"Register {TARGET}", "menu item", menu()) is None,
            "permission update did not remove Register",
        )
        current = eventually(menu, "permission update closed entire menu")
        kick = action("Kick…", current)
        eventually(
            lambda: kick.focused, "permission insertion/removal retargeted focus"
        )
        assert not action("Ban…", current).focused
        after = [name for _, role, name, _ in nodes(current) if role == "menu item"]
        assert before.index("Kick…") != after.index("Kick…"), (
            "fixture did not actually shift row indexes"
        )
        shell.screenshot(args.output / "permission-focus.png")
        shell.key("Return")
        current = eventually(dialog, "focused Kick did not open its prompt")
        assert any(label == f"Kick {TARGET}" for _, _, label, _ in nodes(current)), (
            "keyboard activated a different moderation action"
        )
        cancel()
        assert target.process.poll() is None, "fixture target was moderated"
        records["semantic_focus"] = {
            "before": before,
            "after": after,
            "kick_stayed_focused": True,
            "only_opened_and_cancelled_prompt": True,
        }
        admin.permissions(ALL)
        time.sleep(0.3)

        # Murmur's bounded FIFO session pool is deliberately small here. No
        # patching of server/client state: reconnect disposable peers until the
        # real server reissues the old numeric session to another certificate.
        current = open_menu(TARGET)
        stale_profile = action("Profile and connection", current)
        old_session = target.session
        target.close()
        attempts = []
        for number in range(8):
            replacement = Client(
                args.binaries,
                "127.0.0.1",
                murmur.port,
                pin,
                "Replacement target",
                root / "replacement-target",
            )
            attempts.append(replacement.session)
            if replacement.session == old_session:
                break
            replacement.close()
            replacement = None
        assert replacement is not None and replacement.session == old_session, attempts
        eventually(
            lambda: any(
                role == "tree item" and name.startswith("Replacement target")
                for _, role, name, _ in items(application)
            ),
            "replacement peer missing",
        )
        time.sleep(0.3)
        assert named("Profile and connection", "menu item", menu()) is None
        refused = False
        try:
            stale_profile.press()
        except (AssertionError, RuntimeError):
            refused = True
        time.sleep(0.4)
        assert named("Details", "dialog") is None, (
            "old profile action targeted a replacement session"
        )
        records["session_reuse"] = {
            "original_session": old_session,
            "attempts": attempts,
            "replacement_session": replacement.session,
            "stale_accessible_action_refused": refused,
            "replacement_profile_not_opened": True,
        }
        dismiss()
        action("Profile and connection", open_menu("Replacement target")).press()
        close_details()
        records["session_reuse"]["fresh_profile_control"] = True

        stale_name = "Must not create after reconnect"
        open_create(stale_name)
        target.close()
        replacement.close()
        replacement = None
        admin.close()
        admin = None
        bot.close()  # Preserve the harness's ordinary successful teardown.
        murmur.stop()
        murmur.start()
        observer = Client(
            args.binaries,
            "127.0.0.1",
            murmur.port,
            pin,
            "Reconnect observer",
            root / "reconnect-observer",
        )
        eventually(
            lambda: any(
                user["name"] == "Welcome reader" for user in observer.users.values()
            ),
            "desktop did not automatically reconnect",
        )
        time.sleep(0.5)
        current = eventually(
            dialog, "old create prompt was not retained for stale-submit test"
        )
        eventually(
            lambda: named("Create", "push button", current), "stale Create missing"
        ).press()
        eventually(lambda: dialog() is None, "stale submit did not close prompt")
        time.sleep(1.2)
        assert not any(
            channel["name"] == stale_name for channel in observer.channels.values()
        ), "stale prompt reached new connection"
        fresh_name = "Fresh current channel control"
        current = open_create(fresh_name)
        eventually(
            lambda: named("Create", "push button", current), "fresh Create missing"
        ).press()
        observer.wait(
            lambda event: (
                event["kind"] == "channel" and event["channel"]["name"] == fresh_name
            )
        )
        records["automatic_reconnect"] = {
            "stale_channel_absent": True,
            "fresh_channel_created": True,
            "same_saved_server": True,
        }
        shell.screenshot(args.output / "fresh-reconnect-control.png")
        return {
            "passed": True,
            "private_stock_murmur": True,
            "no_actual_moderation": True,
            **records,
        }
    finally:
        for client in (replacement, observer, target):
            if client is not None:
                client.close()
        if admin is not None:
            admin.close()
        (args.output / "authority-cases.json").write_text(
            json.dumps(records, indent=2) + "\n"
        )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ("binaries", "murmur", "at-spi", "pulse-bin", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    args = parser.parse_args()
    args.document_links, args.expect_markers = True, False
    fixture.WELCOME_EXTRA = ""
    fixture.seed_description, fixture.configure_handler, fixture.run_actions = (
        seed,
        context_menus.configure,
        actions,
    )
    welcome_html.run(args)
    proof = json.loads((args.output / "proof.json").read_text())
    proof["menu_authority"] = proof.pop("document_links")
    (args.output / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")


if __name__ == "__main__":
    main()
