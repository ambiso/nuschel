#!/usr/bin/env python3
"""The screenshot at the top of README.md, made reproducibly (`just readme-screenshot`).

A private nuschld on 127.0.0.1 with a small community's channel tree, a few
scripted people (nuschl-cli) in its channels, and the desktop in a private
headless GNOME Shell on the real GPU (never the user's session). In
#Development people chat (code, an image, a formula, an edit), Ada talks and
Lena types. The picture is cropped to Nuschl's window, so nothing else on the
private screen (top bar, notifications) can appear in it; every name, address
and file in it is made up here.

    NUSCHL_WL_TMP=/tmp/... python3 tests/readme_screenshot.py <binaries> --output docs/images/nuschl.png

Needs gnome-shell, dbus-run-session, gdbus and ImageMagick's `magick`; with
`oxipng` on the PATH the PNG is optimised too.
"""
import argparse
import base64
import json
import math
from pathlib import Path
import shutil
import struct
import subprocess
import sys
import tempfile
import threading
import time
import urllib.parse
import uuid
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gnome_session import GnomeSession  # noqa: E402
from local_nuschld import Nuschld, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin, tone  # noqa: E402
from seed_server import seed  # noqa: E402
from workers import exchange  # noqa: E402

SERVER = "Hackspace"
CHANNELS = [
    (1, 0, "Development"),
    (2, 0, "Games night"),
    (3, 2, "Board games"),
    (4, 0, "Lounge"),
    (5, 0, "Music"),
]
DEVELOPMENT = 1
# Who is where: (name, channel, muted, deafened). Ada, Jonas and Lena chat in
# #Development with Andi (the desktop); Ada talks.
PEOPLE = [
    ("Ada", 1, False, False),
    ("Jonas", 1, False, False),
    ("Lena", 1, True, False),
    ("Sam", 3, True, False),
    ("Tom", 3, True, False),
    ("Mira", 4, True, False),
    ("Kai", 4, True, True),
]
CODE = """Found the reconnect bug: nothing resets the attempt counter after a login.

```rust
fn next_delay(&mut self) -> Duration {
    self.attempt = (self.attempt + 1).min(6);
    Duration::from_millis(250 << self.attempt)
}
```"""
FORMULA = ("And the jitter estimate from RFC 3550, for the docs: "
           "$$J_i = J_{i-1} + \\frac{|D_{i-1,i}| - J_{i-1}}{16}$$")


class Server(Nuschld):
    """nuschld with the community's name and no test banner."""

    def write_config(self, message_limit, message_burst):
        super().write_config(message_limit, message_burst)
        self.config.write_text(self.config.read_text()
                               .replace('welcome_text = "Welcome to the <b>Nuschl</b> test server."',
                                        f'registered_name = "{SERVER}"\nwelcome_text = ""'))


def png(width, height, pixel):
    rows = b"".join(b"\x00" + bytes(channel for x in range(width) for channel in pixel(x, y))
                    for y in range(height))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows, 9)) + chunk(b"IEND", b""))


def landscape(width=300, height=160):
    """A sunset over hills and a lake, drawn here (no photograph, nothing personal)."""
    horizon = int(height * 0.72)
    sun_x, sun_y, sun_r = width * 0.68, horizon - height * 0.12, height * 0.075

    def mix(a, b, t):
        t = max(0.0, min(1.0, t))
        return tuple(int(a[i] + (b[i] - a[i]) * t) for i in range(3))

    def ridge(x, base, amplitude, phase):
        return base - amplitude * (0.55 * math.sin(x / width * 5.1 + phase) + 0.3 * math.sin(x / width * 13.7 + phase * 2)
                                   + 0.15 * math.sin(x / width * 31.0 + phase * 3))

    hills = [(horizon - height * 0.20, height * 0.10, 0.7, (88, 64, 110)),
             (horizon - height * 0.11, height * 0.08, 2.3, (58, 42, 82)),
             (horizon - height * 0.03, height * 0.05, 4.1, (34, 26, 52))]

    def sky(x, y):
        colour = mix((32, 40, 98), (238, 140, 92), (y / horizon) ** 1.6)
        distance = math.hypot(x - sun_x, y - sun_y)
        if distance < sun_r:
            return (255, 226, 160)
        return mix(colour, (255, 196, 128), 0.55 * math.exp(-(distance - sun_r) / (height * 0.12)))

    def scene(x, y):
        colour = sky(x, y)
        for base, amplitude, phase, hill in hills:
            if y >= ridge(x, base, amplitude, phase):
                colour = hill
        return colour

    def pixel(x, y):
        if y < horizon:
            return scene(x, y)
        # The lake: the scene mirrored, darker, with ripples.
        depth = y - horizon
        mirrored = horizon - 1 - depth + int(1.5 * math.sin(x * 0.35 + depth * 0.9))
        colour = scene(x, max(0, mirrored))
        return mix(colour, (20, 24, 48), 0.35 + 0.4 * depth / (height - horizon))

    return png(width, height, pixel)


