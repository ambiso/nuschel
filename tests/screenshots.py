#!/usr/bin/env python3
"""Capture desktop screenshots against a private loopback stock Mumble server.

Development aid: `python3 tests/screenshots.py target/debug --murmur <mumble-server>`
under `xvfb-run`. Only a server bound to 127.0.0.1 is used; no host audio devices
are opened (the media worker is pointed at a private PulseAudio if one is given).
"""
import argparse
import base64
import struct
import zlib
import json
import os
import re
from pathlib import Path
import shutil
import signal
import sqlite3
import subprocess
import sys
import tempfile
import time
import urllib.parse
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import items, snapshot, start_registry  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402
from transfer import Wormhole, Worker  # noqa: E402
import frame_timing  # noqa: E402

def SEARCH_BUTTON(width):
    """The conversation header's search button, in window screenshot pixels."""
    return (width - 77, 86)


# The first result in the search panel.
SEARCH_RESULT = (600, 300)
# Ctrl+Shift plus a key on the virtual keyboard.
CONTROL_SHIFT = """
const k = global._nuschlKeyboard, C = imports.gi.Clutter, t = imports.gi.GLib.get_monotonic_time();
k.notify_keyval(t, C.KEY_Control_L, C.KeyState.PRESSED);
k.notify_keyval(t + 5000, C.KEY_Shift_L, C.KeyState.PRESSED);
k.notify_keyval(t + 10000, C.KEY_%s, C.KeyState.PRESSED);
k.notify_keyval(t + 20000, C.KEY_%s, C.KeyState.RELEASED);
k.notify_keyval(t + 25000, C.KEY_Shift_L, C.KeyState.RELEASED);
k.notify_keyval(t + 30000, C.KEY_Control_L, C.KeyState.RELEASED); 'ok'
"""

# The voice card's warning line, in window screenshot pixels.
CLIPPING_WARNING = (205, 728)
# Settings: "Test echo" before the microphone test has started, and each
# shortcut row's Record and Clear (push to talk, toggle mute, toggle deafen, push to mute).
ECHO_TEST_BUTTON = (862, 225)
RECORD_SHORTCUT = [(795, 478 + 54 * row) for row in range(4)]
CLEAR_SHORTCUT = [(872, 478 + 54 * row) for row in range(4)]
# Settings' "Voice activity" option, and the second server in the rail (managed scene).
VOICE_ACTIVITY = (483, 388)
FRIENDS_SERVER = (58, 152)
# The cog next to the server name, Lobby's chevron, and the channel list's right edge.
SERVER_SETTINGS_BUTTON = (279, 86)
LOBBY_CHEVRON = (129, 234)
SIDEBAR_EDGE = (346, 400)
# Wheel clicks for about one screen of the timeline, and the pill that jumps back down.
SCROLL_CLICKS = 8
# The devices scene: more microphones and speakers than the lists show at once.
DEVICES_SCRIPT = "".join(
    f"load-module module-null-source source_name=studio{n} description=\"Studio microphone {n:02}\"\n"
    f"load-module module-null-sink sink_name=room{n} sink_properties='device.description=\"Room speaker {n:02}\"'\n"
    for n in range(1, 13))
DEVICE_LISTS = ("Microphone", "Speakers or headphones")


