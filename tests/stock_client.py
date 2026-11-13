#!/usr/bin/env python3
"""Stock Mumble GUI client and Nuschl in one channel (run inside xvfb-run).

The stock client logs in with a certificate exported by Nuschl (PKCS#12
interoperability), then both exchange voice through the server: the stock
client transmits a tone from a private PulseAudio microphone that Nuschl must
decode, and Nuschl's tone must reach the stock client's speakers. Then chat
both ways: a message typed into the stock window (xdotool; Xvfb has no window
manager) reaches Nuschl, and Nuschl's message shows in the stock client's log
(copied out through the X clipboard with xclip).

Waits that depend on how fast the stock GUI is are generous: under heavy host
load it has opened its microphone more than 40 s after joining.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import queue
import shutil
import signal
import sqlite3
import ssl
import struct
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from local_murmur import eventually  # noqa: E402
from mumble_client import Client, ReferenceOpus, certificate_pin, rms, tone  # noqa: E402


def drain(client):
    """The events queued for a Nuschl client since it last waited, without blocking."""
    events = []
    while True:
        try:
            event = client.queue.get_nowait()
        except queue.Empty:
            return events
        if event is None:
            raise AssertionError(f"the Nuschl client exited: {list(client.events)[-8:]}")
        events.append(event)


def xdotool(*arguments, timeout=30):
    return subprocess.run(["xdotool", *arguments], check=True, capture_output=True, text=True,
                          timeout=timeout).stdout


class StockWindow:
    """The stock client's main window in this Xvfb, driven like a user would."""

    def __init__(self):
        found = xdotool("search", "--sync", "--onlyvisible", "--name", "^Mumble", timeout=120).split()
        sizes = {}
        for window in found:
            geometry = dict(line.split("=", 1) for line in
                            xdotool("getwindowgeometry", "--shell", window).splitlines() if "=" in line)
            sizes[window] = (int(geometry["WIDTH"]), int(geometry["HEIGHT"]))
        assert sizes, "no stock Mumble window"
        # The main window is the largest one (menus and tooltips are small).
        self.window = max(sizes, key=lambda window: sizes[window][0] * sizes[window][1])
        self.width, self.height = sizes[self.window]

    def focus_chat_bar(self):
        # The classic layout: the log on the left with the chat bar along its
        # bottom, the channel tree on the right (which has the focus at first).
        xdotool("windowfocus", "--sync", self.window)
        xdotool("mousemove", "--window", self.window, str(self.width // 6), str(self.height - 12), "click", "1")
        time.sleep(0.3)

    def type_message(self, text):
        self.focus_chat_bar()
        # Replaces whatever an earlier attempt may have left in the chat bar.
        xdotool("key", "ctrl+a")
        xdotool("type", "--delay", "20", text)
        xdotool("key", "Return")

    def log_text(self):
        """The whole log: F6 moves from the chat bar to the channel tree and on to
        the log (MainWindow::focusNextMainWidget), which is then copied."""
        self.focus_chat_bar()
        xdotool("key", "F6", "F6", "ctrl+a", "ctrl+c")
        time.sleep(0.3)
        copied = subprocess.run(["xclip", "-o", "-selection", "clipboard"], capture_output=True, text=True,
                                timeout=10)
        return copied.stdout if copied.returncode == 0 else ""


def screenshot(output):
    """Keep the screen for a failed run (the VM test copies the output directory)."""
    if shutil.which("xwd"):
        subprocess.run(["xwd", "-root", "-silent", "-out", str(output / "screen.xwd")], timeout=20, check=False)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--mumble", type=Path, required=True)
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=64738)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    reference = ReferenceOpus(args.opus_library)
    with tempfile.TemporaryDirectory(prefix="nuschl-stock-", dir=os.environ.get("NUSCHL_SHORT_TMP")) as temporary:
        root = Path(temporary)
        home, runtime, certificates = root / "home", root / "run", root / "certificates"
        for directory in (home / ".config/Mumble/Mumble", runtime / "pulse", certificates):
            directory.mkdir(parents=True)
        runtime.chmod(0o700)
        socket = runtime / "pulse/native"
        script = root / "default.pa"
        script.write_text(f"load-module module-native-protocol-unix socket={socket} auth-anonymous=1\n"
                          "load-module module-null-sink sink_name=speakers rate=48000\n"
                          "load-module module-null-sink sink_name=stockmic rate=48000\n"
                          "set-default-sink speakers\nset-default-source stockmic.monitor\n")
        clean = {key: value for key, value in os.environ.items()
                 if key not in ("DBUS_SESSION_BUS_ADDRESS", "WAYLAND_DISPLAY", "PULSE_SERVER")}
        env = clean | {"HOME": str(home), "XDG_RUNTIME_DIR": str(runtime), "XDG_CONFIG_HOME": str(home / ".config"),
                       "XDG_DATA_HOME": str(home / ".local/share"), "PULSE_SERVER": f"unix:{socket}",
                       "DBUS_SESSION_BUS_ADDRESS": f"unix:path={runtime}/no-bus", "QT_QPA_PLATFORM": "xcb"}
        pulse = subprocess.Popen([str(args.pulse_bin / "pulseaudio"), "--daemonize=no", "--use-pid-file=no",
                                  "--exit-idle-time=-1", "-n", f"--file={script}"],
                                 env=env | {"PULSE_RUNTIME_PATH": str(runtime / "pulse")},
                                 stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        mumble = peer = None
        try:
            eventually(socket.exists, "private PulseAudio did not start", timeout=40)
            # A certificate exported by Nuschl becomes the stock client's identity.
            exported = subprocess.run([str(args.binaries / "nuschl-cli"), "--host", "unused", "--username", "unused",
                                       "--profile-dir", str(root / "stock-identity"), "--export-certificate",
                                       str(certificates / "MumbleAutomaticCertificateBackup.p12")],
                                      capture_output=True, text=True, check=True, timeout=120)
            identity_hash = json.loads(exported.stdout.splitlines()[-1])["hash"]
            # Trust the fixture's self-signed server certificate the way Mumble remembers it.
            der = ssl.PEM_cert_to_DER_cert(ssl.get_server_certificate((args.host, args.port)))
            database = root / "mumble.sqlite"
            with sqlite3.connect(database) as db:
                db.execute("CREATE TABLE IF NOT EXISTS `cert` (`id` INTEGER PRIMARY KEY AUTOINCREMENT, "
                           "`hostname` TEXT, `port` INTEGER, `digest` TEXT)")
                db.execute("INSERT INTO cert (hostname, port, digest) VALUES (?, ?, ?)",
                           (args.host, args.port, hashlib.sha1(der).hexdigest()))
            (home / ".config/Mumble/Mumble/mumble_settings.json").write_text(json.dumps({
                "settings_version": 1,
                "misc": {"audio_wizard_has_been_shown": True, "viewed_server_ping_consent_message": True,
                         "database_location": str(database)},
                "update": {"check_for_updates": False, "check_for_plugin_updates": False, "auto_update_plugins": False},
                "ui": {"send_usage_statistics": False},
                "audio": {"transmit_mode": "Continuous", "input_system": "PulseAudio", "output_system": "PulseAudio"},
                "tts": {"enable_tts": False},
            }))
            pin = certificate_pin(args.binaries, args.host, args.port, root / "probe")
            peer = Client(args.binaries, args.host, args.port, pin, "NuschlPeer", root / "peer")
            peer.send(kind="join", channel=1)
            mumble = subprocess.Popen([str(args.mumble), "--default-certificate-dir", str(certificates),
                                       f"mumble://StockUser@{args.host}:{args.port}/Lobby"],
                                      env=env, stdout=(args.output / "mumble.log").open("w"), stderr=subprocess.STDOUT,
                                      start_new_session=True)
            stock = peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "StockUser"
                              and e["user"]["channel"] == 1, timeout=180)["user"]
            assert stock["certificate_hash"] == identity_hash, "stock Mumble did not use the Nuschl-exported certificate"
            print("Stock Mumble logged in with Nuschl's exported PKCS#12 identity.", flush=True)

            # Stock client → Nuschl: Mumble transmits a tone from its microphone.
            peer.unmute()
            mono = tone(0, 48000)
            second = struct.pack("<%df" % (2 * len(mono)), *[v for s in mono for v in (s, s)])

            def play_second():
                subprocess.run([str(args.pulse_bin / "pacat"), f"--server=unix:{socket}", "--playback",
                                "--device=stockmic", "--raw", "--format=float32le", "--rate=48000", "--channels=2"],
                               input=second, check=True, timeout=60, env=env)

            def stock_audio(events):
                return [e["opus"] for e in events
                        if e["kind"] == "audio" and e["sender"] == stock["session"] and not e["end"]]
            # Mumble opens its microphone some time after joining: seconds on a
            # slow VM, more than 40 s under heavy host load. Play the tone a second
            # at a time until the first packet arrives (up to three minutes), and
            # only then count, so a late start does not eat the counting time.
            drain(peer)
            started = time.monotonic()
            packets, first = [], None
            while len(packets) <= 40:
                play_second()
                packets += stock_audio(drain(peer))
                now = time.monotonic()
                if packets and first is None:
                    first = now
                    print(f"First voice packet from the stock client after {first - started:.1f} s.", flush=True)
                if (first is None and now - started > 180) or (first is not None and now - first > 60):
                    break
            time.sleep(0.5)
            packets += stock_audio(drain(peer))
            assert first is not None, "no voice packet from the stock client within 180 s"
            assert len(packets) > 40, f"too few packets from the stock client: {len(packets)}"
            assert rms(reference.decode(packets)) > 0.01, "stock client's audio did not decode"
            print(f"Stock Mumble → Nuschl voice: {len(packets)} Opus packets decoded.", flush=True)

            # Nuschl → stock client: record what Mumble plays; a busy client may
            # take a while to play the first spurt, so it is sent again until heard.
            recording = root / "stock-playback.pcm"
            played = False
            with recording.open("wb") as sink:
                recorder = subprocess.Popen([str(args.pulse_bin / "parec"), f"--server=unix:{socket}",
                                             "--device=speakers.monitor", "--raw", "--format=float32le",
                                             "--rate=48000", "--channels=1"], stdout=sink, env=env, start_new_session=True)
                try:
                    eventually(lambda: recording.stat().st_size > 48000, "recorder did not start", timeout=60)
                    deadline = time.monotonic() + 90
                    sequence = 0
                    while not played and time.monotonic() < deadline:
                        start = recording.stat().st_size // 4 * 4
                        for index, packet in enumerate(reference.encode_tone(100)):
                            peer.send(kind="voice", sequence=sequence, opus=packet, end=index == 99)
                            sequence += 2
                            time.sleep(0.02)
                        time.sleep(1.0)
                        raw = recording.read_bytes()[start:]
                        raw = raw[:len(raw) // 4 * 4]
                        played = bool(raw) and rms(struct.unpack("<%df" % (len(raw) // 4), raw)) > 0.005
                finally:
                    os.killpg(recorder.pid, signal.SIGTERM)
                    recorder.wait(timeout=10)
            assert played, "stock client did not play Nuschl's voice"
            print("Nuschl → stock Mumble voice played on the stock client's speakers.", flush=True)

            # Chat, typed into the stock window: Nuschl receives it from StockUser.
            window = StockWindow()
            typed, attempt = None, 0
            deadline = time.monotonic() + 120
            while typed is None:
                attempt += 1
                window.type_message(f"Typed in stock Mumble {attempt}")
                try:
                    typed = peer.wait(lambda e: e["kind"] == "text" and e["author"] == "StockUser"
                                      and e["body"].startswith("Typed in stock Mumble"), timeout=20)
                except AssertionError:
                    if peer.process.poll() is not None or time.monotonic() > deadline:
                        raise AssertionError("the message typed into stock Mumble did not reach Nuschl") from None
            assert typed["target"] == {"kind": "channel", "id": 1}, typed
            print(f"Stock Mumble → Nuschl chat: {typed['body']!r} (typed {attempt} time(s)).", flush=True)
            # And Nuschl's message shows in the stock client's log.
            body = f"Hello stock Mumble from Nuschl {os.getpid()}"
            peer.send(kind="text", channel=1, body=body)
            eventually(lambda: f"NuschlPeer: {body}" in window.log_text(),
                       "Nuschl's message is not in the stock client's log", timeout=60)
            print("Nuschl → stock Mumble chat: shown in the stock client's log.", flush=True)
        except BaseException:
            if mumble:
                screenshot(args.output)
            raise
        finally:
            for process in (mumble, pulse):
                if process and process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=8)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
            if peer:
                peer.close()


if __name__ == "__main__":
    main()