def register(binaries, port, pin, name, profile):
    client = Client(binaries, "127.0.0.1", port, pin, name, profile)
    client.send(kind="register")
    client.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == client.session
                and event["user"]["user_id"] is not None)
    client.close()


def conversation(client, channel):
    for event in list(client.events):
        if event["kind"] == "channel_info" and event["channel"] == channel:
            return event["conversation"]
    return client.wait(lambda event: event["kind"] == "channel_info" and event["channel"] == channel)["conversation"]


class Person:
    def __init__(self, binaries, port, pin, root, name, channel, muted, deafened):
        self.client = Client(binaries, "127.0.0.1", port, pin, name, root / name.lower())
        self.client.send(kind="join", channel=channel)
        self.client.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == self.client.session
                         and event["user"]["channel"] == channel)
        if not muted or deafened:
            self.client.send(kind="mute", muted=muted, deafened=deafened)
            self.client.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == self.client.session
                             and event["user"]["self_muted"] == muted and event["user"]["self_deafened"] == deafened)
        self.request = 100

    def say(self, body):
        self.client.send(kind="text", channel=DEVELOPMENT, body=body)
        acked = self.client.wait(lambda event: event["kind"] in ("chat_acked", "chat_failed"))
        assert acked["kind"] == "chat_acked", acked
        time.sleep(0.6)
        return acked["seq"]

    def edit(self, target, content):
        self.request += 1
        self.client.send(kind="chat_edit", request_id=self.request,
                         conversation=conversation(self.client, DEVELOPMENT), target=target, content=content)
        self.client.wait(lambda event: event.get("request_id") == self.request and event["kind"] == "chat_ack")
        time.sleep(0.6)

    def close(self):
        if self.client.process.poll() is None:
            self.client.process.kill()


def window_picture(shell, output):
    """Nuschl's window from a screenshot of the private screen, as an RGBA PNG.

    The frame rectangle may include a margin around what Nuschl draws (the
    shell's background and a faint shadow), depending on when the client set
    its window geometry: make the background transparent from the corners
    inwards, which also clears the rounded corners, then trim what became
    transparent. Returns the picture's size."""
    window = shell.window("Nuschl")
    raw = output.resolve().with_name(output.stem + "-screen.png")  # the shell needs an absolute path
    shell.screenshot(raw)
    crop = f"{window['width']}x{window['height']}+{window['x']}+{window['y']}"
    corners = [f"color {x},{y} floodfill" for x, y in ((0, 0), (window["width"] - 1, 0), (0, window["height"] - 1),
                                                     (window["width"] - 1, window["height"] - 1))]
    subprocess.run(["magick", str(raw), "-crop", crop, "+repage", "-alpha", "set", "-fuzz", "12%", "-fill", "none",
                    *[argument for corner in corners for argument in ("-draw", corner)],
                    "-trim", "+repage", "-strip", f"PNG32:{output}"], check=True, timeout=30)
    raw.unlink()
    return tuple(map(int, subprocess.run(["magick", "identify", "-format", "%w %h", str(output)], check=True,
                                         capture_output=True, text=True, timeout=30).stdout.split()))


