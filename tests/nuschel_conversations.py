#!/usr/bin/env python3
"""Edits, deletions, typing and direct conversations against nuscheld
(docs/extension.md §3, §6, §7), with two nuschel-cli clients and SuperUser:

- an author edits their message (others see the edit; pages show the new
  text); someone else can neither edit nor delete it;
- the author deletes it (others see the deletion; pages keep a tombstone
  without text); a moderator deletes someone else's message, which the
  server's hello allows (moderators_delete), and the tombstone says so;
- typing reaches the other reader, at most once in three seconds;
- a direct conversation stores what was sent while the other account was
  offline, lists it as unread at their next login, pages it, takes their
  read marker, and carries live messages as text in that conversation.

    python3 tests/with_local_nuscheld.py <binaries> tests/nuschel_conversations.py <binaries> --port PORT
"""
import argparse
import secrets
import tempfile
import time
from pathlib import Path

from local_nuscheld import Nuscheld
from mumble_client import Client, certificate_pin
from nuschel_history import conversation, register

EDIT, DELETE = 1, 2
NOT_FOUND = 3


def connect(binaries, args, pin, name, profile, extra=()):
    client = Client(binaries, args.host, args.port, pin, name, profile, extra)
    negotiated = next(event for event in client.events if event["kind"] == "negotiated")
    assert negotiated["nuschel"] and "history/1" in negotiated["capabilities"], negotiated
    assert "typing/1" in negotiated["capabilities"], negotiated
    return client, negotiated


def answer(client, request_id):
    return client.wait(lambda event: event.get("request_id") == request_id
                       and event["kind"] in ("chat_ack", "nuschel_error", "history_page",
                                             "direct_opened", "direct_list"))


def page(client, request_id, conversation_id):
    client.send(kind="history", request_id=request_id, conversation=conversation_id, limit=100)
    event = answer(client, request_id)
    assert event["kind"] == "history_page", event
    return {message["seq"]: message for message in event["messages"]}


