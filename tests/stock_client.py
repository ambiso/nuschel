#!/usr/bin/env python3
"""Stock Mumble GUI client and Nuschel in one channel (run inside xvfb-run).

The stock client logs in with a certificate exported by Nuschel (PKCS#12
interoperability), then both exchange voice through the server: the stock
client transmits a tone from a private PulseAudio microphone that Nuschel must
decode, and Nuschel's tone must reach the stock client's speakers.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import sqlite3
import ssl
import struct
import subprocess
import sys
import tempfile
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mumble_client import Client, ReferenceOpus, certificate_pin, rms, tone  # noqa: E402


def eventually(predicate, message, timeout=40):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.25)
    raise AssertionError(message)


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
    with tempfile.TemporaryDirectory(prefix="nuschel-stock-", dir=os.environ.get("NUSCHEL_SHORT_TMP")) as temporary:
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
            eventually(socket.exists, "private PulseAudio did not start")
            # A certificate exported by Nuschel becomes the stock client's identity.
            exported = subprocess.run([str(args.binaries / "nuschel-cli"), "--host", "unused", "--username", "unused",
                                       "--profile-dir", str(root / "stock-identity"), "--export-certificate",
                                       str(certificates / "MumbleAutomaticCertificateBackup.p12")],
                                      capture_output=True, text=True, check=True, timeout=30)
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
            peer = Client(args.binaries, args.host, args.port, pin, "NuschelPeer", root / "peer")
            peer.send(kind="join", channel=1)
            mumble = subprocess.Popen([str(args.mumble), "--default-certificate-dir", str(certificates),
                                       f"mumble://StockUser@{args.host}:{args.port}/Lobby"],
                                      env=env, stdout=(args.output / "mumble.log").open("w"), stderr=subprocess.STDOUT,
                                      start_new_session=True)
            stock = peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "StockUser"
                              and e["user"]["channel"] == 1, timeout=60)["user"]
            assert stock["certificate_hash"] == identity_hash, "stock Mumble did not use the Nuschel-exported certificate"
            print("Stock Mumble logged in with Nuschel's exported PKCS#12 identity.", flush=True)

            # Stock client → Nuschel: Mumble transmits a tone from its microphone.
            peer.unmute()
            mono = tone(0, 48000)
            second = struct.pack("<%df" % (2 * len(mono)), *[v for s in mono for v in (s, s)])
            before = len(peer.events)

            def received():
                return [e["opus"] for e in list(peer.events)[before:]
                        if e["kind"] == "audio" and e["sender"] == stock["session"] and not e["end"]]
            # Mumble opens its microphone some time after joining (seconds on a slow
            # VM): keep playing the tone, a second at a time, until it transmits.
            deadline = time.monotonic() + 40
            while len(received()) <= 40 and time.monotonic() < deadline:
                subprocess.run([str(args.pulse_bin / "pacat"), f"--server=unix:{socket}", "--playback",
                                "--device=stockmic", "--raw", "--format=float32le", "--rate=48000", "--channels=2"],
                               input=second, check=True, timeout=20, env=env)
            time.sleep(0.5)
            packets = received()
            assert len(packets) > 40, f"too few packets from the stock client: {len(packets)}"
            assert rms(reference.decode(packets)) > 0.01, "stock client's audio did not decode"
            print(f"Stock Mumble → Nuschel voice: {len(packets)} Opus packets decoded.", flush=True)

            # Nuschel → stock client: record what Mumble plays.
            recording = root / "stock-playback.pcm"
            with recording.open("wb") as sink:
                recorder = subprocess.Popen([str(args.pulse_bin / "parec"), f"--server=unix:{socket}",
                                             "--device=speakers.monitor", "--raw", "--format=float32le",
                                             "--rate=48000", "--channels=1"], stdout=sink, env=env, start_new_session=True)
                eventually(lambda: recording.stat().st_size > 48000, "recorder did not start", timeout=10)
                for index, packet in enumerate(reference.encode_tone(100)):
                    peer.send(kind="voice", sequence=index * 2, opus=packet, end=index == 99)
                    time.sleep(0.02)
                time.sleep(1.0)
                os.killpg(recorder.pid, signal.SIGTERM)
                recorder.wait(timeout=5)
            raw = recording.read_bytes()
            samples = struct.unpack("<%df" % (len(raw) // 4), raw)
            assert rms(samples) > 0.005, "stock client did not play Nuschel's voice"
            print("Nuschel → stock Mumble voice played on the stock client's speakers.", flush=True)
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
