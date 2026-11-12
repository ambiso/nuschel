#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Continuous volume and sensitivity sliders and the microphone activity meter,
in a private headless GNOME Shell on the real GPU, with a private PulseAudio
(null devices only) and a private accessibility bus. Nothing of the user's
session, microphone, speakers, clipboard or profile is used.

    python3 tests/slider_scenes.py <binaries> --murmur <mumble-server> \\
        --gnome-shell <gnome-shell> --at-spi <at-spi2-core> --pulse-bin <pulseaudio>/bin \\
        --output DIR

Checks, with a screenshot of each state (look at them):

1. Settings opened alone opens no microphone: the private PulseAudio has no
   source-output until "Test microphone" is pressed, and none again after
   "Stop test" and after Settings closes.
2. The sensitivity slider as a screen reader sees it: role slider, its name,
   a numeric value in percent with its range, and the value text ("50 % ·
   opens above 40 % speech probability") as its description. Right arrow
   changes it at once and settings.json is written once, half a second
   later; Delete resets it.
3. The meter above Sensitivity while the test runs, fed by a private null
   microphone: silence, then a speech-like signal (dsp.rs' `speech_like`),
   then silence again: "Voice gate closed", "Voice gate open", "Voice
   gate closed". The processed input level and speech probability are progress bars
   with numeric values; the level bar marks the gate's floor (one constant
   in nuschl_core, which the worker's own unit test checks it uses).
4. A pointer drag over Sensitivity changes the value while held and writes
   settings.json exactly once, after the release. A drag whose window is
   minimised while the button is still held is saved once, at the focus
   loss; its later moves and release change nothing.
5. Connected, Ada's menu: the volume slider at 100 %, a pointer drag (the
   menu stays open, one write), a key step and Delete back to 100 %; a drag
   whose menu is closed with Escape mid-drag is saved once and edits nothing
   after.
6. While muted (or deafened) in a call, the running test says that it still
   uses the microphone; unmuted, it does not.
7. Quitting within half a second of a key (the compositor closes the window
   while Settings is open) still saves the key's value, exactly once.
"""
import argparse
import array
import json
import math
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
import accessibility  # noqa: E402
from accessibility import items, start_registry  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402

# AT-SPI roles the shared table lacks (atspi-constants.h).
accessibility.ROLES.update({42: "progress bar", 51: "slider"})
WIDTH, HEIGHT = 1280, 800
MUTED_NOTE = "Muted in calls · this test still uses your microphone"
RATE = 48000


def speech_like(seconds):
    """A voiced, amplitude-modulated harmonic series, as dsp.rs' speech_like
    (which RNNoise treats as speech), float32 mono at 48 kHz."""
    samples = array.array("f")
    for index in range(int(seconds * RATE)):
        t = index / RATE
        envelope = 0.5 + 0.5 * math.sin(2 * math.pi * 4 * t)
        value = sum(math.sin(2 * math.pi * 140 * harmonic * t) / harmonic for harmonic in range(1, 13))
        samples.append(0.12 * envelope * value)
    return samples.tobytes()


def saved(path, *keys):
    """The value at `keys` in the JSON file at `path`, or None while it has none."""
    value = json.loads(path.read_text())
    for key in keys:
        if not isinstance(value, dict) or key not in value:
            return None
        value = value[key]
    return value


class Writes:
    """Every change of settings.json, seen by polling its inode, time and size."""

    def __init__(self, path):
        self.path = path
        self.changes = []
        self.running = True
        self.thread = threading.Thread(target=self.watch, daemon=True)
        self.thread.start()

    def stamp(self):
        try:
            status = self.path.stat()
            return status.st_ino, status.st_mtime_ns, status.st_size
        except FileNotFoundError:
            return None

    def watch(self):
        last = self.stamp()
        while self.running:
            now = self.stamp()
            if now != last:
                self.changes.append(time.monotonic())
                last = now
            time.sleep(0.002)

    def since(self, start):
        return sum(1 for at in self.changes if at >= start)

    def stop(self):
        self.running = False
        self.thread.join(timeout=2)


class Scenes:
    def __init__(self, args, root):
        self.args, self.root = args, root
        self.processes = []
        self.out = args.output
        self.shell = None
        self.app = None
        self.socket = None

    # Processes and the private session.

    def spawn(self, command, env, log):
        process = subprocess.Popen(command, env=env, stdout=(self.out / log).open("w"), stderr=subprocess.STDOUT,
                                   start_new_session=True)
        self.processes.append(process)
        return process

    def start_pulse(self):
        socket_dir = self.shell.runtime / "pulse"
        socket_dir.mkdir(mode=0o700, exist_ok=True)
        self.socket = socket_dir / "native"
        script = self.shell.runtime / "default.pa"
        # A null sink is the microphone: what is played into it is recorded
        # from its monitor. Nothing plays at first, so it records silence.
        script.write_text(
            f"load-module module-native-protocol-unix socket={self.socket} auth-anonymous=1\n"
            "load-module module-null-sink sink_name=speakers rate=48000 sink_properties=device.description=Speakers\n"
            "load-module module-null-sink sink_name=microphone rate=48000 channels=1 channel_map=mono"
            " sink_properties=device.description=Mic\n"
            "update-source-proplist microphone.monitor device.description=\"Test microphone\"\n"
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

    def recording(self):
        """Names of the sources something records from (only Nuschl runs here)."""
        sources = {source["index"]: source["name"] for source in self.pactl("list", "sources")}
        return sorted(sources.get(stream["source"], "?") for stream in self.pactl("list", "source-outputs"))

    def feed(self, seconds):
        """Play the speech-like signal into the null microphone, in the background."""
        signal_path = self.root / "speech.f32"
        if not signal_path.exists():
            signal_path.write_bytes(speech_like(seconds))
        return subprocess.Popen([str(self.args.pulse_bin / "pacat"), f"--server=unix:{self.socket}", "--playback",
                                 "--device=microphone", "--raw", "--format=float32le", f"--rate={RATE}",
                                 "--channels=1", "--latency-msec=20"],
                                stdin=signal_path.open("rb"), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                start_new_session=True)

    def launch(self, profile, url=None):
        env = self.shell.app_environment() | {"RUST_LOG": "nuschl=info",
                                              "PULSE_SERVER": f"unix:{self.socket}"}
        env.pop("NO_AT_BRIDGE", None)
        self.app = self.spawn([str(self.args.binaries / "nuschl"), "--profile-dir", str(profile)]
                              + ([url] if url else []), env, f"desktop-{len(self.processes)}.log")
        eventually(lambda: self.app.poll() is None and self.shell.window("Nuschl"), "Nuschl window did not appear")
        self.shell.place("Nuschl", 40, 40, WIDTH, HEIGHT)
        time.sleep(1.5)
        self.nuschl = eventually(lambda: next((node for node in self.registry.children()
                                                if node.name.lower() == "nuschl"), None),
                                 "Nuschl is not in the accessibility registry", timeout=45)

    def quit(self):
        os.killpg(self.app.pid, signal.SIGTERM)
        self.app.wait(timeout=15)

    # Looking and pointing.

    def shot(self, name):
        window = self.shell.window("Nuschl")
        path = self.shell.screenshot(self.out / f"{name}.png",
                                     crop=(window["x"] - 12, window["y"] - 12, window["width"] + 24,
                                           window["height"] + 24))
        print(path, flush=True)

    def find(self, role, name, prefix=False):
        for node, found_role, found_name, _ in items(self.nuschl):
            if (role is None or found_role == role) and (found_name.startswith(name) if prefix else found_name == name):
                return node
        return None

    def wait_for(self, role, name, message, timeout=20, prefix=False):
        return eventually(lambda: self.find(role, name, prefix), message, timeout=timeout)

    def screen(self, x, y):
        """Screen coordinates of a point in AT-SPI window coordinates."""
        window = self.shell.window("Nuschl")
        return window["x"] + x, window["y"] + y

    @staticmethod
    def value(node):
        return node.bus("get-property", node.destination, node.path, "org.a11y.atspi.Value", "CurrentValue")

    @staticmethod
    def description(node):
        return node.bus("get-property", node.destination, node.path, "org.a11y.atspi.Accessible", "Description")

    @staticmethod
    def maximum(node):
        return node.bus("get-property", node.destination, node.path, "org.a11y.atspi.Value", "MaximumValue")

    def thumb(self, slider, position):
        """Where the thumb of `slider` sits at `position` (0..1), on screen:
        inside its 2-pixel focus border, the track inset by half the thumb."""
        x, y, width, height = slider.extents()
        return self.screen(x + 2 + 8 + position * (width - 4 - 16), y + height // 2)

    def press(self):
        self.shell.eval("global._nuschlPointer.notify_button(imports.gi.GLib.get_monotonic_time(), 1,"
                        " imports.gi.Clutter.ButtonState.PRESSED); 'ok'")

    def release(self):
        self.shell.eval("global._nuschlPointer.notify_button(imports.gi.GLib.get_monotonic_time(), 1,"
                        " imports.gi.Clutter.ButtonState.RELEASED); 'ok'")

    def glide(self, start, end, steps=10):
        for step in range(1, steps + 1):
            self.shell.move(start[0] + (end[0] - start[0]) * step / steps,
                            start[1] + (end[1] - start[1]) * step / steps)
            time.sleep(0.025)

    # The scenes.

    def run(self):
        self.shell = GnomeSession(self.args.gnome_shell, WIDTH + 160, HEIGHT + 100,
                                  log=(self.out / "gnome-shell.log").open("w"))
        self.shell.start()
        self.registry = start_registry(self.args.at_spi, self.shell.app_environment(), self.spawn)
        self.start_pulse()
        port = free_port()
        (self.root / "murmur").mkdir()
        murmur = Murmur(self.args.murmur, self.root / "murmur", port)
        murmur.seed()
        murmur.start()
        peer = None
        try:
            pin = certificate_pin(self.args.binaries, "127.0.0.1", port, self.root / "probe")
            profile = self.root / "profile"
            (profile / "config").mkdir(parents=True)
            (profile / "config/servers.json").write_text(json.dumps([dict(
                id=str(uuid.uuid4()), name="Slider server", host="127.0.0.1", port=port, username="Andi",
                certificate_sha256=pin)]))
            settings_path = profile / "config/settings.json"
            settings_path.write_text(json.dumps({"appearance": {"theme": "dark"}}))
            writes = Writes(settings_path)
            # The first launch connects deafened: nothing records until the
            # microphone test starts.
            self.launch(profile)
            self.settings(writes, settings_path)
            self.zoom_geometry(settings_path)
            self.quit()
            peer = Client(self.args.binaries, "127.0.0.1", port, pin, "Ada Lovelace", self.root / "peer")
            peer.send(kind="join", channel=2)
            self.launch(profile, f"mumble://Andi@127.0.0.1:{port}/Lobby/Development")
            peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                      and e["user"]["channel"] == 2, timeout=40)
            time.sleep(2.0)
            self.menu(writes, settings_path)
            self.mute_note()
            self.quit_mid_edit(writes, settings_path)
            writes.stop()
        finally:
            if peer:
                peer.close()
            murmur.stop()

    def settings(self, writes, settings_path):
        self.wait_for("push button", "Settings", "no Settings button").press()
        self.wait_for("dialog", "Settings", "Settings did not open")
        time.sleep(2.0)
        recorded = self.recording()
        assert recorded == [], f"Settings alone opened a microphone: {recorded}"
        print("Settings open, microphone test not started: no source-output on the private PulseAudio.", flush=True)
        slider = self.wait_for("slider", "Voice activity sensitivity", "no sensitivity slider")
        assert self.value(slider) == 50.0 and self.maximum(slider) == 100.0, (self.value(slider), self.maximum(slider))
        assert self.description(slider) == "50 % · opens above 40 % speech probability", self.description(slider)
        assert self.find("progress bar", "Processed input level") is None, "the meter shows without the microphone test"
        self.shot("01-settings-voice")
        print(f"Sensitivity slider: value {self.value(slider)} of {self.maximum(slider)}, "
              f"value text {self.description(slider)!r}; no meter.", flush=True)

        # The keyboard: heard at once, saved once after half a second.
        slider.call("Component", "GrabFocus")
        time.sleep(0.4)
        start = time.monotonic()
        self.shell.key("Right")
        eventually(lambda: abs(self.value(slider) - 51) < 0.01, "Right did not raise the sensitivity", timeout=3)
        changed = time.monotonic()
        assert writes.since(start) == 0 or writes.changes[-1] - start >= 0.4, "the key was saved at once"
        eventually(lambda: abs((saved(settings_path, "audio", "vad_sensitivity") or 0) - 0.51) < 1e-6,
                   "the key's value was not saved", timeout=3)
        assert writes.since(start) == 1, f"{writes.since(start)} writes for one key"
        saved_after = writes.changes[-1] - changed
        self.shot("02-settings-key-focused")
        print(f"Right arrow: 51 at once over AT-SPI, settings.json written once, {saved_after:.2f} s later; "
              f"value text {self.description(slider)!r}.", flush=True)
        self.shell.key("Delete")
        eventually(lambda: self.value(slider) == 50.0, "Delete did not reset the sensitivity", timeout=3)
        eventually(lambda: saved(settings_path, "audio", "vad_sensitivity") == 0.5,
                   "the reset was not saved", timeout=3)
        print("Delete: back to the default, 50, and saved.", flush=True)

        # The real accessibility SetValue route must never turn NaN/infinity
        # into a default gain or save an otherwise unchanged setting.
        time.sleep(0.1)  # Let the preceding Delete write reach the observer.
        start = time.monotonic()
        for invalid in ["nan", "inf", "-inf"]:
            slider.bus("set-property", slider.destination, slider.path,
                       "org.a11y.atspi.Value", "CurrentValue", "d", invalid)
        time.sleep(0.8)
        assert self.value(slider) == 50.0 and saved(settings_path, "audio", "vad_sensitivity") == 0.5
        assert writes.since(start) == 0, "nonfinite AT values changed/saved the setting"
        print("AT SetValue NaN/+inf/-inf: unchanged and no settings write.", flush=True)

        # The microphone test and its meter.
        self.wait_for("push button", "Test microphone", "no Test microphone button").press()
        eventually(lambda: "microphone.monitor" in self.recording() or None,
                   "Test microphone did not open the microphone", timeout=20)
        level = self.wait_for("progress bar", "Processed input level", "the meter did not appear")
        # The first launch is deafened in its call: the test says it records anyway.
        self.wait_for(None, MUTED_NOTE, "the meter did not say the test records while deafened")
        probability = self.wait_for("progress bar", "Speech probability", "no speech probability bar")
        legend = self.description(probability)
        assert "opens above 40 %" in legend and "down to 20 %" in legend, legend
        eventually(lambda: self.find(None, "Voice gate closed"), "the gate state did not read Voice gate closed",
                   timeout=10)
        time.sleep(1.5)
        assert re.search(r"Marked: -[0-9]+ dBFS", self.description(level)), self.description(level)
        silence = (self.value(level), self.description(level), self.value(probability), self.description(probability))
        self.shot("03-meter-silence")
        print(f"Microphone test, silence: level {silence[0]} ({silence[1]!r}); speech {silence[2]} "
              f"({silence[3]!r}); Voice gate closed. Recording: {self.recording()}.", flush=True)
        feeder = self.feed(8)
        try:
            eventually(lambda: self.find(None, "Voice gate open"), "speech did not open the gate", timeout=10)
            time.sleep(0.6)
            speech = (self.value(level), self.description(level), self.value(probability),
                      self.description(probability))
            self.shot("04-meter-speech")
            print(f"Speech-like signal: level {speech[0]} ({speech[1]!r}); speech {speech[2]} ({speech[3]!r}); "
                  "Voice gate open.", flush=True)
        finally:
            os.killpg(feeder.pid, signal.SIGTERM)
            feeder.wait(timeout=5)
        eventually(lambda: self.find(None, "Voice gate closed"), "the gate did not close after the speech",
                   timeout=10)
        time.sleep(1.0)
        after = (self.value(level), self.value(probability))
        self.shot("05-meter-silence-again")
        print(f"Silence again: level {after[0]}, speech {after[1]}; Voice gate closed.", flush=True)

        # A pointer drag: heard while held, one write after the release.
        slider = self.wait_for("slider", "Voice activity sensitivity", "no sensitivity slider")
        start = time.monotonic()
        begin, middle, end = self.thumb(slider, 0.5), self.thumb(slider, 0.65), self.thumb(slider, 0.8)
        self.shell.move(*begin)
        time.sleep(0.1)
        self.press()
        self.glide(begin, middle)
        time.sleep(0.4)
        held = self.value(slider)
        assert 60 <= held <= 70, f"the held drag shows {held}"
        assert writes.since(start) == 0, "the drag was saved while held"
        self.shot("06-sensitivity-dragging")
        self.glide(middle, end)
        self.release()
        released = time.monotonic()
        time.sleep(1.2)
        final = self.value(slider)
        stored = saved(settings_path, "audio", "vad_sensitivity")
        assert writes.since(start) == 1, f"{writes.since(start)} writes for one drag"
        assert abs(stored * 100 - final) < 0.01 and 77 <= final <= 83, (stored, final)
        print(f"Pointer drag: {held:.1f} while held (no write), {final:.1f} after the release; settings.json "
              f"written once, {writes.changes[-1] - released:.3f} s after the release; value text "
              f"{self.description(slider)!r}.", flush=True)
        self.shot("07-sensitivity-dragged")

        # The window loses focus mid-drag (minimised by the compositor while
        # the button is held): what was heard is saved then, once.
        slider = self.wait_for("slider", "Voice activity sensitivity", "no sensitivity slider")
        start = time.monotonic()
        begin, end = self.thumb(slider, 0.8), self.thumb(slider, 0.3)
        self.shell.move(*begin)
        time.sleep(0.1)
        self.press()
        self.glide(begin, end)
        time.sleep(0.4)
        heard = self.value(slider)
        assert writes.since(start) == 0 and 25 <= heard <= 35, (writes.since(start), heard)
        self.shell.eval("global.get_window_actors().map(a => a.meta_window).find(w => (w.get_title() || '')"
                        ".startsWith('Nuschl')).minimize(); 'ok'")
        minimised = time.monotonic()
        eventually(lambda: writes.since(start) >= 1 or None, "losing focus mid-drag saved nothing", timeout=3)
        lag = writes.changes[-1] - minimised
        stored = saved(settings_path, "audio", "vad_sensitivity")
        self.glide(end, (end[0] + 150, end[1]))
        self.release()
        self.shell.place("Nuschl", 40, 40, WIDTH, HEIGHT)
        time.sleep(1.5)
        slider = self.wait_for("slider", "Voice activity sensitivity", "no sensitivity slider")
        assert writes.since(start) == 1, f"{writes.since(start)} writes for a drag that lost focus"
        assert abs(stored * 100 - heard) < 0.01 and abs(self.value(slider) - heard) < 0.01, \
            (stored, heard, self.value(slider))
        self.shot("07b-after-focus-loss")
        print(f"Focus lost mid-drag (window minimised, button held): {heard:.1f} saved once, {lag:.3f} s after "
              "the minimise; moving and releasing afterwards changed nothing.", flush=True)

        self.wait_for("push button", "Stop test", "no Stop test button").press()
        eventually(lambda: self.recording() == [] or None, "Stop test left the microphone open", timeout=10)
        assert self.find("progress bar", "Processed input level") is None, "the meter stayed after the test stopped"
        self.shell.key("Escape")
        eventually(lambda: self.find("dialog", "Settings") is None or None, "Settings did not close")
        time.sleep(1.0)
        assert self.recording() == [], "a microphone is open after Settings closed"
        print("Stop test closed the microphone and the meter; closing Settings left none open.", flush=True)

    def zoom_geometry(self, settings_path):
        """Actual controls/direction labels remain legible and contained after zoom."""
        records = []
        for width, zoom in [(1280, 100), (900, 100), (900, 200)]:
            self.shell.chord("Control_L", "0")
            self.shell.place("Nuschl", 40, 40, width, HEIGHT)
            for _ in range(5 if zoom == 200 else 0):
                self.shell.chord("Control_L", "equal")
                time.sleep(0.1)
            eventually(lambda: abs(saved(settings_path, "appearance", "zoom") - zoom / 100) < 0.001,
                       "zoom shortcut did not update the actual setting")
            self.wait_for("push button", "Settings", "no Settings button").press()
            dialog = self.wait_for("dialog", "Settings", "Settings did not open")
            slider = self.wait_for("slider", "Voice activity sensitivity", "no sensitivity slider")
            slider.call("Component", "GrabFocus")
            time.sleep(0.5)
            labels = [self.wait_for(None, label, "missing sensitivity direction")
                      for label in ["0 %: stricter", "100 %: more sensitive"]]
            bounds = [list(node.extents()) for node in [slider, *labels]]
            self.shot(f"zoom-{width}-{zoom}")
            window = self.shell.window("Nuschl")
            parent = list(dialog.extents())
            for x, y, w, h in bounds:
                assert w > 0 and h > 0 and x >= 0 and y >= 0, bounds
                assert x + w <= window["width"] + 1 and y + h <= window["height"] + 1, (width, zoom, window, bounds)
                assert x >= parent[0] - 1 and x + w <= parent[0] + parent[2] + 1, (parent, bounds)
            left, right = bounds[1:]
            assert left[0] + left[2] <= right[0] + 1, ("direction labels overlap", bounds)
            records.append({"width": width, "zoom": zoom, "dialog": parent, "controls": bounds})
            self.close_settings()
        self.shell.chord("Control_L", "0")
        self.shell.place("Nuschl", 40, 40, WIDTH, HEIGHT)
        (self.out / "zoom-geometry.json").write_text(json.dumps(records, indent=2) + "\n")
        print("Actual sensitivity direction/slider geometry:1280/100,900/100,900/200 contained.", flush=True)

    def menu(self, writes, settings_path):
        # Ada's row; right-click until her menu (with a volume slider) opens.
        slider = None
        for node, role, name, _ in items(self.nuschl):
            if not name.startswith("Ada Lovelace") or role not in ("tree item", "list item", "push button"):
                continue
            x, y, width, height = node.extents()
            if width <= 0 or height <= 0:
                continue
            self.shell.click(*self.screen(x + min(width // 2, 60), y + height // 2), button=3)
            time.sleep(0.8)
            slider = self.find("slider", "Volume")
            if slider:
                break
            self.shell.key("Escape")
            time.sleep(0.4)
        assert slider, "no menu with a volume slider on Ada"
        assert self.value(slider) == 100.0 and self.maximum(slider) == 400.0, (self.value(slider), self.maximum(slider))
        assert self.description(slider) == "100 % (+0.0 dB)", self.description(slider)
        self.shot("10-menu-volume-100")
        print(f"Ada's menu: volume slider {self.value(slider)} of {self.maximum(slider)}, "
              f"{self.description(slider)!r}.", flush=True)

        start = time.monotonic()
        unity = (1 / 4) ** (1 / 3)
        begin, middle, end = self.thumb(slider, unity), self.thumb(slider, 0.55), self.thumb(slider, 0.45)
        self.shell.move(*begin)
        time.sleep(0.1)
        self.press()
        self.glide(begin, middle)
        time.sleep(0.4)
        held = self.value(slider)
        assert held < 90, f"the held drag shows {held}"
        assert writes.since(start) == 0, "the menu drag was saved while held"
        self.glide(middle, end)
        self.release()
        time.sleep(1.2)
        slider = self.find("slider", "Volume")
        assert slider, "the menu closed during the drag"
        final = self.value(slider)
        volumes = saved(settings_path, "user_volumes")
        assert writes.since(start) == 1, f"{writes.since(start)} writes for one menu drag"
        assert len(volumes) == 1 and abs(next(iter(volumes.values())) * 100 - final) < 0.01, (volumes, final)
        assert 30 <= final <= 42, final
        hash_ = next(iter(volumes))
        self.shot("11-menu-volume-dragged")
        print(f"Menu drag: {held:.1f} while held (no write), {final:.1f} after the release "
              f"({self.description(slider)!r}); the menu stayed open; settings.json written once "
              f"with only Ada's volume.", flush=True)

        slider.call("Component", "GrabFocus")
        time.sleep(0.4)
        start = time.monotonic()
        self.shell.key("Right")
        eventually(lambda: self.value(self.find("slider", "Volume")) > final, "Right did not raise the volume",
                   timeout=3)
        time.sleep(1.0)
        assert writes.since(start) == 1, f"{writes.since(start)} writes for one key in the menu"
        self.shell.key("Delete")
        eventually(lambda: self.value(self.find("slider", "Volume")) == 100.0, "Delete did not reset the volume",
                   timeout=3)
        time.sleep(1.0)
        assert saved(settings_path, "user_volumes") == {hash_: 1.0}
        self.shot("12-menu-volume-reset")
        print("Menu keys: Right raised the volume (one write after the key), Delete reset it to 100 % (saved).",
              flush=True)

        # Escape closes the menu mid-drag: what was heard is saved once; the
        # rest of the drag edits no one.
        slider = self.find("slider", "Volume")
        start = time.monotonic()
        begin, end = self.thumb(slider, unity), self.thumb(slider, 0.5)
        self.shell.move(*begin)
        time.sleep(0.1)
        self.press()
        self.glide(begin, end)
        time.sleep(0.3)
        heard = self.value(slider)
        self.shell.key("Escape")
        eventually(lambda: self.find("slider", "Volume") is None or None, "Escape did not close the menu")
        time.sleep(0.5)
        closed = writes.since(start)
        self.glide(end, (end[0] - 120, end[1]))
        self.release()
        time.sleep(1.0)
        volumes = saved(settings_path, "user_volumes")
        assert closed == 1 and writes.since(start) == 1, (closed, writes.since(start))
        assert list(volumes) == [hash_] and abs(volumes[hash_] * 100 - heard) < 0.01, (volumes, heard)
        self.shot("13-menu-closed-mid-drag")
        print(f"Escape mid-drag: the menu closed, the heard {heard:.1f} was saved once; moving on and releasing "
              "afterwards edited no one.", flush=True)

    def microphone_test(self):
        self.wait_for("push button", "Settings", "no Settings button").press()
        self.wait_for("dialog", "Settings", "Settings did not open")
        time.sleep(1.0)
        self.wait_for("push button", "Test microphone", "no Test microphone button").press()
        self.wait_for("progress bar", "Processed input level", "the meter did not appear")
        eventually(lambda: self.find(None, "Voice gate closed"), "no reading", timeout=10)
        time.sleep(1.0)

    def close_settings(self):
        self.shell.key("Escape")
        eventually(lambda: self.find("dialog", "Settings") is None or None, "Settings did not close")
        time.sleep(0.5)

    def mute_note(self):
        """Muting a call does not stop an explicitly started test, and the
        meter says so; unmuted, there is nothing to say."""
        self.microphone_test()
        assert self.find(None, MUTED_NOTE), "muted in the call, the meter did not say the test still records"
        self.shot("14-test-while-muted")
        self.close_settings()
        self.wait_for("push button", "Unmute", "no Unmute button").press()
        self.wait_for("push button", "Mute", "Unmute did not unmute")
        self.microphone_test()
        assert self.find(None, MUTED_NOTE) is None, "unmuted, the meter still says muted in calls"
        self.shot("15-test-unmuted")
        self.close_settings()
        self.wait_for("push button", "Mute", "no Mute button").press()
        self.wait_for("push button", "Unmute", "Mute did not mute")
        print(f"Muted in the call, the running test says {MUTED_NOTE!r}; unmuted, it does not.", flush=True)

    def quit_mid_edit(self, writes, settings_path):
        """A key's value is saved half a second after it; quitting sooner saves it too.

        The window is closed by the compositor (Alt+F4, the overview's close)
        with Settings still open, so Settings' own close, which also saves, is
        not what saves here. (A click on the title bar's Close button while
        Settings is open lands outside the dialog and closes only the dialog.)
        """
        self.wait_for("push button", "Settings", "no Settings button").press()
        self.wait_for("dialog", "Settings", "Settings did not open")
        time.sleep(1.0)
        slider = self.wait_for("slider", "Voice activity sensitivity", "no sensitivity slider")
        before = self.value(slider)
        slider.call("Component", "GrabFocus")
        time.sleep(0.4)
        start = time.monotonic()
        self.shell.key("Right")
        eventually(lambda: abs(self.value(slider) - (before + 1)) < 0.01, "Right did not raise the sensitivity",
                   timeout=3)
        pressed = time.monotonic()
        unsaved = writes.since(start)
        self.shell.eval("global.get_window_actors().map(a => a.meta_window).find(w => (w.get_title() || '')"
                        ".startsWith('Nuschl')).delete(global.get_current_time()); 'ok'")
        assert pressed - start < 0.4 and unsaved == 0, (
            f"inconclusive: the close came {pressed - start:.2f} s after the key, {unsaved} writes before it")
        eventually(lambda: self.shell.window("Nuschl") is None or None, "the window stayed", timeout=5)
        code = self.app.wait(timeout=20)
        time.sleep(0.3)
        value = saved(settings_path, "audio", "vad_sensitivity")
        print(f"Quit {pressed - start:.2f} s after a key, Settings still open, by the compositor's close: exit "
              f"{code}; saved {value}, {writes.since(start)} settings.json writes.", flush=True)
        assert code == 0, code
        assert value is not None and abs(value * 100 - (before + 1)) < 1e-4, (value, before)
        assert writes.since(start) == 1, f"{writes.since(start)} writes when quitting after one key"

    def close(self):
        for process in reversed(self.processes):
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
        if self.shell:
            self.shell.stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True, help="at-spi2-core package (bus launcher, registry)")
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschl-sliders-") as temporary:
        scenes = Scenes(args, Path(temporary))
        try:
            scenes.run()
        finally:
            scenes.close()
    print("All slider and meter checks passed.", flush=True)


if __name__ == "__main__":
    main()