def send(client, body):
    client.send(kind="text", channel=0, body=body)
    acked = client.wait(lambda event: event["kind"] in ("chat_acked", "chat_failed"))
    assert acked["kind"] == "chat_acked", acked
    return acked["seq"]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    args = parser.parse_args()
    token = secrets.token_hex(3)
    with tempfile.TemporaryDirectory(prefix="nuschel-conversations-") as temporary:
        root = Path(temporary)
        pin = certificate_pin(args.binaries, args.host, args.port, root / "probe")
        alice, negotiated = connect(args.binaries, args, pin, f"Alice{token}", root / "alice")
        assert negotiated["policy"]["moderators_delete"], negotiated
        register(alice)
        bob, _ = connect(args.binaries, args, pin, f"Bob{token}", root / "bob")
        register(bob)
        lobby = conversation(alice)

        # The author edits; Bob sees the edit live, and pages show it.
        typo = send(alice, f"teh typo {token}")
        time.sleep(1.1)  # the shared text bucket
        alice.send(kind="chat_edit", request_id=1, conversation=lobby, target=typo,
                   content=f"the typo, fixed {token}")
        assert answer(alice, 1)["kind"] == "chat_ack"
        edited = bob.wait(lambda event: event["kind"] == "chat" and event["message"]["kind"] == EDIT)
        assert edited["message"]["target"] == typo, edited
        assert edited["message"]["content"] == f"the typo, fixed {token}", edited
        shown = page(bob, 2, lobby)[typo]
        assert shown["content"] == f"the typo, fixed {token}" and shown["edits"] == 1, shown

        # Nobody else edits it, and Bob is no moderator.
        bob.send(kind="chat_edit", request_id=3, conversation=lobby, target=typo, content="mine now")
        refused = answer(bob, 3)
        assert refused["kind"] == "nuschel_error" and refused["code"] == NOT_FOUND, refused
        time.sleep(1.1)
        bob.send(kind="chat_delete", request_id=4, conversation=lobby, target=typo)
        refused = answer(bob, 4)
        assert refused["kind"] == "nuschel_error" and refused["code"] == NOT_FOUND, refused

        # The author deletes it: a tombstone without text, live and in pages.
        time.sleep(1.1)
        alice.send(kind="chat_delete", request_id=5, conversation=lobby, target=typo)
        assert answer(alice, 5)["kind"] == "chat_ack"
        gone = bob.wait(lambda event: event["kind"] == "chat" and event["message"]["kind"] == DELETE)
        assert gone["message"]["target"] == typo, gone
        tombstone = page(bob, 6, lobby)[typo]
        assert tombstone["deleted"] and tombstone["content"] == "", tombstone
        assert tombstone["delete_reason"] == 0, tombstone

        # A moderator (SuperUser holds Write everywhere) deletes Bob's message.
        time.sleep(1.1)
        rude = send(bob, f"something rude {token}")
        password = root / "superuser-password"
        password.write_text(Nuscheld.SUPERUSER_PASSWORD)
        admin, _ = connect(args.binaries, args, pin, "SuperUser", root / "admin",
                           ("--password-file", str(password)))
        admin.send(kind="chat_delete", request_id=7, conversation=lobby, target=rude)
        assert answer(admin, 7)["kind"] == "chat_ack"
        moderated = page(alice, 8, lobby)[rude]
        assert moderated["deleted"] and moderated["delete_reason"] == 1, moderated
        admin.close()

        # Typing reaches Bob, once per three seconds.
        alice.send(kind="typing", conversation=lobby)
        typing = bob.wait(lambda event: event["kind"] == "typing")
        assert typing["conversation"] == lobby and typing["session"] == alice.session, typing
        alice.send(kind="typing", conversation=lobby)
        try:
            bob.wait(lambda event: event["kind"] == "typing", timeout=1.5)
            raise AssertionError("a second typing within three seconds was forwarded")
        except AssertionError as error:
            if "forwarded" in str(error):
                raise

        # A direct conversation: stored while Bob is offline.
        bob_id = bob.users[bob.session]["user_id"]
        alice.send(kind="open_direct", request_id=9, user_id=bob_id)
        opened = answer(alice, 9)
        assert opened["kind"] == "direct_opened", opened
        direct = opened["direct"]["conversation"]
        bob.close()
        time.sleep(1.1)
        alice.send(kind="direct_text", conversation=direct, body=f"while you were away {token}")
        acked = alice.wait(lambda event: event["kind"] in ("chat_acked", "chat_failed"))
        assert acked["kind"] == "chat_acked", acked
        stored = acked["seq"]
        bob, _ = connect(args.binaries, args, pin, f"Bob{token}", root / "bob")
        listed = bob.wait(lambda event: event["kind"] == "direct_list")
        mine = next(entry for entry in listed["directs"] if entry["conversation"] == direct)
        assert mine["head"] >= stored and mine["read"] < stored, mine
        assert page(bob, 10, direct)[stored]["content"] == f"while you were away {token}"
        bob.send(kind="read_marker", conversation=direct, seq=stored)
        time.sleep(0.5)
        bob.send(kind="list_direct", request_id=11)
        relisted = answer(bob, 11)
        mine = next(entry for entry in relisted["directs"] if entry["conversation"] == direct)
        assert mine["read"] == stored, mine

        # Live: Alice's next message reaches Bob as text in the conversation.
        time.sleep(1.1)
        alice.send(kind="direct_text", conversation=direct, body=f"live {token}")
        live = bob.wait(lambda event: event["kind"] == "text" and event["body"] == f"live {token}")
        assert live["target"]["kind"] == "direct_conversation", live
        assert bytes(live["target"]["id"]).hex() == direct and live["seq"] is not None, live
        bob.close()
        alice.close()
    print("Edits, deletions, typing and direct conversations against nuscheld: ok")


if __name__ == "__main__":
    main()
