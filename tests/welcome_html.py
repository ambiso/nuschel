#!/usr/bin/env python3
"""Compare real stock-Murmur welcome and chat HTML in a private native desktop."""

import argparse
import json
import os
import signal
import subprocess
import tempfile
import time
import uuid
from pathlib import Path

from accessibility import items, start_registry
from gnome_session import GnomeSession
from local_murmur import Murmur, eventually, free_port
from mumble_client import Client, certificate_pin
from workers import exchange

WELCOME = (
    "Welcome to the <b>Nuschl</b> screenshot server.<br/>"
    "Second line: <a href='https://example.test/welcome'>Welcome help</a>"
    "<script>hiddenWelcome()</script><img src='https://tracker.invalid/pixel'>"
)
CONTROL = "Normal control <b>Nuschl</b><br/>Second control: <a href='https://example.test/chat'>Chat help</a>"


def run(args):
    args.output.mkdir(parents=True, exist_ok=True)
    (prepared,) = exchange(
        args.binaries / "nuschl-content-worker",
        [{"kind": "legacy_html", "html": WELCOME}],
    )
    (args.output / "worker.json").write_text(json.dumps(prepared, indent=2) + "\n")
    assert prepared["status"] == "prepared"
    assert "\ufffcb\ufffc" in prepared["value"]["source"]
    assert "<b>" not in prepared["value"]["document"]["blocks"][0]["text"]
    with tempfile.TemporaryDirectory(prefix="nuschl-welcome-") as temporary:
        root = Path(temporary)
        murmur = Murmur(args.murmur, root, free_port())
        murmur.ini.write_text(
            murmur.ini.read_text().replace(
                'welcometext="Welcome to the <b>Nuschl</b> screenshot server."',
                f'welcometext="{WELCOME}"',
            )
        )
        murmur.seed()
        murmur.start()
        shell = GnomeSession(
            args.gnome_shell, 1440, 900, log=(args.output / "gnome.log").open("w")
        )
        helpers = []
        app = bot = None
        try:
            shell.start()

            def spawn(command, env, log):
                process = subprocess.Popen(
                    command,
                    env=env,
                    stdout=(args.output / log).open("w"),
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
                helpers.append(process)
                return process

            registry = start_registry(args.at_spi, shell.app_environment(), spawn)
            pin = certificate_pin(
                args.binaries, "127.0.0.1", murmur.port, root / "probe"
            )
            bot = Client(
                args.binaries,
                "127.0.0.1",
                murmur.port,
                pin,
                "Control sender",
                root / "bot",
            )
            welcome = bot.wait(lambda event: event["kind"] == "welcome")
            assert welcome["body"] == WELCOME, welcome
            (args.output / "server-welcome.json").write_text(
                json.dumps(welcome, indent=2) + "\n"
            )
            profile = root / "profile"
            (profile / "config").mkdir(parents=True)
            bookmark = {
                "id": str(uuid.uuid4()),
                "name": "Welcome fixture",
                "host": "127.0.0.1",
                "port": murmur.port,
                "username": "Welcome reader",
                "certificate_sha256": pin,
            }
            (profile / "config/servers.json").write_text(json.dumps([bookmark]))
            (profile / "config/settings.json").write_text(
                json.dumps(
                    {
                        "appearance": {"theme": "dark"},
                        "audio": {"transmit": "push_to_talk"},
                    }
                )
            )
            env = shell.app_environment() | {
                "RUST_LOG": "nuschl=info",
                "PULSE_SERVER": f"unix:{root}/no-pulse",
                "PIPEWIRE_REMOTE": str(root / "no-pipewire"),
            }
            env.pop("NO_AT_BRIDGE", None)
            app = subprocess.Popen(
                [
                    str(args.binaries / "nuschl"),
                    "--profile-dir",
                    str(profile),
                    f"mumble://Welcome%20reader@127.0.0.1:{murmur.port}/",
                ],
                env=env,
                stdout=(args.output / "desktop.log").open("w"),
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            eventually(lambda: shell.window("Nuschl"), "native window missing")
            shell.place("Nuschl", 40, 40, 1280, 800)
            bot.wait(
                lambda event: (
                    event["kind"] == "user"
                    and event["user"]["name"] == "Welcome reader"
                )
            )
            bot.send(kind="html", channel=0, html=CONTROL)
            application = eventually(
                lambda: next(
                    (
                        node
                        for node in registry.children()
                        if node.name.lower() == "nuschl"
                    ),
                    None,
                ),
                "native accessibility application missing",
            )

            def contents():
                rows = []
                for node, role, name, _ in items(application):
                    text = ""
                    if role in ("label", "text", "list item"):
                        try:
                            text = node.call("Text", "GetText", "ii", "0", "-1")[0]
                        except RuntimeError:
                            pass
                    rows.append({"role": role, "name": name, "text": text})
                return rows

            def visible(rows):
                return "\n".join(
                    row["name"] + "\n" + row["text"] for row in rows
                ).replace("\ufffc", "")

            rows = eventually(
                lambda: (
                    (value if "Normal control Nuschl" in visible(value) else None)
                    if (value := contents())
                    else None
                ),
                "normal chat HTML control missing",
            )
            time.sleep(0.4)
            rows = contents()
            text = visible(rows)
            (args.output / "accessible.json").write_text(
                json.dumps(rows, indent=2) + "\n"
            )
            shell.screenshot(args.output / "welcome.png")
            assert "Normal control Nuschl\nSecond control: Chat help" in text, text
            if args.expect_markers:
                assert "bNuschl/b screenshot server." in text, text
            else:
                assert (
                    "Welcome to the Nuschl screenshot server.\nSecond line: Welcome help"
                    in text
                ), text
                assert (
                    "bNuschl/b" not in text
                    and "hiddenWelcome()" not in text
                    and "tracker.invalid" not in text
                ), text
            proof = {
                "passed": True,
                "baseline_markers": args.expect_markers,
                "stock_server_body_exact": True,
                "normal_chat_control": True,
                "welcome_document_rendered": not args.expect_markers,
                "public_urls_activated": False,
            }
            (args.output / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
            print(json.dumps(proof))
        finally:
            for process in ([app] if app else []) + helpers:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait(timeout=5)
            if bot:
                bot.close()
            shell.stop()
            murmur.stop()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--expect-markers", action="store_true")
    run(parser.parse_args())
