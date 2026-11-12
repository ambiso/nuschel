#!/usr/bin/env python3
"""Voice bitrate on owned loopback servers: requested versus effective.

The terminal client in plain lines (the real native driver and its sandboxed
media worker) transmits continuously from a private PulseAudio whose
microphone is a null sink fed a 440 Hz tone plus synthetic noise. A
`nuschl-cli` peer in the same channel records every Opus frame it receives.
Nothing opens host audio devices, profiles or recordings, and only loopback
servers this script starts are used:

- stock Murmur at its own default budget (558 kb/s in 1.4/1.5) and lowered
  (72 kb/s, Mumble's former default), each with a private D-Bus session bus
  so `setConf("bandwidth")` changes the limit live (Murmur then sends every
  client a ServerConfig);
- nuschld at its default and, after a restart, lowered (reconnect handling).

For each, the client's `/bitrate` must name the requested and effective
targets and the reason, every frame the peer receives must fit the effective
per-frame allowance, a requested change must keep the same media worker and
capture stream (an encoder-only reconfiguration), a budget too small for
voice must stop capture but not playback, and settings.json must hold
`voice_bitrate` only while it differs from the default.

    python3 tests/bitrate_matrix.py <binaries> --murmur <mumble-server> \\
        --pulse-bin <pulseaudio>/bin --output <directory> [--murmur14 <mumble-server>]
"""
import argparse
import json
import math
import os
from pathlib import Path
import queue
import random
import re
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
from desktop_mumble import media_workers  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from local_nuschld import Nuschld  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402
from tui_loopback import Terminal, environment  # noqa: E402

LOBBY = 1
RESULTS = []
# The keys settings.json had in Nuschl 0.1.0 (ab843f7a): any other makes that
# version set the file aside and reset every setting, relay-only included.
RELEASED_KEYS = {
    "appearance", "audio", "auto_reconnect", "check_updates", "enter_sends", "force_tcp",
    "friend_notifications", "friends", "key_transparency", "local_mutes", "nicknames", "notifications",
    "recovery_key", "relay_only", "send_typing", "server_icons", "server_volumes", "share_web_host",
    "shortcuts", "sidebar_width", "sound_effects", "sound_volume", "user_volumes", "wormhole_relay",
    "wormhole_rendezvous",
}


