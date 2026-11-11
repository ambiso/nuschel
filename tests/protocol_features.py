#!/usr/bin/env python3
"""Mumble protocol features through Nuschl's session against a stock server.

Whisper/shout routing isolation, listening, permissions, user statistics,
comments and avatars via blobs, self-registration, PKCS#12 identities created
by OpenSSL, and SuperUser administration (ban list, registered users, ACL,
kick). Uses only servers we start ourselves.
"""
import argparse
from pathlib import Path
import struct
import subprocess
import sys
import tempfile
import time
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mumble_client import Client, ReferenceOpus, certificate_pin  # noqa: E402

SPEAK = 0x8
KICK = 0x10000


def join(client, channel):
    client.send(kind="join", channel=channel)
    client.wait(lambda e: e["kind"] == "user" and e["user"]["session"] == client.session and e["user"]["channel"] == channel)


def heard(listener, speaker, since):
    return any(e["kind"] == "audio" and e["sender"] == speaker.session for e in list(listener.events)[since:])


def speak(client, packets, target, base):
    for index, packet in enumerate(packets):
        client.send(kind="voice", target=target, sequence=base + index * 2, opus=packet, end=index == len(packets) - 1)
        time.sleep(0.02)
    time.sleep(0.8)


def png(path, size):
    rows = b"".join(b"\x00" + bytes((x * 7) % 256 for x in range(size * 3)) for _ in range(size))
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 2, 0, 0, 0))
                     + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))
    return path


def openssl_identity(openssl, root, legacy):
    """An RSA client certificate exported the way Mumble/OpenSSL do it."""
    key, cert, p12 = root / "rsa.key", root / "rsa.pem", root / ("legacy.p12" if legacy else "modern.p12")
    subprocess.run([openssl, "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", key, "-out", cert,
                    "-days", "2", "-subj", "/CN=Imported Mumble user"], check=True, capture_output=True)
    command = [openssl, "pkcs12", "-export", "-inkey", key, "-in", cert, "-out", p12, "-passout", "pass:p12pass"]
    if legacy:
        command.append("-legacy")
    subprocess.run(command, check=True, capture_output=True)
    der = subprocess.run([openssl, "x509", "-in", cert, "-outform", "DER"], check=True, capture_output=True).stdout
    import hashlib
    return p12, hashlib.sha1(der).hexdigest()


