#!/usr/bin/env python3
"""Voice on several servers: listen anywhere, speak in one place (docs/clients.md).

The real desktop in a private headless GNOME Shell, operated over AT-SPI as a
screen reader would, against two loopback stock Mumble servers ("Alpha" and
"Beta"), with a private PulseAudio: a 440 Hz tone is the microphone, a null
sink the speakers. Push-to-talk is F9 and the mute shortcut F10. Checks:

- starting connects both muted and deafened, and no audio stream is open;
- one media worker serves both servers: one playback stream however many you
  listen to, and you hear both;
- your voice reaches only the talk server; the other sees you muted;
- push-to-talk transmits only while held, and only to the talk server;
- unmuting on the other server asks "Speak on two servers?" in a dialog with
  named buttons; Cancel changes nothing, "Speak on both" sends to both;
- the mute shortcut acts on the talk servers, not on the server in front;
- the media worker restarts after a crash and keeps that state;
- screenshots of the rail and the voice card in each state (voice-*.png).

    python3 tests/voice_servers.py <binaries> --murmur <mumble-server> \\
        --at-spi <at-spi2-core> --pulse-bin <pulseaudio>/bin --opus-library <libopus.so> \\
        --output <directory> [--gnome-shell <gnome-shell>]
"""
import argparse
import json
import os
from pathlib import Path
import signal
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import snapshot, start_registry  # noqa: E402
from desktop_mumble import media_workers  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client, ReferenceOpus, certificate_pin, rms  # noqa: E402

WIDTH, HEIGHT = 1280, 800


