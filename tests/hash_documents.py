#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Hash-only stock-Murmur documents in a disposable native desktop.

Reuse the owned GNOME/null-Pulse/Murmur harness unchanged. No host profile,
clipboard, audio device, external URL fetch or public server is involved.
"""
import argparse
import hashlib
import json
from pathlib import Path
import time

from accessibility import items
import document_link_regression as fixture
from local_murmur import eventually
from mumble_client import Client
import welcome_html
from hash_document_wire import capture

DESCRIPTION = (
    "<a href='https://hash.test/description'>Hash description</a><br>"
    "Description fetched on first channel selection. " + "description filler " * 20
)
COMMENT = "https://hash.test/comment Hash comment body " + "comment filler " * 20
REPLACEMENT = "https://hash.test/replacement Replacement body " + "replacement filler " * 20
HIDDEN = "https://hash.test/hidden Hidden replacement " + "hidden filler " * 20


def wire_comment(text):
    # Each controlled CLI input is plain ASCII with exactly one leading URL;
    # the native SetComment command linkifies that URL before sending HTML.
    url, tail = text.split(" ", 1)
    return f'<a href="{url}">{url}</a> {tail}'


def actions(args, root, shell, application, bot, murmur):
    shell.key("Shift_L")
    shell.place("Nuschl", 40, 40, 1280, 800)
    records = []

    def find(name, role=None):
        return next((node for node, kind, label, _ in items(application)
                     if label == name and (role is None or kind == role)), None)

    def link(name):
        return find(name, "role 88") or find(name, "link")

    def position(node):
        geometry = json.loads(shell.eval(
            "JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window).find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return {x:b.x,y:b.y};})())"))
        x, y, width, height = node.extents()
        return geometry["x"] + x + width // 2, geometry["y"] + y + height // 2

    def open_profile():
        user = eventually(lambda: next((node for node, role, name, _ in items(application)
            if role == "tree item" and name.startswith("Control sender")), None), "profile user missing")
        shell.click(*position(user), button=3)
        eventually(lambda: find("Profile and connection"), "profile action missing").press()

    def launches():
        path = args.output / "url-handler.jsonl"
        return [json.loads(line)[0] for line in path.read_text().splitlines()] if path.exists() else []

    def activate(node, url):
        before = launches()
        node.press()
        eventually(lambda: len(launches()) > len(before), "owned handler did not record link")
        assert launches()[len(before):] == [url]
        records.append(url)

    pin = bot.process.args[bot.process.args.index("--certificate-sha256") + 1]
    wire = capture(murmur.port, pin, bot.session, DESCRIPTION, wire_comment(COMMENT))
    (args.output / "stock-wire.json").write_text(json.dumps(wire, indent=2) + "\n")

    # A newly connected independent observer establishes that this exact stock
    # server supplies hashes rather than the long bodies before any blob request.
    observer = Client(args.binaries, "127.0.0.1", murmur.port,
                      pin,
                      "Hash observer", root / "observer")
    try:
        initial_channel = observer.channels[0].copy()
        initial_user = eventually(lambda: observer.users.get(bot.session), "hash-only observer user missing").copy()
        assert initial_channel["description"] == "" and initial_channel["description_hash"]
        assert initial_user["comment"] is None and initial_user["comment_hash"]
        observer.send(kind="blobs", comments=[bot.session], descriptions=[0])
        channel_body = observer.wait(lambda event: event["kind"] == "channel" and
            event["channel"]["id"] == 0 and event["channel"]["description"] == DESCRIPTION)["channel"]
        user_body = eventually(lambda: (value if (value := observer.users.get(bot.session)) and
            value.get("comment") == wire_comment(COMMENT) else None), "observer comment blob missing")
        for value, field, expected in [(channel_body, "description", DESCRIPTION), (user_body, "comment", wire_comment(COMMENT))]:
            assert value[field + "_hash"] == hashlib.sha1(expected.encode()).hexdigest()
        (args.output / "stock-hash-only.json").write_text(json.dumps({
            "initial_channel": initial_channel, "initial_user": initial_user,
            "body_channel": channel_body, "body_user": user_body,
            "description_bytes": len(DESCRIPTION.encode()), "comment_bytes": len(COMMENT.encode()),
        }, indent=2) + "\n")
    finally:
        observer.close()

    if args.expect_missing:
        time.sleep(1)
        assert link("Hash description") is None, "baseline description gap no longer reproduces"
        open_profile()
        time.sleep(1)
        assert link("https://hash.test/comment") is None, "baseline first-open comment gap no longer reproduces"
        # The baseline does fetch the body; reopening is what finally renders it.
        eventually(lambda: find("Close"), "profile close missing").press()
        open_profile()
        eventually(lambda: link("https://hash.test/comment"), "baseline comment did not render after reopen")
        shell.screenshot(args.output / "hash-documents-baseline.png")
        return {"passed": False, "expected_baseline_failure": True,
                "description_missing": True, "comment_requires_reopen": True,
                "stock_hash_only_confirmed": True}

    description = eventually(lambda: link("Hash description"), "selected hash-only description never rendered")
    activate(description, "https://hash.test/description")
    shell.screenshot(args.output / "hash-description.png")
    open_profile()
    comment = eventually(lambda: link("https://hash.test/comment"), "first-open hash-only comment never rendered")
    activate(comment, "https://hash.test/comment")
    shell.screenshot(args.output / "hash-comment-first-open.png")
    bot.send(kind="comment", text=REPLACEMENT)
    replacement = eventually(lambda: link("https://hash.test/replacement"), "open profile replacement required reopening")
    # AT-SPI paths identify the current accessible node, not a frozen callback.
    # AccessKit may reuse the path after replacing its label/target. The SDK
    # tests separately exercise late old-source renderer inputs and hidden work.
    assert link("https://hash.test/comment") is None
    path_reused = comment.path == replacement.path
    if path_reused:
        assert comment.name == replacement.name == "https://hash.test/replacement"
    activate(replacement, "https://hash.test/replacement")
    shell.screenshot(args.output / "hash-comment-replacement.png")
    eventually(lambda: find("Close"), "profile close missing").press()
    bot.send(kind="comment", text=HIDDEN)
    bot.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == bot.session
             and event["user"].get("comment_hash") == hashlib.sha1(wire_comment(HIDDEN).encode()).hexdigest())
    time.sleep(.3)
    assert link("https://hash.test/hidden") is None
    open_profile()
    activate(eventually(lambda: link("https://hash.test/hidden"), "reopened profile failed latest hash"),
             "https://hash.test/hidden")
    return {"passed": True, "stock_hash_only_confirmed": True,
            "description_first_selection": True, "comment_first_open": True,
            "open_profile_hash_replacement": True, "old_target_removed": True,
            "accessible_path_reused_for_current_node": path_reused,
            "hidden_then_explicit_reopen_latest": True, "activations": records}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--expect-missing", action="store_true", help="preserve expected baseline failure evidence")
    args = parser.parse_args()
    args.document_links = True
    args.expect_markers = False
    # Explicit fixture customization only; the imported harness and its owned
    # process cleanup remain unchanged. This process runs exactly one scenario.
    fixture.DESCRIPTION, fixture.COMMENT, fixture.run_actions = DESCRIPTION, COMMENT, actions
    welcome_html.run(args)


if __name__ == "__main__":
    main()
