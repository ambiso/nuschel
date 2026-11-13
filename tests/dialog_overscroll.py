#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Dialogs end at their content (#198): no blank page below the last control.

The real desktop in a private headless GNOME Shell (real GPU), operated over
AT-SPI, against an owned loopback stock Murmur with one nuschl-cli peer, and a
private PulseAudio with null sinks. Every zoom gets a fresh profile; no host
display, profile, clipboard or device is used.

At 100 % and 200 % zoom it opens Settings and the peer's "Profile and
connection" (the Details dialog), scrolls each to the end with the End key and
again with a wheel spin over the dialog's own scrollbar gutter, and requires
the lowest accessible control to sit within the dialog's bottom padding. It
also fails on accessible text with a run of three or more spaces between words.
Run against a build without the fix, it fails: the end is a blank page.
"""
import argparse
import json
import os
import re
import signal
import subprocess
import tempfile
import time
import uuid
from pathlib import Path

from accessibility import items, start_registry
from gnome_session import GnomeSession
from local_murmur import Murmur, eventually, free_port
from mumble_client import Client, certificate_pin

PEER = "Control sender"
# The dialogs' own bottom padding is 20 to 25 logical px; a blank page is a
# whole viewport (hundreds of px).
PADDING = 32


def descendants(node):
    """Extents of every accessible descendant with an area."""
    found, stack = [], [node]
    while stack:
        for child in stack.pop().children():
            extents = child.extents()
            if extents[2] > 0 and extents[3] > 0:
                found.append(extents)
            stack.append(child)
    return found


def settled(dialog):
    """The dialog's and its descendants' extents once smooth scrolling stops."""
    previous = None
    for _ in range(30):
        current = (dialog.extents(), descendants(dialog))
        if current == previous:
            return current
        previous = current
        time.sleep(0.4)
    raise AssertionError("the dialog did not stop scrolling")


class Desktop:
    """One desktop run with a fresh profile at one zoom."""

    def __init__(self, args, shell, registry, env, murmur, pin, zoom, root):
        self.args, self.shell, self.registry, self.zoom = args, shell, registry, zoom
        self.name = f"Reader {int(zoom * 100)}"
        profile = root / f"profile-{int(zoom * 100)}"
        (profile / "config").mkdir(parents=True)
        (profile / "config/servers.json").write_text(json.dumps([{
            "id": str(uuid.uuid4()), "name": "Dialog fixture", "host": "127.0.0.1", "port": murmur.port,
            "username": self.name, "certificate_sha256": pin}]))
        (profile / "config/settings.json").write_text(json.dumps(
            {"appearance": {"theme": "dark", "zoom": zoom}, "audio": {"transmit": "push_to_talk"}}))
        self.process = subprocess.Popen(
            [str(args.binaries / "nuschl"), "--profile-dir", str(profile),
             f"mumble://{self.name.replace(' ', '%20')}@127.0.0.1:{murmur.port}/"],
            env=env, stdout=(args.output / f"desktop-{int(zoom * 100)}.log").open("w"),
            stderr=subprocess.STDOUT, start_new_session=True)

    def stop(self):
        os.killpg(self.process.pid, signal.SIGTERM)
        try:
            self.process.wait(timeout=10)
        except subprocess.TimeoutExpired:
            os.killpg(self.process.pid, signal.SIGKILL)
            self.process.wait(timeout=5)
        eventually(lambda: not self.shell.window("Nuschl"), "the window did not close")

    def find(self, role, name, prefix=False):
        def found():
            return next((node for node, kind, label, _ in items(self.application) if kind == role
                         and (label.startswith(name) if prefix else label == name)), None)
        return eventually(found, f"missing {role} {name!r}")

    def window_point(self, extents, dx, dy):
        origin = json.loads(self.shell.eval(
            "JSON.stringify((()=>{const w=global.get_window_actors().map(a=>a.meta_window)"
            ".find(w=>w.get_title().startsWith('Nuschl'));const b=w.get_buffer_rect();return [b.x,b.y];})())"))
        return origin[0] + extents[0] + dx, origin[1] + extents[1] + dy

    def end_gap(self, dialog, how, label):
        """Scrolls from the top to the end; the space below the lowest control."""
        self.shell.key("Home")
        settled(dialog)
        if how == "key":
            self.shell.key("End")
        else:
            box = dialog.extents()
            point = self.window_point(box, box[2] - round(6 * self.zoom), box[3] // 2)
            for _ in range(4):
                self.shell.wheel(*point, 40)
                time.sleep(0.3)
        (_, y, _, height), controls = settled(dialog)
        self.shell.screenshot(self.args.output / f"{label}-end-{how}.png")
        return y + height - max(top + tall for _, top, _, tall in controls)

    def check(self, results):
        eventually(lambda: self.shell.window("Nuschl"), "native window missing")
        self.shell.place("Nuschl", 40, 40, 1280, 800)
        self.application = eventually(lambda: next(
            (node for node in self.registry.children() if node.name.lower() == "nuschl"), None),
            "native accessibility application missing")
        peer = self.find("tree item", PEER, prefix=True).extents()
        for dialog_name in ("Settings", "Details"):
            if dialog_name == "Settings":
                self.find("push button", "Settings").press()
            else:
                self.shell.click(*self.window_point(peer, peer[2] // 2, peer[3] // 2), button=3)
                self.find("menu item", "Profile and connection").press()
            dialog = self.find("dialog", dialog_name)
            label = f"{dialog_name.lower()}-{int(self.zoom * 100)}"
            spaced = [name for _, _, name, _ in items(self.application) if re.search(r"\S {3,}\S", name)]
            gaps = {how: self.end_gap(dialog, how, label) for how in ("key", "wheel")}
            results.append({"dialog": dialog_name, "zoom": self.zoom, "gap_below_last": gaps,
                            "height": dialog.extents()[3], "spaced_text": spaced})
            self.shell.key("Escape")
            eventually(lambda: not any(kind == "dialog" for _, kind, _, _ in items(self.application)),
                       f"{dialog_name} did not close")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binaries", type=Path, required=True)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    results, helpers = [], []
    with tempfile.TemporaryDirectory(prefix="nuschl-dialogs-") as temporary:
        root = Path(temporary)
        murmur = Murmur(args.murmur, root, free_port())
        murmur.seed()
        murmur.start()
        shell = GnomeSession(args.gnome_shell, 1440, 900, log=(args.output / "gnome.log").open("w"))
        bot = None
        try:
            shell.start()

            def spawn(command, env, log):
                helpers.append(subprocess.Popen(command, env=env, stdout=(args.output / log).open("w"),
                                                stderr=subprocess.STDOUT, start_new_session=True))
                return helpers[-1]

            registry = start_registry(args.at_spi, shell.app_environment(), spawn)
            pulse = shell.runtime / "pulse"
            pulse.mkdir(mode=0o700, exist_ok=True)  # GNOME Shell may already have made it
            (pulse / "default.pa").write_text(
                f"load-module module-native-protocol-unix socket={pulse / 'native'} auth-anonymous=1\n"
                "load-module module-null-sink sink_name=speakers rate=48000\n"
                "load-module module-null-sink sink_name=microphone rate=48000\n"
                "set-default-sink speakers\nset-default-source microphone.monitor\n")
            spawn([str(args.pulse_bin / "pulseaudio"), "--daemonize=no", "--use-pid-file=no",
                   "--exit-idle-time=-1", "-n", f"--file={pulse / 'default.pa'}"],
                  shell.environment() | {"PULSE_RUNTIME_PATH": str(pulse), "PULSE_STATE_PATH": str(pulse / "state"),
                                         "DBUS_SESSION_BUS_ADDRESS": f"unix:path={root}/no-pulse-bus"},
                  "pulse.log")
            eventually(lambda: (pulse / "native").exists(), "private null audio missing")
            env = shell.app_environment() | {
                "RUST_LOG": "nuschl=info", "PIPEWIRE_REMOTE": str(root / "no-pipewire"),
                "PULSE_SERVER": f"unix:{pulse}/native", "NUSCHL_AUDIO_BACKEND": "pulseaudio",
                "PULSE_SINK": "speakers", "PULSE_SOURCE": "microphone.monitor"}
            env.pop("NO_AT_BRIDGE", None)
            pin = certificate_pin(args.binaries, "127.0.0.1", murmur.port, root / "probe")
            bot = Client(args.binaries, "127.0.0.1", murmur.port, pin, PEER, root / "bot")
            for zoom in (1.0, 2.0):
                desktop = Desktop(args, shell, registry, env, murmur, pin, zoom, root)
                try:
                    desktop.check(results)
                finally:
                    desktop.stop()
        finally:
            for process in helpers:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=10)
            if bot:
                bot.close()
            shell.stop()
            murmur.stop()
            (args.output / "results.json").write_text(json.dumps(results, indent=2) + "\n")
    failures = [result for result in results
                if result["spaced_text"] or any(gap > PADDING * result["zoom"]
                                                for gap in result["gap_below_last"].values())]
    print(json.dumps(results))
    assert len(results) == 4 and not failures, f"blank space below the last control or spaced text: {failures}"


if __name__ == "__main__":
    main()
