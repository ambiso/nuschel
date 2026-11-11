#!/usr/bin/env python3
"""End-to-end-encrypted direct conversations against a running nuscheld
(docs/e2ee.md §6, `mls/1`): two clients with their own crypto workers register,
publish their identities, upload key packages, open an encrypted direct
conversation, exchange messages both ways, and read what arrived while one of
them was offline. With --phase second, the same users (same directories under
--state) check after a server restart that the group and the stream survived.

    python3 tests/e2ee_direct.py <binaries> --host 127.0.0.1 --port PORT [--state DIR]

The binaries directory holds nuschel-cli and nuschel-crypto-worker (which runs in
its bubblewrap sandbox where bwrap and prlimit are on PATH).
"""
import argparse
import json
import secrets
import tempfile
from pathlib import Path

from mumble_client import Client, certificate_pin


def connect(args, pin, name, profile, identity=None):
    extra = ["--identity-dir", str(identity), "--e2ee"] if identity else []
    return Client(args.binaries, args.host, args.port, pin, name, profile, extra)


def register(args, pin, name, profile):
    """Registers a fresh name's certificate; returns the registration number."""
    client = connect(args, pin, name, profile)
    client.send(kind="register")
    user = client.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == client.session
                       and event["user"]["user_id"] is not None)["user"]
    leave(client)
    return user["user_id"]


def leave(client):
    client.send(kind="disconnect")
    client.process.wait(timeout=10)


def key_packages(client, timeout=90):
    """Waits until the server holds our key packages (after our identity is in the log)."""
    client.wait(lambda event: event["kind"] == "e2ee_key_packages" and event["one_time"] >= 4,
                timeout=timeout)


def established(client, conversation=None, timeout=60):
    def ready(event):
        view = event.get("view", {})
        return (event["kind"] == "e2ee_conversation" and view.get("pinned")
                and (conversation is None or event["conversation"] == conversation)
                and view["lock"]["state"] in ("encrypted", "verified"))
    return client.wait(ready, timeout=timeout)


def send(client, conversation, text):
    client.send(kind="e2ee_send", conversation=conversation, text=text)
    event = client.wait(lambda event: event["kind"] in ("e2ee_sent", "e2ee_not_sent")
                        and event["conversation"] == conversation, timeout=30)
    assert event["kind"] == "e2ee_sent", event
    return event["seq"]


def received(client, text, timeout=30):
    return client.wait(lambda event: event["kind"] == "e2ee_received"
                       and event["received"].get("kind") == "message"
                       and event["received"].get("text") == text, timeout=timeout)["received"]


def first_phase(args, pin, root, token):
    alice_name, bob_name = f"Alice{token}", f"Bob{token}"
    alice_id = register(args, pin, alice_name, root / "alice")
    bob_id = register(args, pin, bob_name, root / "bob")

    bob = connect(args, pin, bob_name, root / "bob", root / "bob-identity")
    negotiated = next(event for event in bob.events if event["kind"] == "negotiated")
    assert "mls/1" in negotiated["capabilities"], negotiated
    key_packages(bob)
    alice = connect(args, pin, alice_name, root / "alice", root / "alice-identity")
    key_packages(alice)

    alice.send(kind="e2ee_open", user_id=bob_id)
    view = established(alice)
    conversation = view["conversation"]
    assert len(view["view"]["members"]) == 2, view
    assert all(member["verified"] for member in view["view"]["members"]), view
    established(bob, conversation)

    seq = send(alice, conversation, "hello from alice")
    message = received(bob, "hello from alice")
    assert message["seq"] == seq and message["order"] == {"order": "in_order"}, message
    send(bob, conversation, "hello from bob")
    received(alice, "hello from bob")

    # Offline delivery: the stream keeps it for Bob's device.
    leave(bob)
    send(alice, conversation, "while bob was away")
    bob = connect(args, pin, bob_name, root / "bob", root / "bob-identity")
    received(bob, "while bob was away")

    leave(alice)
    leave(bob)
    (root / "state.json").write_text(json.dumps({
        "alice": alice_name, "bob": bob_name, "alice_id": alice_id, "bob_id": bob_id,
        "conversation": conversation}))


def second_phase(args, pin, root):
    """After a server restart: the group, the key packages and the stream are still there."""
    state = json.loads((root / "state.json").read_text())
    alice = connect(args, pin, state["alice"], root / "alice", root / "alice-identity")
    bob = connect(args, pin, state["bob"], root / "bob", root / "bob-identity")
    alice.send(kind="e2ee_status", conversation=state["conversation"])
    established(alice, state["conversation"])
    send(alice, state["conversation"], "after the restart")
    received(bob, "after the restart")
    leave(alice)
    leave(bob)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    parser.add_argument("--state", type=Path, help="keep users here between phases")
    parser.add_argument("--phase", choices=("first", "second"), default="first")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="nuschel-e2ee-") as temporary:
        root = args.state or Path(temporary)
        root.mkdir(parents=True, exist_ok=True)
        pin = certificate_pin(args.binaries, args.host, args.port, root / "probe")
        if args.phase == "first":
            first_phase(args, pin, root, secrets.token_hex(3))
        else:
            second_phase(args, pin, root)
    print("e2ee direct conversation: ok")


if __name__ == "__main__":
    main()