def check_listening(clients, alice, bob, carol, dave, packets):
    carol.send(kind="listen", channel=2, listen=True)
    carol.wait(lambda e: e["kind"] == "user" and e["user"]["session"] == carol.session and 2 in e["user"]["listening"])
    marks = {client.session: len(client.events) for client in clients}
    speak(dave, packets, 0, 0)
    assert heard(carol, dave, marks[carol.session]), "listener did not hear the channel"
    assert not heard(bob, dave, marks[bob.session])
    print("Channel listening passed.", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--superuser-password", default="nuschl-super")
    parser.add_argument("--openssl", default="openssl")
    parser.add_argument("--no-listening", action="store_true", help="server predates channel listeners (1.4)")
    args = parser.parse_args()
    reference = ReferenceOpus(args.opus_library)
    packets = reference.encode_tone(8)
    with tempfile.TemporaryDirectory(prefix="nuschl-protocol-") as temporary:
        root = Path(temporary)
        pin = certificate_pin(args.binaries, args.host, args.port, root / "probe")
        connect = lambda name, *extra: Client(args.binaries, args.host, args.port, pin, name, root / name, list(extra))
        clients = []
        try:
            alice, bob, carol, dave = (connect(name) for name in ("ProtoAlice", "ProtoBob", "ProtoCarol", "ProtoDave"))
            clients = [alice, bob, carol, dave]
            for client in (alice, bob, carol):
                join(client, 1)
            join(dave, 2)
            for client in clients:
                client.unmute()

            # Whisper to Bob only: Carol (same channel) and Dave must not hear it.
            alice.send(kind="whisper", id=1, sessions=[bob.session])
            time.sleep(0.3)
            marks = {client.session: len(client.events) for client in clients}
            speak(alice, packets, 1, 0)
            assert heard(bob, alice, marks[bob.session]), "whisper did not reach Bob"
            assert not heard(carol, alice, marks[carol.session]), "whisper leaked into the channel"
            assert not heard(dave, alice, marks[dave.session]), "whisper leaked to another channel"
            # Shout to Development: only Dave is there.
            alice.send(kind="whisper", id=2, channel=2)
            time.sleep(0.3)
            marks = {client.session: len(client.events) for client in clients}
            speak(alice, packets, 2, 40)
            assert heard(dave, alice, marks[dave.session]), "shout did not reach the target channel"
            assert not heard(bob, alice, marks[bob.session]) and not heard(carol, alice, marks[carol.session])
            print("Whisper and shout routing isolation passed.", flush=True)

            # Carol listens to Development and hears Dave's normal speech; Bob does not (1.4+).
            if args.no_listening:
                print("Channel listening skipped (server predates Mumble 1.4).", flush=True)
            else:
                check_listening(clients, alice, bob, carol, dave, packets)
            # Permissions: ordinary users may speak in Lobby but cannot kick. Servers
            # also push permissions on join, so earlier events count too.
            bob.send(kind="permissions", channel=1)
            event = next((e for e in reversed(bob.events) if e["kind"] == "permissions" and e["channel"] == 1), None) \
                or bob.wait(lambda e: e["kind"] == "permissions" and e["channel"] == 1)
            assert event["permissions"] & SPEAK
            root_permissions = [e["permissions"] for e in bob.events if e["kind"] == "permissions" and e["channel"] == 0]
            assert not root_permissions or not root_permissions[-1] & KICK
            bob.send(kind="moderate", session=carol.session, action="kick", reason="nope")
            bob.wait(lambda e: e["kind"] == "permission_denied")
            print("Permission queries and denial passed.", flush=True)

            # Statistics, comments and avatars through blobs.
            bob.send(kind="stats", session=alice.session)
            stats = bob.wait(lambda e: e["kind"] == "stats" and e["stats"]["session"] == alice.session)["stats"]
            # Version details are only shown to admins and the user themselves.
            assert stats["online_seconds"] is not None and stats["from_client"]["good"] > 0, stats
            alice.send(kind="comment", text="Hello <from> Alice\nsecond line")
            comment = bob.wait(lambda e: e["kind"] == "user" and e["user"]["session"] == alice.session
                               and (e["user"]["comment"] or e["user"]["comment_hash"]))["user"]
            if not comment["comment"]:
                bob.send(kind="blobs", comments=[alice.session])
                comment = bob.wait(lambda e: e["kind"] == "user" and e["user"]["session"] == alice.session and e["user"]["comment"])["user"]
            assert "&lt;from&gt;" in comment["comment"] and "<br/>" in comment["comment"], comment["comment"]
            alice.send(kind="avatar", path=str(png(root / "avatar.png", 32)))
            bob.wait(lambda e: e["kind"] == "user" and e["user"]["session"] == alice.session and e["user"]["texture_hash"])
            print("User statistics, comments and avatars passed.", flush=True)

            # Self-registration succeeds for certificate users unless the ACL forbids it.
            carol.send(kind="register")
            result = carol.wait(lambda e: e["kind"] == "permission_denied"
                                or (e["kind"] == "user" and e["user"]["session"] == carol.session and e["user"]["user_id"]))
            print("Self-registration:", "denied by ACL" if result["kind"] == "permission_denied" else f"user id {result['user']['user_id']}", flush=True)

            # SuperUser administration.
            password = root / "superuser-password"
            password.write_text(args.superuser_password)
            admin = connect("SuperUser", "--password-file", str(password))
            clients.append(admin)
            admin.send(kind="bans")
            admin.wait(lambda e: e["kind"] == "bans")
            admin.send(kind="registered_users")
            users = admin.wait(lambda e: e["kind"] == "registered_users")["users"]
            assert result["kind"] == "permission_denied" or any(user["name"] == "ProtoCarol" for user in users), users
            admin.send(kind="acl", channel=1)
            acl = admin.wait(lambda e: e["kind"] == "acl")["acl"]
            assert acl["channel"] == 1 and acl["entries"], acl
            admin.send(kind="moderate", session=dave.session, action="mute")
            bob.wait(lambda e: e["kind"] == "activity" and "ProtoDave was muted by SuperUser" in e["activity"]["text"])
            admin.send(kind="moderate", session=dave.session, action="kick", reason="test over")
            bob.wait(lambda e: e["kind"] == "activity" and "ProtoDave was kicked by SuperUser: test over" in e["activity"]["text"])
            dave.process.wait(timeout=10)
            assert dave.process.returncode != 0
            clients.remove(dave)
            print("SuperUser ban list, registered users, ACL, server mute and kick passed.", flush=True)

            # Identities exported by OpenSSL (modern PBES2 and legacy 3DES/RC2) log in as themselves.
            for legacy in (False, True):
                p12, sha1 = openssl_identity(args.openssl, root, legacy)
                imported = Client(args.binaries, args.host, args.port, pin, f"Imported{int(legacy)}", root / f"imported{int(legacy)}",
                                  ["--import-certificate", str(p12), "--certificate-password", "p12pass"])
                clients.append(imported)
                seen = bob.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == f"Imported{int(legacy)}")["user"]
                assert seen["certificate_hash"] == sha1, (seen["certificate_hash"], sha1)
            print("PKCS#12 identities from OpenSSL (modern and legacy) passed.", flush=True)
        finally:
            for client in clients:
                try:
                    client.close()
                except AssertionError:
                    pass


if __name__ == "__main__":
    main()
