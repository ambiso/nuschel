#!/usr/bin/env python3
"""The desktop's voice bitrate: requested, effective and why, on real servers.

The real desktop in a private headless GNOME Shell, operated over AT-SPI,
against two loopback stock Mumble servers: "Wide" at Murmur's own default
budget (558 kb/s) and "Narrow" at 72 kb/s, each with a private D-Bus so
`setConf("bandwidth")` changes its limit live. A private PulseAudio's null
sink with a 440 Hz tone is the microphone; transmission is continuous; a
`nuschl-cli` peer on each server records the Opus frames it receives. No host
audio device, profile or recording is used. Checks and screenshots:

- a profile without the setting starts at 128 kb/s and writes no new file;
- Settings → VOICE QUALITY names each server below the requested rate with
  its effective rate and reason, and the rest in one line; choosing 256 kb/s
  writes voice-quality.json only (settings.json keeps the keys Nuschl 0.1.0
  reads);
- the change reconfigures the encoder only: the same media worker and
  capture stream, frames up to the new per-frame allowance;
- the voice card shows each server's rate; speaking on both lowers Wide to
  Narrow's 48 kb/s ("shared with another server");
- a live ServerConfig (setConf) lowers Narrow's rate, and one too small for
  voice stops sending there without stopping playback;
- narrow and zoomed layouts (layout-narrow.json): a window requested at
  760×700 at 175 % zoom (four Ctrl+= steps). The compositor commits the
  application's 860 px minimum width, and the run records both sizes.
  1280×800 at 100 % is the gated control; 860×520 at 200 % is recorded but
  not gated, because the sidebar's width budget keeps it cramped. Synthetic
  timeline content in Lobby: a long link, a long code token, a wide image,
  joins and a file offer. Checks:
  * walking the Root and Lobby timelines entry by entry with the keyboard,
    every laid-out accessible node except push buttons stays inside the
    timeline, short of its scrollbar gutter. No text or image pixel lands
    in that gutter. The floating "Jump to latest" pill is an overlay, not
    timeline content: its rows of the gutter are excluded and its own
    bounds are recorded.
    Entries that are not laid out are 1x1 stand-ins with no real bounds;
    they are counted, not measured;
  * "You joined Lobby" is one line at 100 % and at most five lines at 175 %
    (the defect stacked it one glyph per line);
  * every voice destination's name and hang-up can be revealed inside the
    voice card by focus, by the mouse wheel (turned to the end), and by Tab
    into the card then End. A visible scrollbar thumb is painted whenever
    the card overflows. Whether some intermediate wheel position shows the
    last hang-up whole is recorded, not gated;
  * no interactive accessible node is unnamed.

    python3 tests/bitrate_desktop.py <binaries> --murmur <mumble-server> \\
        --at-spi <at-spi2-core> --pulse-bin <pulseaudio>/bin --output <directory> [--gnome-shell ...]

The pixel checks use ImageMagick (`MAGICK`, default `magick`).
"""
import argparse
import base64
import json
import os
from pathlib import Path
import signal
import struct
import subprocess
import sys
import tempfile
import time
import urllib.parse
import uuid
import zlib

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import FOCUSED, INTERACTIVE, ROLES, Node, snapshot, start_registry  # noqa: E402
from bitrate_matrix import Bus, allowance, received  # noqa: E402
from desktop_mumble import media_workers  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402

WIDTH, HEIGHT = 1280, 860
RELEASED_KEYS = {
    "appearance", "audio", "auto_reconnect", "check_updates", "enter_sends", "force_tcp",
    "friend_notifications", "friends", "key_transparency", "local_mutes", "nicknames", "notifications",
    "recovery_key", "relay_only", "send_typing", "server_icons", "server_volumes", "share_web_host",
    "shortcuts", "sidebar_width", "sound_effects", "sound_volume", "user_volumes", "wormhole_relay",
    "wormhole_rendezvous",
}
# (label, requested width, height, Ctrl+= steps from 100 %, zoom, gated). The
# 100 % control runs first: it measures the single-line height that the
# zoomed line counts are relative to.
GEOMETRIES = [
    ("control-1280x800-100", 1280, 800, 0, 1.0, True),
    ("requested-760x700-175", 760, 700, 4, 1.75, True),
    ("minimum-860x520-200", 860, 520, 5, 2.0, False),
]
# The timeline's scrollbar area, in zoom-aware logical pixels (view.rs).
TIMELINE_GUTTER = 12
CONTROL_ROLES = {"push button", "toggle button", "menu item", "check box", "radio button"}
THUMB = (0x35, 0x3E, 0x4E)  # dark theme BORDER_STRONG
# Every dark-theme text colour (TEXT_FAINT, the faintest, is 0x949fb2) has a
# channel at or above this; surfaces, the scrollbar track and its thumb are
# all below it.
BRIGHT = 0x70
JOINED = "You joined Lobby"
OFFER_NAME = "holiday-photos-from-the-long-trip-2026.zip"


