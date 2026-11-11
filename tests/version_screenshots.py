#!/usr/bin/env python3
"""Screenshots of a Nuschel server's minimum-version policy (docs/extension.md,
"Minimum versions"), against a private loopback nuscheld, in a private headless
GNOME Shell (never the user's session).

    NUSCHEL_WL_TMP=/tmp python3 tests/version_screenshots.py <binaries> --output DIR

Three scenes, each with the server restarted under another policy: a recommended
version above this client's (a notice, and the same after "Dismiss"), a minimum above it
with a date three weeks ahead (a warning with the date), and a minimum in force
(the connection is refused with the version to update to). The versions are one
minor release above this build's.
"""
import argparse
import datetime
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gnome_session import GnomeSession  # noqa: E402
from local_nuscheld import Nuscheld, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402

# The recommendation's "Dismiss", in window screenshot pixels.
DISMISS_BUTTON = (1213, 151)


def register(binaries, port, pin, name, profile):
    """Register `name` with the profile's certificate, so the scenes show a member
    reading the server's history rather than a guest who may not."""
    client = Client(binaries, "127.0.0.1", port, pin, name, profile)
    client.send(kind="register")
    client.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == client.session
                and event["user"]["user_id"] is not None)
    client.send(kind="disconnect")
    client.process.wait(timeout=10)


def newer_version():
    manifest = tomllib.loads((Path(__file__).resolve().parent.parent / "Cargo.toml").read_text())
    major, minor, _ = (int(part) for part in manifest["workspace"]["package"]["version"].split("."))
    return f"{major}.{minor + 1}.0"


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
    newer = newer_version()
    deadline = (datetime.date.today() + datetime.timedelta(days=21)).isoformat()
    scenes = [
        ("version-01-recommended", f'[history]\nrecommended_client = "{newer}"\n'),
        ("version-02-required", f'[history]\nminimum_client = "{newer}"\nminimum_client_from = {deadline}\n'),
        ("version-03-refused", f'[history]\nminimum_client = "{newer}"\n'),
    ]
    with tempfile.TemporaryDirectory(prefix="nuschel-version-screens-") as temporary:
        root = Path(temporary)
        port = free_port()
        (root / "server").mkdir()
        server = Nuscheld(args.binaries, root / "server", port)
        server.seed()
        shell = GnomeSession(args.gnome_shell, width + 160, height + 100,
                             log=(args.output / "gnome-shell.log").open("w"))
        app = None
        pin = None
        profile = root / "profile"
        (profile / "config").mkdir(parents=True)
        (profile / "config/settings.json").write_text(json.dumps({"appearance": {"theme": args.theme}}))
        try:
            shell.start()
            for name, policy in scenes:
                server.extra = policy
                server.write_config(message_limit=1, message_burst=5)
                server.start()
                if pin is None:
                    pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
                    register(args.binaries, port, pin, "Andi", profile)
                    bookmark = {"id": str(uuid.uuid4()), "name": "Nuschel test server", "host": "127.0.0.1",
                                "port": port, "username": "Andi", "certificate_sha256": pin,
                                "cached_channels": []}
                    (profile / "config/servers.json").write_text(json.dumps([bookmark]))
                log = (args.output / f"{name}.log").open("w")
                app = subprocess.Popen([str(args.binaries / "nuschel"), "--profile-dir", str(profile),
                                        f"mumble://Andi@127.0.0.1:{port}/Lobby"],
                                       env=shell.app_environment() | {"RUST_LOG": "nuschel=info"},
                                       stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
                eventually(lambda: app.poll() is None and shell.window("Nuschel"), "Nuschel window did not appear")

                def placed():
                    shell.place("Nuschel", 40, 40, width, height)
                    time.sleep(0.3)
                    window = shell.window("Nuschel")
                    return window["width"] == width and window["height"] == height

                eventually(placed, "Nuschel window could not be placed")
                time.sleep(5.0)
                window = shell.window("Nuschel")
                shell.click(window["x"] - 12 + width // 2, window["y"] - 12 + 140)
                time.sleep(0.5)
                window = shell.window("Nuschel")
                path = shell.screenshot(args.output / f"{name}.png",
                                        crop=(window["x"] - 12, window["y"] - 12,
                                              window["width"] + 24, window["height"] + 24))
                print(path, flush=True)
                if name.endswith("recommended"):
                    shell.click(window["x"] - 12 + DISMISS_BUTTON[0], window["y"] - 12 + DISMISS_BUTTON[1])
                    time.sleep(0.8)
                    path = shell.screenshot(args.output / f"{name}-dismissed.png",
                                            crop=(window["x"] - 12, window["y"] - 12,
                                                  window["width"] + 24, window["height"] + 24))
                    print(path, flush=True)
                app.terminate()
                try:
                    app.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    app.kill()
                app = None
                server.stop()
        finally:
            if app:
                app.kill()
            shell.stop()
            server.stop()


if __name__ == "__main__":
    main()
