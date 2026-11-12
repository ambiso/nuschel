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
- Settings → VOICE QUALITY names the requested rate and, per server, the
  effective one and its reason; choosing 256 kb/s writes voice-quality.json
  only (settings.json keeps the keys Nuschl 0.1.0 reads);
- the change reconfigures the encoder only: the same media worker and
  capture stream, frames up to the new per-frame allowance;
- the voice card shows each server's rate; speaking on both lowers Wide to
  Narrow's 48 kb/s ("shared with another server");
- a live ServerConfig (setConf) lowers Narrow's rate, and one too small for
  voice stops sending there without stopping playback;
- narrow window and 150 % zoom keep the voice card's lines bounded.

    python3 tests/bitrate_desktop.py <binaries> --murmur <mumble-server> \\
        --at-spi <at-spi2-core> --pulse-bin <pulseaudio>/bin --output <directory> [--gnome-shell ...]
"""
import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import snapshot, start_registry  # noqa: E402
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

        # Settings: the requested rate and each server's effective one.
        self.press("push button", "Settings")
        self.find("dialog", "Settings")
        group = self.find("radio button", "128 kb/s")
        group.call("Component", "ScrollTo", "u", "0")
        self.named("Wide: 128 kb/s, as requested.")
        self.named("Narrow: 48 kb/s: this server allows 72 kb/s per user, 24 kb/s of which is kept for "
                   "Mumble packet overhead.")
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
        self.named("Wide: 256 kb/s, as requested.")
        self.named("Narrow: 48 kb/s: this server allows 72 kb/s per user, 24 kb/s of which is kept for "
                   "Mumble packet overhead.")
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

        # Narrow window and 150 % zoom: the card's lines stay bounded.
        self.shell.place("Nuschl", 40, 40, 760, 700)
        self.shot("8-narrow")
        for _ in range(4):
            self.shell.chord("Control_L", "equal")
            time.sleep(0.3)
        self.shot("9-narrow-zoomed")
        (self.args.output / "bitrate-desktop.json").write_text(json.dumps(self.results, indent=2))
        print("Desktop voice bitrate: ok", flush=True)

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
