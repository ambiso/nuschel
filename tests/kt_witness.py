#!/usr/bin/env python3
"""Witnessed key transparency against a running nuscheld that submits to a running
nuschel-witness (docs/witness.md, docs/e2ee.md §5.9): a client that requires the
witness sees the server's log witnessed once the auditor has caught up, and finds
a contact whose registration number and name labels name their account; a client
that requires a witness the server does not reach sees "Automatic verification
unavailable", and first contact still proceeds.

    python3 tests/kt_witness.py <binaries> --host server --witness-key FILE --other-key FILE

The key files hold C2SP verifier keys (`nuschel-witness vkey`).
"""
import argparse
import secrets
import tempfile
import time
from pathlib import Path

from key_transparency import published, register
from mumble_client import Client, certificate_pin


def connect(args, pin, name, profile, identity, witness):
    extra = ["--identity-dir", str(identity), "--kt-witness", witness]
    return Client(args.binaries, args.host, args.port, pin, name, profile, extra)


def head_check(client, timeout=60):
    """The outcome of the connection's first head check."""
    return client.wait(lambda event: event["kind"] == "identity_log", timeout=timeout)["outcome"]


def contact(client, user_id, name):
    client.send(kind="identity_check", user_id=user_id, direct=False, name=name)
    return client.wait(lambda event: event["kind"] == "identity_contact" and event["user_id"] == user_id,
                       timeout=30)


def leave(client):
    client.send(kind="disconnect")
    client.process.wait(timeout=10)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    parser.add_argument("--witness-key", type=Path, required=True)
    parser.add_argument("--other-key", type=Path, required=True)
    parser.add_argument("--timeout", type=float, default=180)
    args = parser.parse_args()
    witness = args.witness_key.read_text().strip()
    other = args.other_key.read_text().strip()
    token = secrets.token_hex(3)
    with tempfile.TemporaryDirectory(prefix="nuschel-kt-witness-") as temporary:
        root = Path(temporary)
        pin = certificate_pin(args.binaries, args.host, args.port, root / "probe")
        alice_name, bob_name, carol_name = f"Alice{token}", f"Bob{token}", f"Carol{token}"
        alice_id = register(args, pin, alice_name, root / "alice")
        register(args, pin, bob_name, root / "bob")
        register(args, pin, carol_name, root / "carol")

        alice = connect(args, pin, alice_name, root / "alice", root / "alice-identity", witness)
        negotiated = next(event for event in alice.events if event["kind"] == "negotiated")
        assert "kt-witness/1" in negotiated["capabilities"], negotiated
        published(alice)
        leave(alice)

        # The server submits its epochs every few seconds; once the auditor holds the
        # newest, a new connection is witnessed.
        deadline = time.monotonic() + args.timeout
        while True:
            bob = connect(args, pin, bob_name, root / "bob", root / "bob-identity", witness)
            outcome = head_check(bob)
            status = outcome.get("witness", {}).get("witness")
            if outcome["status"] == "accepted" and status == "witnessed":
                break
            leave(bob)
            assert time.monotonic() < deadline, f"never witnessed: {outcome}"
            time.sleep(2)
        seen = contact(bob, alice_id, alice_name)
        assert seen["state"] == {"state": "unverified"}, seen
        leave(bob)

        # A witness this server does not submit to: unavailable, and first contact
        # proceeds as "not verified".
        carol = connect(args, pin, carol_name, root / "carol", root / "carol-identity", other)
        outcome = head_check(carol)
        assert outcome["status"] == "accepted", outcome
        assert outcome["witness"]["witness"] == "unavailable", outcome
        seen = contact(carol, alice_id, alice_name)
        assert seen["state"].get("state") == "unavailable", seen
        assert seen["state"]["verified"] is False, seen
        leave(carol)
        print("witnessed key transparency: cosigned by the auditor, names checked, "
              "an unreachable witness shows as unavailable")


if __name__ == "__main__":
    main()
