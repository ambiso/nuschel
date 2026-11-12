#!/usr/bin/env python3
"""Private rendered reproduction for the report "Markdown doesn't render in chat".

The same Markdown-looking message reaches the native desktop three ways, and
every view is screenshotted in a private headless GNOME Shell (never the
user's session) against a private loopback server:

- typed: the desktop user types it into the composer and presses Enter (the
  local echo, `html: false`, rendered with `ContentRequest::Render`);
- received: a scripted nuschl-cli sends it as text (on stock Murmur this is
  the escaped HTML `legacy_body` makes; on nuschld a `TEXT` chat message),
  and as stock-Mumble-style HTML (`<b>`, `<i>`, `<code>`, `<ul>`, `<pre>`);
- restored: the desktop is stopped and started again on the same profile, and
  the history comes back from storage (Murmur: local history only; nuschld:
  the server's pages, prepared by format, and the local cache).

It records the content worker's documents, the accessible text and the
screenshots, for a person to look at, and checks what Markdown should do
(docs/desktop.md, "Markdown"): in the accessible text of the typed message and
of the one received as text, live and restored, no delimiter is left (`**`,
`*`, a backtick, `[`, `](`, `#`, a list's `-`), the blank lines typed are kept
(two after the heading, one between the loose list's items, none after the
fence), and the restored messages read as the live ones. The stock-HTML
message reads as it did before Markdown. `--ui-font`/`--mono-font` set
concrete families instead of the defaults ("sans-serif", "monospace") for
control runs: whether bold, italic and monospace spans the worker produced
are visible depends on them.

    NUSCHL_WL_TMP=... python3 tests/markdown_repro.py --binaries BIN --at-spi AT_SPI \
        --pulse-bin PULSE/bin --gnome-shell SHELL --output DIR (--murmur MURMUR | --nuschld) \
        [--ui-font "DejaVu Sans" --mono-font "DejaVu Sans Mono"]
"""

import argparse
import json
import os
import signal
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import items, snapshot, start_registry  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402
from workers import exchange  # noqa: E402

DEVELOPMENT = 2
LINES = [
    "# Heading",
    "",
    "",
    "**bold** *italic* `inline code` [a link](https://example.test/{tag}) $x^2$",
    "- item one",
    "",
    "- item two",
    "```rust",
    "fn main() {{}}",
    "```",
    "after the fence",
]
# The message as shown and read out: no delimiters, the typed lines.
SHOWN = ("{tag}:\nHeading\n\n\nbold italic inline code a link $x^2$\n• item one\n\n"
         "• item two\nfn main() {{}}\nafter the fence, 1 link")
STOCK_SHOWN = ("stock-html: bold italic inline code a link $x^2$\n• item one\n• item two\n"
               "fn main() {}\nafter the fence, 1 link")


def markdown(tag):
    return f"{tag}:\n" + "\n".join(LINES).format(tag=tag)


def check_rows(rows, stage):
    """Verdicts on the accessible names of the three messages."""
    names = [row["name"] for row in rows]
    verdicts = {}
    for author, tag in (("Andi", "typed"), ("Mark Bot", "plain")):
        name = next((name for name in names if name.startswith(f"{author}: {tag}:")), "")
        body = name[len(author) + 2:]
        expected = SHOWN.format(tag=tag)
        verdicts[f"{stage} {tag}: shown as {expected!r}"] = body.startswith(expected)
        verdicts[f"{stage} {tag}: no delimiter left"] = bool(body) and not any(
            mark in body for mark in ("**", "`", "](", "[a link", "# ", "- item", "*italic"))
    stock = next((name for name in names if name.startswith("Mark Bot: stock-html:")), "")
    verdicts[f"{stage} stock-html: reads as before"] = stock.startswith(f"Mark Bot: {STOCK_SHOWN}")
    return verdicts


