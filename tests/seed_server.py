#!/usr/bin/env python3
"""Create the stock fixture's channels on a server as SuperUser, over the protocol.

The counterpart of mumble_fixture.py (which writes a stopped Murmur's database):
works against any server, so the VM seeds nuschld this way. Channel ids must
come out as in the fixture (Lobby = 1, Development = 2, ...).
"""
import argparse
from pathlib import Path
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mumble_client import Client, certificate_pin  # noqa: E402

FIXTURE = [(1, 0, "Lobby"), (2, 1, "Development")]


def verify(binaries, host, port, channels=FIXTURE):
    """The fixture channels exist (after a restart, say), seen by an ordinary user."""
    with tempfile.TemporaryDirectory(prefix="nuschl-verify-") as temporary:
        root = Path(temporary)
        pin = certificate_pin(binaries, host, port, root / "probe")
        user = Client(binaries, host, port, pin, "Verifier", root / "user")
        try:
            user.wait(lambda e: e["kind"] == "channel" and e["channel"]["id"] == channels[-1][0], timeout=5) \
                if channels[-1][0] not in user.channels else None
            for channel_id, parent, name in channels:
                channel = user.channels.get(channel_id)
                assert channel and channel["name"] == name and channel["parent"] == parent, (channel_id, channel)
        finally:
            user.close()


def seed(binaries, host, port, password_file, channels=FIXTURE, profile=None):
    with tempfile.TemporaryDirectory(prefix="nuschl-seed-") as temporary:
        root = Path(profile or temporary)
        pin = certificate_pin(binaries, host, port, root / "probe")
        admin = Client(binaries, host, port, pin, "SuperUser", root / "admin", ["--password-file", str(password_file)])
        try:
            for channel_id, parent, name in channels:
                admin.send(kind="create_channel", parent=parent, name=name, temporary=False)
                event = admin.wait(lambda e: e["kind"] == "permission_denied"
                                   or (e["kind"] == "channel" and e["channel"]["name"] == name))
                assert event["kind"] == "channel" and event["channel"]["id"] == channel_id, event
        finally:
            admin.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    parser.add_argument("--password-file", type=Path)
    parser.add_argument("--verify", action="store_true", help="only check that the channels exist")
    args = parser.parse_args()
    if args.verify:
        verify(args.binaries, args.host, args.port)
        print("The fixture channels exist.")
    else:
        seed(args.binaries, args.host, args.port, args.password_file)
        print("Seeded the fixture channels.")
