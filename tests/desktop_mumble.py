#!/usr/bin/env python3
"""Real GPUI, stock Mumble, confined workers and private synthetic PulseAudio I/O.

Run inside xvfb-run. This never opens the host microphone or normal audio service.
"""
import argparse
import json
import math
import os
from pathlib import Path
import re
import signal
import sqlite3
import struct
import subprocess
import tempfile
import time

from mumble_client import Client, ReferenceOpus, certificate_pin, rms, tone


def eventually(predicate, message, timeout=25):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            result = predicate()
            if result:
                return result
        except (OSError, sqlite3.Error, ValueError, subprocess.CalledProcessError) as error:
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
        self.window = None
        self.reference = ReferenceOpus(args.opus_library)

    def start_audio(self):
        script = self.runtime / "default.pa"
        script.write_text(f"load-module module-native-protocol-unix socket={self.socket} auth-anonymous=1\n"
                          "load-module module-null-sink sink_name=nuschel_fixture rate=48000 channels=2\n"
                          "set-default-sink nuschel_fixture\nset-default-source nuschel_fixture.monitor\n")
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

    def start_app(self):
        self.app = subprocess.Popen([str(self.args.binaries / "nuschel"), "--profile-dir", str(self.profile)],
                                    env=self.env, stdout=(self.output / "desktop.log").open("a"),
                                    stderr=subprocess.STDOUT, start_new_session=True)
        def window():
            if self.app.poll() is not None:
                raise AssertionError((self.output / "desktop.log").read_text()[-4000:])
            result = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "^Nuschel$"],
                                    env=self.env, capture_output=True, text=True)
            return result.stdout.splitlines()[0] if result.returncode == 0 else None
        self.window = eventually(window, "Nuschel did not open a window")
        self.xdo("windowsize", self.window, "1120", "850")
        self.xdo("windowmove", self.window, "0", "0")

    def xdo(self, *arguments):
        subprocess.run(["xdotool", *map(str, arguments)], env=self.env, check=True, timeout=10)

    def click(self, x, y):
        self.xdo("mousemove", "--window", self.window, x, y, "click", 1)

    def fill(self, x, y, text):
        self.click(x, y)
        self.xdo("key", "ctrl+a")
        if text:
            self.xdo("type", "--clearmodifiers", "--delay", 1, "--", text)

    def type_message(self, body):
        self.click(460, 690)
        self.xdo("key", "ctrl+a")
        for index, line in enumerate(body.split("\n")):
            if index:
                self.xdo("key", "Return")
            if line:
                self.xdo("type", "--clearmodifiers", "--delay", 1, "--", line)

    def capture(self, name):
        path = self.output / (name + ".png")
        subprocess.run(["magick", "import", "-window", self.window, str(path)],
                       env=self.env, check=True, timeout=10)
        return path

    def text(self):
        path = self.capture("current")
        texts = []
        # Tesseract's single-block mode mistakes the rail for the entire page.
        # Auto-layout reads chat; a cropped block reads native modal dialogs.
        for crop, mode in [(None, "3"), ("500x685+310+82", "6")]:
            source = path
            if crop:
                source = self.output / "ocr.png"
                subprocess.run(["magick", str(path), "-crop", crop, "-alpha", "off",
                                "-colorspace", "gray", "-negate", "-normalize", "-threshold", "60%",
                                "-resize", "200%", str(source)], env=self.env, check=True, timeout=10)
            result = subprocess.run(["tesseract", str(source), "stdout", "--psm", mode],
                                    env=self.env, capture_output=True, text=True, check=True, timeout=15)
            texts.append(result.stdout)
        return "\n".join(texts)

    def wait_text(self, value):
        return eventually(lambda: value.casefold() in self.text().casefold(), f"UI did not show {value!r}")

    def rows(self):
        with sqlite3.connect(self.profile / "data/history.sqlite3") as database:
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
        mono = tone(0, int(48000 * seconds))
        data = struct.pack("<%df" % (2 * len(mono)), *[value for sample in mono for value in (sample, sample)])
        subprocess.run([
            str(self.args.pulse_bin / "pacat"), "--server=unix:" + str(self.socket),
            "--playback", "--device=nuschel_fixture", "--raw", "--format=float32le", "--rate=48000",
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

    def run(self):
        pin = certificate_pin(self.args.binaries, self.args.host, self.args.port, self.root / "certificate-probe")
        self.peer = Client(self.args.binaries, self.args.host, self.args.port, pin, "DesktopPeer", self.root / "peer")
        self.peer.send(kind="join", channel=2)
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == self.peer.session
                       and event["user"]["channel"] == 2)
        self.start_audio()
        self.start_app()
        self.wait_text("Add a server")
        self.click(36, 76)
        self.wait_text("Hostname or IP address")
        self.fill(500, 212, "Fixture Mumble")
        self.fill(500, 311, self.args.host)
        self.fill(500, 410, str(self.args.port))
        self.fill(500, 509, "NuschelDesktop")
        self.fill(500, 608, "not-saved-fixture-password")
        self.click(710, 725)
        self.wait_text("Confirm server certificate")
        assert not any(user["name"] == "NuschelDesktop" for user in self.peer.users.values())
        self.capture("certificate")
        self.click(500, 253)
        gui = self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["name"] == "NuschelDesktop")["user"]
        session = gui["session"]
        self.wait_text("TCP voice")
        eventually(lambda: self.pulse_json("sink-inputs"), "desktop playback did not start")
        assert not self.pulse_json("source-outputs"), "muted startup opened a microphone"
        bookmark = json.loads((self.profile / "config/servers.json").read_text())[0]
        assert bookmark["certificate_sha256"] == pin
        assert "not-saved-fixture-password" not in json.dumps(bookmark) and "password" not in bookmark
        self.click(160, 212)  # Browse the nested Development channel.
        time.sleep(0.5)
        assert self.peer.users[session]["channel"] == 0, "browsing text moved voice"
        self.click(680, 35)  # Explicit Join voice.
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == session
                       and event["user"]["channel"] == 2)
        body = "DESKTOPHISTORYMARKER\n\n```rust\nlet connected = true;\n```"
        self.type_message(body)
        self.xdo("key", "ctrl+Return")
        eventually(lambda: self.has_message(body), "desktop composer lost newlines or failed to save/send")
        self.peer.wait(lambda event: event["kind"] == "text" and event["author"] == "NuschelDesktop")
        reply = "PEERHISTORYMARKER\n\n```rust\nfn legacy_server() {}\n```"
        self.peer.send(kind="text", channel=2, body=reply)
        eventually(lambda: self.has_message(reply), "incoming rich text was not normalized and saved")
        self.wait_text("PEERHISTORYMARKER")
        self.capture("chat")
        print("Desktop add/certificate flow, channel tree, independent browsing/join, multiline chat and history passed.", flush=True)
        self.recorded_playback(self.reference.encode_tone(50))
        self.click(790, 35)
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == session
                       and not event["user"]["self_muted"])
        eventually(lambda: self.pulse_json("source-outputs"), "unmute did not open capture")
        self.captured_audio(session)
        self.click(790, 35)
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == session
                       and event["user"]["self_muted"])
        eventually(lambda: not self.pulse_json("source-outputs"), "mute left microphone capture open")
        time.sleep(0.3)
        before = len([event for event in self.peer.events if event["kind"] == "audio" and event["sender"] == session and not event["end"]])
        self.inject_tone(0.5)
        time.sleep(0.2)
        after = len([event for event in self.peer.events if event["kind"] == "audio" and event["sender"] == session and not event["end"]])
        assert before == after, "muted audio was transmitted"
        self.click(865, 35)
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == session
                       and event["user"]["self_deafened"])
        eventually(lambda: not self.pulse_json("source-outputs") and not self.pulse_json("sink-inputs"),
                   "deafen retained audio streams")
        time.sleep(1.1)
        self.click(865, 35)
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
        self.click(790, 35)
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == session
                       and not event["user"]["self_muted"])
        eventually(lambda: self.pulse_json("source-outputs"), "capture did not recover")
        self.captured_audio(session)
        print("PulseAudio restart recovered capture without reconnecting the Mumble session; chat stayed live.", flush=True)
        identity = (self.profile / "config/identity.json").read_bytes()
        terminate(self.app)
        self.app = None
        self.peer.wait(lambda event: event["kind"] == "user_removed" and event["session"] == session)
        self.start_app()
        self.wait_text("Disconnected")
        self.click(160, 212)
        self.wait_text(marker)
        assert not any(user["name"] == "NuschelDesktop" for user in self.peer.users.values())
        self.capture("offline-history")
        self.click(970, 35)
        self.wait_text("Save and connect")
        self.click(710, 725)
        self.peer.wait(lambda event: event["kind"] == "user" and event["user"]["name"] == "NuschelDesktop")
        self.wait_text("TCP voice")
        assert (self.profile / "config/identity.json").read_bytes() == identity
        print("Saved server/certificate/client identity and offline nested-channel history survived restart.", flush=True)

    def close(self):
        terminate(self.app)
        terminate(self.pulse)
        if self.peer:
            self.peer.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
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