# What a stock Mumble 1.4+ chat bar makes of Markdown is HTML with these tags;
# representative, not captured from the stock GUI.
STOCK_HTML = (
    "stock-html: <b>bold</b> <i>italic</i> <code>inline code</code> "
    '<a href="https://example.test/stock">a link</a> $x^2$<br/>'
    "<ul><li>item one</li><li>item two</li></ul><pre>fn main() {}</pre>after the fence"
)


def worker_probe(binaries, output):
    escaped = (
        markdown("plain")
        .replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace("\n", "<br/>")
    )
    requests = {
        "render(typed markdown)": {"kind": "render", "text": markdown("typed")},
        "legacy_html(escaped text from a Nuschl sender on Murmur)": {
            "kind": "legacy_html",
            "html": escaped,
        },
        "legacy_html(stock-style HTML)": {"kind": "legacy_html", "html": STOCK_HTML},
    }
    results = exchange(binaries / "nuschl-content-worker", list(requests.values()))
    probe = dict(zip(requests, results))
    (output / "worker.json").write_text(json.dumps(probe, indent=2, ensure_ascii=False) + "\n")
    return probe


def run(args):
    args.output.mkdir(parents=True, exist_ok=True)
    worker_probe(args.binaries, args.output)
    notes = {"server": "nuschld" if args.nuschld else "murmur"}
    with tempfile.TemporaryDirectory(prefix="nuschl-markdown-") as temporary:
        root = Path(temporary)
        port = free_port()
        if args.nuschld:
            from local_nuschld import Nuschld

            (root / "server").mkdir()
            # The desktop is a guest: guests read no server history by default.
            server = Nuschld(args.binaries, root / "server", port, extra='[history]\nguests = "full"')
            server.seed()
            server.write_config(message_limit=100, message_burst=100)
            server.start()
        else:
            (root / "server").mkdir()
            server = Murmur(args.murmur, root / "server", port)
            server.ini.write_text(server.ini.read_text() + "messagelimit=1000\nmessageburst=1000\n")
            server.seed()
            server.start()
        shell = GnomeSession(args.gnome_shell, 1600, 1200, log=(args.output / "gnome.log").open("w"))
        helpers = []
        app = None
        bot = None
        try:
            shell.start()

            def spawn(command, env, log):
                process = subprocess.Popen(
                    command,
                    env=env,
                    stdout=(args.output / log).open("a"),
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
                helpers.append(process)
                return process

            base_env = shell.app_environment()
            base_env.pop("NO_AT_BRIDGE", None)
            registry = start_registry(args.at_spi, base_env, spawn)
            pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
            bot = Client(
                args.binaries,
                "127.0.0.1",
                port,
                pin,
                "Mark Bot",
                root / "bot",
                ["--message-limit", "1000", "--message-burst", "1000"],
            )
            bot.send(kind="join", channel=DEVELOPMENT)
            bot.wait(
                lambda e: e["kind"] == "user"
                and e["user"]["session"] == bot.session
                and e["user"]["channel"] == DEVELOPMENT
            )
            profile = root / "profile"
            (profile / "config").mkdir(parents=True)
            (profile / "config/servers.json").write_text(
                json.dumps(
                    [
                        {
                            "id": str(uuid.uuid4()),
                            "name": "Markdown fixture",
                            "host": "127.0.0.1",
                            "port": port,
                            "username": "Andi",
                            "certificate_sha256": pin,
                        }
                    ]
                )
            )
            appearance = {"theme": "dark"}
            # Control runs: concrete families instead of the defaults
            # ("sans-serif" and "monospace", theme.rs).
            if args.ui_font:
                appearance["ui_font"] = args.ui_font
            if args.mono_font:
                appearance["mono_font"] = args.mono_font
            notes["appearance"] = appearance
            (profile / "config/settings.json").write_text(
                json.dumps({"appearance": appearance, "audio": {"transmit": "push_to_talk"}})
            )
            # Private null audio only: never the host's microphone or speakers.
            pulse = shell.runtime / "pulse"
            pulse.mkdir(mode=0o700, exist_ok=True)
            script = pulse / "default.pa"
            script.write_text(
                f"load-module module-native-protocol-unix socket={pulse / 'native'} auth-anonymous=1\n"
                "load-module module-null-sink sink_name=speakers rate=48000\n"
                "load-module module-null-sink sink_name=microphone rate=48000\n"
                "set-default-sink speakers\nset-default-source microphone.monitor\n"
            )
            spawn(
                [
                    str(args.pulse_bin / "pulseaudio"),
                    "--daemonize=no",
                    "--use-pid-file=no",
                    "--exit-idle-time=-1",
                    "-n",
                    f"--file={script}",
                ],
                shell.environment()
                | {
                    "PULSE_RUNTIME_PATH": str(pulse),
                    "PULSE_STATE_PATH": str(pulse / "state"),
                    "DBUS_SESSION_BUS_ADDRESS": f"unix:path={root}/no-pulse-bus",
                },
                "pulse.log",
            )
            eventually(lambda: (pulse / "native").exists(), "private null audio missing")
            env = base_env | {
                "RUST_LOG": "nuschl=info",
                "PULSE_SERVER": f"unix:{pulse}/native",
                "NUSCHL_AUDIO_BACKEND": "pulseaudio",
                "PULSE_SINK": "speakers",
                "PULSE_SOURCE": "microphone.monitor",
                "PIPEWIRE_REMOTE": str(root / "no-pipewire"),
            }

            def launch(log):
                process = subprocess.Popen(
                    [
                        str(args.binaries / "nuschl"),
                        "--profile-dir",
                        str(profile),
                        f"mumble://Andi@127.0.0.1:{port}/Lobby/Development",
                    ],
                    env=env,
                    stdout=(args.output / log).open("a"),
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
                eventually(lambda: shell.window("Nuschl"), "native window missing", timeout=45)
                shell.place("Nuschl", 20, 20, 1540, 1140)
                return process

            def application():
                assert app.poll() is None, (args.output / "desktop.log").read_text()[-3000:]
                return next((n for n in registry.children() if n.name.lower() == "nuschl"), None)

            def contents(nuschl):
                rows = []
                for node, role, name, _ in items(nuschl):
                    if role != "list item":
                        continue
                    text = ""
                    try:
                        text = node.call("Text", "GetText", "ii", "0", "-1")[0]
                    except RuntimeError:
                        pass
                    rows.append({"name": name, "text": text})
                return rows

            def wait_rows(nuschl, needles, what):
                def found():
                    rows = contents(nuschl)
                    joined = "\n".join(r["name"] + "\n" + r["text"] for r in rows)
                    return rows if all(needle in joined for needle in needles) else None

                return eventually(found, what, timeout=45)

            def window_shot(name):
                time.sleep(1.0)
                window = shell.window("Nuschl")
                crop = (window["x"], window["y"], window["width"], window["height"])
                return shell.screenshot(args.output / name, crop)

            app = launch("desktop.log")
            desktop = bot.wait(
                lambda e: e["kind"] == "user"
                and e["user"]["name"] == "Andi"
                and e["user"]["channel"] == DEVELOPMENT,
                timeout=60,
            )["user"]
            nuschl = eventually(application, "desktop not in the accessibility registry", timeout=45)
            eventually(
                lambda: any(role == "entry" and name == "Message" for _, _, role, name in snapshot(nuschl)),
                "composer missing",
                timeout=30,
            )

            # Received: as text (Nuschl sender), then as stock-style HTML.
            bot.send(kind="text", channel=DEVELOPMENT, body=markdown("plain"))
            if args.nuschld:
                acked = bot.wait(lambda e: e["kind"] in ("chat_acked", "chat_failed"))
                notes["bot_text_ack"] = acked
            time.sleep(1.2)
            bot.send(kind="html", channel=DEVELOPMENT, html=STOCK_HTML)
            time.sleep(1.2)

            # Typed: line by line, Shift+Enter between lines, Enter to send.
            composer = next(
                node for _, node, role, name in snapshot(nuschl) if role == "entry" and name == "Message"
            )
            composer.call("Component", "GrabFocus")
            time.sleep(0.5)
            typed = markdown("typed").split("\n")
            for index, line in enumerate(typed):
                shell.type(line)
                time.sleep(0.15)
                if index + 1 < len(typed):
                    shell.press("Shift_L", "Return")
                    time.sleep(0.15)
            time.sleep(0.5)
            window_shot("composer.png")
            shell.key("Return")
            received = bot.wait(
                lambda e: e["kind"] == "text" and not e["outgoing"] and "typed:" in e["body"],
                timeout=20,
            )
            notes["bot_received_typed"] = received
            notes["typed_exact"] = received["body"]
            rows = wait_rows(
                nuschl,
                ["typed:", "plain:", "stock-html: "],
                "the three messages did not reach the timeline",
            )
            time.sleep(1.5)
            rows = contents(nuschl)
            (args.output / "live-accessible.json").write_text(json.dumps(rows, indent=2, ensure_ascii=False) + "\n")
            live_rows = rows
            verdicts = check_rows(rows, "live")
            window_shot("live.png")

            # Restored: stop and start again on the same profile.
            os.killpg(app.pid, signal.SIGTERM)
            try:
                app.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(app.pid, signal.SIGKILL)
                app.wait(timeout=5)
            notes["desktop_exit"] = app.returncode
            bot.wait(lambda e: e["kind"] == "user_removed" and e["session"] == desktop["session"], timeout=20)
            eventually(lambda: shell.window("Nuschl") is None, "window stayed", timeout=20)
            eventually(
                lambda: not any(n.name.lower() == "nuschl" for n in registry.children()),
                "old desktop still registered",
                timeout=20,
            )
            app = launch("desktop-restart.log")
            nuschl = eventually(application, "restarted desktop not registered", timeout=45)
            try:
                wait_rows(
                    nuschl,
                    ["typed:", "plain:", "stock-html: "],
                    "history was not restored",
                )
                notes["restored_all_three"] = True
            except AssertionError:
                # Recorded, not fatal: the screenshot shows what did come back.
                notes["restored_all_three"] = False
            time.sleep(2.0)
            rows = contents(nuschl)
            (args.output / "restored-accessible.json").write_text(
                json.dumps(rows, indent=2, ensure_ascii=False) + "\n"
            )
            verdicts |= check_rows(rows, "restored")

            def message_names(found):
                return sorted(row["name"].rsplit(", Today", 1)[0] for row in found
                              if any(tag in row["name"] for tag in ("typed:", "plain:", "stock-html:")))

            verdicts["restored messages read as the live ones"] = message_names(live_rows) == message_names(rows)
            window_shot("restored.png")
            notes["checks"] = verdicts
            (args.output / "notes.json").write_text(json.dumps(notes, indent=2, ensure_ascii=False) + "\n")
            print(json.dumps({"done": True, "output": str(args.output), "checks": verdicts},
                             indent=1, ensure_ascii=False))
            if not all(verdicts.values()):
                raise SystemExit(1)
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
                try:
                    bot.close()
                except AssertionError:
                    pass
            shell.stop()
            server.stop()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--murmur", type=Path)
    parser.add_argument("--nuschld", action="store_true")
    parser.add_argument("--at-spi", type=Path, required=True)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--ui-font", help="appearance.ui_font for a control run")
    parser.add_argument("--mono-font", help="appearance.mono_font for a control run")
    arguments = parser.parse_args()
    if not arguments.nuschld and not arguments.murmur:
        parser.error("--murmur or --nuschld")
    run(arguments)