def make_png(width, height):
    """A synthetic gradient PNG, standard library only."""
    rows = b"".join(b"\x00" + bytes(channel for x in range(width)
                                    for channel in (x * 255 // width, y * 255 // height, 160))
                    for y in range(height))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


def inside(box, outer):
    x, y, w, h = box
    ox, oy, ow, oh = outer
    return w > 0 and h > 0 and x >= ox - 1 and y >= oy - 1 and x + w <= ox + ow + 1 and y + h <= oy + oh + 1


class DesktopBitrate:
    def __init__(self, args, root):
        self.args, self.root = args, root
        self.processes, self.peers, self.servers, self.results = [], {}, {}, []
        self.shell = None

    def spawn(self, command, env, log):
        process = subprocess.Popen(command, env=env, stdout=(self.args.output / log).open("w"),
                                   stderr=subprocess.STDOUT, start_new_session=True)
        self.processes.append(process)
        return process

    def start_pulse(self):
        directory = self.shell.runtime / "pulse"
        directory.mkdir(mode=0o700, exist_ok=True)
        self.socket = directory / "native"
        script = self.shell.runtime / "default.pa"
        script.write_text(
            f"load-module module-native-protocol-unix socket={self.socket} auth-anonymous=1\n"
            "load-module module-null-sink sink_name=speakers rate=48000 sink_properties=device.description=Speakers\n"
            "load-module module-null-sink sink_name=microphone rate=48000 sink_properties=device.description=Mic\n"
            "load-module module-sine sink=microphone frequency=440\n"
            "set-default-sink speakers\nset-default-source microphone.monitor\n")
        self.spawn([str(self.args.pulse_bin / "pulseaudio"), "--daemonize=no", "--use-pid-file=no",
                    "--exit-idle-time=-1", "-n", "--file=" + str(script)],
                   self.shell.environment() | {"PULSE_RUNTIME_PATH": str(directory),
                                               "PULSE_STATE_PATH": str(self.shell.runtime / "pulse-state")},
                   "pulse.log")
        eventually(self.socket.exists, "private PulseAudio did not start")

    def pactl(self, *arguments):
        result = subprocess.run([str(self.args.pulse_bin / "pactl"), f"--server=unix:{self.socket}",
                                 "--format=json", *arguments], capture_output=True, text=True, check=True,
                                timeout=10)
        return json.loads(result.stdout)

    def capture_streams(self):
        """(stream, owning process) of every microphone capture: the same
        pair before and after a change means neither the capture stream nor
        the media worker that holds it was replaced."""
        sources = {source["index"]: source["name"] for source in self.pactl("list", "sources")}
        return sorted((stream["index"], stream["client"])
                      for stream in self.pactl("list", "source-outputs")
                      if sources.get(stream["source"]) == "microphone.monitor")

    def playback_streams(self):
        return [stream["index"] for stream in self.pactl("list", "sink-inputs")
                if stream["properties"].get("application.name") == "Nuschl"]

    # AT-SPI.

    def nodes(self):
        return snapshot(self.nuschl)

    def find(self, role, name, prefix=False, timeout=15):
        def found():
            for _, node, node_role, node_name in self.nodes():
                if node_role == role and (node_name.startswith(name) if prefix else node_name == name):
                    return node
            return None
        return eventually(found, f"no {role} named {name!r}", timeout=timeout)

    def press(self, role, name, prefix=False):
        node = self.find(role, name, prefix)
        assert node.call("Action", "DoAction", "i", "0")[0] is not False
        time.sleep(0.4)

    def named(self, text, timeout=15):
        def found():
            return next((name for _, _, role, name in self.nodes()
                         if name == text and role != "radio button"), None)
        return eventually(found, f"nothing named {text!r}; names with 'kb/s': "
                          f"{[n for _, _, _, n in self.nodes() if 'kb/s' in n]}", timeout=timeout)

    def select(self, server):
        self.press("push button", f"{server};", prefix=True)

    def shot(self, name):
        time.sleep(0.8)
        window = self.shell.window("Nuschl")
        path = self.shell.screenshot(self.args.output / f"bitrate-{name}.png",
                                     crop=(window["x"] - 12, window["y"] - 12, window["width"] + 24,
                                           window["height"] + 24))
        print(path, flush=True)

    # The servers.

    def own(self, server):
        return next(user for user in self.peers[server].users.values() if user["name"] == "Andi")

    def frames(self, server, seconds=3):
        return received(self.peers[server], self.own(server)["session"], seconds)

    def check(self, server, step, effective, frames=None):
        frames = self.frames(server) if frames is None else frames
        if effective == 0:
            assert not frames, f"{server} {step}: sent past a budget too small for voice"
            largest, mean = 0, 0
        else:
            assert frames, f"{server} {step}: no voice received"
            largest, mean = max(frames), sum(frames) * 8 * 50 / len(frames)
            assert largest <= allowance(effective), f"{server} {step}: {largest} B > {allowance(effective)}"
        self.results.append(dict(server=server, step=step, effective=effective, frames=len(frames),
                                 largest=largest, allowance=allowance(effective) if effective else 0,
                                 mean_bps=round(mean)))
        print(f"  {server:<7} {step:<44} {len(frames):>3} frames, largest {largest:>3} B, "
              f"mean {mean / 1000:.1f} kb/s", flush=True)

    def recovered(self, server, step):
        """Stock Murmur polices each user over its last 360 accepted packets
        and records no refused one: after a live reduction it refuses even
        conforming frames until that history decays. Seconds until it accepts."""
        started = time.monotonic()
        while time.monotonic() - started < 90:
            if self.frames(server, 1):
                seconds = time.monotonic() - started
                self.results.append(dict(server=server, step=f"{step}: first accepted frame after",
                                         seconds=round(seconds, 1)))
                print(f"  {server:<7} {step:<44} first accepted frame after {seconds:.1f} s", flush=True)
                return seconds
        raise AssertionError(f"{server} {step}: no voice within 90 s")

    def files(self):
        config = self.profile / "config"
        settings = json.loads((config / "settings.json").read_text()) if (config / "settings.json").exists() else {}
        sidecar = config / "voice-quality.json"
        return settings, (json.loads(sidecar.read_text()) if sidecar.exists() else None)

    # Narrow and zoomed layouts.

    def tree(self):
        """Every accessible node from the Cache interface in one call:
        {path: (node, role, name)} and {parent path: [child paths]}."""
        app = self.nuschl
        data = app.bus("call", app.destination, "/org/a11y/atspi/cache", "org.a11y.atspi.Cache", "GetItems")[0]
        info, children = {}, {}
        for (destination, path), _, (_, parent), _, _, _, name, role, _, _ in data:
            info[path] = (Node(app.bus, destination, path), ROLES.get(role, f"role {role}"), name)
            children.setdefault(parent, []).append(path)
        return info, children

    def node(self, name, role=None, timeout=15):
        def found():
            info, _ = self.tree()
            return next((node for node, node_role, node_name in info.values()
                         if node_name == name and (role is None or node_role == role)), None)
        return eventually(found, f"no {role or 'node'} named {name!r}", timeout=timeout)

    @staticmethod
    def extents(node):
        try:
            return list(node.extents())
        except RuntimeError:
            return None

    def zoom(self):
        settings, _ = self.files()
        return settings.get("appearance", {}).get("zoom", 1.0)

    def rects(self):
        return json.loads(self.shell.eval(
            "JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window)"
            ".find(w=>(w.get_title()||'').startsWith('Nuschl'));const f=w.get_frame_rect(),"
            "b=w.get_buffer_rect();return {frame:[f.x,f.y,f.width,f.height],"
            "buffer:[b.x,b.y,b.width,b.height]};})())"))

    def settled_rects(self):
        """The compositor's rectangles once two samples agree."""
        previous, deadline = None, time.monotonic() + 8
        while time.monotonic() < deadline:
            current = self.rects()
            if current == previous:
                return current
            previous = current
            time.sleep(0.4)
        return previous

    def park_pointer(self):
        bx, by, _, _ = self.rects()["buffer"]
        # Over the title strip: no hover wash on rows, no hovered thumb.
        self.shell.move(bx + 140, by + 30)
        time.sleep(0.3)

    def pixels(self, shot, box):
        """The RGB pixels of a screen-coordinate box, clipped to the screen."""
        x, y, w, h = (round(value) for value in box)
        x0, y0 = max(x, 0), max(y, 0)
        x1, y1 = min(x + w, WIDTH + 160), min(y + h, HEIGHT + 100)
        if x1 <= x0 or y1 <= y0:
            return 0, 0, b""
        raw = subprocess.run([os.environ.get("MAGICK", "magick"), str(shot), "-crop",
                              f"{x1 - x0}x{y1 - y0}+{x0}+{y0}", "+repage", "-depth", "8", "rgb:-"],
                             check=True, capture_output=True, timeout=30).stdout
        assert len(raw) == (x1 - x0) * (y1 - y0) * 3, (box, len(raw))
        return x1 - x0, y1 - y0, raw

    def post_content(self):
        """Synthetic messages from the Narrow peer into Narrow's Lobby, where
        Andi is (a channel message reaches only the channel's members);
        Murmur allows a text message a second. The peer's joins go between
        Lobby and Music, never Root, whose timeline keeps the standalone
        "You joined Lobby" line the defect was recorded on."""
        peer = self.peers["Narrow"]
        url = ("https://example.org/" + "-".join(["averyveryverylongpathsegment"] * 3)
               + "?query=unbrokentoken0123456789")
        token = "unbroken_identifier_" + "x" * 44
        encoded = base64.b64encode(make_png(480, 200)).decode()
        image = "\n".join(urllib.parse.quote(encoded[i:i + 72], safe="") for i in range(0, len(encoded), 72))
        offer = f"📎 {OFFER_NAME} (48 MB) — receive with magic-wormhole: wormhole receive 7-guitarist-revenge"
        for command in (dict(kind="text", channel=1, body=f"A long link: {url}"),
                        dict(kind="text", channel=1, body=f"```\n{token}\n```"),
                        dict(kind="html", channel=1,
                             html=f"A wide picture:<br/><img src=\"data:image/PNG;base64,{image}\" />"),
                        dict(kind="text", channel=1, body=offer)):
            peer.send(**command)
            time.sleep(1.3)
        # Moves out of and back into Lobby: a run of activity there.
        for channel in (6, 1, 6, 1):
            peer.send(kind="join", channel=channel)
            time.sleep(1.3)

        self.press("tree item", "Lobby")

        def arrived():
            info, _ = self.tree()
            names = [name for _, role, name in info.values() if role == "list item"]
            return any(OFFER_NAME in name for name in names) and names
        eventually(arrived, "the synthetic content did not reach the timeline", timeout=40)
        time.sleep(2.0)

    def focus_timeline(self, timeline):
        """The keyboard's way in: Shift+Tab from the composer."""
        self.node("Message", "entry").call("Component", "GrabFocus")
        time.sleep(0.5)

        def focused():
            # Focus anywhere in the timeline: the list itself (its newest
            # entry active) or a tab stop inside an entry, such as an
            # expandable run of activity, whose keys bubble to the list.
            app = self.nuschl
            data = app.bus("call", app.destination, "/org/a11y/atspi/cache", "org.a11y.atspi.Cache",
                           "GetItems")[0]
            children, states = {}, {}
            for (_, path), _, (_, parent), _, _, _, _, _, _, state in data:
                children.setdefault(parent, []).append(path)
                states[path] = state[0]
            stack, seen = [timeline.path], set()
            while stack:
                path = stack.pop()
                if path in seen:
                    continue
                seen.add(path)
                if states.get(path, 0) & FOCUSED:
                    return True
                stack.extend(children.get(path, []))
            return False
        for _ in range(4):
            self.shell.chord("Shift_L", "Tab")
            time.sleep(0.5)
            if focused():
                return True
        return False

    def timeline_pass(self, label, zoom, directory, channel):
        """Walk a channel's timeline from the newest entry up to the welcome,
        one entry per Up, measuring every laid-out node and the gutter pixels."""
        # At high zoom the channel can be outside the tree's small viewport.
        # GPUI's accessible click lands at the row's centre (outside the
        # tree's clip) and it ignores ScrollTo, so use the keyboard instead:
        # focus the row, then Enter.
        item = self.node(channel, "tree item")
        item.call("Component", "GrabFocus")
        eventually(lambda: item.focused, f"the {channel} row did not take focus", timeout=5)
        self.shell.key("Return")
        time.sleep(0.4)
        welcome_name = f"Welcome to {channel}"
        timeline = self.node(f"Messages in #{channel}", "list")
        record = dict(focused=self.focus_timeline(timeline), steps=[], violations=[], controls=[],
                      joined_heights=[], not_laid_out=0, laid_out=0, top_reached=False, pill=[])
        self.shell.key("End")
        time.sleep(0.6)
        for step in range(80):
            self.park_pointer()
            info, children = self.tree()
            box = self.extents(timeline)
            left, right = box[0], box[0] + box[2] - round(TIMELINE_GUTTER * zoom)
            stack, nodes = list(children.get(timeline.path, [])), []
            while stack:
                path = stack.pop()
                if path in info:
                    nodes.append(info[path])
                    stack.extend(children.get(path, []))
            welcome = False
            for node, role, name in nodes:
                extents = self.extents(node)
                if not extents or extents[2] <= 1 or extents[3] <= 1:
                    record["not_laid_out"] += 1
                    continue
                record["laid_out"] += 1
                x, _, w, h = extents
                over = max(left - x, x + w - right)
                entry = dict(step=step, role=role, name=name[:80], extents=extents, over=over)
                if over > 1:
                    (record["controls"] if role in CONTROL_ROLES else record["violations"]).append(entry)
                if role == "label" and name == JOINED:
                    record["joined_heights"].append(h)
                if role == "label" and name == welcome_name:
                    welcome = True
            bx, by, _, _ = self.rects()["buffer"]
            shot = directory / f"{label}-{channel}-step{step:02}.png"
            self.shell.screenshot(shot)
            strip = (bx + right + 1, by + box[1], box[0] + box[2] - right - 1, box[3])
            width, height, raw = self.pixels(shot, strip)
            # The "Jump to latest" pill floats over the timeline while it is
            # scrolled up. It is an overlay, not timeline content: its rows
            # of the strip are excluded, and its own bounds are reported.
            pill = next((self.extents(node) for node, role, name in info.values()
                         if role == "push button" and name.endswith("Jump to latest")), None)
            skip = range(0)
            if pill and pill[2] > 1 and pill[3] > 1:
                skip = range(pill[1] - box[1], pill[1] - box[1] + pill[3])
                record["pill"].append(dict(step=step, extents=pill,
                                           over=max(box[0] - pill[0], pill[0] + pill[2] - box[0] - box[2])))
            bright = sum(1 for i in range(0, len(raw), 3)
                         if (i // 3) // max(width, 1) not in skip and max(raw[i:i + 3]) >= BRIGHT)
            record["steps"].append(dict(step=step, timeline=box, gutter_strip=strip, gutter_pixels=width * height,
                                        bright_pixels=bright, png=shot.name))
            if welcome:
                record["top_reached"] = True
                break
            self.shell.key("Up")
            time.sleep(0.5)
        self.shell.key("End")
        time.sleep(0.6)
        record["timeline_logical_width"] = round(box[2] / zoom, 1)
        return record

    def voice_pass(self, label, zoom, directory):
        """Reveal each destination's name and hang-up inside the voice card by
        focus, wheel and Tab-then-End, and look for the painted thumb."""
        region = self.node("Voice destinations")
        info, _ = self.tree()
        links = [node for node, role, name in info.values() if role == "push button" and name.startswith("Go to ")]
        hangups = [node for node, role, name in info.values()
                   if role == "push button" and name.startswith("Disconnect from ") and name.endswith("(voice and chat)")]
        record = dict(destinations=len(hangups), focus={}, wheel=None, tab_end=None)
        assert links and hangups, "no voice destinations"
        box = self.extents(region)
        centre = None

        def to_top():
            nonlocal centre
            bx, by, _, _ = self.rects()["buffer"]
            centre = (bx + box[0] + box[2] // 2, by + box[1] + box[3] // 2)
            self.shell.wheel(*centre, -30)
            time.sleep(0.8)
        to_top()
        self.park_pointer()
        last = max(hangups, key=lambda node: self.extents(node)[1])
        first = min(links, key=lambda node: self.extents(node)[1])
        record["region"] = box
        record["last"] = last.name
        record["last_at_top"] = self.extents(last)
        record["overflows"] = not inside(self.extents(last), box)
        # The painted thumb, read only from the screenshot.
        shot = directory / f"{label}-voice.png"
        self.shell.screenshot(shot)
        bx, by, _, _ = self.rects()["buffer"]
        gutter = max(8, round(8 * zoom))
        width, height, raw = self.pixels(shot, (bx + box[0] + box[2] - gutter, by + box[1], gutter, box[3]))
        rows = [row for row in range(height)
                if sum(all(abs(a - b) <= 2 for a, b in zip(raw[(row * width + col) * 3:(row * width + col + 1) * 3],
                                                             THUMB)) for col in range(width)) >= max(1, round(2 * zoom))]
        record["thumb_rows"] = [min(rows), max(rows)] if rows else None
        record["thumb_png"] = shot.name

        def revealed(node, timeout=6):
            def now():
                extents, outer = self.extents(node), self.extents(region)
                return bool(extents and outer and inside(extents, outer))
            try:
                eventually(now, "", timeout=timeout)
                return True
            except AssertionError:
                return False
        # Wheel: from the top, turn the wheel down over the card.
        self.shell.wheel(*centre, 12)
        record["wheel"] = revealed(last)
        # Recorded, not gated: whether some wheel position, one click at a
        # time from the top, shows the last hang-up whole.
        to_top()
        record["wheel_any"] = False
        for _ in range(20):
            self.shell.wheel(*centre, 1)
            if revealed(last, timeout=1):
                record["wheel_any"] = True
                break
        self.park_pointer()
        # Focus: each name and each hang-up, as Tab would reach them.
        for node in links + hangups:
            node.call("Component", "GrabFocus")
            record["focus"][node.name] = revealed(node)
        # Tab into the card (its first destination), then End.
        to_top()
        self.park_pointer()
        first.call("Component", "GrabFocus")
        time.sleep(0.4)
        self.shell.key("End")
        record["tab_end"] = revealed(last)
        record["after"] = self.extents(last)
        return record

    def layout_matrix(self):
        directory = self.args.output / "layout"
        directory.mkdir(exist_ok=True)
        self.post_content()
        report = dict(geometries=[], reference_line_height=None)
        for label, width, height, steps, zoom, gated in GEOMETRIES:
            self.shell.chord("Control_L", "0")
            eventually(lambda: abs(self.zoom() - 1.0) < 0.001, "zoom did not return to 100 %")
            time.sleep(0.4)
            self.shell.place("Nuschl", 40, 40, width, height)
            committed = self.settled_rects()
            if label.startswith("requested-760"):
                self.shot("8-narrow")
            for _ in range(steps):
                self.shell.chord("Control_L", "equal")
                time.sleep(0.3)
            eventually(lambda: abs(self.zoom() - zoom) < 0.001, f"zoom {zoom} not reached")
            time.sleep(1.0)
            zoomed = self.settled_rects()
            self.shot("9-narrow-zoomed" if label.startswith("requested-760") else f"layout-{label}")
            entry = dict(label=label, requested=[width, height], zoom=zoom, gated=gated,
                         committed=committed, committed_zoomed=zoomed)
            entry["timeline"] = {channel: self.timeline_pass(label, zoom, directory, channel)
                                 for channel in ("Root", "Lobby")}
            entry["voice"] = self.voice_pass(label, zoom, directory)
            info, _ = self.tree()
            entry["unnamed_interactive"] = sorted({role for _, role, name in info.values()
                                                   if role in INTERACTIVE and not name.strip()})
            heights = [h for walk in entry["timeline"].values() for h in walk["joined_heights"]]
            if label.startswith("control"):
                report["reference_line_height"] = max(heights) if heights else None
            reference = report["reference_line_height"]
            entry["joined_lines"] = (round(max(heights) / (reference * zoom))
                                     if heights and reference else None)
            report["geometries"].append(entry)
            walks = entry["timeline"].values()
            print(f"  {label}: committed frame {committed['frame'][2]}×{committed['frame'][3]} "
                  f"({zoomed['frame'][2]}×{zoomed['frame'][3]} zoomed), timeline "
                  f"{entry['timeline']['Lobby']['timeline_logical_width']} logical px, "
                  f"{sum(len(walk['violations']) for walk in walks)} node overflows, "
                  f"{sum(s['bright_pixels'] for walk in walks for s in walk['steps'])} gutter pixels, "
                  f"'{JOINED}' {entry['joined_lines']} line(s), voice wheel {entry['voice']['wheel']} "
                  f"(any position {entry['voice']['wheel_any']}) "
                  f"tab+end {entry['voice']['tab_end']} thumb {entry['voice']['thumb_rows']}", flush=True)
        self.shell.chord("Control_L", "0")
        failures = []
        for entry in report["geometries"]:
            if not entry["gated"]:
                continue
            label, voice = entry["label"], entry["voice"]
            for channel, timeline in entry["timeline"].items():
                where = f"{label} #{channel}"
                if not timeline["focused"]:
                    failures.append(f"{where}: Shift+Tab from the composer did not reach the timeline")
                if not timeline["top_reached"]:
                    failures.append(f"{where}: the walk did not reach 'Welcome to {channel}'")
                if timeline["violations"]:
                    failures.append(f"{where}: {len(timeline['violations'])} laid-out nodes leave the "
                                    f"timeline, e.g. {timeline['violations'][:3]}")
                bright = [(s["step"], s["bright_pixels"]) for s in timeline["steps"] if s["bright_pixels"]]
                if bright:
                    failures.append(f"{where}: text or image pixels in the scrollbar gutter at steps {bright}")
            limit = 1 if entry["zoom"] == 1.0 else 5
            if entry["joined_lines"] is None or entry["joined_lines"] > limit:
                failures.append(f"{label}: '{JOINED}' takes {entry['joined_lines']} lines (at most {limit})")
            if not voice["wheel"] or not voice["tab_end"] or not all(voice["focus"].values()):
                failures.append(f"{label}: voice destinations not all reachable: wheel {voice['wheel']}, "
                                f"tab+end {voice['tab_end']}, focus {voice['focus']}")
            if voice["overflows"] and not voice["thumb_rows"]:
                failures.append(f"{label}: the voice card overflows without a painted scrollbar thumb")
            if entry["unnamed_interactive"]:
                failures.append(f"{label}: unnamed interactive nodes {entry['unnamed_interactive']}")
        report["failures"] = failures
        (self.args.output / "layout-narrow.json").write_text(json.dumps(report, indent=2) + "\n")
        assert not failures, "narrow layout:\n" + "\n".join(failures)
        print("Narrow layout: ok", flush=True)

    def run(self):
        self.shell = GnomeSession(self.args.gnome_shell, WIDTH + 160, HEIGHT + 100,
                                  log=(self.args.output / "gnome-shell.log").open("w"))
        self.shell.start()
        env = self.shell.app_environment()
        env.pop("NO_AT_BRIDGE", None)
        registry = start_registry(self.args.at_spi, env, self.spawn)
        self.start_pulse()
        bookmarks = []
        for name, bandwidth in (("Wide", None), ("Narrow", 72_000)):
            root = self.root / name
            root.mkdir()
            bus = Bus(root)
            port = free_port()
            murmur = Murmur(self.args.murmur, root, port)
            murmur.ini.write_text(murmur.ini.read_text().replace(
                "bandwidth=72000\n", (f"bandwidth={bandwidth}\n" if bandwidth else "") + "dbus=session\n"))
            os.environ["DBUS_SESSION_BUS_ADDRESS"] = bus.address
            murmur.seed()
            murmur.start()
            self.servers[name] = (murmur, bus)
            pin = certificate_pin(self.args.binaries, "127.0.0.1", port, root / "probe")
            peer = Client(self.args.binaries, "127.0.0.1", port, pin, f"{name}Peer", root / "peer")
            peer.send(kind="join", channel=1)
            self.peers[name] = peer
            bookmarks.append(dict(id=str(uuid.uuid4()), name=name, host="127.0.0.1", port=port, username="Andi",
                                  certificate_sha256=pin, auto_connect=True, last_channel=1))
        self.profile = self.root / "profile"
        (self.profile / "config").mkdir(parents=True)
        (self.profile / "config/servers.json").write_text(json.dumps(bookmarks))
        (self.profile / "config/settings.json").write_text(json.dumps({
            "server_icons": False, "audio": {"transmit": "continuous", "echo_cancellation": "off"},
            "appearance": {"theme": "dark"}}))
        app_env = env | {"PULSE_SERVER": "unix:" + str(self.socket), "NUSCHL_AUDIO_BACKEND": "pulse",
                         "RUST_LOG": os.environ.get("NUSCHL_TEST_LOG", "nuschl=info")}
        app = self.app = self.spawn([str(self.args.binaries / "nuschl"), "--profile-dir", str(self.profile)],
                                    app_env, "desktop.log")
        eventually(lambda: app.poll() is None and self.shell.window("Nuschl"), "Nuschl window did not appear")
        self.shell.place("Nuschl", 40, 40, WIDTH, HEIGHT)

        def application():
            assert app.poll() is None, (self.args.output / "desktop.log").read_text()[-3000:]
            return next((node for node in registry.children() if node.name.lower() == "nuschl"), None)
        self.nuschl = eventually(application, "Nuschl did not register with the accessibility registry", timeout=45)
        for server, peer in self.peers.items():
            peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi", timeout=40)

        # Listening on both, speaking on Wide (the default 128 kb/s request).
        for server in ("Narrow", "Wide"):
            self.select(server)
            self.press("push button", "Undeafen")
        self.select("Wide")
        self.press("push button", "Unmute")
        eventually(lambda: not self.own("Wide")["self_muted"], "Wide still sees Andi muted")
        eventually(self.capture_streams, "unmuting did not open the microphone", timeout=20)
        time.sleep(1.5)
        self.named("128 kb/s")
        self.named("48 of 128 kb/s · server limit")
        settings, sidecar = self.files()
        assert sidecar is None and "voice_bitrate" not in settings, (settings, sidecar)
        self.check("Wide", "default request (128 kb/s)", 128_000)
        self.shot("1-voice-card-default")

        # Settings: the server below the requested rate, and the rest.
        self.press("push button", "Settings")
        self.find("dialog", "Settings")
        group = self.find("radio button", "128 kb/s")
        group.call("Component", "ScrollTo", "u", "0")
        self.named("1 other server: 128 kb/s")
        self.named("Narrow: 48 kb/s (server limit)")
        self.shot("2-settings-default")
        capture = self.capture_streams()
        assert len(capture) == 1, capture
        print(f"  capture stream {capture[0][0]} on PulseAudio client {capture[0][1]}; "
              f"workers now {media_workers(app.pid)} (Settings adds its own preview worker)", flush=True)
        self.press("radio button", "256 kb/s")
        settings, sidecar = eventually(lambda: (lambda f: f if f[1] else None)(self.files()),
                                       "voice-quality.json was not written")
        assert sidecar == {"version": 1, "voice_bitrate": 256_000}, sidecar
        assert set(settings) <= RELEASED_KEYS and "voice_bitrate" not in settings, settings
        self.named("1 other server: 256 kb/s")
        self.named("Narrow: 48 kb/s (server limit)")
        self.find("radio button", "256 kb/s").call("Component", "ScrollTo", "u", "0")
        self.shot("3-settings-256")
        time.sleep(1.0)
        self.check("Wide", "requested 256 kb/s", 256_000)
        assert self.capture_streams() == capture, "a requested change reopened capture or replaced its worker"
        self.press("radio button", "64 kb/s")
        time.sleep(1.0)
        self.check("Wide", "requested 64 kb/s", 64_000)
        assert self.capture_streams() == capture, "a requested change reopened capture or replaced its worker"
        self.press("radio button", "256 kb/s")
        self.shell.key("Escape")
        eventually(lambda: not any(role == "dialog" and name == "Settings" for _, _, role, name in self.nodes()),
                   "Settings did not close")
        self.named("256 kb/s")
        self.named("48 of 256 kb/s · server limit")
        self.shot("4-voice-card-256")

        # Speaking on both: one encoder, Narrow's 48 kb/s for both.
        self.select("Narrow")
        self.press("push button", "Unmute")
        self.press("push button", "Speak on both")
        eventually(lambda: not self.own("Narrow")["self_muted"], "Narrow still sees Andi muted")
        self.named("48 of 256 kb/s · shared encoder", timeout=20)
        time.sleep(1.5)
        self.check("Wide", "speaking on both (shared encoder)", 48_000)
        self.check("Narrow", "speaking on both", 48_000)
        self.shot("5-voice-card-shared")

        # A live ServerConfig: Narrow lowers its limit, then too far.
        narrow_bus = self.servers["Narrow"][1]
        narrow_bus.set_bandwidth(40_000)
        self.named("16 of 256 kb/s · server limit", timeout=20)
        self.named("16 of 256 kb/s · shared encoder")
        self.recovered("Narrow", "live limit 40 kb/s")
        self.check("Narrow", "live limit 40 kb/s", 16_000)
        self.check("Wide", "live limit on Narrow (shared)", 16_000)
        self.shot("6-voice-card-live-40k")
        narrow_bus.set_bandwidth(29_999)
        self.named("Not sending · server limit", timeout=20)
        self.named("256 kb/s", timeout=20)
        time.sleep(2.0)
        self.check("Narrow", "live limit 29,999 b/s", 0)
        self.check("Wide", "Narrow too small: Wide alone", 256_000)
        assert self.playback_streams(), "a budget too small for voice stopped playback"
        self.shot("7-voice-card-live-too-small")

        (self.args.output / "bitrate-desktop.json").write_text(json.dumps(self.results, indent=2))
        print("Desktop voice bitrate: ok", flush=True)

        # Narrow and zoomed: the timeline wraps inside itself and every voice
        # destination stays reachable (layout-narrow.json).
        self.layout_matrix()

    def close(self):
        for peer in self.peers.values():
            try:
                peer.close()
            except (AssertionError, OSError):
                pass
        for murmur, bus in self.servers.values():
            murmur.stop()
            bus.stop()
        for process in reversed(self.processes):
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
        if self.shell:
            self.shell.stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschl-bitrate-desktop-", dir=args.output) as temporary:
        test = DesktopBitrate(args, Path(temporary))
        try:
            test.run()
        except BaseException:
            if test.shell and getattr(test, "app", None) is not None and test.app.poll() is None:
                try:
                    test.shot("failure")
                    (args.output / "failure-accessibility.txt").write_text("\n".join(
                        f"{depth} {role} {name!r}" for depth, node, role, name in test.nodes()))
                except BaseException:
                    pass
            raise
        finally:
            test.close()


if __name__ == "__main__":
    main()