def JUMP_TO_LATEST(width, height):
    """The "Jump to latest" pill, centred over the timeline's bottom edge."""
    return (width // 2 + 160, height - 112)


# In #Development's member list: Ada's and Grace's rows. Entries of the user
# menu opened on Ada (on Grace they are 60 pixels lower).
ADA_ROW = (236, 301)
GRACE_ROW = (236, 361)
MENU_PROFILE = (300, 323)
MENU_ADD_FRIEND = (300, 471)
MENU_NICKNAME = (300, 506)
PROFILE_DONE = (925, 178)

# Andi's file card's "Stop offering" once Ada's offer has ended below it.
STOP_OFFERING = (835, 461)
# The screenshot sent into #Development, and the Lobby row.
CHAT_IMAGE = (580, 670)
LOBBY_ROW = (189, 233)
# The patch's copy button, and words in the first messages: "patch:", "println",
# "good" and the end of "Ship" in Grace's reply.
COPY_CODE = (1240, 288)
PATCH_WORD = (536, 254)
PRINTLN_WORD = (478, 350)
GOOD_WORD = (472, 450)
SHIP_END = (464, 474)
# In the math scene's second message (the Gaussian integral), and the Render
# math switch in Settings, scrolled five steps up from the end.
MATH_MESSAGE = (520, 330)
RENDER_MATH = (886, 143)
# In the message the links scene sends: "the guide" and "come to the Lobby".
GUIDE_LINK = (497, 684)
LOBBY_LINK = (835, 684)


# GNOME Shell exports its freedesktop notification daemon but, headless, never
# claims the bus name: claim it, so Nuschl's notifications reach the real one.
# Ask GNOME Shell for the clipboard's text; CLIPBOARD_TEXT reads the answer.
READ_CLIPBOARD = """
global._nuschlClipboard = null;
imports.gi.St.Clipboard.get_default().get_text(imports.gi.St.ClipboardType.CLIPBOARD,
    (_, text) => { global._nuschlClipboard = text; });
'ok'
"""
CLIPBOARD_TEXT = "global._nuschlClipboard"

# Read the typed image from this private shell's clipboard, independently of
# Nuschl's own paste handler. A text/data-URI clipboard entry is not sufficient.
READ_CLIPBOARD_IMAGE = """
global._nuschlClipboardImage = null;
imports.gi.St.Clipboard.get_default().get_content(imports.gi.St.ClipboardType.CLIPBOARD,
    'image/png', (_, bytes) => {
        global._nuschlClipboardImage = bytes
            ? imports.gi.GLib.base64_encode(bytes.get_data()) : '';
    });
'ok'
"""

CLAIM_NOTIFICATIONS = """
imports.gi.Gio.bus_own_name_on_connection(imports.gi.Gio.DBus.session,
    'org.freedesktop.Notifications', 0, null, null);
'ok'
"""


# The message box, its attach button, and a clipboard holding a file's PNG
# (as screenshot tools leave it).
COMPOSER = (700, 735)
ATTACH_BUTTON = (386, 735)
# The Light choice in Settings scrolled to its Appearance section.
LIGHT_THEME = (652, 140)
INTERFACE_FONT = (834, 213)
SET_CLIPBOARD = """
const [, data] = imports.gi.GLib.file_get_contents(%s);
imports.gi.St.Clipboard.get_default().set_content(imports.gi.St.ClipboardType.CLIPBOARD,
    'image/png', new imports.gi.GLib.Bytes(data)); 'ok'
"""
PASTE = """
const k = global._nuschlKeyboard, C = imports.gi.Clutter, t = imports.gi.GLib.get_monotonic_time();
k.notify_keyval(t, C.KEY_Control_L, C.KeyState.PRESSED);
k.notify_keyval(t + 10000, C.KEY_v, C.KeyState.PRESSED);
k.notify_keyval(t + 20000, C.KEY_v, C.KeyState.RELEASED);
k.notify_keyval(t + 30000, C.KEY_Control_L, C.KeyState.RELEASED); 'ok'
"""


def make_photo(path, width, height):
    """A detailed picture that PNG cannot squeeze into an image message."""
    noise = os.urandom(width * 3)
    rows = b"".join(
        b"\x00" + bytes((x * 255 // width ^ noise[(x * 3 + y * 7) % len(noise)] & 0x1f) % 256
                        if channel == 0 else (y * 255 // height + noise[(x + y) % len(noise)] % 24) % 256
                        if channel == 1 else 140
                        for x in range(width) for channel in range(3))
        for y in range(height)
    )
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    path.write_bytes(b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
                     + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))
    return path


def notice_zoom(args, root, shell, registry, bots, profile, port, pin, shot):
    """Real Murmur failures at real page zoom: accessible text, hit bounds and dismissal."""
    assert registry, "notice-zoom requires --at-spi"
    application = eventually(lambda: next((node for node in registry.children()
                                          if node.name.lower() == "nuschl"), None),
                             "Nuschl is not in the accessibility registry")

    def named(role, name, prefix=False):
        return next((node for node, r, n, _ in items(application)
                     if (r == role if role else True)
                     and (n.startswith(name) if prefix else n == name)), None)

    def inside(inner, outer):
        x, y, w, h = inner
        ox, oy, ow, oh = outer
        return w > 0 and h > 0 and x >= ox - 1 and y >= oy - 1 and x + w <= ox + ow + 1 and y + h <= oy + oh + 1

    password = root / "moderator-password"
    password.write_text(Murmur.SUPERUSER_PASSWORD)
    password.chmod(0o600)
    moderator = Client(args.binaries, "127.0.0.1", port, pin, "SuperUser", root / "moderator",
                       ["--password-file", str(password)])
    bots.append(moderator)
    # Mumble forwards this real moderation reason, and Nuschl renders its actual
    # disconnected-session notice. No injected widget state or mocked rendering.
    reason = "Please reconnect after checking this private server's channel access and certificate settings."
    records = []
    base_rail_width = None
    report = args.output / "notice-zoom.json"
    for size_name, width in [("normal", 1280), ("narrow", 900)]:
        shell.chord("Control_L", "0")
        shell.place("Nuschl", 40, 40, width, 800)
        for percent, steps in [(100, 0), (125, 2), (150, 3), (200, 5)]:
            # Reconnect at the baseline size before exercising the notice at
            # the requested zoom. Header actions can lie outside a narrow,
            # heavily zoomed viewport; they are not the behavior under test.
            shell.chord("Control_L", "0")
            connect = named("push button", "Connect")
            if connect:
                connect.press()
            eventually(lambda: next((node for node, role, name, _ in items(application)
                                     if role == "push button" and name.startswith("Screenshot server;")
                                     and "voice here, muted" in name), None),
                       "the server did not reconnect while muted", timeout=30)
            for _ in range(steps):
                shell.chord("Control_L", "equal")
                time.sleep(0.2)
            eventually(lambda: abs(json.loads((profile / "config/settings.json").read_text())
                                   ["appearance"]["zoom"] - percent / 100) < 0.001,
                       f"Ctrl++ did not reach {percent}%")
            rail = eventually(lambda: next((node for node, role, name, _ in items(application)
                                            if role == "push button" and name.startswith("Screenshot server;")
                                            and "voice here, muted" in name), None),
                              "the server rail does not announce muted voice", timeout=30)
            session = eventually(lambda: next((u["session"] for u in bots[0].users.values()
                                               if u["name"] == "Andi"), None), "Andi did not reconnect")
            time.sleep(0.6)
            rail_bounds = rail.extents()
            if base_rail_width is None:
                base_rail_width = rail_bounds[2]
            assert abs(rail_bounds[2] / base_rail_width - percent / 100) < 0.08, \
                f"accessibility bounds did not follow real zoom: {rail_bounds} at {percent}%"
            name = f"44-muted-{size_name}-{percent}"
            shot(name)
            rx, ry, rw, rh = rail_bounds
            margin = round(12 * percent / 100)
            subprocess.run(["magick", str(args.output / f"{name}.png"), "-crop",
                            f"{rw + 2 * margin}x{rh + 2 * margin}+{max(0, rx + 12 - margin)}+{max(0, ry + 12 - margin)}", "+repage",
                            str(args.output / f"{name}-badge.png")], check=True)
            moderator.send(kind="moderate", session=session, action="kick", reason=reason)
            notice = eventually(lambda: named("status bar", "Connection notice"),
                                "the actual Mumble failure has no accessible Connection notice status")
            descendants = eventually(lambda: (rows if any(reason in n for _, _, _, n in rows) else None)
                                     if (rows := snapshot(notice)) else None,
                                     "the complete server failure reason is not accessible")
            dismiss = next((node for _, node, role, name in descendants
                            if role == "push button" and name == "Dismiss notice"), None)
            assert dismiss, "the notice has no accessible Dismiss button"
            toast_bounds, button_bounds = notice.extents(), dismiss.extents()
            window = shell.window("Nuschl")
            viewport = (0, 0, window["width"], window["height"])
            entry = dict(width=size_name, requested_width=width, zoom=percent,
                         window=window, rail_bounds=rail_bounds, toast_bounds=toast_bounds,
                         dismiss_bounds=button_bounds, accessible_text=[name for _, _, _, name in descendants],
                         button_inside_toast=inside(button_bounds, toast_bounds),
                         button_inside_window=inside(button_bounds, viewport),
                         toast_inside_window=inside(toast_bounds, viewport))
            records.append(entry)
            report.write_text(json.dumps(records, indent=2))
            shot(f"45-notice-{size_name}-{percent}")
            assert entry["button_inside_toast"], f"Dismiss escaped Connection notice: {entry}"
            assert entry["button_inside_window"], f"Dismiss escaped the visible window: {entry}"
            assert entry["toast_inside_window"], f"Connection notice escaped the visible window: {entry}"
            # Exercise actual pointer hit testing at the zooms where the
            # reported bug appeared, as well as the screen-reader action.
            if percent in (125, 200):
                bx, by = dismiss.centre()
                shell.click(window["x"] + bx, window["y"] + by)
                entry["activation"] = "pointer"
            else:
                dismiss.press()
                entry["activation"] = "AT-SPI"
            eventually(lambda: named(None, "Connection notice") is None
                       and named("push button", "Dismiss notice") is None,
                       f"Dismiss did not close the actual notice at {percent}%")
            entry["dismissed"] = True
            report.write_text(json.dumps(records, indent=2))
            print(f"Notice {size_name} {percent}%: full text accessible; Dismiss contained and activated.", flush=True)
    shell.chord("Control_L", "0")
    shell.place("Nuschl", 40, 40, *map(int, args.size.split("x")))


def make_png(path, width, seed, height=None):
    """A small gradient PNG written with the standard library only."""
    height = height or width
    rows = b"".join(
        b"\x00" + bytes(
            channel
            for x in range(width)
            for channel in ((x * 255 // width + seed * 40) % 256, (y * 255 // height + seed * 90) % 256, (128 + seed * 60) % 256)
        )
        for y in range(height)
    )
    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF)
    png = (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
           + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))
    path.write_bytes(png)
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--size", default="1280x800")
    parser.add_argument("--settle", type=float, default=4.0)
    parser.add_argument("--scenes", default="all", help="comma-separated: select,multiline,links,rich,friends,runs,unread,dialog,settings,shortcuts,server,menus,long,search,finder,emoji,toggles,clipping,viewer,notify,scroll,paste,attach,transfer,appearance,maximize; not in all: layout (long migration paths/voice names and zoom matrix, needs --at-spi), layout-controls (dialog/nested-control focus), layout-performance (matched frame timings), native-spacing (rail bottom/footer gaps; requires --pulse-bin and --at-spi), voice-status (two-server voice facts, consent/reconnect and visible zoom layout; requires --pulse-bin and --at-spi), rail-layout (first/last paint gutters and animated contours, needs --at-spi), resize (measures resizing), managed, math (formulas; run it with --theme light and --zoom 1.25 too), devices (wheel over Settings' device lists, checked over AT-SPI; needs --pulse-bin and --at-spi), notice-zoom (real Mumble failures and Dismiss bounds at 100–200%%, needs --at-spi), dialog-overscroll (Settings and Details end at their content at 100%% and 200%%, needs --at-spi)")
    parser.add_argument("--dragon-drop", type=Path, help="dragon-drop, to drop files on the conversation (attach scene)")
    # Dark by default, as the scenes were drawn; the private shell has no
    # settings portal, so "system" means GPUI's fallback (light).
    parser.add_argument("--theme", choices=["system", "light", "dark"], default="dark")
    parser.add_argument("--zoom", type=float, default=1.0, help="scenes' coordinates assume 1.0")
    parser.add_argument("--pulse-bin", type=Path, help="start a private PulseAudio with a sine-wave microphone")
    parser.add_argument("--at-spi", type=Path,
                        help="at-spi2-core: the devices scene then reads the device lists as a screen reader does")
    parser.add_argument("--resize-messages", type=int, default=1000,
                        help="messages the resize scene sends before measuring a busy channel (0: only the empty one)")
    parser.add_argument("--wormhole-python", type=Path,
                        help="a python with magic-wormhole-mailbox-server and -transit-relay (transfer scene)")
    parser.add_argument("--portals", type=Path, nargs="+", default=[],
                        help="xdg-desktop-portal, xdg-desktop-portal-gnome and gnome-control-center packages: "
                             "the shortcuts scene then binds through the real GlobalShortcuts portal")
    parser.add_argument("--zenity", type=Path, help="zenity, for another focused window in the shortcuts scene")
    args = parser.parse_args()
    if {"native-spacing", "voice-status"} & set(args.scenes.split(",")) and (not args.pulse_bin or not args.at_spi):
        parser.error("native-spacing and voice-status require --pulse-bin and --at-spi")
    args.output.mkdir(parents=True, exist_ok=True)
    width, height = map(int, args.size.split("x"))
    with tempfile.TemporaryDirectory(prefix="nuschl-screens-") as temporary:
        root = Path(temporary)
        port = free_port()
        murmur = Murmur(args.murmur, root, port)
        bot_options = []
        resizing = "resize" in args.scenes.split(",")
        if resizing or {"all", "scroll", "search", "voice-status"} & set(args.scenes.split(",")):
            # The scroll scene fills several pages of history quickly; the
            # stock server otherwise drops text beyond one message a second.
            # The bots need not wait for it either (Nuschl itself still does).
            murmur.ini.write_text(murmur.ini.read_text() + "messagelimit=1000\nmessageburst=1000\n")
            bot_options = ["--message-limit", "1000", "--message-burst", "1000"]
        murmur.seed()
        rail_scene = bool({"rail-layout", "server-order", "native-spacing", "voice-status"} & set(args.scenes.split(",")))
        layout_scene = bool({"layout", "layout-controls", "layout-performance"} & set(args.scenes.split(",")))
        if layout_scene:
            murmur.ini.write_text(murmur.ini.read_text() + "suggestPushToTalk=true\n")
            from layout_regression import CHANNEL_NAME
            with sqlite3.connect(murmur.database) as database:
                database.execute("UPDATE channels SET name=? WHERE server_id=1 AND channel_id=2", (CHANNEL_NAME,))
        if resizing:
            # A busy server's tree: forty more channels under Music, below the rows the scenes click.
            with sqlite3.connect(murmur.database) as database:
                database.executemany(
                    "INSERT OR IGNORE INTO channels (server_id, channel_id, parent_id, name, inheritacl)"
                    " VALUES (1, ?, 6, ?, 1)", [(100 + n, f"Room {n:02}") for n in range(40)])
        murmur.start()
        rail_murmur = None
        if rail_scene:
            (root / "rail-server").mkdir()
            rail_port = free_port()
            rail_murmur = Murmur(args.murmur, root / "rail-server", rail_port)
            rail_murmur.seed()
            if "voice-status" in args.scenes.split(","):
                # The rapid-control matrix measures client intent, not the stock
                # server's shared five-control-message burst limiter.
                rail_murmur.ini.write_text(rail_murmur.ini.read_text() + "messagelimit=1000\nmessageburst=1000\n")
                subprocess.run([str(args.murmur), "-ini", str(rail_murmur.ini), "-supw", "owned-status-test"], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=15)
            rail_murmur.start()
        bots = []
        app = None
        wormhole = None
        offered = None
        shell = GnomeSession(args.gnome_shell, width + 160, height + 100,
                             log=(args.output / "gnome-shell.log").open("w"), portals=args.portals)
        pulse = None
        helpers = []
        registry = None
        try:
            shell.start()
            if args.at_spi:
                def spawn(command, env, log):
                    process = subprocess.Popen(command, env=env, stdout=(args.output / log).open("w"),
                                               stderr=subprocess.STDOUT, start_new_session=True)
                    helpers.append(process)
                    return process
                registry = start_registry(args.at_spi, shell.app_environment(), spawn)
            if args.wormhole_python and {"all", "transfer"} & set(args.scenes.split(",")):
                # A private mailbox server and transit relay on loopback; Andi
                # offers a file in Development as soon as Nuschl has joined it.
                wormhole = Wormhole(args.wormhole_python, root)
                offered = root / "offered" / "holiday-photos.zip"
                offered.parent.mkdir()
                offered.write_bytes(os.urandom(48 * 1000 * 1000))
            if args.pulse_bin:
                socket_dir = shell.runtime / "pulse"
                socket_dir.mkdir(mode=0o700, exist_ok=True)
                script = shell.runtime / "default.pa"
                script.write_text(f"load-module module-native-protocol-unix socket={socket_dir / 'native'} auth-anonymous=1\n"
                                  "load-module module-null-sink sink_name=speakers sink_properties=device.description=Speakers\n"
                                  # A tone played into a null sink: its monitor is the microphone.
                                  # (Nuschl records nothing from module-sine-source; see TODO.md.)
                                  "load-module module-null-sink sink_name=microphone sink_properties=device.description=Mic\n"
                                  "load-module module-sine sink=microphone frequency=220\n"
                                  "update-source-proplist microphone.monitor device.description=\"Test microphone\"\n"
                                  "set-default-sink speakers\nset-default-source microphone.monitor\n"
                                  + (DEVICES_SCRIPT if "devices" in args.scenes.split(",") else ""))
                pulse = subprocess.Popen([str(args.pulse_bin / "pulseaudio"), "--daemonize=no", "--use-pid-file=no",
                                          "--exit-idle-time=-1", "-n", "--file=" + str(script)],
                                         env=shell.environment() | {"PULSE_RUNTIME_PATH": str(socket_dir),
                                                                    "PULSE_STATE_PATH": str(shell.runtime / "pulse-state")},
                                         stdout=(args.output / "pulse.log").open("w"), stderr=subprocess.STDOUT,
                                         start_new_session=True)
                eventually(lambda: (socket_dir / "native").exists(), "private PulseAudio did not start")
            pin = certificate_pin(args.binaries, "127.0.0.1", port, root / "probe")
            people = [("Ada Lovelace", 2), ("Grace", 2), ("Linus", 5), ("Margaret", 7)]
            if resizing:
                people += [(f"Guest {n}", 100 + n * 3) for n in range(12)]
            for index, (name, channel) in enumerate(people):
                bot = Client(args.binaries, "127.0.0.1", port, pin, name, root / f"bot{index}", bot_options)
                bot.send(kind="join", channel=channel)
                bots.append(bot)
            time.sleep(1.2)
            profile = root / "profile"
            if layout_scene:
                # Produce the actual rename warning with a long absolute path;
                # no widget state, secrets or key parsing is injected.
                for _ in range(4):
                    profile /= "unbroken-profile-segment-" + "a" * 140
                (profile / "data/keys").mkdir(parents=True)
                (profile / "data/keys/layout-evidence").write_text("synthetic, not a key\n")
            (profile / "config").mkdir(parents=True)
            bookmark = {"id": str(uuid.uuid4()), "name": "Screenshot server", "host": "127.0.0.1",
                        "port": port, "username": "Andi", "certificate_sha256": pin}
            if layout_scene:
                from layout_regression import SERVER_NAME
                bookmark["name"] = SERVER_NAME
            second = {"id": str(uuid.uuid4()), "name": "Friends", "host": "127.0.0.1",
                      "port": 1, "username": "Andi", "certificate_sha256": None}
            bookmarks = [bookmark, second]
            if rail_scene:
                rail_pin = certificate_pin(args.binaries, "127.0.0.1", rail_port, root / "rail-probe")
                bookmarks = [bookmark] + [dict(second, id=str(uuid.uuid4()), name=f"Rail filler {n:02}") for n in range(18)] + [dict(bookmark, id=str(uuid.uuid4()), name="Rail last", username="Rail Last", port=rail_port, certificate_sha256=rail_pin)]
            if "voice-status" in args.scenes.split(","):
                bookmarks = [dict(second, name="Offline earlier"), dict(bookmark, name="Voice Alpha"), dict(bookmark, id=str(uuid.uuid4()), name="Voice Beta", username="Voice Beta client", port=rail_port, certificate_sha256=rail_pin)]
            (profile / "config/servers.json").write_text(json.dumps(bookmarks))
            # The web host share links point at (docs/web.md); nothing is fetched from it.
            (profile / "config/settings.json").write_text(json.dumps({"appearance": {"theme": args.theme, "zoom": args.zoom},
                                                                      "share_web_host": "web.example.org"}))
            if "voice-status" in args.scenes.split(","):
                saved = json.loads((profile / "config/settings.json").read_text())
                saved["audio"] = {"transmit":"push_to_talk"}
                (profile / "config/settings.json").write_text(json.dumps(saved))
            if "managed" in args.scenes.split(","):
                # What Home Manager's programs.nuschl.settings and .servers write
                # (only on request: managed controls move the other scenes' targets).
                (profile / "config/managed.json").write_text(json.dumps({
                    "settings": {"audio": {"transmit": "push_to_talk"}, "shortcuts": {"push-to-talk": "f13"}},
                    "servers": [{key: second[key] for key in ("id", "name", "host", "port", "username")}]}))
            if args.portals:
                # As the package installs it: the portal finds an unsandboxed app's ID by its entry.
                applications = shell.home / "data/applications"
                applications.mkdir(parents=True)
                (applications / "org.nuschl.Nuschl.desktop").write_text(
                    f"[Desktop Entry]\nType=Application\nName=Nuschl\nExec={args.binaries / 'nuschl'} %u\n")
            env = shell.app_environment() | {"RUST_LOG": os.environ.get("NUSCHL_SCREENSHOT_LOG", "nuschl=info")}
            # AccessKit honours NO_AT_BRIDGE (NixOS sets it where at-spi2-core is off).
            env.pop("NO_AT_BRIDGE", None)
            if {"native-spacing", "voice-status"} & set(args.scenes.split(",")):
                env["NUSCHL_AUDIO_BACKEND"] = "pulse"
                env["PULSE_SERVER"] = "unix:" + str(socket_dir / "native")
            if wormhole:
                env |= wormhole.environment()
            if resizing or layout_scene or rail_scene:
                # Read by a build with --features frame-timing; others ignore it.
                env["NUSCHL_FRAME_TIMING"] = str(args.output / "frames.log")
            log = (args.output / "desktop.log").open("w")
            # For profiling, e.g. NUSCHL_SCREENSHOT_WRAPPER="perf record -o nuschl.perf --".
            wrapper = os.environ.get("NUSCHL_SCREENSHOT_WRAPPER", "").split()
            app = subprocess.Popen([*wrapper, str(args.binaries / "nuschl"), "--profile-dir", str(profile),
                                    f"mumble://Andi@127.0.0.1:{port}/Lobby/{urllib.parse.quote(CHANNEL_NAME) if layout_scene else 'Development'}"]
                                   + (["--offer", str(offered)] if offered else []),
                                   env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            eventually(lambda: app.poll() is None and shell.window("Nuschl"), "Nuschl window did not appear")
            shell.place("Nuschl", 40, 40, width, height)
            # Channel messages reach only current members: wait until Andi has joined.
            bots[0].wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                         and e["user"]["channel"] == 2, timeout=30)
            time.sleep(1.0)
            avatars = [make_png(root / f"avatar{index}.png", 96, index) for index in range(2)]
            bots[0].send(kind="avatar", path=str(avatars[0]))
            bots[1].send(kind="avatar", path=str(avatars[1]))
            bots[0].send(kind="text", channel=2, body="Hi! Here is the patch:\n\n```rust\nfn main() {\n    println!(\"hello\");\n}\n```")
            time.sleep(0.4)
            bots[1].send(kind="text", channel=2, body="Looks good to me.\nShip it!")
            time.sleep(0.4)
            # Encoded as stock Mumble's Log::imageToImg does: base64 in 72-character
            # lines, each percent-encoded.
            encoded = base64.b64encode(make_png(root / "shot.png", 240, 7, height=135).read_bytes()).decode()
            mumble_image = "\n".join(urllib.parse.quote(encoded[i:i + 72], safe="")
                                     for i in range(0, len(encoded), 72))
            bots[0].send(kind="html", channel=2,
                         html=f"Here is a screenshot:<br/><img src=\"data:image/PNG;base64,{mumble_image}\" />")
            time.sleep(args.settle)

            def shot(name):
                window = shell.window("Nuschl")
                path = shell.screenshot(args.output / f"{name}.png",
                                        crop=(window["x"] - 12, window["y"] - 12, window["width"] + 24, window["height"] + 24))
                print(path, flush=True)

            shot("01-connected")
            scenes = set(args.scenes.split(","))

            def want(scene):
                return "all" in scenes or scene in scenes

            def click(x, y, count=1, button=1):
                # Coordinates are pixels in the window screenshots above.
                window = shell.window("Nuschl")
                shell.click(window["x"] - 12 + x, window["y"] - 12 + y, button=button, count=count)

            def hover(x, y):
                window = shell.window("Nuschl")
                shell.move(window["x"] - 12 + x, window["y"] - 12 + y)

            def wheel(x, y, clicks):
                window = shell.window("Nuschl")
                shell.wheel(window["x"] - 12 + x, window["y"] - 12 + y, clicks)

            def drag(start, end):
                window = shell.window("Nuschl")
                origin = (window["x"] - 12, window["y"] - 12)
                shell.drag((origin[0] + start[0], origin[1] + start[1]), (origin[0] + end[0], origin[1] + end[1]))
            click(width // 2, 140)  # the first click after mapping can be swallowed; spend it on the timeline
            time.sleep(0.4)

            def clipboard():
                shell.eval(READ_CLIPBOARD)
                time.sleep(0.5)
                return shell.eval(CLIPBOARD_TEXT)

            def copy_shortcut():
                # Ctrl+C on the virtual keyboard: press Control, press and release C, release Control.
                keyval = "imports.gi.Clutter.KEY_"
                state = "imports.gi.Clutter.KeyState."
                shell.eval("const k = global._nuschlKeyboard; const t = imports.gi.GLib.get_monotonic_time();"
                           f" k.notify_keyval(t, {keyval}Control_L, {state}PRESSED);"
                           f" k.notify_keyval(t + 10000, {keyval}c, {state}PRESSED);"
                           f" k.notify_keyval(t + 20000, {keyval}c, {state}RELEASED);"
                           f" k.notify_keyval(t + 30000, {keyval}Control_L, {state}RELEASED); 'ok'")
                time.sleep(0.4)

            if {"layout", "layout-controls"} & scenes:
                from layout_regression import run as run_layout
                run_layout(args, root, shell, registry, bots, profile, port, pin, shot)
            if "voice-status" in scenes:
                from voice_status import run as run_status
                run_status(args, root, shell, registry, bots, profile, rail_port, rail_pin, shot, rail_murmur)
            if "native-spacing" in scenes:
                from native_spacing import run as run_spacing
                run_spacing(args, shell, registry, bots, profile, shot)
            if "dialog-overscroll" in scenes:
                from dialog_overscroll import run as run_overscroll
                run_overscroll(args, shell, registry, profile, shot)
            if "server-order" in scenes:
                from server_order import run as run_order
                run_order(args, root, shell, registry, bots, profile, port, pin, shot, app)
            if "rail-layout" in scenes:
                from rail_layout import run as run_rail
                run_rail(args, shell, registry, profile, shot)
            if "layout-performance" in scenes:
                from layout_regression import performance
                performance(args, shell, profile, shot)

            if "notice-zoom" in scenes:
                notice_zoom(args, root, shell, registry, bots, profile, port, pin, shot)

            if want("select"):
                # Runs first, while the patch is where the coordinates expect it.
                click(*COPY_CODE)
                time.sleep(0.6)
                shot("27-code-copied")
                copied = clipboard()
                assert copied == 'fn main() {\n    println!("hello");\n}', copied
                # Drag over Grace's reply: "good to me." and "Ship".
                drag(GOOD_WORD, SHIP_END)
                time.sleep(0.6)
                shot("28-text-selected")
                copy_shortcut()
                copied = clipboard()
                assert copied.startswith("good to me.\nShi"), copied
                click(SHIP_END[0] + 40, SHIP_END[1] - 12, button=3)  # right-click the selected message
                time.sleep(0.6)
                shot("29-message-menu")
                click(SHIP_END[0] + 80, SHIP_END[1] + 46)  # "Copy message", below "Copy"
                time.sleep(0.4)
                copied = clipboard()
                assert copied == "Looks good to me.\nShip it!", copied
                # Across a text and a code block, then a word by double click.
                drag(PATCH_WORD, PRINTLN_WORD)
                time.sleep(0.6)
                shot("30-selected-into-code")
                copy_shortcut()
                copied = clipboard()
                assert copied.startswith("patch:\nfn main() {\n    pr"), copied
                click(SHIP_END[0] - 20, SHIP_END[1], count=2)
                time.sleep(0.4)
                copy_shortcut()
                copied = clipboard()
                assert copied == "Ship", copied
                click(width // 2, 140)  # clears the selection
                time.sleep(0.4)
            if "math" in scenes:
                # LaTeX in messages, as people type it; a price stays a price,
                # code stays code, and a broken formula shows as its source.
                bots[0].send(kind="text", channel=2, body=(
                    "Euler: $e^{i\\pi} + 1 = 0$, and a circle's area is $A = \\pi r^2$. "
                    "It costs $5 and $10; `$HOME` is code."))
                time.sleep(0.4)
                bots[1].send(kind="text", channel=2, body=(
                    "The Gaussian integral:\n$$\\int_{-\\infty}^{\\infty} e^{-x^2}\\,dx = \\sqrt{\\pi}$$\n"
                    "with a fraction $\\frac{a+b}{2}$ inline, between the lines."))
                time.sleep(0.4)
                # Stock Mumble sends HTML: the formula arrives escaped and comes back as typed.
                bots[0].send(kind="html", channel=2, html=(
                    "Cases: $$f(x) = \\begin{cases} x^2 &amp; x \\ge 0 \\\\ -x &amp; \\text{otherwise} "
                    "\\end{cases}$$ and a typo: $\\frac{1}$ next to $\\sum_{i=1}^n i$."))
                time.sleep(0.4)
                bots[1].send(kind="text", channel=2, body=(
                    "```math\n\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}\n```\n$$\\foo{x}$$"))
                time.sleep(3.0)
                shot("60-math")
                # "Copy message" copies the formulas as they were typed.
                click(*MATH_MESSAGE, button=3)
                time.sleep(0.8)
                shot("61-math-menu")
                click(MATH_MESSAGE[0] + 60, MATH_MESSAGE[1] + 22)  # "Copy message"
                time.sleep(0.4)
                copied = clipboard()
                # Whichever message is under the pointer (at another zoom, another one).
                typed = [
                    "Euler: $e^{i\\pi} + 1 = 0$, and a circle's area is $A = \\pi r^2$. "
                    "It costs $5 and $10; `$HOME` is code.",
                    "The Gaussian integral:\n$$\\int_{-\\infty}^{\\infty} e^{-x^2}\\,dx = \\sqrt{\\pi}$$\n"
                    "with a fraction $\\frac{a+b}{2}$ inline, between the lines.",
                    "Cases: $$f(x) = \\begin{cases} x^2 & x \\ge 0 \\\\ -x & \\text{otherwise} "
                    "\\end{cases}$$ and a typo: $\\frac{1}$ next to $\\sum_{i=1}^n i$.",
                ]
                assert copied in typed, copied
                print(f"Copy message gave the message as typed: {copied!r}", flush=True)
                # Settings > Appearance > Render math.
                click(239, 773)  # Settings
                time.sleep(1.0)
                wheel(640, 400, 40)
                time.sleep(1.2)
                wheel(640, 400, -5)
                time.sleep(0.8)
                shot("62-math-setting")
                click(*RENDER_MATH)  # off: formulas show as typed
                time.sleep(0.6)
                shell.key("Escape")
                time.sleep(1.0)
                shot("63-math-off")

            if want("multiline"):
                # A paragraph that wraps over three rows, and three lines in one message.
                words = " ".join(f"wrap{n:02}" for n in range(36))
                bots[1].send(kind="text", channel=2, body=words)
                time.sleep(0.4)
                bots[0].send(kind="text", channel=2, body="first line\nsecond line\nthird line")
                time.sleep(3.0)  # both rendered and the timeline at its bottom
                shot("37-multiline")

                def selects(start, end, expected=None, begins=None, finishes=None, hold=0.0):
                    # Press where the pointer has settled, drag, and (held past an edge,
                    # where the timeline scrolls itself) stay a while before releasing.
                    window = shell.window("Nuschl")
                    origin = (window["x"] - 12, window["y"] - 12)
                    pointer = "global._nuschlPointer.notify_button(imports.gi.GLib.get_monotonic_time(), 1, "
                    shell.move(origin[0] + start[0], origin[1] + start[1])
                    time.sleep(0.15)
                    shell.eval(pointer + "imports.gi.Clutter.ButtonState.PRESSED); 'ok'")
                    for step in range(1, 13):
                        shell.move(origin[0] + start[0] + (end[0] - start[0]) * step / 12,
                                   origin[1] + start[1] + (end[1] - start[1]) * step / 12)
                        time.sleep(0.02)
                    time.sleep(hold + 0.1)
                    shell.eval(pointer + "imports.gi.Clutter.ButtonState.RELEASED); 'ok'")
                    time.sleep(0.5)
                    copy_shortcut()
                    copied = clipboard()
                    assert expected is None or copied == expected, copied
                    assert begins is None or copied.startswith(begins), copied
                    assert finishes is None or copied.endswith(finishes), copied
                    return copied

                three = "first line\nsecond line\nthird line"
                selects((426, 635), (490, 683), three)  # three lines of one message
                selects((426, 635), (480, 760), three)  # on below the message, over the composer
                selects((426, 635), (950, 659), "first line\nsecond line")  # past a line's end
                selects((426, 521), (950, 569), words)  # a paragraph's three wrapped rows
                # Three lines over two messages, then up into earlier messages.
                selects((426, 222), (590, 312), "Looks good to me.\nShip it!\nHere is a screenshot:")
                selects((470, 659), (430, 145), begins="}\nLooks good", finishes="first line\nsecon")
                time.sleep(0.4)
                shot("38-selected-across-messages")
                # Held above the timeline, it scrolls up to the first message.
                selects((490, 683), (600, 90), begins="Hi! Here is the patch:", finishes=three, hold=2.0)
                click(width // 2, 140)  # clears the selection
                time.sleep(0.4)
            if want("links"):
                # Only the mumble link is clicked: an http one would start a browser.
                bots[0].send(kind="html", channel=2, html=(
                    "Read <a href=\"https://docs.example/guide\">the guide</a> or https://example.org/faq. "
                    f"Then <a href=\"mumble://Andi@127.0.0.1:{port}/Lobby\">come to the Lobby</a>"
                    " <a href=\"javascript:alert(1)\">(not a link)</a>"))
                time.sleep(1.5)
                shot("31-links")
                hover(*GUIDE_LINK)
                time.sleep(0.8)
                shot("32-link-hover")
                click(*GUIDE_LINK, button=3)
                time.sleep(0.6)
                shot("33-link-menu")
                click(GUIDE_LINK[0] + 40, GUIDE_LINK[1] + 22)  # "Copy link"
                time.sleep(0.4)
                copied = clipboard()
                assert copied == "https://docs.example/guide", copied
                # A mumble link to this server's Lobby: Nuschl goes there itself.
                click(*LOBBY_LINK)
                time.sleep(1.5)
                shot("34-mumble-link")
                click(228, 299, count=2)  # back into Development, as the other scenes expect
                time.sleep(1.5)
            if want("rich"):
                # Formatting as stock Mumble's rich text editor and markdown send it.
                bots[1].send(kind="html", channel=2, html=(
                    "<h2>Release notes</h2><p><b>Bold</b>, <i>italic</i>, <u>underlined</u>, <s>struck</s>, "
                    "<code>inline code</code>, <span style=\" color:#000080;\">navy</span> and "
                    "<font color=\"#ff8800\">orange</font>.</p><ul><li>First</li><li>Second<ul><li>Nested</li>"
                    "</ul></li></ul><ol><li>One</li><li>Two</li></ol><blockquote>A quote<br/>on two lines</blockquote>"
                    "<hr/><pre>fn main() {\n    println!(\"hi\");\n}</pre>"
                    "<table><tr><th>Key</th><th>Value</th></tr><tr><td>a</td><td>1</td></tr></table>"))
                time.sleep(1.5)
                shot("35-rich-text")
                click(461, 685)  # Show more
                time.sleep(1.0)
                shot("36-rich-text-expanded")
                click(461, 685)  # Show less, so the timeline is as later scenes expect
                time.sleep(0.6)

            if want("friends"):
                # Before the server scene, which collapses Lobby and its members.
                click(228, 268)  # #Development
                time.sleep(0.6)
                click(*ADA_ROW, button=3)
                time.sleep(0.8)
                click(*MENU_ADD_FRIEND)
                time.sleep(0.6)
                click(*ADA_ROW, button=3)
                time.sleep(0.8)
                click(*MENU_NICKNAME)
                time.sleep(0.8)
                shell.type("Countess")
                time.sleep(0.6)
                shot("37-nickname-prompt")
                shell.key("Return")
                time.sleep(0.6)
                click(*GRACE_ROW, button=3)
                time.sleep(0.8)
                click(MENU_ADD_FRIEND[0], MENU_ADD_FRIEND[1] + 60)
                time.sleep(0.6)
                # A friend's arrival shows here although she connects to Root.
                bots[1].close()
                time.sleep(1.0)
                bots[1] = Client(args.binaries, "127.0.0.1", port, pin, "Grace", root / "bot1")
                bots[1].send(kind="join", channel=2)
                bots[1].send(kind="avatar", path=str(avatars[1]))
                time.sleep(1.5)
                shot("38-friends")
                hover(ADA_ROW[0] - 20, ADA_ROW[1])
                time.sleep(1.5)
                shot("39-nickname-tooltip")
                click(*ADA_ROW, button=3)
                time.sleep(0.8)
                shot("40-friend-menu")
                click(*MENU_PROFILE)
                time.sleep(1.5)
                shot("41-friend-profile")
                click(*PROFILE_DONE)
                time.sleep(0.4)
                click(width // 2, 140)  # a click after the dialog can be swallowed, as after mapping
                time.sleep(0.4)
                settings = json.loads((profile / "config/settings.json").read_text())
                ada = next(e["user"]["certificate_hash"] for e in bots[1].events
                           if e["kind"] == "user" and e["user"]["name"] == "Ada Lovelace")
                assert settings["nicknames"] == {ada: "Countess"}, settings
                assert settings["friends"][ada] == "Ada Lovelace", settings
                assert "Grace" in settings["friends"].values(), settings

            if want("dialog"):
                click(59, 227)
                time.sleep(1.0)
                shot("02-add-server")
                # The address has focus, and the name follows it.
                shell.type("voice.example.org")
                time.sleep(0.6)
                shot("02a-add-server-typed")
                shell.key("Escape")
                time.sleep(0.5)
            if want("settings"):
                click(239, 773)
                time.sleep(1.0)
                shot("04-settings")
                # Still muted: the echo test opens the sine microphone itself
                # (and so starts the microphone test with its meter).
                click(*ECHO_TEST_BUTTON)
                time.sleep(2.0)
                shot("05-settings-live")
                # The verdict comes about 7.5 s after the click.
                time.sleep(7.0)
                shot("05b-echo-test")
                shell.key("Escape")
                time.sleep(0.4)
            if "devices" in scenes:
                # Settings' microphone and speaker lists hold more devices than
                # they show. A wheel over a list, with the pointer parked, moves
                # that list alone: its entries pass under the pointer once each,
                # in order, and the dialog stays put, so another list never
                # slides under the pointer. (Each step used to move the dialog
                # as well: the speakers then scrolled by under a pointer parked
                # on the microphones, "System default" and all.)
                assert registry, "the devices scene reads the lists over AT-SPI: pass --at-spi"
                nuschl = eventually(lambda: next((node for node in registry.children()
                                                   if node.name.lower() == "nuschl"), None),
                                     "Nuschl is not in the accessibility registry", timeout=30)

                def named(role, name, prefix=False):
                    """The first node named `name` (or starting with it) with `role`, or any but a label."""
                    return next((node for _, node, r, n in snapshot(nuschl)
                                 if (r == role if role else r != "label")
                                 and (n.startswith(name) if prefix else n == name)), None)
                named("push button", "Settings").call("Action", "DoAction", "i", "0")
                lists = {title: eventually(lambda: named(None, title), f"Settings shows no {title} list",
                                           timeout=20) for title in DEVICE_LISTS}
                dx, dy, dw, dh = eventually(lambda: named("dialog", "Settings"), "no Settings dialog").extents()
                # AT-SPI reports window coordinates; the screenshots' (and click's) are 12 pixels larger.
                margin = (dx + 12 + 10, dy + 12 + dh // 2)  # in the dialog's padding, beside every list

                def entries(group):
                    return [child for child in group.children() if child.role == "radio button"]

                def seen(area, groups):
                    """The entries a person sees in `area` (window coordinates), top to bottom:
                    those whose middle lies within it and within their own list."""
                    x, y, w, h = area
                    shown = []
                    for group in groups:
                        lx, ly, lw, lh = group.extents()
                        for child in entries(group):
                            ex, ey, ew, eh = child.extents()
                            middle = ey + eh / 2
                            if y <= middle < y + h and ly <= middle < ly + lh:
                                shown.append((ey, child.name))
                    return [name for _, name in sorted(shown)]

                def in_order(names, shown, where):
                    assert shown, f"no entries seen {where}"
                    assert shown[0] in names, f"{shown} {where}: {shown[0]!r} is not in this list ({names})"
                    start = names.index(shown[0])
                    assert shown == names[start:start + len(shown)], \
                        f"{where}, the entries under the pointer are {shown}, not in the list's order {names}"
                    return start

                def park(group, title):
                    """Bring the list into the upper half of the dialog with the wheel beside it,
                    and park the pointer on it; returns its area and the pointer."""
                    for _ in range(60):
                        _, ly, _, lh = group.extents()
                        if dy + 40 <= ly <= dy + dh // 2:
                            break
                        wheel(*margin, 1 if ly > dy + 40 else -1)
                        time.sleep(0.5)
                    else:
                        raise AssertionError(f"could not scroll the {title} list into view")
                    time.sleep(1.0)  # a new run of wheel steps
                    area = tuple(group.extents())
                    lx, ly, lw, lh = area
                    pointer = (lx + 12 + lw // 2, ly + 12 + lh // 2)
                    hover(*pointer)
                    time.sleep(0.6)
                    return area, pointer

                for title, group in lists.items():
                    slug = "microphones" if title == "Microphone" else "speakers"
                    names = [child.name for child in entries(group)]
                    assert len(names) == len(set(names)) > 6, f"{title}: {names}"
                    area, pointer = park(group, title)
                    groups = list(lists.values())
                    shot(f"43-{slug}")
                    start = in_order(names, seen(area, groups), f"in the {title} list before scrolling")
                    assert start == 0, f"the {title} list starts scrolled: {seen(area, groups)}"
                    # One step at a time down to the end, then back up.
                    for direction, last in ((1, names[-1]), (-1, names[0])):
                        for step in range(len(names)):
                            wheel(*pointer, direction)
                            time.sleep(0.6)
                            where = f"after {step + 1} wheel steps {'down' if direction > 0 else 'up'}"
                            shown = seen(area, groups)
                            if direction > 0 and step == 0:
                                shot(f"43-{slug}-step")
                            now = in_order(names, shown, f"{where} over the {title} list")
                            assert tuple(group.extents()) == area, \
                                f"{where} over the {title} list, the dialog moved: {group.extents()} != {area}"
                            assert (now - start) * direction >= 0, \
                                f"{where} over the {title} list, it went the wrong way: {shown}"
                            start = now
                            if last in shown:
                                break
                        else:
                            raise AssertionError(f"wheel steps did not reach {last!r} in the {title} list")
                        print(f"{title}: {step + 1} wheel steps {'down' if direction > 0 else 'up'} "
                              f"passed the entries in order; the dialog stayed put.", flush=True)
                    # A quick spin past the end is one run: it stays with the list.
                    wheel(*pointer, len(names) + 4)
                    time.sleep(1.0)
                    assert tuple(group.extents()) == area, f"a spin over the {title} list moved the dialog"
                    shown = seen(area, groups)
                    in_order(names, shown, f"after a spin over the {title} list")
                    assert names[-1] in shown, f"a spin did not reach the end of the {title} list: {shown}"
                    shot(f"43-{slug}-spun")
                    # The next spin, at the list's end, moves the dialog.
                    wheel(*pointer, 2)
                    time.sleep(1.0)
                    assert group.extents()[1] < area[1], \
                        f"at the end of the {title} list, the wheel left the dialog still"
                    shot(f"43-{slug}-dialog")
                    print(f"{title}: a spin past the end stayed with the list; the next one moved the dialog.",
                          flush=True)
                # The font picker's list scrolls inside Settings the same way.
                named("push button", "Interface font: ", prefix=True).call("Action", "DoAction", "i", "0")
                fonts = eventually(lambda: named(None, "Installed fonts"), "the font picker did not open")
                families = [child.name for child in entries(fonts)]
                area, pointer = park(fonts, "font")
                shown = seen(area, [fonts])
                if len(shown) < len(families):
                    wheel(*pointer, 1)
                    time.sleep(0.6)
                    assert tuple(fonts.extents()) == area, "a wheel step over the font picker moved the dialog"
                    moved = seen(area, [fonts])
                    in_order(families, moved, "after a wheel step over the font picker")
                    assert families.index(moved[0]) > 0, f"a wheel step left the font picker still: {moved}"
                    shot("43-fonts-step")
                    print(f"The font picker moved alone, to {moved[0]!r}.", flush=True)
                else:
                    print(f"The font picker shows all {len(families)} fonts; nothing to scroll.", flush=True)
                shell.key("Escape")
                time.sleep(0.4)
                if named("dialog", "Settings"):  # the first closed the picker
                    shell.key("Escape")
                    time.sleep(0.4)
            if want("shortcuts"):
                andi = next(e["user"]["session"] for e in reversed(bots[0].events)
                            if e["kind"] == "user" and e["user"]["name"] == "Andi")

                def toggle_mute(where):
                    # Ctrl+Shift+M flips Andi's mute as the server sees it, once.
                    muted = bots[0].users[andi]["self_muted"]
                    shell.press("Control_L", "Shift_L", "m")
                    eventually(lambda: bots[0].users[andi]["self_muted"] != muted,
                               f"Toggle mute did nothing {where}", timeout=10)
                    time.sleep(1.0)
                    assert bots[0].users[andi]["self_muted"] != muted, f"Toggle mute acted twice {where}"
                    print(f"Toggle mute {where}: {'unmuted' if muted else 'muted'}", flush=True)

                def portal_dialog():
                    return next((w for w in shell.windows()
                                 if not (w["title"] or "").startswith(("Nuschl", "Another window"))), None)

                def add_in_portal_dialog(name):
                    # GNOME asks whether to add shortcuts it has not seen, proposing Nuschl's keys.
                    dialog = eventually(portal_dialog, "GNOME did not ask to add the shortcut", timeout=20)
                    time.sleep(1.0)
                    print(shell.screenshot(args.output / name), dialog["title"], flush=True)
                    shell.click(dialog["x"] + dialog["width"] - 39, dialog["y"] + 21)  # Add
                    eventually(lambda: not portal_dialog(), "the shortcut dialog did not close", timeout=10)
                    time.sleep(1.5)
                click(239, 773)  # Settings
                time.sleep(1.0)
                shot("37-shortcuts")
                click(*RECORD_SHORTCUT[1])  # Toggle mute
                time.sleep(0.5)
                shot("38-shortcut-recording")
                shell.press("Control_L", "Shift_L", "m")
                time.sleep(1.5)
                if args.portals:
                    add_in_portal_dialog("39a-portal-dialog.png")
                shot("39-shortcut-bound")
                shell.key("Escape")
                time.sleep(0.5)
                # With a system-wide binding GNOME takes the key before the window
                # does; either way the press acts once.
                toggle_mute("in the window")
                toggle_mute("in the window again")
                if args.portals and args.zenity:
                    other = subprocess.Popen([str(args.zenity), "--entry", "--title=Another window", "--text=Type here"],
                                             env=shell.app_environment(), stdout=subprocess.DEVNULL,
                                             stderr=(args.output / "zenity.log").open("w"), start_new_session=True)
                    try:
                        eventually(lambda: shell.window("Another window"), "the other window did not appear")
                        shell.place("Another window", 700, 300, 420, 200)
                        time.sleep(1.0)
                        toggle_mute("while another window is focused")
                        print(shell.screenshot(args.output / "40-other-window.png"), flush=True)
                        toggle_mute("while another window is focused, again")
                        # Changed bindings bind again: GNOME asks about a new one only.
                        shell.place("Nuschl", 40, 40, width, height)
                        time.sleep(0.6)
                        click(239, 773)
                        time.sleep(1.0)
                        click(*RECORD_SHORTCUT[2])  # Toggle deafen
                        time.sleep(0.5)
                        shell.press("Control_L", "Shift_L", "d")
                        add_in_portal_dialog("41-portal-dialog-again.png")
                        shot("42-two-shortcuts")
                        click(*CLEAR_SHORTCUT[2])
                        time.sleep(3.0)
                        assert not portal_dialog(), "GNOME asked again about a shortcut it knows"
                        shell.key("Escape")
                        shell.place("Another window", 700, 300, 420, 200)
                        time.sleep(1.0)
                        toggle_mute("while another window is focused, after binding again")
                    finally:
                        os.killpg(other.pid, signal.SIGTERM)
            if want("server"):
                click(*SERVER_SETTINGS_BUTTON)
                time.sleep(1.0)
                shot("02b-server-settings")
                shell.key("Escape")
                time.sleep(0.4)
                # Collapse Lobby: its people show as a badge, and widen the list.
                click(*LOBBY_CHEVRON)
                time.sleep(0.6)
                drag(SIDEBAR_EDGE, (SIDEBAR_EDGE[0] + 140, SIDEBAR_EDGE[1]))
                time.sleep(0.8)
                shot("16-collapsed-wide")
            if want("menus"):
                bots[2].send(kind="join", channel=2)  # Linus joins Development
                time.sleep(0.8)
                bots[3].close()  # Margaret disconnects
                bots.pop(3)
                time.sleep(1.2)
                shot("06-activity")
                click(236, 301)  # left: opens a DM with Ada
                time.sleep(0.8)
                shot("06b-direct")
                click(236, 301, button=3)  # Ada Lovelace
                time.sleep(0.8)
                shot("07-user-menu")
                click(290, 317)  # first entry: profile
                time.sleep(1.5)
                shot("08-profile")
                shell.key("Escape")
                time.sleep(0.4)
                click(200, 233, button=3)  # Lobby
                time.sleep(0.8)
                shot("09-channel-menu")
                # Copy share link: a stock server, so the channel's path and the
                # certificate pin travel in the fragment (docs/web.md).
                click(287, 430)
                time.sleep(0.8)
                shot("09b-share-link-copied")
                copied = clipboard()
                expected = f"https://web.example.org/join/#s=127.0.0.1:{port}&c=Lobby&fp=sha256:{pin}"
                assert copied == expected, copied
                shell.key("Escape")
                time.sleep(0.4)
            if want("long"):
                click(228, 268)  # back to #Development (the menus scene opens a DM)
                time.sleep(0.6)
                # One paragraph that fits when wide but wraps past the collapse
                # height in a narrow window, and one that is always long.
                paragraph = " ".join(f"word{n}" for n in range(100))
                bots[1].send(kind="text", channel=2, body=paragraph)
                time.sleep(0.4)
                bots[0].send(kind="text", channel=2, body="\n".join(f"line {n}" for n in range(40)))
                time.sleep(1.5)
                shot("10-long-wide")
                shell.place("Nuschl", 40, 40, 900, height)
                time.sleep(1.5)
                shot("11-long-narrow")
                shell.place("Nuschl", 40, 40, width, height)
                time.sleep(1.0)
            if want("search"):
                click(*SEARCH_BUTTON(width))
                time.sleep(0.8)
                shell.type("patc")  # the last word matches as a prefix while typed
                time.sleep(1.5)
                shot("12-search")
                shell.key("Escape")
                time.sleep(0.6)
                shot("13-search-closed")
                # Older history: reopened, the conversation holds only its latest page.
                click(228, 268)  # #Development
                time.sleep(0.6)
                for n in range(1, 121):
                    bots[(n // 3) % 2].send(kind="text", channel=2, body=f"History message {n}")
                    time.sleep(0.05)
                time.sleep(2.0)
                click(*LOBBY_ROW)
                time.sleep(0.8)
                click(228, 268)
                time.sleep(2.0)
                # Ctrl+F searches this conversation; quotes match a phrase exactly.
                shell.chord("Control_L", "f")
                time.sleep(0.8)
                shell.type('"history message 7" ')
                time.sleep(1.5)
                shot("12a-search-phrase")
                # A result opens its message among the history around it, highlighted.
                click(*SEARCH_RESULT)
                time.sleep(1.5)
                shot("12b-search-jumped")
                # A live message waits for the newer history, which loads below
                # as the reader scrolls down; "Jump to latest" returns to the end.
                bots[0].send(kind="text", channel=2, body="A live message while you read older ones")
                time.sleep(1.2)
                timeline = (width // 2 + 160, height // 2)
                for _ in range(3):
                    wheel(*timeline, 2 * SCROLL_CLICKS)
                    time.sleep(1.0)
                shot("12c-search-newer")
                click(*JUMP_TO_LATEST(width, height))
                time.sleep(2.0)
                shot("12d-search-latest")
                # Ctrl+Shift+F searches every server, with the same words.
                shell.eval(CONTROL_SHIFT % ("F", "F"))
                time.sleep(1.5)
                shot("12e-search-all-servers")
                shell.key("Escape")
                time.sleep(0.6)
            if want("finder"):
                click(228, 268)  # #Development
                time.sleep(0.6)
                # Ctrl+K: recent conversations first; typing finds channels and people.
                shell.chord("Control_L", "k")
                time.sleep(0.8)
                shot("40-finder")
                shell.type("dev")
                time.sleep(0.8)
                shot("41-finder-channel")
                shell.key("BackSpace")
                shell.key("BackSpace")
                shell.key("BackSpace")
                shell.type("ada")
                time.sleep(0.8)
                shot("42-finder-person")
                shell.key("Return")  # a direct conversation with Ada
                time.sleep(1.0)
                shot("43-finder-direct")
                shell.chord("Control_L", "k")
                time.sleep(0.6)
                shell.type("lby")  # letters in order
                time.sleep(0.6)
                shell.key("Down")
                shell.key("Up")
                shot("44-finder-fuzzy")
                shell.key("Return")
                time.sleep(1.0)
                shot("45-finder-lobby")
                # Not behind a dialog: Settings keeps the keys.
                click(239, 773)
                time.sleep(0.8)
                shell.chord("Control_L", "k")
                time.sleep(0.8)
                shot("46-finder-not-behind-settings")
                shell.key("Escape")
                time.sleep(0.6)
                click(228, 268)  # back to #Development, as later scenes expect
                time.sleep(0.8)
            if want("runs"):
                # Routine activity in a row collapses into one line; a click expands it.
                for channel in (1, 2, 1, 2):
                    bots[2].send(kind="join", channel=channel)
                    time.sleep(0.5)
                time.sleep(1.0)
                shot("40-activity-run")
                click(520, 683)  # the run, the timeline's last line
                time.sleep(0.8)
                shot("41-activity-run-expanded")
            if want("unread"):
                # Messages out of sight: a count on the channel; a mention turns it red
                # and puts a red count on the server.
                click(189, 233)  # browse Lobby; Andi stays in Development
                time.sleep(0.8)
                bots[0].send(kind="text", channel=2, body="Anyone here?")
                time.sleep(1.2)
                shot("37-unread")
                bots[0].send(kind="text", channel=2, body="Andi, are you there?")
                time.sleep(1.2)
                shot("38-unread-mention")
                click(228, 267)  # back to Development: read
                time.sleep(0.8)
                shot("39-read")
            if "managed" in scenes:
                click(239, 773)  # Settings
                time.sleep(1.0)
                shot("40-managed-settings")
                click(*VOICE_ACTIVITY)  # does nothing: the system configuration sets it
                time.sleep(0.6)
                shot("41-managed-settings-unchanged")
                shell.key("Escape")
                time.sleep(0.4)
                click(*FRIENDS_SERVER)
                time.sleep(0.8)
                click(*SERVER_SETTINGS_BUTTON)
                time.sleep(1.0)
                shot("42-managed-server")
                shell.key("Escape")
                time.sleep(0.4)
            if want("toggles"):
                # Mute animates (slash, red); rapid clicks reverse it smoothly.
                def footer(name):
                    shot(name)
                    subprocess.run(["magick", str(args.output / f"{name}.png"), "-crop", "130x60+210+743",
                                    str(args.output / f"{name}-footer.png")], check=False)
                footer("30-muted")
                click(279, 773)
                time.sleep(0.6)
                footer("31-unmuted")
                click(279, 773)
                footer("32-muting")
                time.sleep(0.6)
                footer("33-muted-again")
                for _ in range(7):
                    click(279, 773)
                    time.sleep(0.04)
                time.sleep(0.6)
                footer("34-after-rapid-clicks")
            if want("emoji"):
                # Behind an open dialog, neither the picker nor Enter-to-send work.
                click(800, 738)  # the composer
                shell.type("draft")
                click(239, 773)  # Settings
                time.sleep(0.8)
                shell.chord("Control_L", "period")
                shell.key("Return")
                time.sleep(0.8)
                shot("26-settings-keeps-keys")
                shell.key("Escape")
                time.sleep(0.6)
                for _ in range(5):
                    shell.key("BackSpace")
                time.sleep(0.4)
                # Ctrl+. opens the picker; typing searches; Enter inserts the first match.
                click(800, 738)  # the composer
                time.sleep(0.4)
                shell.chord("Control_L", "period")
                time.sleep(1.0)
                shot("27-emoji")
                shell.type("thumbs")
                time.sleep(0.8)
                shot("28-emoji-search")
                shell.key("Return")
                time.sleep(0.8)
                shot("29-emoji-inserted")
            if want("clipping") and args.pulse_bin:
                # A second microphone driven to full scale: the adaptive controller warns.
                server = f"--server=unix:{shell.runtime / 'pulse' / 'native'}"
                pactl = [str(args.pulse_bin / "pactl"), server]
                subprocess.run([*pactl, "load-module", "module-null-sink", "sink_name=hot",
                                "sink_properties=device.description=Hot"], check=True, capture_output=True)
                subprocess.run([*pactl, "set-source-volume", "hot.monitor", "100%"], check=True)
                subprocess.run([*pactl, "set-default-source", "hot.monitor"], check=True)
                square = root / "square.raw"
                square.write_bytes((struct.pack("<f", 1.0) * 24 + struct.pack("<f", -1.0) * 24) * 1000)
                player = subprocess.Popen(
                    ["sh", "-c", f'while :; do cat "{square}"; done | "{args.pulse_bin / "pacat"}" {server} '
                                 '--device=hot --raw --format=float32le --rate=48000 --channels=1'],
                    start_new_session=True)
                try:
                    click(279, 773)  # unmute: the worker restarts on the new default microphone
                    time.sleep(9)
                    shot("14-clipping")
                    click(*CLIPPING_WARNING)
                    time.sleep(1.0)
                    shot("15-clipping-settings")
                    shell.key("Escape")
                    time.sleep(0.4)
                finally:
                    os.killpg(player.pid, signal.SIGTERM)
            if want("viewer"):
                click(*CHAT_IMAGE)
                time.sleep(2.0)
                shot("17-viewer")
                click(width // 2, height // 2 - 40, button=3)  # right-click the enlarged image
                time.sleep(0.8)
                shot("18-image-menu")
                # In the viewer Copy image is first (Open image is unnecessary).
                click(width // 2 + 60, height // 2 - 20)
                time.sleep(0.5)
                shot("18b-image-copied")
                def clipboard_image():
                    # Encoding and clipboard publication are asynchronous. A
                    # request issued before publication must be retried too.
                    shell.eval(READ_CLIPBOARD_IMAGE)
                    time.sleep(0.2)
                    return shell.eval("global._nuschlClipboardImage")
                encoded_clipboard = eventually(
                    clipboard_image,
                    "Copy image did not publish image/png on the private clipboard")
                copied_image = args.output / "18-copied-image.png"
                copied_image.write_bytes(base64.b64decode(encoded_clipboard, validate=True))
                assert copied_image.read_bytes().startswith(b"\x89PNG\r\n\x1a\n")
                # Compare decoded pixels, not PNG compression choices. A channel
                # swap, a blank clipboard image or a copied URI must fail.
                def rgba(path):
                    return subprocess.run(["magick", str(path), "-depth", "8", "rgba:-"],
                                          check=True, capture_output=True, timeout=15).stdout
                assert rgba(copied_image) == rgba(root / "shot.png"), \
                    "the clipboard image differs from the displayed image"
                print("Copy image: native image/png clipboard pixels match the displayed image.", flush=True)
                shell.key("Escape")
                time.sleep(0.5)
            if want("notify"):
                shell.eval(CLAIM_NOTIFICATIONS)
                andi = next(e["user"]["session"] for e in reversed(bots[1].events)
                            if e["kind"] == "user" and e["user"]["name"] == "Andi")
                click(*LOBBY_ROW)  # look at another channel: Development is out of sight
                time.sleep(0.8)
                bots[1].send(kind="direct", session=andi, body="hey Andi, <b>got a minute</b>? & more")
                time.sleep(2.0)
                path = shell.screenshot(args.output / "19-notification-direct.png")
                print(path, flush=True)
                bots[0].send(kind="html", channel=2,
                             html=f"<img src=\"data:image/PNG;base64,{mumble_image}\" />")
                time.sleep(2.0)
                path = shell.screenshot(args.output / "20-notification-image.png")
                print(path, flush=True)
                failures = [line for line in (args.output / "desktop.log").read_text().splitlines()
                            if "notification failed" in line]
                assert not failures, failures
                received = json.loads(shell.eval(
                    "JSON.stringify(Main.messageTray.getSources().flatMap(source => source.notifications"
                    ".map(note => ({title: note.title, body: note.body, gicon: Boolean(note.gicon)}))))"))
                print("GNOME received:", received, flush=True)
                # GNOME keeps the body as markup: the message's own tags must arrive escaped.
                assert any(note["title"] == "Grace"
                           and "&lt;b&gt;got a minute&lt;/b&gt;? &amp; more" in note["body"]
                           for note in received), received
                assert any(note["title"].endswith("in Development") and note["gicon"] for note in received), received
            if want("scroll"):
                click(228, 268)  # #Development (other scenes may have opened another conversation)
                time.sleep(0.6)
                # Several 40-message pages of local history, paced for the content worker's queue.
                for n in range(1, 151):
                    bots[(n // 3) % 2].send(kind="text", channel=2, body=f"History message {n}")
                    time.sleep(0.05)
                time.sleep(2.0)
                # They arrived live; reopened, the conversation starts from its latest page.
                click(*LOBBY_ROW)
                time.sleep(0.8)
                click(228, 268)
                time.sleep(4.0)
                shot("21-scroll-latest")
                timeline = (width // 2 + 160, height // 2)
                # Up the latest page, short of a screen below its top: nothing loads yet.
                wheel(*timeline, -5)
                time.sleep(0.8)
                # Then to its top: the previous page is on its way (one
                # content worker renders it, so it may already be there)…
                wheel(*timeline, -12)
                time.sleep(0.4)
                shot("22-scroll-loading")
                # …and is prepended above without moving what is on screen.
                time.sleep(4.0)
                shot("23-scroll-prepended")
                # Further up until local history begins.
                for _ in range(6):
                    wheel(*timeline, -2 * SCROLL_CLICKS)
                    time.sleep(3.0)
                shot("24-scroll-beginning")
                wheel(*timeline, SCROLL_CLICKS)
                time.sleep(0.6)
                bots[0].send(kind="text", channel=2, body="A live message while you read older ones")
                time.sleep(1.2)
                shot("25-scroll-new-messages")
                click(*JUMP_TO_LATEST(width, height))
                time.sleep(1.0)
                shot("26-scroll-jumped")
            if want("paste"):
                click(228, 268)  # #Development
                time.sleep(0.6)
                photo = make_photo(root / "paste.png", 1920, 1080)
                shell.eval(SET_CLIPBOARD % json.dumps(str(photo)))
                click(*COMPOSER)
                time.sleep(0.4)
                started = time.monotonic()
                shell.eval(PASTE)
                # Pasting attaches the image; Enter sends it.
                time.sleep(4.0)
                shot("27a-pasted-attached")
                shell.key("Return")
                sent = bots[0].wait(lambda e: e["kind"] == "text" and e["author"] == "Andi", timeout=60)
                print(f"pasted image sent after {time.monotonic() - started:.1f} s: "
                      f"{len(sent['body'])} bytes, {sent['body'][:40]}", flush=True)
                # Stock Mumble's format, within the stock image message limit.
                assert sent["html"] and sent["body"].startswith('<img src="data:image/jpeg;base64,'), sent["body"][:80]
                assert len(sent["body"]) <= 131072 and ("%2B" in sent["body"] or "%2F" in sent["body"])
                time.sleep(2.0)
                shot("27-pasted-image")
            if want("attach"):
                click(228, 268)  # #Development
                time.sleep(0.6)
                # Without a file chooser portal in this session, the attach
                # button says so (Escape closes a chooser that did open).
                click(*ATTACH_BUTTON)
                time.sleep(1.5)
                shot("28-attach-chooser")
                shell.key("Escape")
                time.sleep(0.4)
                # Five pasted pictures…
                click(*COMPOSER)
                time.sleep(0.4)
                for n in range(5):
                    shell.eval(SET_CLIPBOARD % json.dumps(str(make_png(root / f"attach{n}.png", 320, 20 + n, height=200))))
                    time.sleep(0.3)
                    shell.eval(PASTE)
                    time.sleep(0.6)
                # …and a drawing and the notes, dropped together on the conversation.
                images = 5
                drawing = make_png(root / "drawing.png", 400, 31, height=300)
                notes = root / "notes.pdf"
                notes.write_bytes(b"%PDF-1.4\n" + b"%" * 12_300)
                dragon = args.dragon_drop or shutil.which("dragon-drop")
                if dragon:
                    source = subprocess.Popen([str(dragon), "--all-compact", "--and-exit", str(drawing), str(notes)],
                                              env=shell.app_environment(), stdout=subprocess.DEVNULL,
                                              stderr=subprocess.DEVNULL, start_new_session=True)
                    eventually(lambda: any(not (w["title"] or "").startswith("Nuschl") for w in shell.windows()),
                               "dragon-drop did not appear")
                    title = next(w["title"] for w in shell.windows() if not (w["title"] or "").startswith("Nuschl"))
                    time.sleep(1.0)
                    # Its one button ("2 files") fills the bottom of its small window.
                    box = shell.window(title)
                    nuschl = shell.window("Nuschl")
                    shell.drag((box["x"] + box["width"] // 2, box["y"] + box["height"] - 15),
                               (nuschl["x"] + 900, nuschl["y"] + 250), steps=30, hold=1.0)
                    time.sleep(1.0)
                    source.wait(timeout=10)
                    images += 1
                else:
                    print("no dragon-drop: dropping files is not covered", flush=True)
                click(*COMPOSER)
                shell.type("Five pictures, a drawing & the notes")
                time.sleep(4.0)
                shot("29-attach-tray")
                started = time.monotonic()
                shell.key("Return")
                time.sleep(0.4)
                shot("30-attach-sending")
                # One image per message, the text with the first: none lost to
                # the stock server's rate limit (a burst of five, then one a second).
                received = []
                for _ in range(images):
                    received.append(bots[0].wait(lambda e: e["kind"] == "text" and e["author"] == "Andi", timeout=60))
                    received[-1]["after"] = time.monotonic() - started
                print("attached images arrived after", ", ".join(f"{m['after']:.1f} s" for m in received), flush=True)
                assert received[0]["body"].startswith("Five pictures, a drawing &amp; the notes<br/><img src=\"data:image/"), \
                    received[0]["body"][:80]
                assert all(m["html"] and m["body"].count("<img ") == 1 and len(m["body"]) <= 131072 for m in received)
                assert all(m["body"].startswith("<img src=\"data:image/png;base64,") for m in received[1:])
                # Four at once, then one every 1.25 s.
                assert received[-1]["after"] > 1.25 * (images - 4) - 0.3, "faster than the rate limit allows"
                time.sleep(2.0)
                shot("31-attach-sent")
                wheel(800, 400, -2 * SCROLL_CLICKS)  # up to the text that went with the first picture
                time.sleep(1.0)
                shot("32-attach-caption")
            if want("transfer") and wormhole:
                click(228, 268)  # #Development
                time.sleep(0.6)
                offer = bots[0].wait(lambda e: e["kind"] == "text" and e["author"] == "Andi"
                                     and "wormhole" in e["body"], timeout=60)
                code = re.search(r"receive ([0-9]+-[a-z0-9-]+)</code>", offer["body"]).group(1)
                print("Andi offers with", code, flush=True)
                time.sleep(1.0)
                shot("30-offer-waiting")
                # Someone receives (slowly: a 4 MB/s disk), and the next code is posted.
                receiver = Worker(args.binaries, {"kind": "receive", "code": code, "max_size": 60 * 1000 * 1000,
                                                  "servers": wormhole.servers})
                receiver.slow = 0.06
                receiver.wait("started")
                time.sleep(4.0)
                shot("31-offer-sending")
                receiver.wait("done", timeout=120)
                receiver.close()
                time.sleep(2.5)
                shot("32-offer-sent")
                # Ada offers a file the same way, and then stops offering it.
                (root / "notes.txt").write_text("meeting notes\n" * 5000)
                sender = Worker(args.binaries, {"kind": "offer", "name": "meeting-notes.txt",
                                                "servers": wormhole.servers}, root / "notes.txt")
                ada_code = sender.wait("code")["code"]
                announcement = (f"📎 <b>meeting-notes.txt</b> (70 kB) — receive with <a href=\"https://magic-wormhole."
                                f"readthedocs.io/\">magic-wormhole</a>: <code>wormhole --relay-url="
                                f"{wormhole.servers['rendezvous']} receive {ada_code}</code>")
                bots[0].send(kind="html", channel=2, html=announcement)
                time.sleep(2.0)
                shot("33-offer-received")
                sender.send({"kind": "cancel"})
                sender.close()
                bots[0].send(kind="html", channel=2,
                             html="📎 <b>meeting-notes.txt</b> (70 kB) — no longer available.")
                time.sleep(2.0)
                shot("34-offer-ended")
                # Andi stops offering: the channel is told, the card says so.
                click(*STOP_OFFERING)
                bots[0].wait(lambda e: e["kind"] == "text" and e["author"] == "Andi"
                             and "no longer available" in e["body"], timeout=30)
                time.sleep(1.5)
                shot("35-offer-stopped")
            if want("appearance"):
                click(239, 773)  # Settings
                time.sleep(1.0)
                # To the end of Settings, then back up to the section's heading.
                wheel(640, 400, 40)
                time.sleep(1.2)
                wheel(640, 400, -1)
                time.sleep(0.8)
                shot("35-appearance-dark")
                click(*LIGHT_THEME)
                time.sleep(0.8)
                shot("36-appearance-light")
                click(*INTERFACE_FONT)
                time.sleep(0.6)
                shell.type("sans")
                time.sleep(0.6)
                shot("36b-font-picker")
                click(*INTERFACE_FONT)  # closes the picker
                time.sleep(0.4)
                # Zoom from anywhere, with Settings open: two steps are 125 %.
                shell.chord("Control_L", "equal")
                time.sleep(0.6)
                shell.chord("Control_L", "equal")
                time.sleep(0.8)
                shot("37-settings-light-125")
                shell.key("Escape")
                time.sleep(0.8)
                shot("38-light-125")
                # Zoom keys leave the composer's text alone.
                click(700, 722)  # the composer, at 125 %
                shell.type("hi")
                shell.chord("Control_L", "0")
                time.sleep(0.8)
                shot("39-light")
                shell.key("BackSpace")
                shell.key("BackSpace")
            if resizing:
                measured = {}

                def measure(name):
                    """Sweep the size as a script would, then drag the corner as a person would."""
                    window = shell.window("Nuschl")
                    w, h = window["width"], window["height"]
                    # Two round trips down to 900x620 and back, one size per tick.
                    sweep = []
                    for _ in range(2):
                        for i in range(121):
                            t = 1 - abs(i - 60) / 60
                            sweep.append([round(w - (w - 900) * t), round(h - (h - 620) * t)])
                    ticks = shell.resize_sweep("Nuschl", sweep)
                    measured[f"{name} sweep"] = {"ticks": frame_timing.lag(ticks, sweep), "span": frame_timing.moving(ticks, sweep),
                                                 "log": ticks, "steps": sweep}
                    time.sleep(0.5)
                    shell.place("Nuschl", 40, 40, w, h)
                    time.sleep(1.0)
                    # The bottom right corner, in the shadow where the resize edge is, at
                    # 125 Hz like a mouse: in by 380x180 over a second, half way out
                    # again, and held still for a moment before letting go.
                    window = shell.window("Nuschl")
                    corner = (window["x"] + window["width"] + 4, window["y"] + window["height"] + 4)
                    path, expected = [], []
                    for i in range(251):
                        t = i / 125 if i <= 125 else max(0.5, 1 - (i - 125) / 126)
                        dx, dy = round(-380 * t), round(-180 * t)
                        path.append([corner[0] + dx, corner[1] + dy])
                        expected.append([window["width"] + dx, window["height"] + dy])
                    ticks = shell.drag_resize("Nuschl", corner, path)
                    measured[f"{name} drag"] = {"ticks": frame_timing.lag(ticks, expected), "span": frame_timing.moving(ticks, expected),
                                                "log": ticks, "steps": expected}
                    time.sleep(0.5)
                    shell.place("Nuschl", 40, 40, w, h)
                    time.sleep(1.0)

                def zoomed(name):
                    """Measure again at 125 % (two zoom steps), which changes the window's scale."""
                    shell.chord("Control_L", "equal")
                    shell.chord("Control_L", "equal")
                    time.sleep(1.0)
                    shot(f"{name}-125")
                    measure(f"{name} 125%")
                    shell.chord("Control_L", "0")
                    time.sleep(1.0)

                def report():
                    log = args.output / "frames.log"
                    events = frame_timing.read_log(log) if log.exists() else []
                    for name, entry in measured.items():
                        entry["frames"] = frame_timing.summarize(events, *entry["span"])
                        print(f"{name}: {entry['ticks']}\n{frame_timing.format_summary(entry['frames'])}", flush=True)
                    (args.output / "resize.json").write_text(json.dumps(measured, indent=1))

                click(*LOBBY_ROW)  # an empty conversation
                time.sleep(1.5)
                shot("40-resize-empty")
                measure("empty")
                zoomed("40-resize-empty")
                if not args.resize_messages:
                    report()
                    return
                click(228, 268)  # #Development
                time.sleep(1.0)
                # A thousand messages, as a long day in a busy channel leaves them:
                # code, pictures, long paragraphs and formatting among short lines.
                encoded_image = mumble_image
                for n in range(1, args.resize_messages + 1):
                    bot = bots[n % 2]
                    if n % 50 == 0:
                        bot.send(kind="html", channel=2, html=f"Screenshot {n}:<br/><img src=\"data:image/PNG;base64,{encoded_image}\" />")
                    elif n % 10 == 0:
                        bot.send(kind="text", channel=2, body=f"Patch {n}:\n\n```rust\nfn handle_{n}(event: &Event) -> Result<()> {{\n"
                                 "    let state = self.state.lock();\n    state.apply(event)?;\n    Ok(())\n}\n```")
                    elif n % 7 == 0:
                        bot.send(kind="text", channel=2, body=" ".join(f"word{n}-{k}" for k in range(70)))
                    elif n % 13 == 0:
                        bot.send(kind="html", channel=2, html=f"<b>Release {n}</b>: <i>notes</i><ul><li>First</li><li>Second</li></ul>")
                    else:
                        bot.send(kind="text", channel=2, body=f"Message {n}: sounds good, let's do that")
                    time.sleep(0.05)
                time.sleep(6.0)
                shot("41-resize-busy")
                measure("busy")
                shot("42-resize-busy-after")
                zoomed("42-resize-busy")
                # Scrolled up into the middle: the anchor holds what is read in place.
                wheel(width // 2 + 160, height // 2, -6 * SCROLL_CLICKS)
                time.sleep(1.5)
                shot("43-resize-scrolled")
                measure("scrolled")
                shot("44-resize-scrolled-after")
                report()
            if want("maximize"):
                click(width // 2, 41, count=2)
                time.sleep(1.5)
                window = shell.window("Nuschl")
                path = shell.screenshot(args.output / "03-maximized.png")
                print(path, flush=True)
                shell.click(width // 2, window["y"] + 18, count=2)
                time.sleep(1.0)
        finally:
            if app and app.poll() is None:
                os.killpg(app.pid, signal.SIGTERM)
                try:
                    app.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    os.killpg(app.pid, signal.SIGKILL)
            profile = root / "profile"
            if profile.exists():
                # What the profile holds where (crates/session/src/layout.rs), for review.
                (args.output / "profile-files.txt").write_text("".join(
                    f"{oct(path.stat().st_mode & 0o777)} {path.relative_to(profile)}\n"
                    for path in sorted(profile.rglob("*"))))
            for bot in bots:
                try:
                    bot.close()
                except Exception:  # noqa: BLE001 - best-effort cleanup
                    pass
            if pulse and pulse.poll() is None:
                os.killpg(pulse.pid, signal.SIGTERM)
            for helper in helpers:
                if helper.poll() is None:
                    os.killpg(helper.pid, signal.SIGTERM)
            shell.stop()
            murmur.stop()
            if rail_murmur:
                rail_murmur.stop()
            if wormhole:
                wormhole.stop()


if __name__ == "__main__":
    main()
