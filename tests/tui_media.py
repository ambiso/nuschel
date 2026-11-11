#!/usr/bin/env python3
"""Native local TUI image paste and math in a private KGX (or secondary Foot).

Only private GNOME, clipboard, profiles and loopback Murmur are used. A PTY
relay preserves real terminal negotiation/input and records bytes for independent
proof. --fixture-only validates the terminal harness with an older package;
it does not claim that image paste or rendered formulas work.
"""
import argparse
import base64
from collections import Counter
import fcntl
import hashlib
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import shutil
import struct
import subprocess
import sys
import tempfile
import termios
import time
import tty
import urllib.parse
import uuid
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import items, start_registry  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402
from tui_loopback import Screen, Terminal, environment  # noqa: E402

TITLE = "Nuschl terminal media test"
FORMULA = r"\frac{a+b}{c}+\sqrt{x}"
COLORS = [(255, 0, 0), (0, 255, 0), (0, 0, 255), (255, 255, 0)]


def write_all(fd, data):
    while data:
        data = data[os.write(fd, data):]


def relay(directory, command):
    """Transparent PTY, including resize and real replies from the terminal."""
    directory.mkdir(parents=True, exist_ok=True)
    saved = termios.tcgetattr(0)
    tty.setraw(0)
    pid = fd = None
    try:
        # Probe this actual terminal before starting the application. Its own
        # later queries are forwarded unchanged as well.
        write_all(1, b"\x1b[c\x1b[16t\x1b]11;?\x07")
        replies = bytearray()
        until = time.monotonic() + 1.0
        while time.monotonic() < until:
            if select.select([0], [], [], 0.1)[0]:
                replies.extend(os.read(0, 4096))
        (directory / "capabilities.json").write_text(json.dumps({"replies": replies.decode("ascii", "backslashreplace"),
                                                                "hex": replies.hex()}, indent=2))
        pid, fd = pty.fork()
        if pid == 0:
            os.execv(command[0], command)
        (directory / "child.pid").write_text(str(pid))

        last_size = None

        def resize(*_):
            nonlocal last_size
            size = fcntl.ioctl(0, termios.TIOCGWINSZ, bytes(8))
            if size == last_size:
                return
            last_size = size
            fcntl.ioctl(fd, termios.TIOCSWINSZ, size)
            rows, cols, width, height = struct.unpack("HHHH", size)
            pending = directory / "size.json.next"
            pending.write_text(json.dumps(dict(rows=rows, cols=cols, width=width, height=height)))
            pending.replace(directory / "size.json")
        signal.signal(signal.SIGWINCH, resize)
        resize()
        with (directory / "output.bin").open("wb", buffering=0) as output, \
                (directory / "input.bin").open("wb", buffering=0) as incoming:
            while True:
                # A nested PTY's foreground process group can keep the outer
                # SIGWINCH from reaching this proxy; compare real dimensions
                # too, rather than silently testing an unchanged 80x24 child.
                resize()
                ready, _, _ = select.select([0, fd], [], [], 0.5)
                if fd in ready:
                    try:
                        data = os.read(fd, 65536)
                    except OSError:
                        data = b""
                    if not data:
                        break
                    output.write(data)
                    write_all(1, data)
                if 0 in ready:
                    data = os.read(0, 65536)
                    if not data:
                        break
                    incoming.write(data)
                    write_all(fd, data)
        _, status = os.waitpid(pid, 0)
        pid = None
        return os.waitstatus_to_exitcode(status)
    finally:
        termios.tcsetattr(0, termios.TCSANOW, saved)
        if pid:
            os.kill(pid, signal.SIGTERM)
        if fd:
            os.close(fd)


class GraphicsScreen(Screen):
    def csi(self, params, final):
        if final == "s":
            self.saved = (self.y, self.x)
        elif final == "u" and hasattr(self, "saved"):
            self.y, self.x = self.saved
        else:
            super().csi(params, final)