def talk(person, packets, stop):
    """Send voice in 20 ms frames until `stop` is set, as a microphone would."""
    sequence = 0
    started = time.monotonic()
    while not stop.is_set():
        packet = packets[sequence // 2 % len(packets)]
        person.client.send(kind="voice", sequence=sequence, opus=packet)
        sequence += 2
        time.sleep(max(0.0, started + sequence * 0.01 - time.monotonic()))
    person.client.send(kind="voice", sequence=sequence, opus=packets[0], end=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--output", type=Path, required=True, help="the PNG to write")
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--size", default="1078x782", help="the picture (what Nuschl draws), in pixels")
    parser.add_argument("--logs", type=Path, help="keep the server's, the shell's and the desktop's logs here")
    args = parser.parse_args()
    width, height = map(int, args.size.split("x"))
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschl-readme-") as temporary:
        root = Path(temporary)
        logs = args.logs or root / "logs"
        logs.mkdir(parents=True, exist_ok=True)
        port = free_port()
        (root / "server").mkdir()
        server = Server(args.binaries, root / "server", port)
        server.write_config(message_limit=100, message_burst=100)
        server.start()
        shell = GnomeSession(args.gnome_shell, width + 160, height + 100, log=(logs / "gnome-shell.log").open("w"))
        people = []
        app = None
        stop = threading.Event()
        talker = None
        try:
            pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
            seed(args.binaries, "127.0.0.1", port, server.password, CHANNELS, root / "seed")
            profile = root / "profile"
            (profile / "config").mkdir(parents=True)
            register(args.binaries, port, pin, "Andi", profile)
            for name in ("Ada", "Jonas", "Lena"):
                register(args.binaries, port, pin, name, root / name.lower())
            people = [Person(args.binaries, port, pin, root, *person) for person in PEOPLE]
            ada, jonas, lena = people[:3]
            shell.start()

            bookmark = {"id": str(uuid.uuid4()), "name": SERVER, "host": "127.0.0.1", "port": port,
                        "username": "Andi", "certificate_sha256": pin, "auto_connect": False}
            (profile / "config/servers.json").write_text(json.dumps([bookmark]))
            (profile / "config/settings.json").write_text(json.dumps({"appearance": {"theme": "dark"}}))
            # The link connects without deafening (as at startup), so Andi hears Ada.
            app = subprocess.Popen([str(args.binaries / "nuschl"), "--profile-dir", str(profile),
                                    f"mumble://Andi@127.0.0.1:{port}/Development"],
                                   env=shell.app_environment() | {"RUST_LOG": "nuschl=info"},
                                   stdout=(logs / "desktop.log").open("w"), stderr=subprocess.STDOUT,
                                   start_new_session=True)
            eventually(lambda: app.poll() is None and shell.window("Nuschl"), "Nuschl window did not appear")

            def placed(frame_width, frame_height):
                shell.place("Nuschl", 40, 40, frame_width, frame_height)
                time.sleep(0.3)
                window = shell.window("Nuschl")
                return window["width"] == frame_width and window["height"] == frame_height

            # Size the frame so that what Nuschl draws has the requested size
            # (the frame's margin, if any, is known once it is drawn).
            frame = (width, height)
            for _ in range(3):
                eventually(lambda: placed(*frame), "Nuschl window could not be placed")
                time.sleep(1.0)
                drawn = window_picture(shell, root / "probe.png")
                if drawn == (width, height):
                    break
                frame = (frame[0] + width - drawn[0], frame[1] + height - drawn[1])
            else:
                raise AssertionError(f"Nuschl draws {drawn}, not {width}x{height}")
            ada.client.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                            and e["user"]["channel"] == DEVELOPMENT, timeout=30)
            time.sleep(2.0)

            jonas.say("Morning! 0.2 is tagged as a release candidate. Who has time to test tonight?")
            # Andi answers from the composer.
            window = shell.window("Nuschl")
            margin = (window["height"] - height) // 2
            for _ in range(2):  # the first click after mapping can be swallowed
                shell.click(window["x"] + window["width"] // 2 + 90, window["y"] + margin + height - 62)
                time.sleep(0.4)
            reply = "I'll take the Windows build after dinner."
            shell.type(reply)
            shell.key("Return")
            ada.client.wait(lambda e: e["kind"] == "text" and e["body"] == reply, timeout=15)
            # Park the pointer away from the window, so no hover state shows.
            shell.move(window["x"] + width + 60, window["y"] + height + 60)
            time.sleep(0.6)
            typo = ada.say(CODE.replace("after a login", "afetr a login"))
            ada.edit(typo, CODE)
            # Encoded as stock Mumble's Log::imageToImg does: base64 in
            # 72-character lines, each percent-encoded.
            encoded = base64.b64encode(landscape()).decode()
            image = "\n".join(urllib.parse.quote(encoded[i:i + 72], safe="") for i in range(0, len(encoded), 72))
            jonas.client.send(kind="html", channel=DEVELOPMENT, html=(
                f"Nice catch! Unrelated: the view from the hut on Saturday.<br/>"
                f"<img src=\"data:image/PNG;base64,{image}\" />"))
            time.sleep(0.8)
            lena.say(FORMULA)
            time.sleep(1.0)

            # Ada talks while Lena types.
            packets = [packet["value"] for packet in exchange(
                args.binaries / "nuschl-media-worker",
                [{"kind": "encode_opus", "samples": tone(frame * 960, 960)} for frame in range(25)])]
            talker = threading.Thread(target=talk, args=(ada, packets, stop), daemon=True)
            talker.start()
            # The math worker's first formula, the image and the talking ring
            # need a moment; typing shows for a few seconds after each notice.
            for _ in range(2):
                lena.client.send(kind="typing", conversation=conversation(lena.client, DEVELOPMENT))
                time.sleep(1.5)

            w, h = window_picture(shell, args.output)
            stop.set()
            assert (w, h) == (width, height), f"the window changed size: {w}x{h}"
            if shutil.which("oxipng"):
                subprocess.run(["oxipng", "--quiet", "--opt", "max", "--strip", "all", str(args.output)],
                               check=True, timeout=300)
            print(f"{args.output}: {w}x{h}, {args.output.stat().st_size} bytes", flush=True)
        except BaseException:
            if not args.logs:
                kept = Path(tempfile.mkdtemp(prefix="nuschl-readme-logs-"))
                shutil.copytree(logs, kept, dirs_exist_ok=True)
                shutil.copy(server.log, kept)
                print(f"logs kept in {kept}", file=sys.stderr, flush=True)
            raise
        finally:
            stop.set()
            if talker:
                talker.join(timeout=5)
            for person in people:
                person.close()
            if app:
                app.terminate()
                try:
                    app.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    app.kill()
            shell.stop()
            server.stop()


if __name__ == "__main__":
    main()
