#!/usr/bin/env python3
"""Key transparency against a running nuscheld (docs/e2ee.md §5): two clients with
their own crypto workers register, publish their identities and find them in the
server's log, see the same safety number, verify each other, and a new identity
of one of them shows up as a change the other must accept.

    python3 tests/key_transparency.py <binaries> --host 127.0.0.1 --port PORT

The binaries directory holds nuschel-cli and nuschel-crypto-worker (which runs in
its bubblewrap sandbox where bwrap and prlimit are on PATH).
"""
import argparse
import secrets
import tempfile
from pathlib import Path

from mumble_client import Client, certificate_pin


def connect(args, pin, name, profile, identity=None):
    extra = ["--identity-dir", str(identity)] if identity else []
    return Client(args.binaries, args.host, args.port, pin, name, profile, extra)


def register(args, pin, name, profile):
    """Registers a fresh name's certificate; returns the registration number."""
    client = connect(args, pin, name, profile)
    client.send(kind="register")
    user = client.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == client.session
                       and event["user"]["user_id"] is not None)["user"]
    client.send(kind="disconnect")
    client.process.wait(timeout=10)
    return user["user_id"]


def published(client, timeout=60):
    """Waits until self-monitoring found our binding in the log."""
    client.wait(lambda event: event["kind"] == "identity_monitor"
                and event["outcome"].get("status") == "consistent" and event["outcome"]["current"],
                timeout=timeout)


def contact(client, user_id, direct=True):
    client.send(kind="identity_check", user_id=user_id, direct=direct)
    return client.wait(lambda event: event["kind"] == "identity_contact" and event["user_id"] == user_id,
                       timeout=30)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    args = parser.parse_args()
    token = secrets.token_hex(3)
    with tempfile.TemporaryDirectory(prefix="nuschel-kt-") as temporary:
        root = Path(temporary)
        pin = certificate_pin(args.binaries, args.host, args.port, root / "probe")
        alice_name, bob_name = f"Alice{token}", f"Bob{token}"
        alice_id = register(args, pin, alice_name, root / "alice")
        bob_id = register(args, pin, bob_name, root / "bob")

        alice = connect(args, pin, alice_name, root / "alice", root / "alice-identity")
        negotiated = next(event for event in alice.events if event["kind"] == "negotiated")
        assert "kt/1" in negotiated["capabilities"], negotiated
        published(alice)
        bob = connect(args, pin, bob_name, root / "bob", root / "bob-identity")
        published(bob)

        seen = contact(bob, alice_id)
        assert seen["state"] == {"state": "unverified"}, seen
        number = seen["safety_number"]
        assert len(number.replace(" ", "")) == 60, number
        back = contact(alice, bob_id, direct=False)
        assert back["safety_number"] == number, (back, number)

        bob.send(kind="identity_verify", account=seen["account"])
        verified = bob.wait(lambda event: event["kind"] == "identity_contact"
                            and event["state"].get("state") == "verified", timeout=15)
        assert verified["state"] == {"state": "verified", "elsewhere": None}, verified

        # Alice comes back with a new identity: Bob, who verified her, must accept it.
        alice.send(kind="disconnect")
        alice.process.wait(timeout=10)
        alice = connect(args, pin, alice_name, root / "alice", root / "alice-new-identity")
        published(alice)
        changed = contact(bob, alice_id)
        assert changed["state"] == {"state": "key_changed", "blocking": True}, changed
        assert changed["safety_number"] == number, "the verified pin stays until accepted"
        bob.send(kind="identity_accept", account=changed["account"])
        accepted = bob.wait(lambda event: event["kind"] == "identity_contact"
                            and event["state"].get("state") == "unverified", timeout=15)
        assert accepted["safety_number"] != number, accepted
        for client in (alice, bob):
            client.send(kind="disconnect")
            client.process.wait(timeout=10)
        print("key transparency: published, looked up, verified, key change detected")


if __name__ == "__main__":
    main()
