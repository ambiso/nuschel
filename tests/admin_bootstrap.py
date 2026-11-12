#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Real loopback nuschld SuperUser/password and registered admin-group bootstrap."""

import argparse
import hashlib
import json
import socket
import ssl
import struct
import subprocess
import tempfile
from pathlib import Path

from hash_document_wire import fields, varint
from local_murmur import free_port
from local_nuschld import Nuschld
from mumble_client import Client, certificate_pin


def scalar(tag, value):
    return varint(tag << 3) + varint(value)


def blob(tag, value):
    return varint((tag << 3) | 2) + varint(len(value)) + value


def encode(values):
    return b"".join(
        scalar(tag, value) if isinstance(value, int) else blob(tag, value)
        for tag, items in sorted(values.items())
        for value in items
    )


class Rejected(Exception):
    pass


class Operator:
    """Stock Mumble protocol, explicit private pin, no ALPN/MLS negotiation."""

    def __init__(self, port, pin, password, certificate, key):
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
        context.check_hostname = False
        context.verify_mode = ssl.CERT_NONE
        context.load_cert_chain(certificate, key)
        self.stream = context.wrap_socket(
            socket.create_connection(("127.0.0.1", port), timeout=5),
            server_hostname="localhost",
        )
        assert (
            hashlib.sha256(self.stream.getpeercert(binary_form=True)).hexdigest() == pin
        )
        self.send(0, scalar(1, 0x010500) + blob(2, b"Private bootstrap fixture"))
        self.send(2, blob(1, b"SuperUser") + blob(2, password.encode()) + scalar(5, 1))
        try:
            self.sync = self.wait(5)
        except BaseException:
            self.close()
            raise

    def send(self, kind, body):
        self.stream.sendall(struct.pack(">HI", kind, len(body)) + body)

    def receive(self):
        def exact(size):
            data = bytearray()
            while len(data) < size:
                part = self.stream.recv(size - len(data))
                if not part:
                    raise EOFError("private server closed")
                data.extend(part)
            return bytes(data)

        kind, size = struct.unpack(">HI", exact(6))
        assert size <= 256 * 1024
        value = fields(exact(size))
        if kind == 4:
            raise Rejected(value.get(2, [b"rejected"])[0].decode(errors="replace"))
        return kind, value

    def wait(self, wanted):
        for _ in range(200):
            kind, value = self.receive()
            if kind == wanted:
                return value
        raise AssertionError(f"private protocol frame {wanted} missing")

    def acl(self):
        self.send(13, scalar(1, 0) + scalar(5, 1))
        return self.wait(13)

    def promote(self, user_id):
        value = self.acl()
        before_entries = [fields(raw) for raw in value.get(4, [])]

        def group_state(acl):
            # inherited_members (7) is query-only, not persisted group state.
            return {
                fields(raw)[1][0]: {
                    tag: items for tag, items in fields(raw).items() if tag != 7
                }
                for raw in acl.get(3, [])
            }

        before_groups = group_state(value)
        assert not before_groups[b"admin"].get(5), "fixture must start without admins"
        groups = []
        found = False
        for raw in value.get(3, []):
            group = fields(raw)
            if group[1] == [b"admin"]:
                found = True
                group[5] = sorted(set(group.get(5, []) + [user_id]))
                group[6] = [member for member in group.get(6, []) if member != user_id]
            group.pop(7, None)  # inherited members are a read-only query result
            groups.append(encode(group))
        assert found, "default root admin group missing"
        value[3], value[5] = groups, [0]
        self.send(13, encode(value))
        checked = self.acl()
        assert [fields(raw) for raw in checked.get(4, [])] == before_entries, (
            "promotion changed root ACL entries, their flags, or their order"
        )
        assert checked.get(2) == value.get(2), "promotion changed ACL inheritance"
        after_groups = group_state(checked)
        assert {
            name: group for name, group in after_groups.items() if name != b"admin"
        } == {
            name: group for name, group in before_groups.items() if name != b"admin"
        }, "promotion changed another group"
        admin = after_groups[b"admin"]
        assert {tag: items for tag, items in admin.items() if tag not in (5, 6)} == {
            tag: items
            for tag, items in before_groups[b"admin"].items()
            if tag not in (5, 6)
        }, "promotion changed admin group flags"
        assert admin[5] == [user_id], admin
        assert admin.get(6, []) == [
            member for member in before_groups[b"admin"].get(6, []) if member != user_id
        ], "promotion changed unrelated exclusions"
        return {
            "root_acl_entries_preserved_exactly": len(before_entries),
            "acl_inheritance_and_admin_flags_preserved": True,
            "other_groups_preserved_exactly": len(before_groups) - 1,
            "initial_admin_members_empty": True,
            "admin_members": admin[5],
        }

    def close(self):
        self.stream.close()


