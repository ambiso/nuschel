#!/usr/bin/env python3
"""The desktop's conversations on a Nuschl server, end to end: edits,
deletions, typing and a direct conversation with server history, against a
private loopback nuschld in a private headless GNOME Shell (never the user's
session). Ada is a scripted nuschl-cli client; Andi is the desktop.

    NUSCHL_WL_TMP=/tmp/... python3 tests/conversation_screenshots.py <binaries> --output DIR

Checks along the way (each scene is also a screenshot to look at):

- Ada's edit and deletion from before Andi connected arrive in the history
  page; her live edit and deletion change the open timeline;
- Up in Andi's empty composer edits his last message and Enter saves it:
  Ada receives the edit; emptying an edit and confirming deletes it: Ada
  receives the deletion;
- Andi typing reaches Ada; Ada typing shows under Andi's composer;
- Ada's direct messages from while Andi was offline wait for him: the direct
  conversation is listed unread, opens with the server's history and takes
  his reply as a stored message;
- the keyboard reaches a link in a message (Tab), Enter opens it as a click
  would (an unsaved server's form first), and a letter typed in the timeline
  goes to the composer;
- the desktop's local history keeps what the server's pages brought, by seq:
  the edited text, the tombstone without text, and the direct messages.
"""
import argparse
import json
from pathlib import Path
import sqlite3
import subprocess
import sys
import tempfile
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gnome_session import GnomeSession  # noqa: E402
from local_nuschld import Nuschld, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402

EDIT, DELETE = 1, 2
DEVELOPMENT = 2


def register(binaries, port, pin, name, profile):
    client = Client(binaries, "127.0.0.1", port, pin, name, profile)
    client.send(kind="register")
    user = client.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == client.session
                       and event["user"]["user_id"] is not None)
    client.close()
    return user["user"]["user_id"]


def conversation(client, channel):
    for event in list(client.events):
        if event["kind"] == "channel_info" and event["channel"] == channel:
            return event["conversation"]
    return client.wait(lambda event: event["kind"] == "channel_info" and event["channel"] == channel)["conversation"]


def answer(client, request_id):
    return client.wait(lambda event: event.get("request_id") == request_id
                       and event["kind"] in ("chat_ack", "nuschl_error", "direct_opened"))