class VoiceTest:
    def __init__(self, args, root):
        self.args, self.root = args, root
        self.processes = []
        self.peers = {}
        self.murmurs = []
        self.shell = None
        self.reference = ReferenceOpus(args.opus_library)

    def spawn(self, command, env, log):
        process = subprocess.Popen(command, env=env, stdout=(self.args.output / log).open("w"),
                                   stderr=subprocess.STDOUT, start_new_session=True)
        self.processes.append(process)
        return process

    # Audio: a private PulseAudio in the shell's runtime directory.

    def start_pulse(self):
        socket_dir = self.shell.runtime / "pulse"
        socket_dir.mkdir(mode=0o700, exist_ok=True)
        self.socket = socket_dir / "native"
        script = self.shell.runtime / "default.pa"
        script.write_text(
            f"load-module module-native-protocol-unix socket={self.socket} auth-anonymous=1\n"
            "load-module module-null-sink sink_name=speakers rate=48000 sink_properties=device.description=Speakers\n"
            "load-module module-null-sink sink_name=microphone rate=48000 sink_properties=device.description=Mic\n"
            "load-module module-sine sink=microphone frequency=440\n"
            "set-default-sink speakers\nset-default-source microphone.monitor\n")
        self.spawn([str(self.args.pulse_bin / "pulseaudio"), "--daemonize=no", "--use-pid-file=no",
                    "--exit-idle-time=-1", "-n", "--file=" + str(script)],
                   self.shell.environment() | {"PULSE_RUNTIME_PATH": str(socket_dir),
                                               "PULSE_STATE_PATH": str(self.shell.runtime / "pulse-state")},
                   "pulse.log")
        eventually(lambda: self.socket.exists(), "private PulseAudio did not start")

    def pactl(self, *arguments):
        result = subprocess.run([str(self.args.pulse_bin / "pactl"), f"--server=unix:{self.socket}",
                                 "--format=json", *arguments], capture_output=True, text=True, check=True, timeout=10)
        return json.loads(result.stdout)

    def playback_streams(self):
        return [stream for stream in self.pactl("list", "sink-inputs")
                if stream["properties"].get("application.name") == "Nuschel"]

    def microphone_open(self):
        """Nuschel records the microphone (not just the speakers, its echo reference)."""
        sources = {source["index"]: source["name"] for source in self.pactl("list", "sources")}
        return any(sources.get(stream["source"]) == "microphone.monitor"
                   for stream in self.pactl("list", "source-outputs"))

    def capturing(self):
        """The microphone is open and the worker ready: it records the
        speakers too (its echo canceller's reference), last."""
        sources = {source["index"]: source["name"] for source in self.pactl("list", "sources")}
        recorded = {sources.get(stream["source"]) for stream in self.pactl("list", "source-outputs")}
        return {"microphone.monitor", "speakers.monitor"} <= recorded

    def wait_capturing(self, message):
        eventually(self.capturing, message, timeout=20)
        time.sleep(0.8)

    def heard(self, peer, label):
        """What Nuschel plays while `peer` talks."""
        recording = self.args.output / f"heard-{label}.pcm"
        with recording.open("wb") as output:
            recorder = subprocess.Popen([
                str(self.args.pulse_bin / "parec"), f"--server=unix:{self.socket}", "--device=speakers.monitor",
                "--raw", "--format=float32le", "--rate=48000", "--channels=1", "--latency-msec=20"],
                stdout=output, start_new_session=True)
            try:
                time.sleep(0.3)
                for index, packet in enumerate(self.reference.encode_tone(60)):
                    peer.send(kind="voice", sequence=self.sequence + index * 2, opus=packet, end=index == 59)
                    time.sleep(0.02)
                self.sequence += 200
                time.sleep(0.5)
            finally:
                os.killpg(recorder.pid, signal.SIGTERM)
                recorder.wait(timeout=5)
        data = recording.read_bytes()
        return rms(struct.unpack("<%df" % (len(data) // 4), data))

    def talk(self, peer, seconds):
        """`peer` talks for `seconds`, in the background."""
        packets = self.reference.encode_tone(int(seconds * 50))
        start = self.sequence
        self.sequence += 2 * len(packets) + 100

        def send():
            for index, packet in enumerate(packets):
                peer.send(kind="voice", sequence=start + index * 2, opus=packet, end=index == len(packets) - 1)
                time.sleep(0.02)
        thread = threading.Thread(target=send, daemon=True)
        thread.start()
        return thread

    # The desktop over AT-SPI.

    def nodes(self):
        return snapshot(self.nuschel)

    def find(self, role, name, prefix=False):
        def found():
            for _, node, node_role, node_name in self.nodes():
                if node_role == role and (node_name.startswith(name) if prefix else node_name == name):
                    return node
            return None
        return eventually(found, f"no {role} named {name!r}", timeout=15)

    def press(self, role, name, prefix=False):
        node = self.find(role, name, prefix)
        assert node.call("Action", "DoAction", "i", "0")[0] is not False
        time.sleep(0.4)

    def select(self, server):
        self.press("push button", f"{server};", prefix=True)

    def named(self, prefix):
        """The name of whatever node's name begins with `prefix`."""
        def found():
            return next((name for _, _, _, name in self.nodes() if name.startswith(prefix)), None)
        return eventually(found, f"nothing named {prefix!r}…", timeout=15)

    def rail_label(self, server):
        return self.find("push button", f"{server};", prefix=True).name

    def shot(self, name):
        time.sleep(0.8)
        window = self.shell.window("Nuschel")
        path = self.shell.screenshot(self.args.output / f"voice-{name}.png",
                                     crop=(window["x"] - 12, window["y"] - 12, window["width"] + 24,
                                           window["height"] + 24))
        print(path, flush=True)

    def hold(self, keysym, pressed):
        self.shell._pointer()
        state = "PRESSED" if pressed else "RELEASED"
        self.shell.eval("global._nuschelKeyboard.notify_keyval(imports.gi.GLib.get_monotonic_time(),"
                        f" imports.gi.Clutter.KEY_{keysym}, imports.gi.Clutter.KeyState.{state}); 'ok'")

    # The servers' view of the desktop.

    def own(self, server):
        peer = self.peers[server]
        return next(user for user in peer.users.values() if user["name"] == "Andi")

    def sees(self, server, muted, deafened=False):
        peer = self.peers[server]
        session = self.own(server)["session"]
        peer_sees = lambda: (peer.users[session]["self_muted"], peer.users[session]["self_deafened"])  # noqa: E731
        eventually(lambda: peer_sees() == (muted, deafened),
                   f"{server} saw muted/deafened {peer_sees()}, expected {(muted, deafened)}", timeout=15)

    def voice_packets(self, server):
        peer = self.peers[server]
        session = self.own(server)["session"]
        return sum(1 for event in list(peer.events)
                   if event["kind"] == "audio" and event["sender"] == session and not event["end"])

    def transmit(self, seconds=1.5, push_to_talk=True):
        """Packets each server received from the desktop while it transmitted."""
        before = {server: self.voice_packets(server) for server in self.peers}
        if push_to_talk:
            self.hold("F9", True)
        time.sleep(seconds)
        if push_to_talk:
            self.hold("F9", False)
        time.sleep(0.6)
        return {server: self.voice_packets(server) - before[server] for server in self.peers}

    def run(self):
        self.shell = GnomeSession(self.args.gnome_shell, WIDTH + 160, HEIGHT + 100,
                                  log=(self.args.output / "gnome-shell.log").open("w"))
        self.shell.start()
        env = self.shell.app_environment()
        env.pop("NO_AT_BRIDGE", None)
        registry = start_registry(self.args.at_spi, env, self.spawn)
        self.start_pulse()
        bookmarks = []
        for name in ("Alpha", "Beta"):
            port = free_port()
            (self.root / name).mkdir()
            murmur = Murmur(self.args.murmur, self.root / name, port)
            murmur.seed()
            if self.args.unread_navigation:
                with sqlite3.connect(murmur.database) as database:
                    database.executemany(
                        "INSERT OR IGNORE INTO channels (server_id, channel_id, parent_id, name, inheritacl)"
                        " VALUES (1, ?, 0, ?, 1)",
                        [(100 + n, f"Unread room {n:02}") for n in range(50)])
            murmur.start()
            self.murmurs.append(murmur)
            pin = certificate_pin(self.args.binaries, "127.0.0.1", port, self.root / f"probe-{name}")
            peer = Client(self.args.binaries, "127.0.0.1", port, pin, f"{name}Peer", self.root / f"peer-{name}")
            peer.send(kind="join", channel=149 if self.args.unread_navigation else 1)
            self.peers[name] = peer
            bookmarks.append(dict(id=str(uuid.uuid4()), name=name, host="127.0.0.1", port=port, username="Andi",
                                  certificate_sha256=pin, auto_connect=True,
                                  last_channel=149 if self.args.unread_navigation else 1))
        if self.args.unread_navigation:
            bookmarks[1:1] = [dict(id=str(uuid.uuid4()), name=f"Offline {n:02}", host="127.0.0.1",
                                   port=64738, username="Andi", auto_connect=False) for n in range(20)]
        self.sequence = 0
        profile = self.root / "profile"
        (profile / "config").mkdir(parents=True)
        (profile / "config/servers.json").write_text(json.dumps(bookmarks))
        (profile / "config/settings.json").write_text(json.dumps({
            "server_icons": False,
            "audio": {"transmit": "push_to_talk"},
            "shortcuts": {"push-to-talk": "f9", "toggle-mute": "f10", "toggle-deafen": "f11"},
            "appearance": {"theme": "dark"}}))
        app_env = env | {"NUSCHEL_AUDIO_BACKEND": "pulse", "RUST_LOG": os.environ.get("NUSCHEL_TEST_LOG", "nuschel=info")}
        app = self.app = self.spawn([str(self.args.binaries / "nuschel"), "--profile-dir", str(profile)], app_env,
                                    "desktop.log")
        eventually(lambda: app.poll() is None and self.shell.window("Nuschel"), "Nuschel window did not appear")
        self.shell.place("Nuschel", 40, 40, WIDTH, HEIGHT)

        def application():
            assert app.poll() is None, (self.args.output / "desktop.log").read_text()[-3000:]
            return next((node for node in registry.children() if node.name.lower() == "nuschel"), None)
        self.nuschel = eventually(application, "Nuschel did not register with the accessibility registry", timeout=45)
        # Keys go to the focused window: click into it once.
        window = self.shell.window("Nuschel")
        self.shell.click(window["x"] + WIDTH // 2, window["y"] + 200)

        # Starting: both servers joined muted and deafened, and nothing plays or records.
        for server in self.peers:
            self.peers[server].wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                                    and e["user"]["channel"] == (149 if self.args.unread_navigation else 1), timeout=40)
            self.sees(server, muted=True, deafened=True)
        time.sleep(1)
        assert not self.playback_streams() and not self.microphone_open(), "a deafened start opened audio"
        self.shot("1-deafened")
        print("Started on both servers muted and deafened, with no audio stream open.", flush=True)
        if self.args.unread_navigation:
            self.check_unread_navigation()
            return

        # Listening on both: one worker, one playback stream; both are heard.
        for server in ("Alpha", "Beta"):
            self.select(server)
            self.press("push button", "Undeafen")
            self.sees(server, muted=True)
        eventually(lambda: len(self.playback_streams()) == 1, "no playback stream while listening", timeout=20)
        time.sleep(1.5)
        streams = self.playback_streams()
        assert len(streams) == 1, f"one worker should play every server: {len(streams)} streams"
        assert streams[0]["properties"].get("media.name") == "Voice chat", streams[0]["properties"]
        assert not self.microphone_open(), "listening opened the microphone"
        workers = media_workers(self.app.pid)
        assert len(workers) == 1, f"media workers: {workers}"
        for server, peer in self.peers.items():
            peer.unmute()
        for server, peer in self.peers.items():
            level = self.heard(peer, server)
            assert level > 0.01, f"{server}'s voice was not heard ({level:.4f})"
        print("Listening to both: one media worker, one playback stream, and both servers are heard.", flush=True)

        # Beta (the last connected) is the talk server: its microphone, push-to-talk only there.
        assert "voice here, muted" in self.rail_label("Beta"), self.rail_label("Beta")
        assert self.rail_label("Alpha").endswith("listening"), self.rail_label("Alpha")
        self.select("Beta")
        self.press("push button", "Unmute")
        self.sees("Beta", muted=False)
        self.sees("Alpha", muted=True)
        self.wait_capturing("unmuting did not open the microphone")
        quiet = self.transmit(push_to_talk=False)
        assert quiet == {"Alpha": 0, "Beta": 0}, f"sent without push-to-talk: {quiet}"
        sent = self.transmit()
        assert sent["Beta"] >= 20 and sent["Alpha"] == 0, f"push-to-talk should reach Beta only: {sent}"
        assert "talking here" in self.rail_label("Beta")
        print(f"Push-to-talk reached the talk server only: {sent}; nothing without it.", flush=True)
        self.hold("F9", True)
        self.shot("2-talking-on-beta")
        self.hold("F9", False)
        # Someone talks on Alpha, where you listen: its badge pulses.
        self.select("Alpha")
        talker = self.talk(self.peers["Alpha"], 6)
        eventually(lambda: "someone is talking" in self.rail_label("Alpha"), "Alpha's talker was not shown",
                   timeout=5)
        self.shot("3-listening-pulse")
        talker.join()
        card = self.named("Talking on Beta")
        assert card == "Talking on Beta #Lobby · listening to Alpha #Lobby", card

        # The mute shortcut acts on the talk server, not on Alpha in front.
        self.shell.key("F10")
        self.sees("Beta", muted=True)
        eventually(lambda: not self.microphone_open(), "muting closed no microphone", timeout=20)
        self.sees("Alpha", muted=True)
        self.shell.key("F10")
        self.sees("Beta", muted=False)
        self.sees("Alpha", muted=True)
        self.wait_capturing("unmuting did not open the microphone")
        print("The mute shortcut toggled the talk server, not the server in front.", flush=True)

        # Unmuting on Alpha asks first; Cancel changes nothing.
        self.press("push button", "Unmute")
        dialog = self.find("dialog", "Speak on two servers?")
        self.shot("4-speak-on-two")
        names = {name for _, _, role, name in snapshot(dialog) if role == "push button"}
        assert {"Cancel", "Only on Alpha", "Speak on both"} <= names, names
        self.press("push button", "Cancel")
        eventually(lambda: not any(role == "dialog" for _, _, role, _ in self.nodes()), "the dialog stayed open")
        time.sleep(0.5)
        self.sees("Alpha", muted=True)
        sent = self.transmit()
        assert sent["Alpha"] == 0 and sent["Beta"] >= 20, f"Cancel changed where the voice goes: {sent}"
        # "Only on Alpha": the microphone moves there, and Beta listens.
        self.press("push button", "Unmute")
        self.press("push button", "Only on Alpha")
        self.sees("Alpha", muted=False)
        self.sees("Beta", muted=True)
        self.wait_capturing("moving voice to Alpha did not reopen the microphone")
        sent = self.transmit()
        assert sent["Alpha"] >= 20 and sent["Beta"] == 0, f"'Only on Alpha' should move the voice: {sent}"
        # Beta: "Speak on both", and the one microphone reaches both.
        self.select("Beta")
        self.press("push button", "Unmute")
        self.press("push button", "Speak on both")
        self.sees("Alpha", muted=False)
        self.sees("Beta", muted=False)
        self.wait_capturing("adding the second talk server did not reopen the microphone")
        sent = self.transmit()
        assert sent["Alpha"] >= 20 and sent["Beta"] >= 20, f"both talk servers should receive: {sent}"
        workers = media_workers(self.app.pid)
        assert len(workers) == 1, f"media workers: {workers}"
        self.shot("5-talking-on-both")
        print(f"Unmuting the other server asked first: Cancel kept it muted, 'Only on' moved the voice, "
              f"'Speak on both' reached both: {sent}.", flush=True)

        # Repeated deafen presses must not leave a deafened talk server
        # transmitting, even while older self-state echoes are arriving.
        for _ in range(7):
            self.shell.key("F11")
        for server in self.peers:
            self.sees(server, muted=True, deafened=True)
        eventually(lambda: not self.microphone_open(), "rapid Deafen left capture open", timeout=20)
        assert self.transmit() == {"Alpha": 0, "Beta": 0}, "deafened servers received captured voice"
        self.press("push button", "Undeafen")
        self.press("push button", "Unmute")
        self.wait_capturing("Beta did not resume after rapid Deafen")
        sent = self.transmit()
        assert sent["Alpha"] == 0 and sent["Beta"] >= 20, sent
        self.sees("Alpha", muted=True, deafened=True)
        self.select("Alpha")
        self.press("push button", "Undeafen")
        self.press("push button", "Unmute")
        self.select("Beta")
        print("Rapid Deafen closed capture and stopped both routes; Beta resumed without undeafening Alpha.", flush=True)

        # The shortcut mutes both talk servers, and unmutes both; a deafened
        # one is left alone. Once asked, Beta unmutes without a question.
        self.shell.key("F10")
        self.sees("Alpha", muted=True)
        self.sees("Beta", muted=True)
        eventually(lambda: not self.microphone_open(), "muting left the microphone open", timeout=20)
        self.press("push button", "Unmute")
        self.sees("Beta", muted=False)
        self.sees("Alpha", muted=True)
        self.select("Alpha")
        self.press("push button", "Deafen")
        self.sees("Alpha", muted=True, deafened=True)
        self.shell.key("F10")
        self.sees("Beta", muted=True)
        self.shell.key("F10")
        self.sees("Beta", muted=False)
        self.sees("Alpha", muted=True, deafened=True)
        self.press("push button", "Undeafen")
        self.sees("Alpha", muted=True)
        print("The shortcut toggled both talk servers together and left a deafened one alone.", flush=True)

        # The worker crashes: it comes back with the same state.
        workers = media_workers(self.app.pid)
        assert workers, "no media worker to kill"
        for pid in workers:
            os.kill(pid, signal.SIGKILL)
        time.sleep(1)
        eventually(lambda: len(self.playback_streams()) == 1 and self.capturing(),
                   "the media worker did not come back", timeout=30)
        time.sleep(0.8)
        sent = self.transmit()
        assert sent["Beta"] >= 20 and sent["Alpha"] == 0, f"after the restart: {sent}"
        for server, peer in self.peers.items():
            level = self.heard(peer, f"{server}-restarted")
            assert level > 0.01, f"{server}'s voice was not heard after the restart ({level:.4f})"
        self.shot("6-recovered")
        print("A crashed media worker came back: talk server, listening and push-to-talk as before.", flush=True)

    def check_unread_navigation(self):
        """Off-screen unread channels and servers, opened without joining voice."""
        selected = 1 << 23  # ATSPI_STATE_SELECTED

        def is_selected(name):
            return bool(self.find("tree item", name).call("Accessible", "GetState")[0][0] & selected)

        self.select("Alpha")
        self.press("tree item", "Lobby")
        self.peers["Alpha"].send(kind="text", channel=149, body="Andi: bottom channel notification")
        below = self.find("push button", "Unread channels below:", prefix=True)
        assert "including mentions" in below.name, below.name
        # Move the voice elsewhere before using the unread jump. The jump must
        # browse the old channel's text, not move voice back there.
        self.press("push button", "Join voice")
        eventually(lambda: self.own("Alpha")["channel"] == 1, "voice did not move to Lobby")
        self.shot("7-unread-channel-below")
        self.press("push button", "Unread channels below:", prefix=True)
        eventually(lambda: is_selected("Unread room 49"), "unread jump did not select its conversation")
        assert self.own("Alpha")["channel"] == 1, "unread channel jump joined voice"

        self.peers["Beta"].send(kind="text", channel=149, body="Andi: bottom server notification")
        below = self.find("push button", "Unread servers below:", prefix=True)
        assert "including mentions" in below.name, below.name
        self.shot("8-unread-server-below")
        self.press("push button", "Unread servers below:", prefix=True)
        eventually(lambda: is_selected("Unread room 49"), "server jump did not select its unread conversation")
        assert self.own("Beta")["channel"] == 149
        self.peers["Alpha"].send(kind="text", channel=1, body="Andi: top server notification")
        self.find("push button", "Unread servers above:", prefix=True)
        self.shot("9-unread-server-above")
        self.press("push button", "Unread servers above:", prefix=True)
        eventually(lambda: is_selected("Lobby"), "server jump did not open the unread Lobby")
        assert self.own("Alpha")["channel"] == 1

        window = self.shell.window("Nuschel")
        self.shell.wheel(window["x"] + 200, window["y"] + 350, 100)
        time.sleep(0.5)
        self.press("tree item", "Unread room 49")
        eventually(lambda: is_selected("Unread room 49"), "could not browse the bottom room")
        self.peers["Alpha"].send(kind="text", channel=1, body="Andi: top channel notification")
        self.find("push button", "Unread channels above:", prefix=True)
        self.shot("10-unread-channel-above")
        self.press("push button", "Unread channels above:", prefix=True)
        eventually(lambda: is_selected("Lobby"), "upward jump did not select Lobby")
        assert self.own("Alpha")["channel"] == 1
        assert not self.playback_streams() and not self.microphone_open()
        print("Unread indicators in both directions on both lists opened the right text without joining voice.", flush=True)

    def close(self):
        for peer in self.peers.values():
            try:
                peer.close()
            except (AssertionError, OSError):
                pass
        for murmur in self.murmurs:
            murmur.stop()
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
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--unread-navigation", action="store_true",
                        help="test off-screen unread navigation, with twenty offline rail entries and fifty rooms")
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschel-voice-servers-") as temporary:
        test = VoiceTest(args, Path(temporary))
        try:
            test.run()
        except BaseException:
            if test.shell and getattr(test, "app", None) is not None and test.app.poll() is None:
                try:
                    test.shot("failure")
                    (args.output / "failure-accessibility.txt").write_text("\n".join(
                        f"{depth} {role} {name!r} {node.call('Accessible', 'GetState')}"
                        for depth, node, role, name in test.nodes()))
                except BaseException:
                    pass
            raise
        finally:
            test.close()


if __name__ == "__main__":
    main()
