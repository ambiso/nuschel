#!/usr/bin/env python3
"""Screenshots of identity verification in a user's profile (docs/e2ee.md §4),
against a private loopback nuscheld with key transparency, in a private headless
GNOME Shell (never the user's session).

    NUSCHEL_WL_TMP=/tmp python3 tests/identity_screenshots.py <binaries> --output DIR

Ada is a scripted client with her own crypto worker; Andi is the desktop, registered
beforehand with the same profile. The scene opens Ada's profile (not verified, with
the safety number), marks her verified, then Ada returns with a new identity and the
profile shows the change.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gnome_session import GnomeSession  # noqa: E402
from local_nuscheld import Nuscheld, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402

# In #Development's member list: Ada's row, and the first entry of her menu.
ADA_ROW = (236, 301)
MENU_PROFILE = (290, 317)


def register(binaries, port, pin, name, profile):
    client = Client(binaries, "127.0.0.1", port, pin, name, profile)
    client.send(kind="register")
    client.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == client.session
                and event["user"]["user_id"] is not None)
    client.send(kind="disconnect")
    client.process.wait(timeout=10)


def ada(binaries, port, pin, root, identity):
    client = Client(binaries, "127.0.0.1", port, pin, "Ada", root / "ada", ["--identity-dir", str(identity)])
    client.wait(lambda event: event["kind"] == "identity_monitor"
                and event["outcome"].get("status") == "consistent" and event["outcome"]["current"], timeout=60)
    client.send(kind="join", channel=2)
    return client


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
    with tempfile.TemporaryDirectory(prefix="nuschel-identity-screens-") as temporary:
        root = Path(temporary)
        port = free_port()
        (root / "server").mkdir()
        server = Nuscheld(args.binaries, root / "server", port, extra="[kt]\npublish_interval = 1\n")
        server.seed()
        server.start()
        shell = GnomeSession(args.gnome_shell, width + 160, height + 100,
                             log=(args.output / "gnome-shell.log").open("w"))
        app = None
        bots = []
        try:
            shell.start()
            pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
            profile = root / "profile"
            (profile / "config").mkdir(parents=True)
            register(args.binaries, port, pin, "Andi", profile)
            register(args.binaries, port, pin, "Ada", root / "ada")
            bots.append(ada(args.binaries, port, pin, root, root / "ada-identity"))
            bookmark = {"id": str(uuid.uuid4()), "name": "Nuschel test server", "host": "127.0.0.1",
                        "port": port, "username": "Andi", "certificate_sha256": pin, "cached_channels": []}
            (profile / "config/servers.json").write_text(json.dumps([bookmark]))
            (profile / "config/settings.json").write_text(json.dumps({"appearance": {"theme": args.theme}}))
            log = (args.output / "desktop.log").open("w")
            app = subprocess.Popen([str(args.binaries / "nuschel"), "--profile-dir", str(profile),
                                    f"mumble://Andi@127.0.0.1:{port}/Lobby/Development"],
                                   env=shell.app_environment() | {"RUST_LOG": "nuschel=info"},
                                   stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            eventually(lambda: app.poll() is None and shell.window("Nuschel"), "Nuschel window did not appear")

            def placed():
                # The window may map maximised first; place it until it has the size the
                # coordinates below assume.
                shell.place("Nuschel", 40, 40, width, height)
                time.sleep(0.3)
                window = shell.window("Nuschel")
                return window["width"] == width and window["height"] == height

            eventually(placed, "Nuschel window could not be placed")
            bots[0].wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                         and e["user"]["channel"] == 2, timeout=30)
            # Andi's identity is created and published on connect.
            time.sleep(6.0)

            def shot(name):
                window = shell.window("Nuschel")
                path = shell.screenshot(args.output / f"{name}.png",
                                        crop=(window["x"] - 12, window["y"] - 12,
                                              window["width"] + 24, window["height"] + 24))
                print(path, flush=True)

            def click(x, y, button=1):
                window = shell.window("Nuschel")
                shell.click(window["x"] - 12 + x, window["y"] - 12 + y, button=button)

            click(width // 2, 140)  # the first click after mapping can be swallowed
            time.sleep(0.4)
            shot("identity-01-connected")

            def open_profile(name):
                click(*ADA_ROW, button=3)
                time.sleep(0.8)
                click(*MENU_PROFILE)
                time.sleep(3.0)  # the lookup, its proof and the record are checked
                shot(name)

            open_profile("identity-02-unverified")
            # "Mark as verified", found on the screenshot above.
            click(*VERIFY_BUTTON)
            time.sleep(1.5)
            shot("identity-03-verified")
            shell.key("Escape")
            time.sleep(0.4)

            # Ada comes back with a new identity.
            bots[0].send(kind="disconnect")
            bots[0].process.wait(timeout=10)
            bots[0] = ada(args.binaries, port, pin, root, root / "ada-new-identity")
            time.sleep(1.0)
            open_profile("identity-04-changed")
            shell.key("Escape")
            time.sleep(0.6)
            shot("identity-05-notice")
        finally:
            for bot in bots:
                bot.process.kill()
            if app:
                app.terminate()
                try:
                    app.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    app.kill()
            shell.stop()
            server.stop()


# The profile panel's "Mark as verified" button, in window screenshot pixels.
VERIFY_BUTTON = (495, 470)

if __name__ == "__main__":
    main()
