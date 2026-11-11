#!/usr/bin/env python3
"""Nuschl against nuschld with the Nuschl extension: the extension is
negotiated, channel text is stored and acknowledged, members read what was said
while they were offline from the server's history, and a client that requires
the Nuschl protocol (an invite's policy=nuschl) logs in."""
import argparse
import secrets
import tempfile
from pathlib import Path

from mumble_client import Client, certificate_pin


def connect(binaries, args, pin, name, profile, extra=()):
    client = Client(binaries, args.host, args.port, pin, name, profile, extra)
    negotiated = next((event for event in client.events if event["kind"] == "negotiated"), None)
    assert negotiated and negotiated["nuschl"], f"no Nuschl negotiation: {negotiated}"
    assert "history/1" in negotiated["capabilities"], negotiated
    return client


def register(client):
    client.send(kind="register")
    client.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == client.session
                and event["user"]["user_id"] is not None)


def conversation(client, channel=0):
    for event in client.events:
        if event["kind"] == "channel_info" and event["channel"] == channel:
            return event["conversation"]
    return client.wait(lambda event: event["kind"] == "channel_info" and event["channel"] == channel)["conversation"]


def page(client, request_id, conversation_id):
    client.send(kind="history", request_id=request_id, conversation=conversation_id, limit=100)
    event = client.wait(lambda event: event.get("request_id") == request_id
                        and event["kind"] in ("history_page", "nuschl_error"))
    assert event["kind"] == "history_page", event
    return [message["content"] for message in event["messages"]]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    args = parser.parse_args()
    token = secrets.token_hex(4)
    with tempfile.TemporaryDirectory(prefix="nuschl-history-") as temporary:
        root = Path(temporary)
        pin = certificate_pin(args.binaries, args.host, args.port, root / "probe")
        alice = connect(args.binaries, args, pin, f"Alice{token}", root / "alice")
        register(alice)
        lobby = conversation(alice)
        first = f"kept for later {token}"
        alice.send(kind="text", channel=0, body=first)
        acked = alice.wait(lambda event: event["kind"] in ("chat_acked", "chat_failed"))
        assert acked["kind"] == "chat_acked", acked
        bob = connect(args.binaries, args, pin, f"Bob{token}", root / "bob",
                      ("--require-nuschl",))
        register(bob)
        assert first in page(bob, 1, lobby), "a later member reads earlier history"
        # Said while Alice is offline; she reads it from the server afterwards.
        alice.close()
        second = f"while you were away {token}"
        bob.send(kind="text", channel=0, body=second)
        assert bob.wait(lambda event: event["kind"] in ("chat_acked", "chat_failed"))["kind"] == "chat_acked"
        alice = connect(args.binaries, args, pin, f"Alice{token}", root / "alice")
        contents = page(alice, 2, lobby)
        assert contents.index(first) < contents.index(second), contents
        # Live: Bob's next message reaches Alice as text with its seq.
        third = f"live {token}"
        bob.send(kind="text", channel=0, body=third)
        live = alice.wait(lambda event: event["kind"] == "text" and event["body"] == third)
        assert live["seq"] is not None, live
        bob.close()
        alice.close()
    print("Nuschl history against nuschld: ok")


if __name__ == "__main__":
    main()
