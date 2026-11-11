#!/usr/bin/env python3
"""Adversarial peer for transfer.py; mailbox and advertised hints are loopback."""
import json
from pathlib import Path
import sys

from twisted.internet import defer, task
from wormhole import create


@defer.inlineCallbacks
def run(reactor):
    rendezvous, code, canary, result = sys.argv[1:]
    assert rendezvous.startswith("ws://127.0.0.1:")
    wormhole = create("lothar.com/wormhole/text-or-file-xfer", rendezvous, reactor,
                      versions={"abilities": ["transfer-v1"]})
    wormhole.set_code(code)
    yield wormhole.get_verifier()
    yield wormhole.get_versions()
    transit = json.loads((yield wormhole.get_message()))["transit"]
    assert transit["abilities-v1"] == [{"type": "relay-v1"}], transit
    assert all(hint["type"] == "relay-v1" for hint in transit["hints-v1"]), transit
    assert "offer" in json.loads((yield wormhole.get_message()))
    hint = {"type": "direct-tcp-v1", "hostname": "127.0.0.1", "port": int(canary)}
    wormhole.send_message(json.dumps({"transit": {
        "abilities-v1": [{"type": "direct-tcp-v1"}, {"type": "relay-v1"}],
        "hints-v1": [hint, {"type": "relay-v1", "hints": [hint]}],
    }}).encode())
    wormhole.send_message(json.dumps({"answer": {"file_ack": "ok"}}).encode())
    Path(result).write_text(json.dumps(transit))
    # Keep the authenticated control channel alive while the sender attempts
    # transit. This peer deliberately never dials either advertised address.
    yield task.deferLater(reactor, 8, lambda: None)
    yield wormhole.close()


task.react(run)