def permissions(client, write):
    client.send(kind="permissions", channel=0)
    return client.wait(
        lambda event: (
            event["kind"] == "permissions"
            and event["channel"] == 0
            and bool(event["permissions"] & 1) == write
        )
    )["permissions"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    result = {
        "binaries": str(args.binaries),
        "loopback_only": True,
        "stock_protocol_bootstrap": True,
    }
    with tempfile.TemporaryDirectory(prefix="admin-bootstrap-") as temporary:
        root = Path(temporary)
        server = Nuschld(args.binaries, root, free_port())
        server.write_config(message_limit=100, message_burst=100)
        server.password.chmod(0o600)
        certificate, key = root / "operator.pem", root / "operator-key.pem"
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
                "/CN=Private bootstrap fixture",
                "-days",
                "1",
                "-keyout",
                str(key),
                "-out",
                str(certificate),
            ],
            check=True,
            timeout=30,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        alice = control = operator = None
        try:
            server.start()
            pin = certificate_pin(
                args.binaries, "127.0.0.1", server.port, root / "probe"
            )
            password_a = server.password.read_text().strip()
            operator = Operator(server.port, pin, password_a, certificate, key)
            result["superuser_startup_file_login"] = True
            alice = Client(
                args.binaries,
                "127.0.0.1",
                server.port,
                pin,
                "Bootstrap Alice",
                root / "alice",
            )
            control = Client(
                args.binaries,
                "127.0.0.1",
                server.port,
                pin,
                "Ordinary control",
                root / "control",
            )
            result["ordinary_before"] = permissions(alice, False)
            alice.send(kind="register")
            registered = alice.wait(
                lambda event: (
                    event["kind"] == "user"
                    and event["user"]["session"] == alice.session
                    and event["user"]["user_id"] is not None
                )
            )["user"]
            user_id = registered["user_id"]
            assert user_id > 0
            result["registered_user_id"] = user_id
            result["promotion"] = operator.promote(user_id)
            result["ordinary_after"] = permissions(alice, True)
            assert result["ordinary_after"] & 0x70000 == 0x70000, (
                "root admin lacks Kick/Ban/Register"
            )
            result["unpromoted_control"] = permissions(control, False)
            control.send(
                kind="create_channel", parent=0, name="Must not create", temporary=False
            )
            control.wait(lambda event: event["kind"] == "permission_denied")
            alice.send(
                kind="create_channel",
                parent=0,
                name="Admin-created control",
                temporary=False,
            )
            alice.wait(
                lambda event: (
                    event["kind"] == "channel"
                    and event["channel"]["name"] == "Admin-created control"
                )
            )
            result["benign_admin_action_and_control_denial"] = True
            alice.close()
            alice = None
            control.close()
            control = None
            operator.close()
            operator = None
            server.stop()
            server.start()
            alice = Client(
                args.binaries,
                "127.0.0.1",
                server.port,
                pin,
                "Bootstrap Alice",
                root / "alice",
            )
            result["ordinary_after_restart"] = permissions(alice, True)
            assert alice.users[alice.session]["user_id"] == user_id
            alice.close()
            alice = None
            server.stop()

            # Startup secret replacement rotates the stored password on restart.
            password_b = "Private fixture rotated password"
            server.password.write_text(password_b + "\n")
            server.start()
            operator = Operator(server.port, pin, password_b, certificate, key)
            operator.close()
            operator = None
            try:
                Operator(server.port, pin, password_a, certificate, key)
                raise AssertionError("old startup password remained accepted")
            except Rejected:
                result["startup_rotation_rejects_old_password"] = True
            server.stop()

            # An offline admin command persists without a startup override.
            password_c = root / "offline-password"
            password_c.write_text("Private fixture offline password\n")
            password_c.chmod(0o600)
            command = [
                str(args.binaries / "nuschld"),
                "--config",
                str(server.config),
                "admin",
                "users",
                "set-password",
                "#0",
                "--password-file",
                str(password_c),
            ]
            changed = subprocess.run(
                command, check=True, capture_output=True, text=True, timeout=30
            )
            assert "set the password of SuperUser" in changed.stdout
            server.config.write_text(
                "\n".join(
                    line
                    for line in server.config.read_text().splitlines()
                    if not line.startswith("superuser_password_file")
                )
                + "\n"
            )
            server.start()
            operator = Operator(
                server.port, pin, password_c.read_text().strip(), certificate, key
            )
            operator.close()
            operator = None
            result["offline_password_persists_without_runtime_file"] = True
            server.stop()

            # Stock-client workaround is unavailable in e2ee mode; do not weaken it.
            server.config.write_text(
                server.config.read_text().replace(
                    'security = "compatible"', 'security = "e2ee"'
                )
            )
            server.start()
            try:
                Operator(
                    server.port, pin, password_c.read_text().strip(), certificate, key
                )
                raise AssertionError("legacy SuperUser admitted to e2ee mode")
            except (Rejected, BrokenPipeError, ConnectionResetError) as error:
                result["e2ee_refuses_stock_protocol"] = str(error)
            server.stop()
            result["passed"] = True
        finally:
            for client in (alice, control, operator):
                if client is not None:
                    client.close()
            server.stop()
            (args.output / "server.log").write_text(server.log.read_text())
            (args.output / "proof.json").write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result))


if __name__ == "__main__":
    main()