def allowance(effective):
    """The Opus bytes one 20 ms frame may use (nuschl_core::bitrate)."""
    return min(effective // 400, 960)


def effective(requested, budget):
    """nuschl_core::bitrate::VoiceBitrate::resolve, for expectations."""
    requested = min(max(requested, 64_000), 256_000)
    available = max(budget - 24_000, 0)
    if available >= requested:
        return requested, "requested"
    if available < 6_000:
        return 0, "no_server_budget"
    return available, "server_budget"


def received(peer, session, seconds):
    """Opus frame sizes `peer` receives from `session` during `seconds`. From
    the client's event queue: its history keeps only the last 1000 events."""
    while True:
        try:
            if peer.queue.get_nowait() is None:
                raise AssertionError("the peer exited")
        except queue.Empty:
            break
    sizes, deadline = [], time.monotonic() + seconds
    while (left := deadline - time.monotonic()) > 0:
        try:
            event = peer.queue.get(timeout=left)
        except queue.Empty:
            break
        if event is None:
            raise AssertionError("the peer exited")
        if event["kind"] == "audio" and event["sender"] == session and not event["end"]:
            sizes.append(len(event["opus"]))
    return sizes


def kbps(bits):
    return f"{bits // 1000} kb/s" if bits % 1000 == 0 else f"{bits / 1000:.1f} kb/s"


class Pulse:
    """A private PulseAudio in the client's own runtime directory."""

    def __init__(self, pulse_bin, runtime, log):
        self.bin = Path(pulse_bin)
        directory = Path(runtime) / "pulse"
        directory.mkdir(mode=0o700, parents=True, exist_ok=True)
        self.socket = directory / "native"
        script = Path(runtime) / "default.pa"
        script.write_text(
            f"load-module module-native-protocol-unix socket={self.socket} auth-anonymous=1\n"
            "load-module module-null-sink sink_name=speakers rate=48000 sink_properties=device.description=Speakers\n"
            "load-module module-null-sink sink_name=microphone rate=48000 sink_properties=device.description=Mic\n"
            "load-module module-sine sink=microphone frequency=440\n"
            "set-default-sink speakers\nset-default-source microphone.monitor\n")
        self.process = subprocess.Popen(
            [str(self.bin / "pulseaudio"), "--daemonize=no", "--use-pid-file=no", "--exit-idle-time=-1", "-n",
             "--file=" + str(script)],
            env={"PATH": os.environ.get("PATH", ""), "HOME": str(runtime), "XDG_RUNTIME_DIR": str(runtime),
                 "PULSE_RUNTIME_PATH": str(directory), "PULSE_STATE_PATH": str(Path(runtime) / "pulse-state")},
            stdout=log.open("w"), stderr=subprocess.STDOUT, start_new_session=True)
        self.noise = None
        try:
            eventually(self.socket.exists, "private PulseAudio did not start")
        except AssertionError:
            self.stop()
            raise

    def pactl(self, *arguments):
        result = subprocess.run([str(self.bin / "pactl"), f"--server=unix:{self.socket}", "--format=json", *arguments],
                                capture_output=True, text=True, check=True, timeout=10)
        return json.loads(result.stdout)

    def start_noise(self):
        """Synthetic noise into the microphone sink beside the tone, so frame
        sizes vary as with real content."""
        self.noise = subprocess.Popen(
            [str(self.bin / "pacat"), f"--server=unix:{self.socket}", "--playback", "--device=microphone",
             "--raw", "--format=float32le", "--rate=48000", "--channels=1"],
            stdin=subprocess.PIPE, start_new_session=True)
        generator = random.Random(1)

        def feed():
            block = struct.pack("<4800f", *[0.08 * (generator.random() * 2 - 1) for _ in range(4800)])
            try:
                while self.noise.poll() is None:
                    self.noise.stdin.write(block)
                    self.noise.stdin.flush()
            except (BrokenPipeError, ValueError, OSError):
                pass
        threading.Thread(target=feed, daemon=True).start()

    def capture_streams(self):
        sources = {source["index"]: source["name"] for source in self.pactl("list", "sources")}
        return sorted((stream["index"], stream["client"]) for stream in self.pactl("list", "source-outputs")
                      if sources.get(stream["source"]) == "microphone.monitor")

    def playback_streams(self):
        return [stream["index"] for stream in self.pactl("list", "sink-inputs")
                if stream["properties"].get("application.name") == "Nuschl"]

    def stop(self):
        for process in (self.noise, self.process):
            if process and process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                process.wait(timeout=5)


class Bus:
    """A private D-Bus session bus for one stock server's setConf."""

    def __init__(self, root):
        self.path = Path(root) / "bus"
        self.process = subprocess.Popen(["dbus-daemon", "--session", "--nofork", f"--address=unix:path={self.path}"],
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        eventually(self.path.exists, "private D-Bus did not start")
        self.address = f"unix:path={self.path}"

    def set_bandwidth(self, value):
        result = subprocess.run(
            ["dbus-send", "--session", "--print-reply", "--dest=net.sourceforge.mumble.murmur", "/",
             "net.sourceforge.mumble.Meta.setConf", "int32:1", "string:bandwidth", f"string:{value}"],
            env=os.environ | {"DBUS_SESSION_BUS_ADDRESS": self.address}, capture_output=True, text=True, timeout=10)
        assert result.returncode == 0, result.stderr

    def stop(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            self.process.wait(timeout=5)


class StockServer:
    def __init__(self, binary, root, bandwidth=None):
        self.root = Path(root)
        self.root.mkdir(parents=True)
        self.bus = Bus(self.root)
        self.port = free_port()
        self.murmur = Murmur(Path(binary), self.root, self.port)
        ini = self.murmur.ini.read_text().replace(
            "bandwidth=72000\n", (f"bandwidth={bandwidth}\n" if bandwidth else "") + "dbus=session\n")
        self.murmur.ini.write_text(ini)
        os.environ["DBUS_SESSION_BUS_ADDRESS"] = self.bus.address
        self.murmur.seed()
        self.murmur.start()

    def stop(self):
        self.murmur.stop()
        self.bus.stop()


class NuschlServer:
    def __init__(self, binaries, root, bandwidth):
        self.root = Path(root)
        self.root.mkdir(parents=True)
        self.port = free_port()
        self.server = Nuschld(binaries, self.root, self.port)
        self.server.seed()
        self.set_bandwidth(bandwidth)
        self.server.start()

    def set_bandwidth(self, bandwidth):
        config = self.server.config.read_text()
        self.server.config.write_text(re.sub(r"max_bandwidth = \d+", f"max_bandwidth = {bandwidth}", config))

    def restart(self, bandwidth):
        self.server.stop()
        self.set_bandwidth(bandwidth)
        self.server.start()

    def stop(self):
        self.server.stop()


class Run:
    """One client and one peer against one server."""

    def __init__(self, args, label, port, root, settings):
        self.args, self.label, self.root = args, label, Path(root)
        self.env = environment(self.root)
        self.pulse = Pulse(args.pulse_bin, self.env["XDG_RUNTIME_DIR"], args.output / f"{label}-pulse.log")
        self.pulse.start_noise()
        self.port = port
        self.pin = certificate_pin(args.binaries, "127.0.0.1", port, self.root / "probe")
        self.peer = None
        self.join_peer()
        self.profile = self.root / "profile"
        (self.profile / "config").mkdir(parents=True)
        self.settings_file = self.profile / "config" / "settings.json"
        self.settings_file.write_text(json.dumps(settings))
        self.tui = None
        self.start_tui()

    def start_tui(self):
        env = self.env | {"PULSE_SERVER": f"unix:{self.pulse.socket}", "NUSCHL_AUDIO_BACKEND": "pulse"}
        self.tui = Terminal(self.label, [str((self.args.binaries / "nuschl-tui").resolve()), "--plain", "--audio",
                                         "--opener", "/bin/true", "--profile-dir", str(self.profile),
                                         f"mumble://Ada@127.0.0.1:{self.port}/Lobby"], env, plain=True)
        # The certificate is trusted on first use; a restarted client knows it.
        if self.wait_any(["Trust it with /trust", "Your voice is in Lobby"]) == "Trust it with /trust":
            self.tui.line("/trust")
            self.tui.wait_for("Your voice is in Lobby", timeout=60)
        self.session = eventually(lambda: next((user["session"] for user in self.peer.users.values()
                                                if user["name"] == "Ada"), None), "peer did not see Ada")

    def join_peer(self):
        """A listener in Lobby (again after a server restart dropped it)."""
        if self.peer:
            try:
                self.peer.close()
            except (AssertionError, OSError, ValueError):
                pass
        self.peer = Client(self.args.binaries, "127.0.0.1", self.port, self.pin, "Peer", self.root / "peer")
        self.peer.send(kind="join", channel=LOBBY)

    def wait_any(self, needles, timeout=60):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for needle in needles:
                if needle in self.tui.output:
                    return needle
            self.tui.pump()
        raise AssertionError(f"{self.label}: none of {needles}: {self.tui.output[-3000:]}")

    def command(self, line, expect):
        mark = len(self.tui.output)
        self.tui.line(line)
        self.tui.wait_for(lambda view: re.search(expect, view[mark:]))
        return self.tui.output[mark:]

    def bitrate(self):
        """`/bitrate`: the requested target and this server's line."""
        text = self.command("/bitrate", r"you asked for ([\d.]+ kb/s)[\s\S]*?\n")
        return text

    def unmute(self):
        self.command("/mute", r"Unmuted|Muted|Deafened")
        if "Unmuted" not in self.tui.output[-200:]:
            self.command("/mute", r"Unmuted")
        eventually(lambda: not self.peer.users[self.session]["self_muted"], "peer still sees Ada muted")
        eventually(self.pulse.capture_streams, "capture did not open", timeout=20)
        time.sleep(1.0)

    def frames(self, seconds):
        """Opus frame sizes the peer received from Ada over `seconds`."""
        return received(self.peer, self.session, seconds)

    def worker(self):
        workers = media_workers(self.tui.pid)
        assert len(workers) == 1, workers
        return workers[0]

    def saved(self):
        """settings.json, which must stay readable by Nuschl 0.1.0."""
        saved = json.loads(self.settings_file.read_text())
        assert "voice_bitrate" not in saved and set(saved) <= RELEASED_KEYS, saved
        return saved

    def sidecar(self):
        path = self.settings_file.with_name("voice-quality.json")
        return json.loads(path.read_text()) if path.exists() else None

    def stop(self):
        try:
            transcript = self.args.output / f"{self.label}-tui.txt"
            with transcript.open("a") as output:
                output.write(self.tui.output)
            self.tui.close("/quit\r")
        finally:
            self.peer.close()
            self.pulse.stop()


def check_frames(run, label, frames, target):
    assert frames, f"{run.label} {label}: no voice received"
    largest = max(frames)
    mean = sum(frames) * 8 * 50 / len(frames)
    assert largest <= allowance(target), f"{run.label} {label}: {largest} bytes over {allowance(target)}"
    RESULTS.append(dict(server=run.label, step=label, effective=target, frames=len(frames), largest=largest,
                        allowance=allowance(target), mean_bps=round(mean)))
    print(f"  {run.label:<16} {label:<34} effective {kbps(target):>9}: {len(frames):>3} frames, "
          f"largest {largest:>3}/{allowance(target)} B, mean {mean / 1000:.1f} kb/s", flush=True)


def scenario(args, run, budget, live=None, restart=None):
    """The common steps; `live` changes the budget in place, `restart`
    restarts the server with another one."""
    run.saved()
    assert run.sidecar() is None
    text = run.bitrate()
    want, reason = effective(128_000, budget)
    assert "you asked for 128 kb/s" in text, text
    run.unmute()
    check_frames(run, "default request", run.frames(3), want)
    run.saved()
    assert run.sidecar() is None, "an unchanged request was written"

    worker, capture = run.worker(), run.pulse.capture_streams()
    text = run.command("/bitrate 256", r"you asked for 256 kb/s")
    want, reason = effective(256_000, budget)
    run.saved()
    assert run.sidecar() == {"version": 1, "voice_bitrate": 256_000}, run.sidecar()
    time.sleep(0.5)
    check_frames(run, "requested 256 kb/s", run.frames(3), want)
    assert run.worker() == worker, "a requested change replaced the media worker"
    assert run.pulse.capture_streams() == capture, "a requested change reopened capture"

    if live:
        for value in (40_000, 29_999, budget):
            live(value)
            want, reason = effective(256_000, value)
            text = run.command("/bitrate", r"you asked for 256 kb/s[\s\S]*?\n")
            if want == 0:
                eventually(lambda: not run.pulse.capture_streams(), "a too-small budget kept capture open")
                assert run.pulse.playback_streams(), "a too-small budget stopped playback"
                assert not run.frames(2), "voice was sent past a too-small budget"
                RESULTS.append(dict(server=run.label, step=f"live {value}", effective=0, frames=0,
                                    playback=True, capture=False))
                print(f"  {run.label:<16} live budget {value:<22} no capture, playback kept", flush=True)
            else:
                if not run.pulse.capture_streams():
                    eventually(run.pulse.capture_streams, "capture did not reopen", timeout=20)
                    time.sleep(1.0)
                # Stock Murmur polices each user over its last 360 accepted
                # packets and records no refused one: after a live reduction
                # it refuses even conforming frames until that history decays
                # (about sum/limit seconds). Measure that, then the frames.
                started = time.monotonic()
                recovered = None
                while time.monotonic() - started < 90:
                    if run.frames(1):
                        recovered = time.monotonic() - started
                        break
                assert recovered is not None, f"{run.label}: no voice within 90 s at {value}"
                RESULTS.append(dict(server=run.label, step=f"live {value} first frame after", seconds=round(recovered, 1)))
                print(f"  {run.label:<16} live budget {value:<22} first accepted frame after {recovered:.1f} s",
                      flush=True)
                check_frames(run, f"live budget {value}", run.frames(3), want)
    if restart:
        mark = len(run.tui.output)
        restart()
        run.join_peer()
        run.tui.wait_for(lambda view: re.search(r"[Rr]econnected|Your voice is in Lobby|Connected", view[mark:]),
                         timeout=90)
        run.session = eventually(lambda: next((user["session"] for user in run.peer.users.values()
                                               if user["name"] == "Ada"), None), "peer did not see Ada again")
        text = run.bitrate()
        assert "you asked for 256 kb/s" in text, text
        run.unmute()
        want, reason = effective(256_000, restart.budget)
        check_frames(run, f"after restart at {restart.budget}", run.frames(3), want)

    # Persisted across a restart of the client, then back to the default.
    with (args.output / f"{run.label}-tui.txt").open("a") as output:
        output.write(run.tui.output)
    run.tui.close("/quit\r")
    run.start_tui()
    assert "you asked for 256 kb/s" in run.bitrate()
    run.command("/bitrate 128", r"you asked for 128 kb/s")
    run.saved()
    assert run.sidecar() == {"version": 1, "voice_bitrate": 128_000}, run.sidecar()


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--murmur14", type=Path)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    settings = {"audio": {"transmit": "continuous", "echo_cancellation": "off"}, "server_icons": False}
    with tempfile.TemporaryDirectory(prefix="bitrate-", dir=args.output) as temporary:
        root = Path(temporary)
        # Short labels: they are path components of the client's PulseAudio
        # socket, and Unix socket paths have at most 108 bytes.
        cases = [("m15-558k", None), ("m15-72k", 72_000)]
        if args.murmur14:
            cases.append(("m14-558k", None))
        for label, bandwidth in cases:
            binary = args.murmur14 if label.startswith("m14") else args.murmur
            server = StockServer(binary, root / label / "server", bandwidth)
            run = None
            try:
                run = Run(args, label, server.port, root / label / "client", settings)
                scenario(args, run, bandwidth or 558_000, live=server.bus.set_bandwidth)
            finally:
                if run:
                    run.stop()
                server.stop()
        server = NuschlServer(args.binaries, root / "nuschld" / "server", 558_000)
        run = None
        try:
            run = Run(args, "nuschld", server.port, root / "nuschld" / "client", settings)

            def restart():
                server.restart(72_000)
            restart.budget = 72_000
            scenario(args, run, 558_000, restart=restart)
        finally:
            if run:
                run.stop()
            server.stop()
    (args.output / "bitrate-matrix.json").write_text(json.dumps(RESULTS, indent=2))
    print("Voice bitrate matrix: ok")


if __name__ == "__main__":
    main()