def screen(directory):
    size = json.loads((directory / "size.json").read_text())
    data = (directory / "output.bin").read_bytes()
    # Graphics DCS is not printable text. Parse the rest as the existing TUI
    # loopback reader does, including the image protocols' saved cursor.
    data = re.sub(rb"\x1bP.*?\x1b\\", b"", data, flags=re.S)
    data = re.sub(rb"\x1bP[^\x1b]*$", b"", data, flags=re.S)
    data = data.replace(b"\x1b7", b"\x1b[s").replace(b"\x1b8", b"\x1b[u")
    parsed = GraphicsScreen(size["rows"], size["cols"])
    parsed.feed(data.decode("utf8", "replace"))
    return parsed.text()


def graphics(directory):
    data = (directory / "output.bin").read_bytes()
    return dict(sixel=len(re.findall(rb"\x1bP[0-9;?]*q.*?\x1b\\", data, re.S)),
                halfblocks=data.count("▀".encode()) + data.count("▄".encode()))


def png(path, width=96, height=64, colors=COLORS):
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xffffffff)
    rows = b"".join(b"\0" + bytes(channel for x in range(width)
                                   for channel in colors[(y >= height // 2) * 2 + (x >= width // 2)])
                    for y in range(height))
    path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
                     + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))
    return path


def pixels(path):
    return subprocess.check_output(["magick", str(path), "-alpha", "on", "-depth", "8", "rgba:-"])


class ImageHtml(HTMLParser):
    def __init__(self, html):
        super().__init__(convert_charrefs=True)
        self.images, self.text = [], []
        self.feed(html)

    def handle_starttag(self, tag, attrs):
        if tag == "img":
            self.images.append(dict(attrs).get("src", ""))

    def handle_data(self, data):
        self.text.append(data)


def received_image(peer, fixture, destination, caption=None):
    event = peer.wait(lambda e: e["kind"] == "text" and e.get("html") is True and "data:image/" in e["body"], timeout=30)
    parsed = ImageHtml(event["body"])
    destination.with_suffix(".json").write_text(json.dumps(event, indent=2))
    assert len(parsed.images) == 1, event
    header, body = urllib.parse.unquote(parsed.images[0]).split(",", 1)
    assert header.startswith("data:image/") and header.endswith(";base64"), header
    data = base64.b64decode(re.sub(r"\s+", "", body), validate=True)
    assert len(data) <= 8 * 1024 * 1024
    destination.write_bytes(data)
    expected, actual = pixels(fixture), pixels(destination)
    assert actual == expected, "received image pixels differ from the private clipboard fixture"
    if caption:
        assert caption in "".join(parsed.text), event
    return dict(bytes=len(data), rgba_sha256=hashlib.sha256(actual).hexdigest(), caption=caption)


def colored_bounds(path):
    width, height = map(int, subprocess.check_output(["magick", "identify", "-format", "%w %h", str(path)]).split())
    data = subprocess.check_output(["magick", str(path), "-alpha", "off", "-depth", "8", "rgb:-"])
    bounds, counts = [], []
    for color in COLORS:
        points = [(i // 3 % width, i // 3 // width) for i in range(0, len(data), 3)
                  if all(abs(data[i + c] - color[c]) <= 12 for c in range(3))]
        counts.append(len(points))
        if points:
            bounds.extend(points)
    # Locate the actual drawn pane borders, independently of Rust's layout
    # constants or the terminal's incomplete GTK4 accessibility tree. The last
    # three long rules are timeline bottom, composer top and composer bottom.
    background = Counter(tuple(data[i:i+3]) for i in range(0, len(data)-2, 27)).most_common(1)[0][0]
    rules = []
    for y in range(60, height - 25):
        start = width * 4 // 5
        samples = Counter(tuple(data[(y*width+x)*3:(y*width+x)*3+3]) for x in range(start, width-20))
        for color, count in samples.most_common():
            if color == background or color in COLORS:
                continue
            if count > (width-start-20) * 0.7:
                if not rules or y > rules[-1][-1]+2:
                    rules.append([y])
                else:
                    rules[-1].append(y)
            break
    borders = [r[len(r)//2] for r in rules]
    return dict(counts=counts, bounds=([min(x for x, _ in bounds), min(y for _, y in bounds),
                                       max(x for x, _ in bounds), max(y for _, y in bounds)] if bounds else None),
                width=width, height=height, horizontal_borders=borders,
                timeline_bottom=borders[-3] if len(borders) >= 3 else None)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("binaries", type=Path)
    p.add_argument("--murmur", type=Path, required=True)
    p.add_argument("--terminal", type=Path, required=True, help="kgx (primary) or foot (secondary)")
    p.add_argument("--kind", choices=["kgx", "foot"], default="kgx")
    p.add_argument("--at-spi", type=Path, required=True)
    p.add_argument("--output", type=Path, required=True)
    p.add_argument("--fixture-only", action="store_true")
    p.add_argument("--permissive-focus", action="store_true", help="baseline diagnostics only: permit compositor focus stealing")
    p.add_argument("--xwayland", action="store_true", help="enable only the private compositor's owned XWayland bridge")
    p.add_argument("--expect-unavailable", action="store_true", help="prove pure-Wayland capability refusal without a popup")
    p.add_argument("--wayland-info", type=Path, default=shutil.which("wayland-info"))
    p.add_argument("--second-paste", choices=["control-v", "command", "deferred-control-v"], default="control-v",
                   help="alternate modes isolate terminal modifier/focus failures; diagnostic only")
    args = p.parse_args()
    args.binaries = args.binaries.resolve()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    saved_env = dict(os.environ)
    helpers, peer, app, plain = [], None, None, None
    with tempfile.TemporaryDirectory(prefix="nuschl-tui-media-") as temporary:
        root = Path(temporary)
        murmur = Murmur(args.murmur, root, free_port())
        murmur.ini.write_text(murmur.ini.read_text() + "messagelimit=1000\nmessageburst=1000\n")
        murmur.seed()
        murmur.start()
        shell = GnomeSession(width=1440, height=1000, log=(args.output / "gnome.log").open("w"), xwayland=args.xwayland)
        try:
            shell.start()
            shell.eval("global._nuschlFocusTrace=[]; global.display.connect('notify::focus-window',()=>{"
                       "const w=global.display.focus_window; global._nuschlFocusTrace.push({"
                       "at:imports.gi.GLib.get_monotonic_time(), title:w?.get_title()||null,"
                       "app:w?.get_wm_class()||null,pid:w?.get_pid()||null});}); 'ok'")
            shell.eval("global._nuschlWindowTrace=[]; global.display.connect('window-created',(_,w)=>{"
                       "global._nuschlWindowTrace.push({at:imports.gi.GLib.get_monotonic_time(),"
                       "title:w.get_title(),app:w.get_wm_class(),pid:w.get_pid()});}); 'ok'")
            focus_policy = "smart" if args.permissive_focus else "strict"
            shell.eval("new imports.gi.Gio.Settings({schema_id:'org.gnome.desktop.wm.preferences'})"
                       f".set_string('focus-new-windows',{json.dumps(focus_policy)}); 'ok'")
            actual_policy = shell.eval("new imports.gi.Gio.Settings({schema_id:'org.gnome.desktop.wm.preferences'})"
                                       ".get_string('focus-new-windows')")
            assert actual_policy == focus_policy
            (args.output / "focus-policy.json").write_text(json.dumps({"focus-new-windows": actual_policy}))
            env = shell.app_environment() | {"HOME": str(shell.home), "GSETTINGS_BACKEND": "memory",
                                             "NUSCHL_AUDIO_BACKEND": "pulseaudio", "PULSE_SERVER": "unix:/nonexistent-nuschl-test-audio"}
            env.pop("NO_AT_BRIDGE", None)
            env["GTK_A11Y"] = "atspi"
            # Automation suppresses terminal colors by default. An interactive
            # Console session should exercise the TUI's actual colored output.
            env.pop("NO_COLOR", None)
            env.pop("SSH_CONNECTION", None)
            env.pop("SSH_TTY", None)
            for key in ("TMUX", "STY", "KITTY_WINDOW_ID", "WEZTERM_EXECUTABLE"):
                env.pop(key, None)
            os.environ.clear()
            os.environ.update(env)
            if args.xwayland:
                (args.output / "private-xwayland.json").write_text(json.dumps({key: env[key] for key in ("DISPLAY", "XAUTHORITY")}))
            assert args.wayland_info, "pass --wayland-info or run with nixpkgs#wayland-utils"
            registry_info = subprocess.check_output([str(args.wayland_info)], env=env, timeout=10).decode()
            (args.output / "wayland-info.txt").write_text(registry_info)
            globals_ = sorted(set(re.findall(r"interface: '([^']+)'", registry_info)))
            (args.output / "wayland-globals.json").write_text(json.dumps(globals_, indent=2))

            def spawn(command, env, log):
                proc = subprocess.Popen(command, env=env, stdout=(args.output / log).open("w"),
                                        stderr=subprocess.STDOUT, start_new_session=True)
                helpers.append(proc)
                return proc
            registry = start_registry(args.at_spi, env, spawn)
            pin = certificate_pin(args.binaries, "127.0.0.1", murmur.port, root / "probe")
            peer = Client(args.binaries, "127.0.0.1", murmur.port, pin, "Media peer", root / "peer",
                          ["--message-limit", "1000", "--message-burst", "1000"])
            peer.send(kind="join", channel=2)
            profile = root / "tui"
            (profile / "config").mkdir(parents=True)
            (profile / "config/servers.json").write_text(json.dumps([dict(id=str(uuid.uuid4()), name="Private media server",
                host="127.0.0.1", port=murmur.port, username="Media TUI", certificate_sha256=pin)]))
            (profile / "config/settings.json").write_text(json.dumps({"appearance": {"render_math": True}}))
            capture = args.output / "terminal"
            command = [sys.executable, str(Path(__file__).resolve()), "--relay", str(capture),
                       str(args.binaries / "nuschl-tui"), "--no-audio", "--opener", "/bin/true",
                       "--profile-dir", str(profile), f"mumble://Media%20TUI@127.0.0.1:{murmur.port}/Lobby/Development"]
            terminal = ([str(args.terminal), "--title", TITLE, "--", *command] if args.kind == "kgx"
                        else [str(args.terminal), "--config", "/dev/null", "--title", TITLE, "--", *command])
            app = subprocess.Popen(terminal, env=env, stdout=(args.output / "terminal.log").open("w"),
                                   stderr=subprocess.STDOUT, start_new_session=True)
            eventually(lambda: shell.window(TITLE), "the private graphics terminal did not open")
            shell.place(TITLE, 40, 40, 1280, 800)
            eventually(lambda: (capture / "output.bin").exists() and "Development" in screen(capture),
                       "TUI did not draw the selected channel in the actual terminal", timeout=60)
            peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Media TUI" and e["user"]["channel"] == 2)
            # KGX applies its initial 80x24 grid after the first mapped-window
            # notification. Resize once it is actually running, then verify
            # both the compositor and child PTY accepted the real geometry.
            shell.place(TITLE, 40, 40, 1280, 800)
            eventually(lambda: shell.window(TITLE)["width"] > 1200, "KGX did not accept the normal window width")
            eventually(lambda: json.loads((capture / "size.json").read_text())["cols"] > 100,
                       "terminal resize did not reach the TUI's PTY")

            def terminal_accessible():
                for application in registry.children():
                    try:
                        for node, role, name, _ in items(application):
                            if role == "role 60":
                                return node
                    except RuntimeError:
                        continue
                return None

            (args.output / "accessibility.json").write_text(json.dumps([
                {"application": a.name, "nodes": [(r, n) for _, r, n, _ in items(a)]}
                for a in registry.children()], indent=2))
            window = shell.window(TITLE)
            shell.click(window["x"] + window["width"] // 2, window["y"] + window["height"] // 2)

            def shot(name):
                window = shell.window(TITLE)
                path = shell.screenshot(args.output / f"{name}.png", crop=(window["x"], window["y"], window["width"], window["height"]))
                (args.output / f"{name}.txt").write_text(screen(capture))
                return path

            # Distinct from the incoming graphic's primary colors: an outgoing
            # preview left on screen cannot make the receive-render check pass.
            fixture = png(root / "clipboard.png", colors=[(0, 200, 200), (200, 0, 200), (200, 100, 0), (100, 0, 200)])
            def publish_clipboard():
                shell.eval("const [,data]=imports.gi.GLib.file_get_contents(" + json.dumps(str(fixture)) + ");"
                           "imports.gi.St.Clipboard.get_default().set_content(imports.gi.St.ClipboardType.CLIPBOARD,"
                           " 'image/png',new imports.gi.GLib.Bytes(data)); 'ok'")
            publish_clipboard()
            shot("01-connected")
            if args.fixture_only:
                reads = []
                for index in range(2):
                    # Distinct content proves the bridge is fresh, not a stale
                    # X selection cached from the first Wayland publication.
                    png(fixture, colors=[tuple((c + index*37) % 256 for c in color) for color in COLORS])
                    publish_clipboard()
                    started = time.monotonic()
                    window_start = int(shell.eval("global._nuschlWindowTrace.length"))
                    targets = None
                    if args.xwayland:
                        # PNG is already published before the lazy X server's
                        # first connection. Do not re-copy after this probe.
                        target_probe = subprocess.run([shutil.which("xclip"), "-selection", "clipboard", "-target", "TARGETS", "-out"],
                                                      env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=5)
                        targets = dict(status=target_probe.returncode, types=target_probe.stdout.decode("ascii", "replace"))
                        time.sleep(0.1)
                    try:
                        command = ([shutil.which("xclip"), "-selection", "clipboard", "-target", "image/png", "-out"]
                                   if args.xwayland else [shutil.which("wl-paste"), "--type", "image/png", "--no-newline"])
                        got = subprocess.run(command,
                                             env=env, stdin=subprocess.DEVNULL, capture_output=True, timeout=5)
                        reads.append(dict(seconds=time.monotonic()-started, status=got.returncode,
                                          exact=got.stdout == fixture.read_bytes(), sha256=hashlib.sha256(got.stdout).hexdigest(),
                                          stderr=got.stderr.decode("utf8", "replace"),
                                          target_probe=targets,
                                          windows=json.loads(shell.eval(f"JSON.stringify(global._nuschlWindowTrace.slice({window_start}))")),
                                          focus=shell.eval("global.display.focus_window?.get_title()||''")))
                    except subprocess.TimeoutExpired:
                        reads.append(dict(seconds=time.monotonic()-started, timeout=True))
                (args.output / "standalone-clipboard.json").write_text(json.dumps(reads, indent=2))
                if args.xwayland:
                    assert all(r.get("exact") and r["status"] == 0 and not r["windows"] and r["focus"] == TITLE for r in reads), reads
                    assert reads[0]["sha256"] != reads[1]["sha256"], "bridge test did not use distinct images"
                print("Private terminal/clipboard fixture ready; media features NOT tested.", flush=True)
                return

            proof = {"terminal": args.kind, "capabilities": json.loads((capture / "capabilities.json").read_text()), "wire_images": [], "screens": []}
            proof["second_paste"] = args.second_paste
            proof["focus_policy"] = actual_policy
            proof["wayland_globals"] = globals_
            proof["acceptance"] = args.second_paste == "control-v" and not args.permissive_focus and not args.expect_unavailable

            def acknowledged(caption=None):
                eventually(lambda: "Image sent to the server." in screen(capture), "send acknowledgment did not clear the attachment")
                view = screen(capture)
                assert "1 image attached;" not in view and "Image sending…;" not in view
                if caption:
                    rows = view.splitlines()
                    at = next(i for i, row in enumerate(rows) if "Message to" in row)
                    assert caption not in "\n".join(rows[at+1:at+3]), "acknowledged caption remains in the composer"
            # No images have been sent yet: these new graphics can only be the
            # formula, not an image worker finishing another request.
            peer.send(kind="text", channel=2, body="```rust\nlet greeting = \"colorful terminal\";\nprintln!(\"{greeting}\");\n``` ")
            eventually(lambda: "greeting" in screen(capture), "code source did not arrive")
            before = graphics(capture)
            peer.send(kind="text", channel=2, body=f"A readable rendered formula: $${FORMULA}$$")
            eventually(lambda: (g["sixel"] > before["sixel"] or g["halfblocks"] > before["halfblocks"])
                       if (g := graphics(capture)) else False, "the terminal never rendered any formula graphics", timeout=45)
            eventually(lambda: "frac" in screen(capture), "formula source fallback disappeared")
            proof["formula_graphics"] = graphics(capture)
            shot("02-code-formula")
            clip_window_start = int(shell.eval("global._nuschlWindowTrace.length"))
            if args.expect_unavailable:
                for shortcut in (False, True):
                    shell.key("Escape")
                    if shortcut:
                        shell.chord("Control_L", "v")
                    else:
                        shell.type("/paste-image")
                        shell.key("Return")
                    eventually(lambda: any(word in screen(capture).splitlines()[-2].lower()
                                           for word in ("unsupported", "not supported", "unavailable", "data-control", "focus-free")),
                               "missing focus-free clipboard capability did not produce a clear refusal")
                    assert "timed out" not in screen(capture).splitlines()[-2].lower()
                proof["clipboard_unavailable"] = True
            else:
                shell.key("Escape")
                shell.type("/paste-image")
                shell.key("Return")
                eventually(lambda: "Clipboard image attached" in screen(capture), "explicit /paste-image did not attach a PNG")
                shell.key("Return")
                proof["wire_images"].append(received_image(peer, fixture, args.output / "received-command.png"))
                acknowledged()
                shot("02b-command-acknowledged")
                shell.key("Escape")
                caption = "Caption from native terminal"
                shell.type(caption)
                png(fixture, colors=[(20, 180, 220), (220, 40, 180), (230, 120, 20), (80, 20, 220)])
                publish_clipboard()
                if args.second_paste == "command":
                    shell.chord("Control_L", "u")
                    shell.type("/paste-image")
                    shell.key("Return")
                elif args.second_paste == "deferred-control-v":
                    pid = int((capture / "child.pid").read_text())
                    assert str(profile).encode() in Path(f"/proc/{pid}/cmdline").read_bytes(), "diagnostic TUI process is no longer current"
                    os.kill(pid, signal.SIGSTOP)
                    try:
                        shell.chord("Control_L", "v")
                        time.sleep(0.3)
                    finally:
                        os.kill(pid, signal.SIGCONT)
                else:
                    shell.chord("Control_L", "v")
                eventually(lambda: "Clipboard image attached" in screen(capture), "Ctrl+V did not attach a PNG")
                if args.second_paste == "command":
                    shell.type(caption)
                shell.key("Return")
                proof["wire_images"].append(received_image(peer, fixture, args.output / "received-control-v.png", caption))
                acknowledged(caption)
                assert proof["wire_images"][0]["rgba_sha256"] != proof["wire_images"][1]["rgba_sha256"], "second paste was not fresh content"
                shot("02c-caption-acknowledged")
            proof["graphics"] = graphics(capture)
            for index in range(16):
                peer.send(kind="text", channel=2, body=f"Older readable row {index:02}: before the incoming image.")
            eventually(lambda: "Older readable row 15" in screen(capture), "older scroll fixture messages did not arrive")
            # Bring a known image in last, independently of the outgoing path.
            image = base64.b64encode(png(root / "incoming.png").read_bytes()).decode()
            peer.send(kind="html", channel=2, html=f'Received image:<img src="data:image/png;base64,{image}">')
            for width, height in [(1280, 800), (900, 620), (1100, 760)]:
                shell.place(TITLE, 40, 40, width, height)
                time.sleep(1.2)
                path = shot(f"03-image-{width}")
                colors = colored_bounds(path)
                assert all(n >= 12 for n in colors["counts"]), f"actual terminal has no four-color rendered image: {colors}"
                assert colors["timeline_bottom"] is not None, f"could not locate visible timeline/composer borders: {colors}"
                assert colors["bounds"][3] < colors["timeline_bottom"], f"graphics painted over the composer/footer: {colors}"
                terminal_node = terminal_accessible()
                if terminal_node:
                    colors["terminal_bounds"] = terminal_node.extents()
                proof["screens"].append(colors)
            # User text is not a terminal control channel, even in a graphics terminal.
            marker = base64.b64encode(b"not-the-private-clipboard").decode()
            peer.send(kind="html", channel=2, html=f"inert control: \x1b]52;c;{marker}\x07")
            eventually(lambda: "inert control:" in screen(capture), "control canary did not reach the native TUI")
            assert f"\x1b]52;c;{marker}".encode() not in (capture / "output.bin").read_bytes(), "message executed OSC52"
            for _ in range(4):
                shell.key("Page_Up")
            time.sleep(0.6)
            scrolled = colored_bounds(shot("04-scrolled"))
            assert all(n < 12 for n in scrolled["counts"]), f"old graphics remained after scrolling to older content: {scrolled}"
            proof["scrolled"] = scrolled
            shell.key("Escape")
            shell.type("/status")
            shell.key("Return")
            time.sleep(0.5)
            assert app.poll() is None, "resize/scroll ended the TUI"
            # The same content remains useful without any graphics terminal.
            plain_env = environment(root / "plain-env") | {"SSH_CONNECTION": "127.0.0.1 1 127.0.0.1 2"}
            plain = Terminal("plain media fallback", [str(args.binaries / "nuschl-tui"), "--plain", "--no-audio",
                    "--profile-dir", str(root / "plain"), f"mumble://Plain@127.0.0.1:{murmur.port}/Lobby/Development"], plain_env, plain=True)
            plain.wait_for("Trust it with /trust")
            plain.line("/trust")
            plain.wait_for("Connected")
            peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Plain" and e["user"]["channel"] == 2)
            peer.send(kind="text", channel=2, body=f"Fallback formula: $${FORMULA}$$")
            plain.wait_for("frac")
            peer.send(kind="html", channel=2, html=f'<img src="data:image/png;base64,{image}">')
            plain.wait_for(lambda text: "image" in text.lower())
            plain.line("/paste-image")
            plain.wait_for("Image paste over SSH needs terminal-mediated image MIME support")
            assert "\x1bP" not in plain.output, "plain fallback emitted graphics"
            (args.output / "plain.txt").write_text(plain.output)
            proof["plain_fallback"] = True
            focus_trace = json.loads(shell.eval("JSON.stringify(global._nuschlFocusTrace)"))
            helper_windows = json.loads(shell.eval(f"JSON.stringify(global._nuschlWindowTrace.slice({clip_window_start}))"))
            proof["focus_trace"] = focus_trace
            proof["helper_windows"] = helper_windows
            if not args.permissive_focus:
                assert all(entry["title"] == TITLE for entry in focus_trace), f"a helper stole focus: {focus_trace}"
                assert not helper_windows, f"clipboard opened helper windows: {helper_windows}"
            (args.output / "proof.json").write_text(json.dumps(proof, indent=2))
            print(f"Native {args.kind}: formula graphics, receive rendering, resize, inert controls and plain fallback passed. "
                  + ("Pure-Wayland clipboard unavailability refused without helper windows."
                     if args.expect_unavailable else "Two fresh pixel-exact pasted images acknowledged. "
                     + (f"DIAGNOSTIC {args.second_paste}: strict ordinary Ctrl+V acceptance NOT proved." if not proof["acceptance"] else "Strict ordinary Ctrl+V acceptance passed.")), flush=True)
        finally:
            if shell.process and shell.process.poll() is None:
                (args.output / "focus-trace.json").write_text(shell.eval("JSON.stringify(global._nuschlFocusTrace||[])"))
                (args.output / "window-trace.json").write_text(shell.eval("JSON.stringify(global._nuschlWindowTrace||[])"))
            if plain:
                plain.close("/quit\r")
            if app and app.poll() is None:
                try:
                    shell.chord("Control_L", "q")
                    app.wait(timeout=8)
                except (subprocess.TimeoutExpired, RuntimeError):
                    os.killpg(app.pid, signal.SIGTERM)
            if peer:
                peer.close()
            for proc in helpers:
                if proc.poll() is None:
                    os.killpg(proc.pid, signal.SIGTERM)
            shell.stop()
            murmur.stop()
            os.environ.clear()
            os.environ.update(saved_env)


if __name__ == "__main__":
    if len(sys.argv) >= 4 and sys.argv[1] == "--relay":
        sys.exit(relay(Path(sys.argv[2]), sys.argv[3:]))
    main()
