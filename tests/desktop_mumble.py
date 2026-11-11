#!/usr/bin/env python3
"""Real GPUI, stock Mumble, confined workers and private synthetic PulseAudio I/O.

Run inside xvfb-run. This never opens the host microphone or normal audio service.
Controls are found as a screen reader finds them, over AT-SPI on a private session
bus (accessibility.py's helpers): by role and accessible name, and clicked at the position
the accessibility tree gives (a double-click, a hover, typing) or activated
through the Action interface. OCR reads only rendered text that has no
accessible counterpart (a tooltip).
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import signal
import sqlite3
import struct
import subprocess
import tempfile
import time
import uuid

from accessibility import items, start_registry
from mumble_client import Client, ReferenceOpus, certificate_pin, rms, tone


def eventually(predicate, message, timeout=25):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            result = predicate()
            if result:
                return result
        except (OSError, sqlite3.Error, ValueError, subprocess.CalledProcessError, RuntimeError) as error:
            last = str(error)
        time.sleep(0.15)
    raise AssertionError(f"{message}; last error: {last}")


def terminate(process):
    if process is not None and process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait(timeout=5)


class DesktopTest:
    def __init__(self, args, root, output):
        self.args, self.root, self.output = args, root, output
        self.runtime = root / "audio-runtime"
        (self.runtime / "pulse").mkdir(parents=True, mode=0o700)
        (self.runtime / "state").mkdir()
        self.runtime.chmod(0o700)
        self.socket = self.runtime / "pulse/native"
        self.profile = root / "profile"
        self.env = os.environ.copy()
        self.env.pop("WAYLAND_DISPLAY", None)
        self.env.pop("DBUS_SESSION_BUS_ADDRESS", None)
        self.env.update(XDG_RUNTIME_DIR=str(self.runtime), NUSCHEL_AUDIO_BACKEND="pulse",
                        LIBGL_ALWAYS_SOFTWARE="1", OMP_NUM_THREADS="2", LP_NUM_THREADS="2", RUST_LOG="nuschel=debug")
        self.pulse = self.app = self.peer = None
        self.window = self.nuschel = self.registry = None
        self.helpers = []
        self.last_click = 0.0
        self.reference = ReferenceOpus(args.opus_library)

    def start_audio(self):
        # Speakers (nuschel_fixture, recorded from its monitor) and a microphone
        # of their own (nuschel_microphone's monitor): Nuschel's echo canceller
        # removes whatever the speakers play from the microphone, so a voice
        # played into the speakers would be cancelled as echo.
        script = self.runtime / "default.pa"
        script.write_text(f"load-module module-native-protocol-unix socket={self.socket} auth-anonymous=1\n"
                          "load-module module-null-sink sink_name=nuschel_fixture rate=48000 channels=2\n"
                          "load-module module-null-sink sink_name=nuschel_microphone rate=48000 channels=2\n"
                          "set-default-sink nuschel_fixture\nset-default-source nuschel_microphone.monitor\n")
        environment = self.env | {"PULSE_RUNTIME_PATH": str(self.runtime / "pulse"),
                                  "PULSE_STATE_PATH": str(self.runtime / "state")}
        self.pulse = subprocess.Popen([
            str(self.args.pulse_bin / "pulseaudio"), "--daemonize=no", "--use-pid-file=no",
            "--exit-idle-time=-1", "--log-target=stderr", "-n", "--file=" + str(script),
        ], env=environment, stdout=(self.output / "pulse.log").open("a"), stderr=subprocess.STDOUT,
            start_new_session=True)
        eventually(lambda: self.socket.exists() and self.pulse_json("sinks"), "private audio daemon did not start")

    def pulse_json(self, resource):
        result = subprocess.run([str(self.args.pulse_bin / "pactl"), "--server=unix:" + str(self.socket),
                                 "--format=json", "list", resource], capture_output=True, text=True,
                                check=True, timeout=5)
        return json.loads(result.stdout)

    def spawn(self, command, env, log):
        """A helper (session bus, accessibility bus, registry), stopped at the end."""
        process = subprocess.Popen(command, env=env, stdout=(self.output / log).open("w"),
                                   stderr=subprocess.STDOUT, start_new_session=True)
        self.helpers.append(process)
        return process

    def start_accessibility(self):
        """A private session bus with an accessibility bus and registry, as a
        desktop session has them, so the test never reaches the user's session."""
        socket = self.root / "session-bus"
        self.spawn([str(self.args.dbus_daemon), "--session", "--nofork", f"--address=unix:path={socket}"],
                   self.env, "session-bus.log")
        eventually(socket.exists, "the private session bus did not start")
        # AccessKit honours NO_AT_BRIDGE (NixOS sets it where at-spi2-core is off).
        self.env.pop("NO_AT_BRIDGE", None)
        self.env["DBUS_SESSION_BUS_ADDRESS"] = f"unix:path={socket}"
        self.registry = start_registry(self.args.at_spi, self.env, self.spawn)

    def start_app(self, *arguments):
        if self.registry is None:
            self.start_accessibility()
        log = self.output / "desktop.log"
        self.app = subprocess.Popen([str(self.args.binaries / "nuschel"), "--profile-dir", str(self.profile),
                                     *arguments], env=self.env, stdout=log.open("a"),
                                    stderr=subprocess.STDOUT, start_new_session=True)
        def window():
            if self.app.poll() is not None:
                raise AssertionError(log.read_text()[-4000:])
            result = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "^Nuschel"],
                                    env=self.env, capture_output=True, text=True)
            return result.stdout.splitlines()[0] if result.returncode == 0 else None
        self.window = eventually(window, "Nuschel did not open a window")
        self.xdo("windowsize", self.window, "1120", "850")
        self.xdo("windowmove", self.window, "0", "0")
        previous = self.nuschel

        def registered():
            if self.app.poll() is not None:
                raise AssertionError(log.read_text()[-4000:])
            return next((node for node in self.registry.children() if node.name.lower() == "nuschel"
                         and (previous is None or node.destination != previous.destination)), None)
        self.nuschel = eventually(registered, "Nuschel did not register with the accessibility registry", timeout=45)

    def xdo(self, *arguments):
        subprocess.run(["xdotool", *map(str, arguments)], env=self.env, check=True, timeout=10)

    def find(self, role, name, partial=False):
        """The first node with this role (None: any) and accessible name: equal
        to `name`, containing it (partial), or accepted by it (a function)."""
        matches = name if callable(name) else (lambda label: name in label) if partial else name.__eq__
        for node, node_role, label, _ in items(self.nuschel):
            if (role is None or node_role == role) and matches(label):
                return node
        return None

    def wait_for(self, role, name, partial=False, timeout=25):
        try:
            return eventually(lambda: self.find(role, name, partial), f"no {role or 'node'} named {name!r}", timeout)
        except AssertionError as error:
            self.write_tree("tree-failure.txt")
            raise AssertionError(f"{error}; see tree-failure.txt") from None

    def absent(self, role, name, partial=False):
        return self.find(role, name, partial) is None

    def write_tree(self, name):
        lines = [f"{role}: {label!r}" for _, role, label, _ in items(self.nuschel)]
        (self.output / name).write_text("\n".join(lines) + "\n")

    def connected(self):
        """The server's status reads Connected (with its transport once known)."""
        self.wait_for("label", lambda label: label.startswith("Connected"))

    def press(self, name, role="push button"):
        """Activate a control through AT-SPI, as a screen reader does."""
        self.wait_for(role, name).press()

    # GPUI counts clicks within 400 ms (and 5 px) of each other as one double
    # or triple click; separate clicks are spaced further apart, as a person's are.
    MULTI_CLICK = 0.4

    def click_node(self, node, count=1):
        """Click a control with the pointer, at the centre the accessibility tree gives."""
        x, y = node.centre()
        time.sleep(max(0.0, self.last_click + self.MULTI_CLICK + 0.1 - time.monotonic()))
        self.xdo("mousemove", "--window", self.window, x, y)
        self.xdo("click", "--repeat", count, "--delay", 100, 1)
        self.last_click = time.monotonic()

    def inside(self, node, area):
        """Whether the node lies wholly within the area node (a scrolled viewport)."""
        _, top, _, height = area.extents()
        _, y, _, h = node.extents()
        return h > 0 and top <= y and y + h <= top + height

    def fill_field(self, name, text, role="entry"):
        """Click into a field and replace its text. (Fields do not expose their
        value over AT-SPI; the saved result is checked instead.)"""
        self.click_node(self.wait_for(role, name))
        self.xdo("key", "ctrl+a")
        if text:
            self.xdo("type", "--clearmodifiers", "--delay", 1, "--", text)

    def type_message(self, body):
        """Type into the composer; Shift+Enter starts a new line (Enter sends)."""
        self.click_node(self.wait_for("entry", "Message"))
        self.xdo("key", "ctrl+a")
        for index, line in enumerate(body.split("\n")):
            if index:
                self.xdo("key", "shift+Return")
            if line:
                self.xdo("type", "--clearmodifiers", "--delay", 1, "--", line)

    def capture(self, name):
        path = self.output / (name + ".png")
        subprocess.run(["magick", "import", "-window", self.window, str(path)],
                       env=self.env, check=True, timeout=10)
        return path

    def read_pixels(self, name, left, top, width, height):
        """OCR of a part of the window, for rendered text with no accessible
        counterpart. Grey, enlarged, and dark text on light in either theme."""
        path = self.capture(name)
        prepared = self.output / f"{name}-ocr.png"
        subprocess.run(["magick", str(path), "-alpha", "off", "-crop", f"{width}x{height}+{left}+{top}", "+repage",
                        "-colorspace", "gray", "-resize", "300%", str(prepared)],
                       env=self.env, check=True, timeout=10)
        mean = float(subprocess.run(["magick", str(prepared), "-format", "%[fx:mean]", "info:"], env=self.env,
                                    capture_output=True, text=True, check=True, timeout=10).stdout)
        if mean < 0.5:
            subprocess.run(["magick", str(prepared), "-negate", str(prepared)], env=self.env, check=True, timeout=10)
        return subprocess.run(["tesseract", str(prepared), "stdout", "--psm", "6"], env=self.env,
                              capture_output=True, text=True, check=True, timeout=20).stdout

    def rows(self):
        with sqlite3.connect(self.profile / "data/history/history.sqlite3") as database:
            return database.execute("SELECT channel, author, body FROM messages ORDER BY id").fetchall()

    def has_message(self, body):
        return any(row[2] == body for row in self.rows())

    def recorded_playback(self, packets):
        self.peer.unmute()
        recording = self.output / "received-synthetic.pcm"
        with recording.open("wb") as output:
            recorder = subprocess.Popen([
                str(self.args.pulse_bin / "parec"), "--server=unix:" + str(self.socket),
                "--device=nuschel_fixture.monitor", "--raw", "--format=float32le", "--rate=48000",
                "--channels=2", "--latency-msec=20", "--process-time-msec=10",
            ], stdout=output, stderr=(self.output / "recorder.log").open("a"), start_new_session=True)
            try:
                time.sleep(0.25)
                for index, packet in enumerate(packets):
                    self.peer.send(kind="voice", sequence=index * 2, opus=packet, end=index == len(packets) - 1)
                    time.sleep(0.02)
                time.sleep(0.5)
            finally:
                terminate(recorder)
        data = recording.read_bytes()
        samples = struct.unpack("<%df" % (len(data) // 4), data)
        assert rms(samples) > 0.01, "Nuschel did not decode/play the reference audio"

    def inject_tone(self, seconds=1):
        """A tone into the microphone."""
        mono = tone(0, int(48000 * seconds))
        data = struct.pack("<%df" % (2 * len(mono)), *[value for sample in mono for value in (sample, sample)])
        subprocess.run([
            str(self.args.pulse_bin / "pacat"), "--server=unix:" + str(self.socket),
            "--playback", "--device=nuschel_microphone", "--raw", "--format=float32le", "--rate=48000",
            "--channels=2", "--latency-msec=20", "--process-time-msec=10",
        ], input=data, capture_output=True, check=True, timeout=10)

    def captured_audio(self, session):
        time.sleep(0.5)
        before = len(self.peer.events)
        self.inject_tone(2)
        time.sleep(0.25)
        packets = [event["opus"] for event in list(self.peer.events)[before:]
                   if event["kind"] == "audio" and event["sender"] == session and not event["end"]]
        assert len(packets) >= 15, f"Too few captured audio packets: {len(packets)}, events {before}->{len(self.peer.events)}, user={self.peer.users.get(session)}, tail={[(event['kind'], event.get('epoch'), event.get('sequence')) for event in list(self.peer.events)[-6:]]}"
        assert rms(self.reference.decode(packets)) > 0.01, "reference decoder did not hear the captured tone"

    def check_channel_browser(self):
        voice_profile = self.profile
        self.profile = self.root / "browser-profile"
        (self.profile / "config").mkdir(parents=True)
        long_name = "A very long channel name with spaces and Unicode 👋 ending in FULLNAMEVISIBLE"
        channels = [
            dict(id=0, parent=None, name="Root", position=0),
            dict(id=1, parent=0, name="Group branch", position=0),
            dict(id=2, parent=1, name="Second depth", position=0),
            dict(id=3, parent=2, name="Third depth", position=0),
            dict(id=4, parent=0, name=long_name, position=1),
        ] + [dict(id=index, parent=0, name=f"Scroll row {index:02}", position=index) for index in range(5, 60)]
        # An earlier version's servers.json, with the tree inside: it moves to the cache.
        (self.profile / "config/servers.json").write_text(json.dumps([
            dict(id=str(uuid.uuid4()), name="Offline channel browser", host="127.0.0.1", port=64738,
                 username="Preview", cached_channels=channels)
        ]))
        self.start_app()
        self.wait_for("tree item", "Group branch")
        assert "cached_channels" not in (self.profile / "config/servers.json").read_text()
        assert len(next(iter(json.loads((self.profile / "cache/channels.json").read_text()).values()))) == 60
        assert self.absent("tree item", "Second depth") and self.absent("tree item", "Third depth"), \
            "branches below the root started expanded"
        self.capture("channel-browser-default")
        self.press("Toggle Group branch")
        self.wait_for("tree item", "Second depth")
        assert self.absent("tree item", "Third depth"), "deeper branches opened automatically"
        self.press("Toggle Second depth")
        self.wait_for("tree item", "Third depth")
        self.press("Toggle Group branch")
        eventually(lambda: self.absent("tree item", "Second depth"), "Group branch did not collapse")
        # Hovering the truncated row shows its full name in a tooltip, which only
        # the pixels show (the row's accessible name is the full name anyway).
        row = self.wait_for("tree item", long_name)
        x, y = row.centre()
        self.xdo("mousemove", "--window", self.window, x, y)
        left, top, _, _ = row.extents()
        eventually(lambda: "FULLNAMEVISIBLE" in self.read_pixels("channel-long-name-tooltip", left, top, 560, 110),
                   "the long name's tooltip did not show its end")
        tree = self.wait_for("tree", "Channels")
        tree_x, tree_y, tree_width, tree_height = tree.extents()
        self.xdo("mousemove", "--window", self.window, tree_x + tree_width // 2, tree_y + tree_height // 2)
        self.xdo("click", "--repeat", 4, "--delay", 25, 5)
        self.capture("channel-browser-wheel-moving")
        group = self.wait_for("tree item", "Group branch")
        eventually(lambda: not self.inside(group, tree), "wheel scrolling did not move the channel list")
        self.capture("channel-browser-wheel")
        # Drag the real scrollbar (along the tree's right edge) to the bottom;
        # fixed-height rows must remain reachable.
        bar = tree_x + tree_width - 7
        self.xdo("mousemove", "--window", self.window, bar, tree_y + 230, "mousedown", 1)
        self.xdo("mousemove", "--window", self.window, bar, tree_y + 390)
        self.xdo("mousemove", "--window", self.window, bar, tree_y + tree_height + 31)
        self.xdo("mouseup", 1)
        last = self.wait_for("tree item", "Scroll row 59")
        eventually(lambda: self.inside(last, tree), "the scrollbar did not reach the last row")
        self.capture("channel-browser-scrollbar")
        terminate(self.app)
        self.app = self.window = None
        self.profile = voice_profile
        print("Offline channel tree defaults, branch expansion, long-name tooltip and scrollbar passed.", flush=True)

    def run(self):
        self.check_channel_browser()
        pin = certificate_pin(self.args.binaries, self.args.host, self.args.port, self.root / "certificate-probe")
        self.peer = Client(self.args.binaries, self.args.host, self.args.port, pin, "DesktopPeer", self.root / "peer")
        self.peer.send(kind="join", channel=2)
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == self.peer.session
                       and event["user"]["channel"] == 2)
        self.start_audio()
        # Capture checks inject a pure tone; voice activation is covered by DSP unit tests.
        (self.profile / "config").mkdir(parents=True, exist_ok=True)
        (self.profile / "config/settings.json").write_text(json.dumps({"audio": {"transmit": "continuous"}}))
        self.start_app()
        self.press("Add a server")
        self.fill_field("Server address", self.args.host)
        self.fill_field("Port", str(self.args.port))
        self.fill_field("Server name", "Fixture Mumble")
        self.fill_field("Username", "NuschelDesktop")
        self.fill_field("Server password", "not-saved-fixture-password", role="password text")
        # Off, so that the restart below starts offline with saved history.
        self.press("Connect when Nuschel starts", role="toggle button")
        self.press("Add and connect")
        self.wait_for("label", "Confirm server certificate")
        assert not any(user["name"] == "NuschelDesktop" for user in self.peer.users.values())
        self.capture("certificate")
        self.press("Trust certificate and connect")
        gui = self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["name"] == "NuschelDesktop")["user"]
        session = gui["session"]
        self.connected()
        eventually(lambda: self.pulse_json("sink-inputs"), "desktop playback did not start")
        assert not self.pulse_json("source-outputs"), "muted startup opened a microphone"
        bookmark = json.loads((self.profile / "config/servers.json").read_text())[0]
        assert (bookmark["name"], bookmark["host"], bookmark["port"], bookmark["username"]) == \
            ("Fixture Mumble", self.args.host, self.args.port, "NuschelDesktop"), bookmark
        assert bookmark["certificate_sha256"] == pin
        assert bookmark.get("auto_connect") is False, f"the server connects at start: {bookmark}"
        assert "not-saved-fixture-password" not in json.dumps(bookmark) and "password" not in bookmark
        # Branches where people are open on connect: DesktopPeer is in Development, under Lobby.
        self.wait_for("tree item", "Development")
        self.browse_development()
        assert self.peer.users[session]["channel"] == 0, "browsing text moved voice"
        self.click_node(self.wait_for("tree item", "Development"), count=2)  # Double-click joins voice.
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == session
                       and event["user"]["channel"] == 2)
        body = "DESKTOPHISTORYMARKER\n\n```rust\nlet connected = true;\n```"
        self.type_message(body)
        self.xdo("key", "Return")
        eventually(lambda: self.has_message(body), "desktop composer lost newlines or failed to save/send")
        sent = self.peer.wait(lambda event: event["kind"] == "text" and event["author"] == "NuschelDesktop")
        assert sent["target"] == {"kind": "channel", "id": 2}, f"the message did not go to Development: {sent}"
        reply = "PEERHISTORYMARKER\n\n```rust\nfn legacy_server() {}\n```"
        self.peer.send(kind="text", channel=2, body=reply)
        eventually(lambda: self.has_message(reply), "incoming rich text was not normalized and saved")
        self.wait_for("list item", "PEERHISTORYMARKER", partial=True)
        self.capture("chat")
        print("Desktop add/certificate flow, channel tree, independent browsing/join, multiline chat and history passed.", flush=True)
        self.recorded_playback(self.reference.encode_tone(50))
        self.press("Unmute")
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == session
                       and not event["user"]["self_muted"])
        eventually(lambda: self.pulse_json("source-outputs"), "unmute did not open capture")
        self.captured_audio(session)
        self.press("Mute")
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == session
                       and event["user"]["self_muted"])
        eventually(lambda: not self.pulse_json("source-outputs"), "mute left microphone capture open")
        time.sleep(0.3)
        before = len([event for event in self.peer.events if event["kind"] == "audio" and event["sender"] == session and not event["end"]])
        self.inject_tone(0.5)
        time.sleep(0.2)
        after = len([event for event in self.peer.events if event["kind"] == "audio" and event["sender"] == session and not event["end"]])
        assert before == after, "muted audio was transmitted"
        self.press("Deafen")
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == session
                       and event["user"]["self_deafened"])
        eventually(lambda: not self.pulse_json("source-outputs") and not self.pulse_json("sink-inputs"),
                   "deafen retained audio streams")
        time.sleep(1.1)
        self.press("Undeafen")
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == session
                       and not event["user"]["self_deafened"])
        eventually(lambda: self.pulse_json("sink-inputs"), "undeafen did not restore playback")
        assert not self.pulse_json("source-outputs"), "undeafen lost mute state"
        print("Real PulseAudio playback/capture, reference-codec audio, mute privacy and deafen passed.", flush=True)
        inode = self.socket.stat().st_ino
        # Hold the old inode so tmpfs cannot recycle its number for the new socket.
        old_socket = os.open(self.socket, os.O_PATH | os.O_CLOEXEC)
        terminate(self.pulse)
        self.pulse = None
        marker = "CHATWHILEAUDIOISDOWN"
        self.peer.send(kind="text", channel=2, body=marker)
        eventually(lambda: self.has_message(marker), "audio failure interrupted chat/history")
        self.start_audio()
        assert self.socket.stat().st_ino != inode, "fixture did not replace the audio socket"
        os.close(old_socket)
        eventually(lambda: self.pulse_json("sink-inputs"), "audio did not recover after replacing service socket", timeout=30)
        assert self.peer.users[session]["channel"] == 2
        assert not self.pulse_json("source-outputs"), "audio recovery lost mute state"
        self.press("Unmute")
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == session
                       and not event["user"]["self_muted"])
        eventually(lambda: self.pulse_json("source-outputs"), "capture did not recover")
        self.captured_audio(session)
        print("PulseAudio restart recovered capture without reconnecting the Mumble session; chat stayed live.", flush=True)
        identity = (self.profile / "data/identity/identity.json").read_bytes()
        terminate(self.app)
        self.app = None
        self.peer.wait(lambda event: event["kind"] == "user_removed" and event["session"] == session)
        self.start_app()
        self.wait_for("label", "Disconnected")
        self.browse_development()
        self.wait_for("list item", marker, partial=True)
        assert not any(user["name"] == "NuschelDesktop" for user in self.peer.users.values())
        self.capture("offline-history")
        self.press("Connect")
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["name"] == "NuschelDesktop")
        self.connected()
        assert (self.profile / "data/identity/identity.json").read_bytes() == identity
        print("Saved server/certificate/client identity and offline nested-channel history survived restart.", flush=True)

    def browse_development(self):
        """Browse Lobby, expand it if it is not, then browse the nested
        Development channel; browsing moves no voice. (AccessKit's AT-SPI
        adapter does not report the expanded state; a shown child says it.)"""
        self.click_node(self.wait_for("tree item", "Lobby"))
        self.wait_for("list", "Messages in #Lobby")
        if self.absent("tree item", "Development"):
            self.press("Toggle Lobby")
        self.click_node(self.wait_for("tree item", "Development"))
        self.wait_for("list", "Messages in #Development")

    def close(self):
        terminate(self.app)
        terminate(self.pulse)
        if self.peer:
            self.peer.close()
        for helper in reversed(self.helpers):
            terminate(helper)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True, help="at-spi2-core package (bus launcher, registry)")
    parser.add_argument("--dbus-daemon", type=Path, default=shutil.which("dbus-daemon"),
                        help="dbus-daemon for the private session bus")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschel-desktop-mumble-") as temporary:
        test = DesktopTest(args, Path(temporary), args.output)
        try:
            test.run()
        except BaseException:
            if test.app is not None and test.app.poll() is None and test.window:
                test.capture("failure")
            raise
        finally:
            test.close()
