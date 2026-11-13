#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Client-view proof for fenced Nix code (GitHub issue #185).

The shared renderer's unit tests show that the content worker and the content
WASM highlight Nix. This fixture checks what the clients actually show. One
synthetic ```nix block (BODY below) and a control block with the same body and
an unknown language are sent by a scripted `nuschl-cli` over an owned loopback
stock Murmur or `nuschld`, and read back from

- desktop: the GPUI app in a private headless GNOME Shell (gnome_session.py:
  own Wayland socket and session bus; here also a private system bus and a
  private null PulseAudio), over AT-SPI (accessibility.py) and screenshots;
- tui: `nuschl-tui` on pseudo-terminals (tui_loopback.py), full screen through
  a VT100 emulator that also keeps every cell's SGR colour, and `--plain`;
- browser: the packaged Svelte app in the confined Chromium wrapper of
  web_audio.py, through the owned web relay, from the DOM.

For every view it checks (a) exact text, whitespace and line breaks, (b) what
copy or selection yields through the application itself, and (c) non-plain
highlighting in the rendered view. The reference for (a) and (c) is the
content worker's own reply for the same message (oracle.json, the shared
renderer: not view evidence by itself); the control block must show the plain
fallback, so colours cannot come from anything but highlighting.

    python3 tests/nix_view_proof.py {desktop|tui|browser|all} --config CONFIG.json --run LABEL
    python3 tests/nix_view_proof.py selftest

Each view and server writes ~/.cache/claude-nuschl/nix-view-proof/LABEL/<view>-<server>/
(result.json, supervisor.json, evidence, manifest.sha256); an existing
directory is never reused or removed. A supervisor runs every scenario in its
own session with a deadline, CPU affinity and free-space floors, is a child
subreaper, samples device file descriptors of every descendant and afterwards
signals only the descendants it recorded (PID and start time). Any failed
check, guard stop, forbidden device or surviving process exits non-zero.

No host microphone, camera, clipboard, profile, display or audio graph is
used: profiles, HOME and XDG folders are fresh; host Pulse/PipeWire/ALSA/D-Bus
addresses point at nonexistent owned paths. The desktop copy checks read the
PRIVATE compositor's clipboard through its own Shell.Eval (not the host's);
the browser intercepts the page's clipboard API. Limits: GNOME and GPUI use
the host render node (/dev/dri) under the existing GPUI rendering exception;
this is not a device-confinement or OS-sandbox claim, nor proof on physical
Windows/macOS/mobile. Desktop colours are measured in pixels with tolerances
(GPUI exposes no AT-SPI text attributes); the TUI has no in-application copy,
so its terminal-selection evidence is the rendered cells and the --plain lines.
"""

import argparse
import collections
import hashlib
import html
import json
import os
from pathlib import Path
import queue
import re
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import unicodedata

TESTS = Path(__file__).resolve().parent
sys.path.insert(0, str(TESTS))

DEFAULT_ROOT = Path.home() / ".cache/claude-nuschl/nix-view-proof"
BOT = "Nix Bot"
DEVELOPMENT = 2
ROOT_CHANNEL = 0
UNKNOWN = "nuschl-unknown"
SENTINEL = "nix-view-proof private clipboard sentinel"
VIEWS = ("desktop", "tui", "browser")
SERVERS = {"desktop": ("murmur", "nuschld"), "tui": ("murmur", "nuschld"), "browser": ("murmur",)}
DEADLINES = {"desktop": 420, "tui": 300, "browser": 360}
HELPERS = ("accessibility.py", "gnome_session.py", "local_murmur.py", "local_nuschld.py", "mumble_client.py",
           "seed_server.py", "tui_loopback.py", "web_audio.py", "workers.py", "nix_view_proof.py",
           "nix_view_proof.browser.mjs")
# Desktop pixel heuristics: per-channel tolerance and pixels needed for a colour to count.
TOLERANCE, MIN_PIXELS, MIN_TOKEN_CHARS = 12, 6, 3

# Comments (# and /* */), strings with ${} and an indented '' string, a nested
# attribute set with inherit, paths, let/in, a function with a default, lists,
# indentation, a blank line, spaces before a closing quote and at a line end
# inside the '' string. No tabs (terminals show spaces) and no URL (links).
BODY = (
    "# Line comment: synthetic nix-view-proof fixture\n"
    "/* Block comment\n"
    "   spanning two lines */\n"
    "{ pkgs, lib ? pkgs.lib, ... }:\n"
    "let\n"
    '  name = "nuschl";\n'
    '  greeting = "hello ${name}, version ${lib.version}";\n'
    '  padded = "two trailing spaces  ";\n'
    "  script = ''\n"
    '    echo "${greeting}"\n'
    "    indented  \n"
    "      deeper\n"
    "  '';\n"
    "\n"
    "  ports = [ 64738 8080 ];\n"
    "in\n"
    "{\n"
    "  inherit name greeting;\n"
    "  services.nuschl = {\n"
    "    enable = true;\n"
    "    settings.paths = [ ./foo ./config/bar.nix <nixpkgs> ];\n"
    "  };\n"
    "  environment.systemPackages = with pkgs; [ hello ];\n"
    "}\n"
)
# What a code block shows and copies: the line break before the fence ends a line.
CODE = BODY[:-1]
LANGUAGES = {"nix": "nix", "control": UNKNOWN}


def message(tag, kind):
    return f"{tag} {kind}:\n```{LANGUAGES[kind]}\n{BODY}```"


def legacy_html(text):
    """What nuschl-cli sends a stock server (nuschl-legacy-html's text_html; no links here)."""
    assert "://" not in text and "www." not in text.lower()
    return (text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
            .replace('"', "&quot;").replace("\n", "<br/>"))


def char_lines(runs):
    """Code runs as lines of (character, rgb); the closing line break is no line."""
    lines = [[]]
    for run in runs:
        rgb = tuple(run["foreground"][:3])
        for character in run["text"]:
            if character == "\n":
                lines.append([])
            else:
                lines[-1].append((character, rgb))
    if not lines[-1]:
        lines.pop()
    return lines


def ink(lines):
    """Colours of the characters that draw something."""
    return collections.Counter(rgb for line in lines for character, rgb in line if not character.isspace())


def sha256(path):
    digest = hashlib.sha256()
    with open(path, "rb") as stream:
        for chunk in iter(lambda: stream.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path, value):
    Path(path).write_text(json.dumps(value, indent=2, ensure_ascii=False, default=str) + "\n")


def write_manifest(directory, name="manifest.sha256", skip=("scratch", "env")):
    """sha256sum lines for every regular file, outside scratch/env folders at any depth."""
    lines = []
    for path in sorted(Path(directory).rglob("*")):
        relative = path.relative_to(directory)
        if set(relative.parts[:-1]) & set(skip) or relative == Path(name) or path.is_symlink() \
                or not path.is_file():
            continue
        lines.append(f"{sha256(path)}  {relative}")
    (Path(directory) / name).write_text("\n".join(lines) + "\n")


def log(text):
    print(f"{time.strftime('%H:%M:%S')} {text}", flush=True)


def eventually(predicate, what, timeout=30.0):
    """local_murmur.eventually, but a node or process vanishing mid-query is a retry."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            value = predicate()
        except (RuntimeError, KeyError, ValueError):
            value = None
        if value:
            return value
        time.sleep(0.2)
    raise AssertionError(what)


class Checks:
    def __init__(self):
        self.items, self.skipped = [], []

    def add(self, group, name, passed, **detail):
        self.items.append({"group": group, "name": name, "passed": bool(passed), "detail": detail})
        log(f"{'PASS' if passed else 'FAIL'} [{group}] {name}")
        return bool(passed)

    def passed(self):
        return bool(self.items) and all(item["passed"] for item in self.items)


# --- The terminal emulator, with colours -----------------------------------

def styled_screen_class():
    from tui_loopback import Screen

    class StyledScreen(Screen):
        """tui_loopback.Screen, which ignores colours, plus SGR per cell:
        (fg, bold, italic, dim, reverse), fg an (r, g, b) tuple, ("index", n) or None."""
        DEFAULT = (None, False, False, False, False)

        def __init__(self, rows, cols):
            super().__init__(rows, cols)
            self.styles = [[self.DEFAULT] * cols for _ in range(rows)]
            self.style = self.DEFAULT

        def put(self, char):
            width = 2 if unicodedata.east_asian_width(char) in "WF" else 1
            if unicodedata.combining(char):
                return
            if self.x + width > self.cols:
                self.x = 0
                self.y = min(self.y + 1, self.rows - 1)
            self.styles[self.y][self.x] = self.style
            super().put(char)

        def erase(self, y, start, end):
            super().erase(y, start, end)
            for x in range(max(start, 0), min(end, self.cols)):
                self.styles[y][x] = self.DEFAULT

        def csi(self, params, final):
            if final == "m" and not params.startswith("?"):
                self.sgr(params)
            else:
                super().csi(params, final)

        def sgr(self, params):
            numbers = [int(part) if part.isdigit() else 0 for part in params.split(";")] if params else [0]
            fg, bold, italic, dim, reverse = self.style
            index = 0
            while index < len(numbers):
                number = numbers[index]
                if number == 0:
                    fg, bold, italic, dim, reverse = self.DEFAULT
                elif number == 1:
                    bold = True
                elif number == 2:
                    dim = True
                elif number == 3:
                    italic = True
                elif number == 7:
                    reverse = True
                elif number == 22:
                    bold = dim = False
                elif number == 23:
                    italic = False
                elif number == 27:
                    reverse = False
                elif 30 <= number <= 37 or 90 <= number <= 97:
                    fg = ("index", number - 30 if number < 90 else number - 82)
                elif number in (38, 48):
                    mode = numbers[index + 1] if index + 1 < len(numbers) else None
                    if mode == 2 and index + 4 < len(numbers):
                        if number == 38:
                            fg = tuple(numbers[index + 2:index + 5])
                        index += 4
                    elif mode == 5 and index + 2 < len(numbers):
                        if number == 38:
                            fg = ("index", numbers[index + 2])
                        index += 2
                elif number == 39:
                    fg = None
                index += 1
            self.style = (fg, bold, italic, dim, reverse)

    return StyledScreen


# --- Shared setup ------------------------------------------------------------

class Context:
    def __init__(self, view, server, cfg, out, tag, private_clipboard):
        self.view, self.server, self.cfg, self.out, self.tag = view, server, cfg, out, tag
        self.private_clipboard = private_clipboard
        self.package = Path(cfg["package"])
        self.binaries = self.package / "bin"
        self.scratch = out / "scratch"
        self.scratch.mkdir(mode=0o700)
        self.checks = Checks()
        # Murmur relays the escaped HTML nuschl-cli makes; nuschld a structured text.
        self.path = "legacy" if server == "murmur" else "render"
        self.oracle = None

    def expected(self, kind):
        return self.oracle[self.path][kind]


def identity(ctx):
    programs = {}
    for name in ("nuschl", "nuschl-cli", ".nuschl-cli-wrapped", "nuschl-tui", ".nuschl-tui-wrapped",
                 "nuschld", "nuschl-content-worker", "nuschl-storage-worker", "nuschl-crypto-worker",
                 "nuschl-media-worker", "nuschl-math-worker", "nuschl-transfer-worker"):
        path = ctx.binaries / name
        if path.is_file():
            programs[name] = sha256(path)
    raw = ctx.package / "libexec/nuschl/nuschl"
    record = {"package": str(ctx.package), "package_resolved": str(ctx.package.resolve()),
              "raw_desktop_sha256": sha256(raw) if raw.is_file() else None, "programs_sha256": programs,
              "expected": ctx.cfg.get("expected", {})}
    want = ctx.cfg.get("expected", {}).get("programs_sha256", {})
    for name, digest in want.items():
        ctx.checks.add("identity", f"{name} has the expected sha256", programs.get(name) == digest,
                       actual=programs.get(name), expected=digest)
    ctx.checks.add("identity", "package is a read-only Nix store path with the programs this view runs",
                   str(ctx.package).startswith("/nix/store/")
                   and all(name in programs for name in ("nuschl-cli", "nuschl-content-worker", "nuschld")),
                   package=str(ctx.package))
    write_json(ctx.out / "identity.json", record)
    return record


def make_oracle(ctx):
    """The content worker's reply for the very messages sent (run directly,
    outside its sandbox: a reference for synthetic input, not view evidence)."""
    from workers import exchange
    names, payloads = [], []
    for kind in ("nix", "control"):
        text = message(ctx.tag, kind)
        names += [("render", kind), ("legacy", kind)]
        payloads += [{"kind": "render", "text": text}, {"kind": "legacy_html", "html": legacy_html(text)}]
    replies = exchange(ctx.binaries / "nuschl-content-worker", payloads)
    oracle = {"render": {}, "legacy": {}, "messages": {kind: message(ctx.tag, kind) for kind in LANGUAGES},
              "note": "content worker run directly on synthetic input; shared-renderer reference only"}
    for (path, kind), reply in zip(names, replies, strict=True):
        document = reply.get("value", {})
        blocks = document.get("blocks") if path == "render" else document.get("document", {}).get("blocks")
        codes = [block for block in blocks or [] if block.get("kind") == "code"]
        code = codes[0] if len(codes) == 1 else {}
        oracle[path][kind] = {"status": reply.get("status"), "block_kinds": [b.get("kind") for b in blocks or []],
                              "code_blocks": len(codes), "language": code.get("language"),
                              "runs": code.get("runs", [])}
    write_json(ctx.out / "oracle.json", oracle | {"replies": replies})
    checks = ctx.checks
    for path in ("render", "legacy"):
        for kind in LANGUAGES:
            entry = oracle[path][kind]
            text = "".join(run["text"] for run in entry["runs"])
            checks.add("oracle", f"shared renderer ({path}) keeps the {kind} block's exact text and language",
                       entry["code_blocks"] == 1 and text == BODY and entry["language"] == LANGUAGES[kind],
                       status=entry["status"], blocks=entry["block_kinds"], language=entry["language"],
                       text_equal=text == BODY)
    plain = ink(char_lines(oracle["render"]["control"]["runs"]))
    tokens = set(ink(char_lines(oracle["render"]["nix"]["runs"]))) - set(plain)
    checks.add("oracle", "shared renderer: control falls back to one plain colour", len(plain) == 1,
               colours=[list(c) for c in plain])
    checks.add("oracle", "shared renderer: Nix has at least four non-plain token colours", len(tokens) >= 4,
               tokens=sorted(list(c) for c in tokens))
    for kind in LANGUAGES:
        checks.add("oracle", f"shared renderer: render and legacy paths colour the {kind} block alike",
                   char_lines(oracle["render"][kind]["runs"]) == char_lines(oracle["legacy"][kind]["runs"]))
    oracle["plain"] = next(iter(plain)) if len(plain) == 1 else None
    oracle["tokens"] = sorted(tokens)
    ctx.oracle = oracle
    return checks.passed()


def start_server(ctx):
    from local_murmur import Murmur, free_port
    root = ctx.scratch / "server"
    root.mkdir()
    port = free_port()
    if ctx.server == "murmur":
        server = Murmur(Path(ctx.cfg["murmur"]), root, port)
        server.ini.write_text(server.ini.read_text() + "messagelimit=1000\nmessageburst=1000\n")
        server.seed()
        server.start()
    else:
        from local_nuschld import Nuschld
        server = Nuschld(ctx.binaries, root, port, extra='[history]\nguests = "full"')
        server.seed()
        server.write_config(message_limit=100, message_burst=100)
        server.start()
    log(f"{ctx.server} listening on 127.0.0.1:{port}")
    return server


def keep_server_log(ctx, server):
    for name in ("murmur.log", "nuschld.log"):
        path = ctx.scratch / "server" / name
        if path.is_file():
            shutil.copyfile(path, ctx.out / f"server-{name}")
    try:
        server.stop()
    except AssertionError as error:
        ctx.checks.add("harness", "server stopped cleanly", False, error=str(error)[-2000:])


def connect_bot(ctx, port, channel):
    from mumble_client import Client, certificate_pin
    pin = certificate_pin(ctx.binaries, "127.0.0.1", port, ctx.scratch / "probe")
    bot = Client(ctx.binaries, "127.0.0.1", port, pin, BOT, ctx.scratch / "bot",
                 ["--message-limit", "1000", "--message-burst", "1000"])
    if channel != ROOT_CHANNEL:
        bot.send(kind="join", channel=channel)
        bot.wait(lambda e: e["kind"] == "user" and e["user"]["session"] == bot.session
                 and e["user"]["channel"] == channel)
    return pin, bot


def present(bot, name, channel, timeout=60):
    """The scripted client sees `name` in `channel` (its user table, not its event queue)."""
    eventually(lambda: any(user["name"] == name and user["channel"] == channel for user in list(bot.users.values())),
               f"{name} did not appear in channel {channel}", timeout)


def send_text(ctx, bot, channel, kind):
    bot.send(kind="text", channel=channel, body=message(ctx.tag, kind))
    if ctx.server == "nuschld":
        ack = bot.wait(lambda e: e["kind"] in ("chat_acked", "chat_failed"), timeout=20)
        if ack["kind"] != "chat_acked":
            raise AssertionError(f"server refused the {kind} message: {ack}")
    log(f"sent the {kind} message")
    # Murmur's control-message limiter, and order on screen.
    time.sleep(1.2)


def close_bot(ctx, bot):
    if bot is None:
        return
    try:
        bot.close()
    except AssertionError as error:
        ctx.checks.add("harness", "the scripted client disconnected cleanly", False, error=str(error)[-2000:])


def stop_processes(processes):
    for process in processes:
        if process is not None and process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)


# --- Desktop -----------------------------------------------------------------

def start_system_bus(ctx):
    """A private system bus with no services: GNOME Shell needs one to start,
    and nothing then reaches the host's."""
    directory = Path(tempfile.mkdtemp(prefix="nsb-", dir=os.environ["NUSCHL_WL_TMP"]))
    socket_path = directory / "bus"
    config = ctx.scratch / "system-bus.conf"
    config.write_text(f'<busconfig><type>system</type><listen>unix:path={socket_path}</listen><auth>EXTERNAL</auth>'
                      '<policy context="default"><allow user="*"/><allow own="*"/><allow send_destination="*"/>'
                      '<allow receive_sender="*"/></policy></busconfig>\n')
    process = subprocess.Popen([ctx.cfg["dbus_daemon"], "--nofork", "--nopidfile", "--nosyslog",
                                f"--config-file={config}", "--print-address=1"],
                               stdout=(ctx.out / "system-bus.log").open("x"), stderr=subprocess.STDOUT,
                               start_new_session=True)
    eventually(lambda: socket_path.exists() or process.poll() is not None, "private system bus missing", 10)
    if process.poll() is not None:
        raise AssertionError("private system bus exited")
    os.environ["DBUS_SYSTEM_BUS_ADDRESS"] = f"unix:path={socket_path}"
    return process, directory


def desktop_pixels(ctx, magick, crops):
    """Token colours in the nix crop, line by line, against the control crop
    of the same text (plain fallback): same font, layout and anti-aliasing."""
    plain = tuple(ctx.oracle["plain"])
    lines = char_lines(ctx.expected("nix")["runs"])
    palette = [plain] + [tuple(c) for c in ctx.oracle["tokens"]]

    def classify(name):
        path, (width, height) = crops[name]
        raw = subprocess.run([magick, str(path), "-alpha", "off", "-depth", "8", "rgb:-"],
                             capture_output=True, check=True, timeout=60).stdout
        if len(raw) != width * height * 3:
            raise AssertionError(f"{name} crop has {len(raw)} bytes, expected {width}x{height}x3")
        rows = []
        for y in range(height):
            counts = [0] * len(palette)
            base = y * width * 3
            for x in range(width):
                red, green, blue = raw[base + 3 * x], raw[base + 3 * x + 1], raw[base + 3 * x + 2]
                best, distance = None, TOLERANCE + 1
                for index, (r, g, b) in enumerate(palette):
                    d = max(abs(red - r), abs(green - g), abs(blue - b))
                    if d < distance:
                        best, distance = index, d
                if best is not None:
                    counts[best] += 1
            rows.append(counts)
        return rows

    nix, control = classify("nix"), classify("control")
    height = crops["nix"][1][1]
    line_height = height / max(len(lines), 1)
    expectations, found = [], 0
    for number, line in enumerate(lines):
        top = int((number + 0.15) * line_height)
        bottom = int((number + 0.85) * line_height)
        for rgb, chars in collections.Counter(rgb for c, rgb in line if not c.isspace()).items():
            if rgb == plain or chars < MIN_TOKEN_CHARS:
                continue
            index = palette.index(rgb)
            pixels = sum(row[index] for row in nix[top:bottom])
            found += pixels >= MIN_PIXELS
            expectations.append({"line": number, "colour": list(rgb), "chars": chars, "pixels": pixels,
                                 "present": pixels >= MIN_PIXELS})
    totals = {"nix": [sum(row[i] for row in nix) for i in range(len(palette))],
              "control": [sum(row[i] for row in control) for i in range(len(palette))]}
    tokens = [{"colour": list(palette[i]), "nix_pixels": totals["nix"][i], "control_pixels": totals["control"][i]}
              for i in range(1, len(palette))]
    report = {"tolerance": TOLERANCE, "min_pixels": MIN_PIXELS, "min_token_chars": MIN_TOKEN_CHARS,
              "line_height": line_height, "lines": len(lines), "plain": list(plain),
              "plain_pixels": {"nix": totals["nix"][0], "control": totals["control"][0]},
              "tokens": tokens, "expectations": expectations,
              "ratio": found / len(expectations) if expectations else 0.0}
    write_json(ctx.out / "desktop-pixels.json", report)
    checks = ctx.checks
    checks.add("highlight", "desktop crop geometry: one text line per code line, plausible height",
               10 <= line_height <= 60, line_height=line_height, crops={k: v[1] for k, v in crops.items()})
    checks.add("highlight", "desktop: the token colours of each code line are drawn on that line (>= 90%)",
               len(expectations) >= 8 and report["ratio"] >= 0.9, found=found, expected=len(expectations))
    drawn = [t for t in tokens if t["nix_pixels"] >= 3 * MIN_PIXELS]
    checks.add("highlight", "desktop: at least four distinct token colours are drawn in the Nix block",
               len(drawn) >= 4, drawn=[t["colour"] for t in drawn])
    leaked = [t for t in tokens if t["control_pixels"] > max(MIN_PIXELS, 0.1 * t["nix_pixels"])]
    checks.add("highlight", "desktop: the unknown-language control block shows none of them (plain fallback)",
               not leaked and totals["control"][0] >= 50, leaked=leaked, control_plain=totals["control"][0])


def scenario_desktop(ctx):
    from accessibility import items, start_registry
    from gnome_session import GnomeSession

    cfg, out, checks, tag = ctx.cfg, ctx.out, ctx.checks, ctx.tag
    magick = shutil.which("magick")
    bus = bus_dir = server = bot = app = shell = None
    helpers = []
    try:
        bus, bus_dir = start_system_bus(ctx)
        server = start_server(ctx)
        shell = GnomeSession(cfg["gnome_shell"], 1600, 1200, log=(out / "gnome.log").open("x"))
        shell.start()
        log("private GNOME Shell ready")

        def spawn(command, env, name):
            process = subprocess.Popen(command, env=env, stdout=(out / name).open("a"), stderr=subprocess.STDOUT,
                                       start_new_session=True)
            helpers.append(process)
            return process

        base_env = shell.app_environment()
        base_env.pop("NO_AT_BRIDGE", None)
        registry = start_registry(Path(cfg["at_spi"]), base_env, spawn)
        pin, bot = connect_bot(ctx, server.port, DEVELOPMENT)
        profile = ctx.scratch / "profile"
        (profile / "config").mkdir(parents=True)
        write_json(profile / "config/servers.json", [{
            "id": "6e697876-6965-7770-726f-6f6600000185", "name": "Nix view fixture", "host": "127.0.0.1",
            "port": server.port, "username": "Andi", "certificate_sha256": pin}])
        write_json(profile / "config/settings.json",
                   {"appearance": {"theme": "dark"}, "audio": {"transmit": "push_to_talk"}})
        # Private null audio only (markdown_repro.py): never the host's devices.
        pulse = shell.runtime / "pulse"
        pulse.mkdir(mode=0o700, exist_ok=True)
        script = pulse / "default.pa"
        script.write_text(f"load-module module-native-protocol-unix socket={pulse / 'native'} auth-anonymous=1\n"
                          "load-module module-null-sink sink_name=speakers rate=48000\n"
                          "load-module module-null-sink sink_name=microphone rate=48000\n"
                          "set-default-sink speakers\nset-default-source microphone.monitor\n")
        spawn([str(Path(cfg["pulse_bin"]) / "pulseaudio"), "--daemonize=no", "--use-pid-file=no",
               "--exit-idle-time=-1", "-n", f"--file={script}"],
              shell.environment() | {"PULSE_RUNTIME_PATH": str(pulse), "PULSE_STATE_PATH": str(pulse / "state"),
                                     "DBUS_SESSION_BUS_ADDRESS": f"unix:path={ctx.scratch}/no-pulse-bus"},
              "pulse.log")
        eventually(lambda: (pulse / "native").exists(), "private null audio missing")
        env = base_env | {"RUST_LOG": "nuschl=info", "PULSE_SERVER": f"unix:{pulse}/native",
                          "NUSCHL_AUDIO_BACKEND": "pulseaudio", "PULSE_SINK": "speakers",
                          "PULSE_SOURCE": "microphone.monitor", "PIPEWIRE_REMOTE": str(ctx.scratch / "no-pipewire")}
        app = subprocess.Popen([str(ctx.binaries / "nuschl"), "--profile-dir", str(profile),
                                f"mumble://Andi@127.0.0.1:{server.port}/Lobby/Development"],
                               env=env, stdout=(out / "desktop.log").open("x"), stderr=subprocess.STDOUT,
                               start_new_session=True)
        eventually(lambda: shell.window("Nuschl"), "native window missing", timeout=60)
        shell.place("Nuschl", 20, 20, 1540, 1140)
        present(bot, "Andi", DEVELOPMENT)

        def application():
            assert app.poll() is None, (out / "desktop.log").read_text()[-3000:]
            return next((n for n in registry.children() if n.name.lower() == "nuschl"), None)

        nuschl = eventually(application, "desktop not in the accessibility registry", timeout=60)

        def nodes(role, name=None, prefix=None):
            found = []
            for node, node_role, node_name, _ in items(nuschl):
                if node_role == role and (name is None or node_name == name) \
                        and (prefix is None or node_name.startswith(prefix)):
                    found.append((node, node_name))
            return found

        def extents(node):
            try:
                return tuple(node.extents())
            except RuntimeError:
                return (0, 0, 0, 0)

        def newest(found):
            shown = [(extents(node), node, name) for node, name in found if extents(node)[3] > 0]
            return max(shown, key=lambda entry: entry[0][1]) if shown else None

        timeline = eventually(lambda: nodes("list", prefix="Messages in #Development"), "timeline missing", 45)[0][0]

        def origin():
            return json.loads(shell.eval(
                "JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window)"
                ".find(w=>(w.get_title()||'').startsWith('Nuschl'));"
                "const b=w.get_buffer_rect();return [b.x,b.y];})())"))

        def on_screen(box):
            ox, oy = origin()
            return ox + box[0], oy + box[1]

        def neutral_pointer():
            ox, oy = origin()
            shell.move(ox + 700, oy + 12)
            time.sleep(0.4)

        def clipboard(text=None):
            St = "imports.gi.St"
            if text is not None:
                shell.eval(f"{St}.Clipboard.get_default().set_text({St}.ClipboardType.CLIPBOARD,"
                           f" {json.dumps(text)}); 'ok'")
                return text
            shell.eval(f"global._nixViewClip = undefined; {St}.Clipboard.get_default().get_text("
                       f"{St}.ClipboardType.CLIPBOARD, (c, t) => {{ global._nixViewClip = t; }}); 'ok'")
            value = eventually(lambda: (lambda v: v if v["done"] else None)(json.loads(shell.eval(
                "JSON.stringify({done: global._nixViewClip !== undefined,"
                " text: global._nixViewClip === undefined ? null : global._nixViewClip})"))),
                "private clipboard read did not finish", 10)
            return value["text"]

        evidence = {}

        def receive(kind):
            try:
                return receive_checked(kind)
            except (AssertionError, RuntimeError):
                # Keep what the screen and the accessibility tree showed.
                try:
                    write_json(out / f"desktop-accessible-{kind}-failure.json",
                               [{"role": role, "name": name, "extents": list(extents(node))}
                                for node, role, name, _ in items(nuschl)])
                    shell.screenshot(out / f"desktop-{kind}-failure.png")
                except (AssertionError, RuntimeError, OSError, subprocess.SubprocessError):
                    pass
                raise

        def receive_checked(kind):
            """Wait for a message, expand it, bring its code into view; AT-SPI text checks."""
            language = LANGUAGES[kind]
            prefix = f"{BOT}: {tag} {kind}:"
            item = eventually(lambda: nodes("list item", prefix=prefix), f"{kind} message missing", 45)[0]
            label = eventually(lambda: newest(nodes("label", CODE)), f"{kind} code text missing over AT-SPI", 30)
            # "Show more" follows the height measured in the previous frame:
            # give it frames, then expand the newest message (clamped code is
            # cut off under a fade, though its accessible text is complete).
            time.sleep(1.5)
            more = newest(nodes("push button", "Show more"))
            expanded = None
            if more and more[0][1] >= label[0][1]:
                count = len(nodes("push button", "Show more"))
                more[1].press()
                eventually(lambda: len(nodes("push button", "Show more")) < count, f"{kind} message did not expand", 10)
                expanded = True
                time.sleep(0.5)
            label = eventually(lambda: newest(nodes("label", CODE)), f"{kind} code text missing over AT-SPI", 30)
            _, top, _, height = extents(timeline)

            def in_view():
                box = extents(label[1])
                return box[3] > 0 and top <= box[1] and box[1] + box[3] <= top + height
            if not in_view():
                item[0].call("Component", "ScrollTo", "u", "0")
                eventually(in_view, f"{kind} code did not scroll into view", 15)
            time.sleep(1.0)
            label = eventually(lambda: newest(nodes("label", CODE)), f"{kind} code text lost its bounds", 10)
            rows = [{"role": role, "name": name} for _, role, name, _ in items(nuschl)]
            name = nodes("list item", prefix=prefix)[0][1]
            evidence[kind] = {"list_item": name, "code_label_extents": list(label[0]), "expanded": expanded,
                              "timeline_extents": list(extents(timeline)), "window_origin": origin()}
            write_json(out / f"desktop-accessible-{kind}.json", {"nodes": rows, **evidence[kind]})
            checks.add("text", f"desktop {kind}: the code block's accessible text is the exact body "
                       "(every line, indentation, blank line, trailing spaces)", label[2] == CODE,
                       actual=label[2])
            expected = f"{BOT}: {tag} {kind}:\n{CODE}"
            checks.add("text", f"desktop {kind}: the message row reads tag line + exact code (the "
                       "'Copy message' text, Document::plain_text)",
                       name.startswith(expected) and name[len(expected):].startswith(", "), actual=name)
            checks.add("text", f"desktop {kind}: the code header names the language {language!r}",
                       bool(nodes("label", language)))
            return label

        def screenshot(kind, label):
            neutral_pointer()
            full = shell.screenshot(out / f"desktop-{kind}-screen.png")
            size = subprocess.run([magick, str(full), "-format", "%w %h", "info:"], capture_output=True,
                                  text=True, check=True, timeout=30).stdout.split()
            checks.add("harness", f"desktop {kind}: screenshot is the 1600x1200 private monitor at scale 1",
                       size == ["1600", "1200"], size=size)
            x, y = on_screen(label[0])
            width, height = label[0][2], label[0][3]
            crop = out / f"desktop-{kind}-code.png"
            subprocess.run([magick, str(full), "-crop", f"{width}x{height}+{x}+{y}", "+repage", str(crop)],
                           check=True, timeout=30)
            window = shell.window("Nuschl")
            subprocess.run([magick, str(full), "-crop", f"{window['width']}x{window['height']}+{window['x']}+"
                            f"{window['y']}", "+repage", str(out / f"desktop-{kind}-window.png")],
                           check=True, timeout=30)
            return crop, (width, height)

        crops = {}
        send_text(ctx, bot, DEVELOPMENT, "nix")
        label = receive("nix")
        crops["nix"] = screenshot("nix", label)
        if ctx.private_clipboard:
            # The real copy paths, read back from the PRIVATE compositor's clipboard.
            clipboard(SENTINEL)
            buttons = [(extents(node), node) for node, _ in nodes("push button", "Copy code")]
            above = [entry for entry in buttons if entry[0][3] > 0 and entry[0][1] <= label[0][1]]
            button = max(above, key=lambda entry: entry[0][1]) if above else None
            if button:
                shell.click(*on_screen((button[0][0] + button[0][2] // 2, button[0][1] + button[0][3] // 2)))
                time.sleep(0.8)
            copied = clipboard()
            checks.add("copy", "desktop: 'Copy code' puts the exact body in the (private) clipboard",
                       copied == CODE, actual=copied, button_found=bool(button))
            clipboard(SENTINEL)
            x, y, width, height = label[0]
            line_height = height / len(CODE.split("\n"))
            start = on_screen((x + 1, y + int(line_height / 2)))
            end = on_screen((x + width - 2, y + height - 2))
            shell.drag(start, end, steps=16)
            time.sleep(0.5)
            shell.screenshot(out / "desktop-nix-selection.png")
            shell.chord("Control_L", "c")
            time.sleep(0.8)
            selected = clipboard()
            checks.add("copy", "desktop: a pointer selection of the whole block copies the exact body (Ctrl+C)",
                       selected == CODE, actual=selected, drag={"from": start, "to": end})
            shell.click(*start)
            neutral_pointer()
        else:
            ctx.checks.skipped.append("desktop private-compositor clipboard reads (--no-private-clipboard)")
        send_text(ctx, bot, DEVELOPMENT, "control")
        label = receive("control")
        crops["control"] = screenshot("control", label)
        desktop_pixels(ctx, magick, crops)
        write_json(out / "desktop-evidence.json", evidence)
    finally:
        stop_processes([app] + helpers)
        close_bot(ctx, bot)
        if shell:
            shell.stop()
        if server:
            keep_server_log(ctx, server)
        stop_processes([bus])
        if bus_dir:
            shutil.rmtree(bus_dir, ignore_errors=True)


# --- Terminal ----------------------------------------------------------------

def tui_screen(ctx, screen, kind):
    """The full screen: the code rows' characters and every cell's colour."""
    language = LANGUAGES[kind]
    lines = char_lines(ctx.expected(kind)["runs"])
    rows = ["".join(row) for row in screen.grid]
    header = f"{BOT}: {ctx.tag} {kind}:"
    found = [index for index, row in enumerate(rows) if header in row]
    report = {"rows": rows, "header_row": found[-1] if found else None}
    checks = ctx.checks
    if not found or found[-1] + 1 + len(lines) >= len(rows) or f"[code: {language}]" not in rows[found[-1] + 1]:
        checks.add("text", f"tui full screen {kind}: the message and its code rows are on screen", False,
                   header_rows=found)
        return report
    head = found[-1]
    column = rows[head + 1].index(f"[code: {language}]") + 2
    text_ok, compared, mismatches, colours, cells, selection = True, 0, [], collections.Counter(), [], []
    for number, line in enumerate(lines):
        row = head + 2 + number
        expected = "".join(character for character, _ in line)
        shown = rows[row][column:column + len(expected)]
        indent = rows[row][column - 4:column]
        text_ok &= shown == expected and indent == "    "
        # A terminal selection from the code column to the line end, without
        # the pane's right border; terminals drop trailing blank cells.
        selected = rows[row][column:].rstrip()
        selection.append(selected[:-1].rstrip() if selected.endswith("│") else selected)
        cells.append([[character, list(fg) if isinstance(fg, tuple) else fg]
                      for character, (fg, *_rest) in zip(rows[row][column:column + len(expected)],
                                                         screen.styles[row][column:column + len(expected)])])
        for offset, (character, rgb) in enumerate(line):
            if character.isspace():
                continue
            compared += 1
            fg = screen.styles[row][column + offset][0]
            colours[fg] += 1
            if fg != rgb:
                mismatches.append({"line": number, "column": offset, "char": character, "shown": fg,
                                   "expected": list(rgb)})
    report |= {"code_column": column, "cells": cells, "mismatches": mismatches[:50], "compared": compared,
               "selection": selection}
    checks.add("text", f"tui full screen {kind}: every code line is shown in order with its indentation "
               "(trailing spaces are not distinguishable from blank cells)", text_ok,
               first_row=head + 2, column=column)
    wanted = "\n".join(line.rstrip() for line in CODE.split("\n"))
    checks.add("copy", f"tui full screen {kind}: the drawn cells a terminal selection copies (from the code "
               "column, border and trailing blanks dropped) equal the body; the TUI has no copy of its own",
               "\n".join(selection) == wanted, selection=selection)
    plain = tuple(ctx.oracle["plain"])
    distinct = {fg for fg in colours if isinstance(fg, tuple) and fg != plain}
    if kind == "nix":
        checks.add("highlight", "tui full screen nix: every drawn character has the content worker's colour",
                   compared > 0 and not mismatches, compared=compared, mismatches=len(mismatches))
        checks.add("highlight", "tui full screen nix: at least four distinct truecolor token colours",
                   len(distinct) >= 4, colours=[list(c) for c in sorted(distinct)])
    else:
        checks.add("highlight", "tui full screen control: plain fallback colour only",
                   compared > 0 and not mismatches and not distinct, compared=compared,
                   mismatches=len(mismatches), other=[list(c) for c in distinct])
    return report


def tui_html(rows, styles, path):
    """The screen as coloured HTML, for a person to look at."""
    parts = ['<!doctype html><meta charset="utf-8"><title>nuschl-tui screen</title>'
             '<pre style="background:#000;color:#ccc;font:13px monospace">']
    for row, style in zip(rows, styles):
        for character, (fg, bold, italic, _dim, _reverse) in zip(row, style):
            css = []
            if isinstance(fg, tuple) and fg and fg[0] != "index":
                css.append("color:rgb(%d,%d,%d)" % fg)
            if bold:
                css.append("font-weight:bold")
            if italic:
                css.append("font-style:italic")
            text = html.escape(character)
            parts.append(f'<span style="{";".join(css)}">{text}</span>' if css else text)
        parts.append("\n")
    parts.append("</pre>\n")
    Path(path).write_text("".join(parts))


def tui_plain(ctx, output, kind):
    lines = output.replace("\r\n", "\n").split("\n")
    header = f"{BOT}: {ctx.tag} {kind}:"
    found = [index for index, line in enumerate(lines) if line.endswith(header)]
    expected = [f"  [code: {LANGUAGES[kind]}]"] + ["    " + line for line in CODE.split("\n")]
    actual = lines[found[-1] + 1:found[-1] + 1 + len(expected)] if found else []
    ctx.checks.add("text", f"tui --plain {kind}: the transcript has every code line exactly, trailing "
                   "spaces included (the lines a screen reader or pipe gets)", actual == expected,
                   actual=actual, expected=expected)
    return actual


def scenario_tui(ctx):
    from tui_loopback import ALT_ENTER, CTRL_K, CTRL_Q, Terminal, environment
    out, checks, tag = ctx.out, ctx.checks, ctx.tag
    rows, cols = 72, 200
    server = bot = plain = full = None
    try:
        server = start_server(ctx)
        _, bot = connect_bot(ctx, server.port, DEVELOPMENT)
        env = environment(ctx.scratch / "terminal")
        tui = str(ctx.binaries / "nuschl-tui")
        common = ["--no-audio", "--opener", shutil.which("true")]
        url = f"127.0.0.1:{server.port}"
        plain = Terminal("plain", [tui, "--plain", *common, "--profile-dir", str(ctx.scratch / "plain"),
                                   f"mumble://Andi@{url}/Lobby/Development"], env, rows=50, cols=cols, plain=True)
        full = Terminal("full", [tui, *common, "--profile-dir", str(ctx.scratch / "full"), f"mumble://Ada@{url}"],
                        env, rows=rows, cols=cols)
        full.screen = styled_screen_class()(rows, cols)
        plain.wait_for("Trust it with /trust", timeout=60)
        plain.line("/trust")
        plain.wait_for(re.compile(r"Connected").search)
        full.wait_for("First connection to", timeout=60)
        full.send("y")
        full.wait_for(lambda view: re.search(r": Connected", view))
        try:
            present(bot, "Andi", DEVELOPMENT, 15)
        except AssertionError:
            # The link's channel path was not followed: join by name instead.
            plain.line("/join Development")
            present(bot, "Andi", DEVELOPMENT, 30)
        if "--- Development on " not in plain.output:
            plain.line("/go Development")
        plain.wait_for("--- Development on ")
        full.send(CTRL_K)
        full.wait_for("Find:")
        full.send("Development")
        full.wait_for("Development, channel")
        full.send(ALT_ENTER)
        full.wait_for("Development on 127.0.0.1, your voice channel")
        present(bot, "Ada", DEVELOPMENT, 30)
        reports = {}
        for kind in ("nix", "control"):
            send_text(ctx, bot, DEVELOPMENT, kind)

            def printed(view, header=f"{BOT}: {tag} {kind}:"):
                lines = view.replace("\r\n", "\n").split("\n")
                found = [index for index, line in enumerate(lines) if line.endswith(header)]
                # The code header, every code line, and the line break after the last.
                return bool(found) and len(lines) >= found[-1] + 3 + len(CODE.split("\n"))
            plain.wait_for(printed)
            full.wait_for(f"{BOT}: {tag} {kind}:")
            full.wait_for(f"[code: {LANGUAGES[kind]}]")
            deadline = time.monotonic() + 1.5
            while time.monotonic() < deadline:
                full.pump(0.05)
            reports[kind] = tui_screen(ctx, full.screen, kind)
            (out / f"tui-full-{kind}.txt").write_text(full.screen.text() + "\n")
            tui_html(reports[kind]["rows"], full.screen.styles, out / f"tui-full-{kind}.html")
            write_json(out / f"tui-full-{kind}-cells.json", {k: v for k, v in reports[kind].items() if k != "rows"})
            tui_plain(ctx, plain.output, kind)
    finally:
        statuses = {}
        for name, terminal, keys in (("full", full, CTRL_Q), ("plain", plain, "/quit\r")):
            if terminal is None:
                continue
            try:
                statuses[name] = terminal.close(keys)
            except (AssertionError, OSError) as error:
                statuses[name] = repr(error)
            (out / f"tui-{name}.ansi").write_text(terminal.output)
        if plain is not None:
            (out / "tui-plain-transcript.txt").write_text(plain.output.replace("\r\n", "\n"))
        if full is not None:
            (out / "tui-full-final.txt").write_text(full.screen.text() + "\n")
            tui_html(["".join(row) for row in full.screen.grid], full.screen.styles, out / "tui-full-final.html")
        if statuses:
            checks.add("harness", "both terminal clients quit with status 0",
                       all(status == 0 for status in statuses.values()), statuses=statuses)
        close_bot(ctx, bot)
        if server:
            keep_server_log(ctx, server)


# --- Browser -----------------------------------------------------------------

def scenario_browser(ctx):
    from web_audio import prepare_browser
    from xml.sax.saxutils import escape
    cfg, out, checks, tag = ctx.cfg, ctx.out, ctx.checks, ctx.tag
    app = Path(cfg["site_app"])
    assets = sorted((app / "assets").glob("nuschl_content_wasm_bg-*.wasm"))
    reference = Path(cfg["content_wasm_reference"])
    checks.add("identity", "browser app's content WASM is byte-identical to the reference content codec",
               len(assets) == 1 and sha256(assets[0]) == sha256(reference),
               app=str(app), assets=[str(a) for a in assets], reference=str(reference),
               app_sha256=[sha256(a) for a in assets], reference_sha256=sha256(reference))
    write_json(out / "browser-app.json", {"app": str(app), "app_resolved": str(app.resolve()), "files_sha256": {
        str(path.relative_to(app)): sha256(path) for path in sorted(app.rglob("*"))
        if path.is_file() and "licenses" not in path.relative_to(app).parts}})
    browser_root = ctx.scratch / "browser"
    report = prepare_browser(browser_root, Path(cfg["chromium"]), Path(cfg["bwrap"]), Path(cfg["font_dir"]))
    # tests/web_math.py's aliases: the code is drawn in the intended monospace face.
    aliases = {"sans-serif": "DejaVu Sans", "ui-sans-serif": "DejaVu Sans", "system-ui": "DejaVu Sans",
               "Inter": "DejaVu Sans", "serif": "DejaVu Serif", "monospace": "DejaVu Sans Mono",
               "ui-monospace": "DejaVu Sans Mono"}
    fonts = browser_root / "fonts.conf"
    fonts.write_text(fonts.read_text().replace("</fontconfig>", "".join(
        f"<alias><family>{escape(generic)}</family><prefer><family>{escape(family)}</family></prefer></alias>"
        for generic, family in aliases.items()) + "</fontconfig>"))
    write_json(out / "browser-confinement.json", report | {"font_aliases": aliases, "fontconfig_sha256": sha256(fonts)})
    oracle = {"tags": {kind: f"{tag} {kind}:" for kind in LANGUAGES}, "body": BODY, "code": CODE,
              "plain": list(ctx.oracle["plain"]), "deadline_ms": 240000}
    for kind in LANGUAGES:
        oracle[kind] = {"language": LANGUAGES[kind], "runs": ctx.expected(kind)["runs"]}
    write_json(out / "browser-oracle.json", oracle)
    server = bot = relay = driver = None
    try:
        server = start_server(ctx)
        from local_murmur import free_port
        relay_port = free_port()
        config = ctx.scratch / "relay.toml"
        config.write_text(f'listen="127.0.0.1:{relay_port}"\nports=["1024-65535"]\nallow_networks=["127.0.0.0/8"]\n')
        relay = subprocess.Popen([cfg["relay"], "--config", str(config)], stdout=(out / "relay.log").open("x"),
                                 stderr=subprocess.STDOUT, start_new_session=True)

        def listening():
            import socket
            with socket.socket() as probe:
                return probe.connect_ex(("127.0.0.1", relay_port)) == 0
        eventually(listening, "owned relay missing", 20)
        _, bot = connect_bot(ctx, server.port, ROOT_CHANNEL)
        env = {"PATH": os.environ["PATH"], "HOME": os.environ["HOME"], "TMPDIR": str(browser_root / "tmp"),
               "LANG": "C.UTF-8"}
        driver = subprocess.Popen([cfg["node"], str(TESTS / "nix_view_proof.browser.mjs"), str(app),
                                   f"ws://127.0.0.1:{relay_port}/relay", str(server.port), report["wrapper"],
                                   cfg["playwright"], str(out), str(out / "browser-oracle.json")],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=(out / "browser-driver.stderr").open("x"), text=True, env=env,
                                  start_new_session=True)
        events = queue.Queue()

        def pump():
            with (out / "browser-driver.stdout").open("x") as copy:
                for line in driver.stdout:
                    copy.write(line)
                    copy.flush()
                    events.put(line)
            events.put(None)
        threading.Thread(target=pump, daemon=True).start()

        def wait_event(name, timeout):
            deadline = time.monotonic() + timeout
            while time.monotonic() < deadline:
                try:
                    line = events.get(timeout=max(0.05, deadline - time.monotonic()))
                except queue.Empty:
                    break
                if line is None:
                    raise AssertionError(f"browser driver exited before {name!r}")
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if event.get("event") == name:
                    return event
            raise AssertionError(f"browser driver: no {name!r} within {timeout} s")

        wait_event("ready", 150)
        present(bot, "nix-reader", ROOT_CHANNEL, 30)
        for kind in ("nix", "control"):
            send_text(ctx, bot, ROOT_CHANNEL, kind)
        driver.stdin.write("sent\n")
        driver.stdin.flush()
        try:
            driver.wait(timeout=240)
        except subprocess.TimeoutExpired:
            os.killpg(driver.pid, signal.SIGKILL)
            driver.wait(timeout=10)
            raise AssertionError("browser driver timed out") from None
        proof_path = out / "browser-proof.json"
        proof = json.loads(proof_path.read_text()) if proof_path.is_file() else {}
        for name, result in proof.get("checks", {}).items():
            group = name.split(":", 1)[0]
            checks.add(group, f"browser {name.split(':', 1)[1].strip()}", result.get("passed"),
                       **{"detail": result.get("detail")})
        checks.add("harness", "browser driver finished with status 0 and wrote its proof",
                   driver.returncode == 0 and bool(proof.get("checks")), status=driver.returncode)
    finally:
        if driver is not None and driver.poll() is None:
            os.killpg(driver.pid, signal.SIGTERM)
            try:
                driver.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(driver.pid, signal.SIGKILL)
                driver.wait(timeout=5)
        close_bot(ctx, bot)
        stop_processes([relay])
        if server:
            keep_server_log(ctx, server)


SCENARIOS = {"desktop": scenario_desktop, "tui": scenario_tui, "browser": scenario_browser}
REQUIRED = {"desktop": ("package", "murmur", "gnome_shell", "at_spi", "pulse_bin", "dbus_daemon", "path"),
            "tui": ("package", "murmur", "path"),
            "browser": ("package", "murmur", "relay", "site_app", "content_wasm_reference", "chromium",
                        "playwright", "node", "font_dir", "bwrap", "path")}


class Terminated(Exception):
    pass


def run_scenario(args):
    def terminated(signum, frame):
        raise Terminated(f"signal {signum}")
    signal.signal(signal.SIGTERM, terminated)
    cfg = json.loads(Path(args.config).read_text())
    out = Path(args.out)
    ctx = Context(args.view, args.server, cfg, out, args.tag, not args.no_private_clipboard)
    log(f"{args.view} on {args.server}, tag {args.tag}")
    try:
        identity(ctx)
        if make_oracle(ctx):
            SCENARIOS[args.view](ctx)
        else:
            ctx.checks.add("harness", "the shared renderer precondition holds, so the views were exercised", False)
    except (Exception, Terminated) as error:
        ctx.checks.add("harness", "the scenario completed without an exception", False, error=repr(error),
                       traceback=traceback.format_exc()[-6000:])
    finally:
        result = {"view": args.view, "server": args.server, "tag": args.tag, "passed": ctx.checks.passed(),
                  "complete": not ctx.checks.skipped, "skipped": ctx.checks.skipped, "checks": ctx.checks.items}
        write_json(out / "result.json", result)
    return 0 if ctx.checks.passed() else 1


# --- Supervision -------------------------------------------------------------

def proc_table():
    table = {}
    for entry in Path("/proc").iterdir():
        if not entry.name.isdecimal():
            continue
        try:
            stat = (entry / "stat").read_text()
            rest = stat.rsplit(")", 1)[1].split()
            table[int(entry.name)] = {"state": rest[0], "ppid": int(rest[1]), "start": int(rest[19]),
                                      "comm": stat[stat.index("(") + 1:stat.rindex(")")]}
        except (OSError, ValueError, IndexError):
            pass
    return table


def device_class(target):
    if not target.startswith("/dev/"):
        return None
    if target.startswith("/dev/snd/"):
        return "sound"
    if re.match(r"/dev/(video|media|v4l)", target):
        return "camera"
    if target.startswith("/dev/dri/renderD"):
        return "render"
    if target.startswith("/dev/dri/"):
        return "drm-card"
    if target.startswith(("/dev/pts/", "/dev/shm/")) or target in (
            "/dev/null", "/dev/zero", "/dev/full", "/dev/random", "/dev/urandom", "/dev/tty", "/dev/ptmx"):
        return None
    return "other"


def resources(path):
    memory = next(int(line.split()[1]) * 1024 for line in Path("/proc/meminfo").read_text().splitlines()
                  if line.startswith("MemAvailable:"))
    return {"tmp": shutil.disk_usage("/tmp").free, "cache": shutil.disk_usage(path).free, "memory": memory}


def owned_environment(cfg, out, wl, software_mesa):
    base = out / "env"
    names = ("home", "config", "data", "cache", "state", "tmp", "runtime")
    for name in names:
        (base / name).mkdir(parents=True, mode=0o700)
    (base / "alsa-null.conf").write_text(
        'pcm.!default { type null }\nctl.!default { type hw card "__nuschl_no_host_device__" }\n')
    (base / "pulse-client.conf").write_text("autospawn = no\nenable-shm = no\n")
    env = {"PATH": ":".join(cfg["path"]), "HOME": str(base / "home"), "XDG_CONFIG_HOME": str(base / "config"),
           "XDG_DATA_HOME": str(base / "data"), "XDG_CACHE_HOME": str(base / "cache"),
           "XDG_STATE_HOME": str(base / "state"), "XDG_RUNTIME_DIR": str(base / "runtime"),
           "TMPDIR": str(base / "tmp"), "NUSCHL_WL_TMP": str(wl), "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8",
           "TZ": "UTC", "PYTHONDONTWRITEBYTECODE": "1",
           "PULSE_SERVER": f"unix:{base}/no-host-pulse", "PULSE_RUNTIME_PATH": str(base / "no-host-pulse-runtime"),
           "PULSE_CLIENTCONFIG": str(base / "pulse-client.conf"), "PIPEWIRE_REMOTE": str(base / "no-host-pipewire"),
           "PIPEWIRE_RUNTIME_DIR": str(base / "no-host-pipewire-runtime"),
           "ALSA_CONFIG_PATH": str(base / "alsa-null.conf"),
           "DBUS_SESSION_BUS_ADDRESS": f"unix:path={base}/no-parent-session-bus",
           "DBUS_SYSTEM_BUS_ADDRESS": f"unix:path={base}/no-host-system-bus",
           "NUSCHL_AUDIO_BACKEND": "pulseaudio"}
    if software_mesa:
        icd = cfg["mesa_lvp_icd"]
        env |= {"LIBGL_ALWAYS_SOFTWARE": "1", "GALLIUM_DRIVER": "llvmpipe", "LP_NUM_THREADS": "2",
                "VK_DRIVER_FILES": icd, "VK_ICD_FILENAMES": icd}
    return env


def supervise(view, server, args, cfg, run_dir, tag):
    out = run_dir / f"{view}-{server}"
    out.mkdir(mode=0o700)
    wl = Path(args.root) / "wl"
    wl.mkdir(mode=0o700, exist_ok=True)
    env = owned_environment(cfg, out, wl, args.software_mesa)
    command = [sys.executable, str(Path(__file__).resolve()), "_scenario", view, server, "--config",
               str(Path(args.config).resolve()), "--out", str(out), "--tag", tag]
    if args.no_private_clipboard:
        command.append("--no-private-clipboard")
    deadline = args.deadline or DEADLINES[view]
    floors = {"tmp": args.floor_gib << 30, "cache": args.floor_gib << 30, "memory": args.memory_floor_gib << 30}
    cpus = sorted(os.sched_getaffinity(0))[:args.cpus]
    record = {"view": view, "server": server, "tag": tag, "command": command, "environment": env, "cpus": cpus,
              "deadline_seconds": deadline, "floors_bytes": floors, "supervisor_pid": os.getpid()}
    first = resources(out)
    if any(first[key] < floors[key] for key in floors):
        record |= {"exit": None, "guard_reason": f"below the free-resource floor before start: {first}",
                   "passed": False}
        write_json(out / "supervisor.json", record)
        return False
    me = os.getpid()
    leases, fds, unreadable = {}, {}, set()
    minimum = dict(first)
    reason = None
    started = time.monotonic()
    with (out / "scenario.log").open("x") as output:
        child = subprocess.Popen(command, cwd=str(TESTS.parent), env=env, stdout=output, stderr=subprocess.STDOUT,
                                 start_new_session=True, preexec_fn=lambda: os.sched_setaffinity(0, cpus))
    leases[child.pid] = proc_table().get(child.pid, {}).get("start")
    log(f"{view}/{server}: scenario PID {child.pid}, deadline {deadline} s, CPUs {cpus}")

    def reap():
        while True:
            try:
                pid, status = os.waitpid(-1, os.WNOHANG)
            except ChildProcessError:
                return
            if pid == 0:
                return
            if pid == child.pid:
                child.returncode = os.waitstatus_to_exitcode(status)

    def track():
        table = proc_table()
        changed = True
        while changed:
            changed = False
            for pid, info in table.items():
                if pid in leases or pid == me:
                    continue
                parent = info["ppid"]
                # A subreaper adopts orphans: they stay in this tree.
                if parent == me or (parent in leases and table.get(parent, {}).get("start") == leases[parent]):
                    leases[pid] = info["start"]
                    changed = True
        return table

    def sample(table):
        for pid, start in list(leases.items()):
            info = table.get(pid)
            if not info or info["start"] != start:
                continue
            try:
                descriptors = os.listdir(f"/proc/{pid}/fd")
            except OSError:
                unreadable.add(info["comm"])
                continue
            for descriptor in descriptors:
                try:
                    target = os.readlink(f"/proc/{pid}/fd/{descriptor}")
                except OSError:
                    continue
                kind = device_class(target)
                if kind:
                    fds.setdefault(kind, {}).setdefault(target, set()).add(info["comm"])

    def stop():
        try:
            os.killpg(child.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        until = time.monotonic() + 45
        while child.returncode is None and time.monotonic() < until:
            reap()
            time.sleep(0.2)
        if child.returncode is None:
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass

    next_sample = 0.0
    while child.returncode is None:
        reap()
        table = track()
        if time.monotonic() >= next_sample:
            sample(table)
            next_sample = time.monotonic() + 2.0
        now = resources(out)
        minimum = {key: min(minimum[key], now[key]) for key in now}
        if reason is None and any(now[key] < floors[key] for key in floors):
            reason = f"free-resource floor: {now}"
            stop()
        elif reason is None and time.monotonic() - started > deadline:
            reason = f"{deadline} s deadline"
            stop()
        time.sleep(0.5)
    track()

    def alive():
        table = proc_table()
        return {pid: table[pid] for pid, start in leases.items()
                if pid in table and table[pid]["start"] == start and table[pid]["state"] != "Z"}
    for _ in range(40):
        reap()
        if not alive():
            break
        time.sleep(0.25)
    residual = alive()
    for pid in residual:
        try:
            os.kill(pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
    if residual:
        for _ in range(20):
            reap()
            if not alive():
                break
            time.sleep(0.25)
        for pid in alive():
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        time.sleep(0.5)
        reap()
    remaining = alive()
    devices = {kind: {target: sorted(comms) for target, comms in targets.items()} for kind, targets in fds.items()}
    forbidden = {kind: devices[kind] for kind in ("sound", "camera") if kind in devices}
    result_path = out / "result.json"
    scenario = json.loads(result_path.read_text()) if result_path.is_file() else {}
    passed = (child.returncode == 0 and reason is None and not forbidden and not remaining
              and scenario.get("passed") is True)
    record |= {"exit": child.returncode, "guard_reason": reason, "seconds": round(time.monotonic() - started, 2),
               "minimum_free_bytes": minimum, "device_fds": devices, "forbidden_device_fds": forbidden,
               "render_node_exception": devices.get("render", {}),
               "fd_sampling": "every 2 s, point in time; not continuous tracing",
               "fd_unreadable_comms": sorted(unreadable),
               "owned_processes": len(leases), "residual_before_cleanup": {str(k): v for k, v in residual.items()},
               "remaining_after_cleanup": {str(k): v for k, v in remaining.items()}, "passed": passed}
    write_json(out / "supervisor.json", record)
    write_manifest(out)
    log(f"{view}/{server}: {'PASS' if passed else 'FAIL'} (exit {child.returncode}, guard {reason}, "
        f"forbidden devices {sorted(forbidden)}, remaining {len(remaining)})")
    return passed


def set_subreaper():
    import ctypes
    libc = ctypes.CDLL(None, use_errno=True)
    PR_SET_CHILD_SUBREAPER = 36
    if libc.prctl(PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) != 0:
        raise OSError(ctypes.get_errno(), "prctl(PR_SET_CHILD_SUBREAPER)")


def validate(cfg, views, args):
    problems = []
    for view in views:
        for key in REQUIRED[view]:
            value = cfg.get(key)
            if not value:
                problems.append(f"{view}: config lacks {key!r}")
            elif isinstance(value, str) and value.startswith("PLACEHOLDER"):
                problems.append(f"{view}: {key!r} is still a placeholder: {value}")
            elif isinstance(value, str) and not Path(value).exists():
                problems.append(f"{view}: {key!r} does not exist: {value}")
    if args.software_mesa and not cfg.get("mesa_lvp_icd"):
        problems.append("--software-mesa needs 'mesa_lvp_icd'")
    return problems


def run(args):
    views = VIEWS if args.view == "all" else (args.view,)
    cfg = json.loads(Path(args.config).read_text())
    problems = validate(cfg, views, args)
    if problems:
        print("\n".join(problems), file=sys.stderr)
        return 2
    if not re.fullmatch(r"[A-Za-z0-9._-]{1,64}", args.run):
        print("--run must be 1-64 characters of [A-Za-z0-9._-]", file=sys.stderr)
        return 2
    run_dir = Path(args.root) / args.run
    run_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
    set_subreaper()
    tag = "nvp" + secrets.token_hex(4)
    source = TESTS.parent

    def git(*arguments):
        # Read-only queries; nothing that refreshes the index.
        try:
            return subprocess.run(["git", "-C", str(source), *arguments], capture_output=True, text=True,
                                  timeout=30).stdout.strip()
        except (OSError, subprocess.TimeoutExpired) as error:
            return f"unavailable: {error}"
    fixtures = {}
    for name in HELPERS:
        path = TESTS / name
        if path.is_file():
            data = path.read_bytes()
            blob = hashlib.sha1(b"blob %d\0" % len(data) + data).hexdigest()
            committed = git("rev-parse", f"{args.fixture_revision}:tests/{name}")
            fixtures[name] = {"sha256": sha256(path), "git_blob": blob, "committed_blob": committed,
                              "equals_committed": blob == committed}
    invocation = {"argv": sys.argv, "view": args.view, "tag": tag,
                  "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
                  "python": sys.executable, "config": str(Path(args.config).resolve()),
                  "config_sha256": sha256(args.config), "config_content": cfg, "source": str(source),
                  "git_head": git("rev-parse", "HEAD"), "fixture_revision": args.fixture_revision,
                  "fixture_commit": git("rev-parse", f"{args.fixture_revision}^{{commit}}"), "fixtures": fixtures,
                  "fixtures_equal_committed": all(entry["equals_committed"] for entry in fixtures.values()),
                  "results": {}}
    stamp = time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    path = run_dir / f"invocation-{args.view}-{stamp}.json"
    write_json(path, invocation)
    passed = True
    for view in views:
        servers = args.servers.split(",") if args.servers else SERVERS[view]
        for server in servers:
            try:
                ok = supervise(view, server, args, cfg, run_dir, tag)
            except FileExistsError:
                log(f"{view}/{server}: {run_dir / f'{view}-{server}'} exists; not reused (choose a new --run)")
                ok = False
            invocation["results"][f"{view}-{server}"] = ok
            passed &= ok
            write_json(path, invocation)
    summary_path = run_dir / "summary.json"
    summary = json.loads(summary_path.read_text()) if summary_path.is_file() else {}
    summary.update(invocation["results"])
    write_json(summary_path, summary)
    if not invocation["fixtures_equal_committed"]:
        log(f"the fixtures differ from {args.fixture_revision}: evidence would not match that committed revision")
        passed = False
    invocation["finished_utc"] = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    invocation["passed"] = passed
    write_json(path, invocation)
    write_manifest(run_dir, skip=("scratch", "env", "wl"))
    # Per-view directories also keep their own manifest; the run's covers all.
    print(json.dumps({"run": str(run_dir), "passed": passed, "results": invocation["results"]}, indent=1))
    return 0 if passed else 1


# --- Self-test of the pure helpers (no processes) -----------------------------

def selftest():
    for needle in ("# Line comment", "/* Block", "${name}", "''\n", "inherit ", "./foo", "<nixpkgs>", "let\n",
                   "in\n", "{ pkgs, lib ? pkgs.lib, ... }:", "[ 64738 8080 ]", "\n\n", "indented  \n",
                   'spaces  "', "services.nuschl = {"):
        assert needle in BODY, needle
    assert "\t" not in BODY and BODY.endswith("}\n")
    text = message("nvp00000000", "nix")
    assert text.startswith("nvp00000000 nix:\n```nix\n") and text.endswith("}\n```")
    escaped = legacy_html(text)
    assert "<nixpkgs>" not in escaped and "&lt;nixpkgs&gt;" in escaped and escaped.count("<br/>") == text.count("\n")
    runs = [{"text": "# c\n", "foreground": [1, 2, 3, 255]}, {"text": "\nab", "foreground": [4, 5, 6, 255]},
            {"text": " c\n", "foreground": [7, 8, 9, 255]}]
    lines = char_lines(runs)
    assert [len(line) for line in lines] == [3, 0, 4], lines
    assert ink(lines) == collections.Counter({(1, 2, 3): 2, (4, 5, 6): 2, (7, 8, 9): 1})
    screen = styled_screen_class()(3, 10)
    screen.feed("\x1b[1;38;2;10;20;30mab\x1b[39mc\x1b[0md\x1b[2;1H\x1b[38;5;3me\x1b[m\x1b[K")
    assert screen.text().split("\n")[0].startswith("abcd") and screen.grid[1][0] == "e"
    assert screen.styles[0][0] == ((10, 20, 30), True, False, False, False)
    assert screen.styles[0][2] == (None, True, False, False, False) and screen.styles[0][3] == screen.DEFAULT
    assert screen.styles[1][0][0] == ("index", 3)
    assert device_class("/dev/snd/pcmC0D0c") == "sound" and device_class("/dev/video0") == "camera"
    assert device_class("/dev/dri/renderD128") == "render" and device_class("/dev/dri/card0") == "drm-card"
    assert device_class("/dev/null") is None and device_class("/dev/input/event3") == "other"
    with tempfile.TemporaryDirectory() as temporary:
        root = Path(temporary)
        (root / "a.txt").write_text("a")
        (root / "scratch").mkdir()
        (root / "scratch/x").write_text("x")
        write_manifest(root)
        assert (root / "manifest.sha256").read_text().split("\n")[0].endswith("  a.txt")
        assert "scratch" not in (root / "manifest.sha256").read_text()
    print("selftest: ok")
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    for name in (*VIEWS, "all"):
        view = sub.add_parser(name)
        view.add_argument("--config", required=True)
        view.add_argument("--run", required=True, help="evidence label under --root; never reused")
        view.add_argument("--root", default=str(DEFAULT_ROOT))
        view.add_argument("--servers", help="comma list of murmur,nuschld (default: desktop/tui both, browser murmur)")
        view.add_argument("--deadline", type=int, help="seconds per scenario (default per view)")
        view.add_argument("--cpus", type=int, default=2)
        view.add_argument("--floor-gib", type=int, default=8, help="free /tmp and cache floor")
        view.add_argument("--memory-floor-gib", type=int, default=4)
        view.add_argument("--software-mesa", action="store_true", help="llvmpipe/lavapipe (needs mesa_lvp_icd)")
        view.add_argument("--fixture-revision", default="HEAD",
                          help="git revision whose tests/ must equal the fixtures run (recorded and enforced)")
        view.add_argument("--no-private-clipboard", action="store_true",
                          help="skip the desktop's private-compositor clipboard reads (recorded as incomplete)")
    sub.add_parser("selftest")
    scenario = sub.add_parser("_scenario")
    scenario.add_argument("view", choices=VIEWS)
    scenario.add_argument("server", choices=("murmur", "nuschld"))
    scenario.add_argument("--config", required=True)
    scenario.add_argument("--out", required=True)
    scenario.add_argument("--tag", required=True)
    scenario.add_argument("--no-private-clipboard", action="store_true")
    args = parser.parse_args()
    if args.command == "selftest":
        return selftest()
    if args.command == "_scenario":
        return run_scenario(args)
    args.view = args.command
    return run(args)


if __name__ == "__main__":
    sys.exit(main())
