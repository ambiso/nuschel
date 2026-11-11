#!/usr/bin/env python3
"""Log in to a server that imported tests/fixtures/murmur-1.5.sql (`nuscheld
--import-murmur`) with the real client: Murmur's PBKDF2 and SHA-1 passwords work,
a wrong one does not, and the imported channel tree is there."""
import argparse
from pathlib import Path
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mumble_client import Client, certificate_pin  # noqa: E402

CHANNELS = {1: (0, "Lobby"), 2: (0, "Secret"), 3: (1, "Sub")}


def login(binaries, host, port, pin, root, name, password):
    secret = root / f"{name}-password"
    secret.write_text(password)
    return Client(binaries, host, port, pin, name, root / name, ["--password-file", str(secret)])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="nuschel-murmur-") as temporary:
        root = Path(temporary)
        pin = certificate_pin(args.binaries, args.host, args.port, root / "probe")
        alice = login(args.binaries, args.host, args.port, pin, root, "alice", "hunter2")
        try:
            alice.wait(lambda e: all(channel in alice.channels for channel in CHANNELS), timeout=10) \
                if not all(channel in alice.channels for channel in CHANNELS) else None
            for channel_id, (parent, name) in CHANNELS.items():
                channel = alice.channels[channel_id]
                assert channel["name"] == name and channel["parent"] == parent, (channel_id, channel)
            assert not any(channel["name"] == "bad>name" for channel in alice.channels.values())
        finally:
            alice.close()
        bob = login(args.binaries, args.host, args.port, pin, root, "bob", "password")
        bob.close()
        try:
            wrong = login(args.binaries, args.host, args.port, pin, root, "eve", "not her password")
        except AssertionError:
            pass
        else:
            wrong.close()
            raise AssertionError("a wrong password was accepted")
        # After the first login the hash is argon2id; the password still works.
        login(args.binaries, args.host, args.port, pin, root, "alice", "hunter2").close()
    print("Murmur's users log in with their passwords; the channel tree was imported.")


if __name__ == "__main__":
    main()