class Ada:
    """The other side: a nuschl-cli client in #Development."""

    def __init__(self, binaries, port, pin, root):
        self.client = Client(binaries, "127.0.0.1", port, pin, "Ada", root / "ada")
        self.client.send(kind="join", channel=DEVELOPMENT)
        self.client.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == self.client.session
                         and event["user"]["channel"] == DEVELOPMENT)
        self.development = conversation(self.client, DEVELOPMENT)
        self.request = 100

    def say(self, body):
        time.sleep(1.1)  # Murmur's text bucket: one a second after a burst
        self.client.send(kind="text", channel=DEVELOPMENT, body=body)
        acked = self.client.wait(lambda event: event["kind"] in ("chat_acked", "chat_failed"))
        assert acked["kind"] == "chat_acked", acked
        return acked["seq"]

    def change(self, target, content=None):
        time.sleep(1.1)
        self.request += 1
        if content is None:
            self.client.send(kind="chat_delete", request_id=self.request, conversation=self.development,
                             target=target)
        else:
            self.client.send(kind="chat_edit", request_id=self.request, conversation=self.development,
                             target=target, content=content)
        assert answer(self.client, self.request)["kind"] == "chat_ack"

    def direct(self, user_id):
        self.request += 1
        self.client.send(kind="open_direct", request_id=self.request, user_id=user_id)
        opened = answer(self.client, self.request)
        assert opened["kind"] == "direct_opened", opened
        return opened["direct"]["conversation"]

    def tell(self, direct, body):
        time.sleep(1.1)
        self.client.send(kind="direct_text", conversation=direct, body=body)
        acked = self.client.wait(lambda event: event["kind"] in ("chat_acked", "chat_failed"))
        assert acked["kind"] == "chat_acked", acked


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--size", default="1280x800")
    parser.add_argument("--theme", choices=["system", "light", "dark"], default="dark")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    width, height = map(int, args.size.split("x"))
    with tempfile.TemporaryDirectory(prefix="nuschl-conversation-screens-") as temporary:
        root = Path(temporary)
        port = free_port()
        (root / "server").mkdir()
        server = Nuschld(args.binaries, root / "server", port)
        server.seed()
        server.start()
        shell = GnomeSession(args.gnome_shell, width + 160, height + 100,
                             log=(args.output / "gnome-shell.log").open("w"))
        app = None
        ada = None
        try:
            shell.start()
            pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
            profile = root / "profile"
            (profile / "config").mkdir(parents=True)
            andi_id = register(args.binaries, port, pin, "Andi", profile)
            register(args.binaries, port, pin, "Ada", root / "ada")

            # Before Andi connects: history with an edit and a deletion, and
            # direct messages that wait for him.
            ada = Ada(args.binaries, port, pin, root)
            ada.say("Morning! The build is green again.")
            typo = ada.say("I moved the release notse to the wiki.")
            ada.change(typo, "I moved the release notes to the wiki.")
            oops = ada.say("Wrong channel, sorry.")
            ada.change(oops)
            ada.say("Who reviews the storage change?")
            direct = ada.direct(andi_id)
            ada.tell(direct, "Hi Andi, are you around this afternoon?")
            ada.tell(direct, "I left the notes for the release in #Development.")

            bookmark = {"id": str(uuid.uuid4()), "name": "Nuschl test server", "host": "127.0.0.1",
                        "port": port, "username": "Andi", "certificate_sha256": pin}
            (profile / "config/servers.json").write_text(json.dumps([bookmark]))
            (profile / "config/settings.json").write_text(json.dumps({"appearance": {"theme": args.theme}}))
            log = (args.output / "desktop.log").open("w")
            app = subprocess.Popen([str(args.binaries / "nuschl"), "--profile-dir", str(profile),
                                    f"mumble://Andi@127.0.0.1:{port}/Lobby/Development"],
                                   env=shell.app_environment() | {"RUST_LOG": "nuschl=info"},
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            eventually(lambda: app.poll() is None and shell.window("Nuschl"), "Nuschl window did not appear")

            def placed():
                shell.place("Nuschl", 40, 40, width, height)
                time.sleep(0.3)
                window = shell.window("Nuschl")
                return window["width"] == width and window["height"] == height

            eventually(placed, "Nuschl window could not be placed")
            andi = ada.client.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                                   and e["user"]["channel"] == DEVELOPMENT, timeout=30)["user"]["session"]
            time.sleep(3.0)

            def shot(name):
                window = shell.window("Nuschl")
                path = shell.screenshot(args.output / f"{name}.png",
                                        crop=(window["x"] - 12, window["y"] - 12,
                                              window["width"] + 24, window["height"] + 24))
                print(path, flush=True)

            def at(x, y):
                window = shell.window("Nuschl")
                return window["x"] - 12 + x, window["y"] - 12 + y

            def click(x, y, button=1):
                shell.click(*at(x, y), button=button)

            composer = (width // 2 + 100, height - 62)
            click(*composer)  # the first click after mapping can be swallowed
            time.sleep(0.4)
            shot("conversation-01-history")

            # Live: an edit and a deletion while Andi watches.
            live = ada.say("Lunch at 12:30 in the big rooom?")
            ada.change(live, "Lunch at 12:30 in the big room?")
            gone = ada.say("Ignore that, wrong window.")
            ada.change(gone)
            time.sleep(1.0)
            shot("conversation-02-live-changes")

            # Andi sends, then edits his last message with Up.
            click(*composer)
            time.sleep(0.3)
            shell.type("I can review it after lunch")
            time.sleep(0.2)
            shot("conversation-02b-composing")
            typing = ada.client.wait(lambda e: e["kind"] == "typing" and e["session"] == andi, timeout=10)
            assert typing["conversation"] == ada.development, typing
            shell.key("Return")
            sent = ada.client.wait(lambda e: e["kind"] == "text" and e["body"] == "I can review it after lunch")
            time.sleep(1.5)
            shell.key("Up")
            time.sleep(0.5)
            shell.key("End")
            shell.type(", promise")
            time.sleep(0.3)
            shot("conversation-03-editing")
            shell.key("Return")
            edit = ada.client.wait(lambda e: e["kind"] == "chat" and e["message"]["kind"] == EDIT
                                   and e["message"]["target"] == sent["seq"], timeout=10)
            assert edit["message"]["content"] == "I can review it after lunch, promise", edit
            time.sleep(1.0)

            # Ada types: the line under the composer says so.
            for _ in range(2):
                ada.client.send(kind="typing", conversation=ada.development)
                time.sleep(3.1)
            ada.client.send(kind="typing", conversation=ada.development)
            time.sleep(0.6)
            shot("conversation-04-typing")

            # Andi deletes his message: Up, empty the edit, Enter, confirm.
            time.sleep(1.2)
            click(*composer)
            shell.key("Up")
            time.sleep(0.3)
            shell.press("Control_L", "a")
            shell.key("BackSpace")
            shell.key("Return")
            time.sleep(0.6)
            shot("conversation-05-confirm-delete")
            shell.key("Tab")
            shell.key("Tab")
            shell.key("Return")
            deleted = ada.client.wait(lambda e: e["kind"] == "chat" and e["message"]["kind"] == DELETE
                                      and e["message"]["target"] == sent["seq"], timeout=10)
            assert deleted["message"]["deleted"], deleted
            time.sleep(1.0)
            shot("conversation-06-own-deleted")

            # The direct conversation from while Andi was offline.
            shell.press("Control_L", "k")
            time.sleep(0.5)
            shell.type("Ada")
            time.sleep(0.5)
            shell.key("Return")
            time.sleep(2.0)
            shot("conversation-07-direct-history")
            click(*composer)
            shell.type("Thanks, I will look at them later.")
            shell.key("Return")
            reply = ada.client.wait(lambda e: e["kind"] == "text" and e["body"] == "Thanks, I will look at them later.",
                                    timeout=10)
            assert reply["target"]["kind"] == "direct_conversation", reply
            assert bytes(reply["target"]["id"]).hex() == direct and reply["seq"], reply
            time.sleep(1.0)
            shot("conversation-08-direct-reply")

            # The keyboard reaches links: Shift+Tab to the newest message, Tab
            # to its link (named under the timeline), Enter opens it as a
            # click would: a server that is not saved opens the form first.
            ada.tell(direct, "Join us there: mumble://voice.example.invalid/Lobby")
            time.sleep(1.0)
            click(*composer)
            # Back past the paperclip to the timeline, on its newest message.
            for _ in range(2):
                shell.chord("Shift_L", "Tab")
                time.sleep(0.3)
            shell.key("Tab")
            time.sleep(0.5)
            shot("conversation-09-keyboard-link")
            shell.key("Return")
            time.sleep(1.0)
            shot("conversation-10-link-confirm")
            shell.key("Escape")
            time.sleep(0.5)
            # A letter typed in the timeline goes to the composer.
            for _ in range(2):
                shell.chord("Shift_L", "Tab")
                time.sleep(0.3)
            shell.type("Sounds good")
            shell.key("Return")
            ada.client.wait(lambda e: e["kind"] == "text" and e["body"] == "Sounds good", timeout=10)

            # What the pages brought is in local history, by seq.
            database = next(profile.rglob("history.sqlite3"))
            rows = sqlite3.connect(database).execute(
                "SELECT channel, seq, body, edited_ms, deleted_ms FROM messages WHERE seq IS NOT NULL").fetchall()
            by_body = {body: (channel, seq, edited, deleted) for channel, seq, body, edited, deleted in rows}
            edited = by_body.get("I moved the release notes to the wiki.")
            assert edited and edited[2] is not None, rows
            tombstones = [row for row in rows if row[4] is not None and row[0] == str(DEVELOPMENT)]
            assert tombstones and all(row[2] == "" for row in tombstones), rows
            assert all("Wrong channel" not in row[2] and "sorry" not in row[2] for row in rows), rows
            assert any(row[0] == f"conversation:{direct}" and "around this afternoon" in row[2] for row in rows), rows
            seqs = [(row[0], row[1]) for row in rows]
            assert len(seqs) == len(set(seqs)), f"kept twice: {rows}"

            # Search finds what the pages kept; after a reconnect the result,
            # older than the latest page, opens from the server's history
            # around it (by seq).
            # One at a time: a burst of sends stalls (TODO.md).
            for n in range(1, 51):
                ada.say(f"Filler message {n}")
            ada.client.process.kill()
            server.stop()
            server.start()
            ada = Ada(args.binaries, port, pin, root)
            ada.client.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi", timeout=60)
            time.sleep(3.0)
            shell.press("Control_L", "k")
            time.sleep(0.5)
            shell.type("Development")
            time.sleep(0.5)
            shell.key("Return")
            time.sleep(2.0)
            shell.press("Control_L", "f")
            time.sleep(0.8)
            shell.type("release notes")
            time.sleep(1.5)
            shot("conversation-11-search")
            click(600, 300)
            time.sleep(2.0)
            shot("conversation-12-search-jumped")
            print("Conversations on a Nuschl server: ok", flush=True)
        finally:
            if ada:
                ada.client.process.kill()
            if app:
                app.terminate()
                try:
                    app.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    app.kill()
            shell.stop()
            server.stop()


if __name__ == "__main__":
    main()
