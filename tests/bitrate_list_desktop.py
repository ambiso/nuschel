#!/usr/bin/env python3
"""Settings names only the servers below the requested voice bitrate.

The real desktop in a private headless GNOME Shell, operated over AT-SPI,
with nine loopback stock Mumble servers at different bandwidth limits and
one offline bookmark. A private PulseAudio is the only audio service; no
host audio device, profile or recording is used. Checks and screenshots:

- a configured rate none of the choices shows reads "Custom: 80 kb/s";
- Settings → VOICE QUALITY names the first five servers below the request,
  in server order, then "+2 more in Connection info", then one line for the
  two servers at the request; nothing for the offline bookmark;
- choosing 128 kb/s removes the custom line and keeps the list;
- Connection info shows the voice bitrate of a limited server, of one too
  small for voice and of one at the request, and none for the offline one.

    python3 tests/bitrate_list_desktop.py <binaries> --murmur <mumble-server> \\
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
from accessibility import start_registry  # noqa: E402
from bitrate_desktop import HEIGHT, WIDTH, DesktopBitrate  # noqa: E402
from gnome_session import GnomeSession  # noqa: E402
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import certificate_pin  # noqa: E402

# (name, per-user bandwidth; None: Murmur's own 558 kb/s), in rail order.
SERVERS = [("Wide", None), ("Stock", 72_000), ("Club", 64_000), ("Big", None), ("Tiny", 20_000),
           ("Guild", 56_000), ("Lan", 48_000), ("Old", 40_000), ("Slow", 32_000)]
BELOW = ["Stock: 48 kb/s (server limit)", "Club: 40 kb/s (server limit)", "Tiny: not sending (server limit)",
         "Guild: 32 kb/s (server limit)", "Lan: 24 kb/s (server limit)"]
UNLISTED = ["Old: 16 kb/s (server limit)", "Slow: 8 kb/s (server limit)"]
MORE = "+2 more in Connection info"


class BitrateList(DesktopBitrate):
    def lines(self):
        """Every accessible name in the Settings voice quality lines' forms."""
        return [name for _, _, role, name in self.nodes()
                if role != "radio button" and ("kb/s" in name or name.startswith("+"))]

    def settings(self, rate, custom):
        expected = BELOW + [MORE, f"2 other servers: {rate}"]
        self.find("radio button", "128 kb/s").call("Component", "ScrollTo", "u", "0")
        for text in expected:
            self.named(text, timeout=60)
        names = self.lines()
        for text in UNLISTED + [f"Wide: {rate}", f"Big: {rate}"]:
            assert text not in names, (text, names)
        assert not any(name.startswith("Offline") for name in names), names
        assert ("Custom: 80 kb/s" in names) == custom, names
        # Server order, the list before its overflow and summary lines.
        shown = [name for name in names if name in expected]
        assert shown == expected, shown
        return names

    def point(self, node):
        bx, by, _, _ = self.rects()["buffer"]
        x, y, width, height = node.extents()
        return bx + x + width // 2, by + y + height // 2

    def connection(self, server, value):
        """Open `server`'s Connection info; its Voice bitrate value (None: no row)."""
        self.select(server)
        self.shell.click(*self.point(self.find("push button", "Server options")))
        self.press("menu item", "Connection info")
        self.named(f"127.0.0.1:{self.ports[server]}")
        self.named("Certificate (SHA-256)")
        time.sleep(0.8)
        names = [name for _, _, _, name in self.nodes() if name]
        found = names[names.index("Voice bitrate") + 1] if "Voice bitrate" in names else None
        assert found == value, (server, found, value)
        self.results.append(dict(server=server, voice_bitrate=found))
        print(f"  Connection info {server:<8} Voice bitrate: {found}", flush=True)
        self.shot(f"connection-{server.lower()}")
        self.press("push button", "Done")
        eventually(lambda: "Certificate (SHA-256)" not in [n for _, _, _, n in self.nodes()],
                   "Connection info did not close")

    def run(self):
        self.shell = GnomeSession(self.args.gnome_shell, WIDTH + 160, HEIGHT + 100,
                                  log=(self.args.output / "gnome-shell.log").open("w"))
        self.shell.start()
        env = self.shell.app_environment()
        env.pop("NO_AT_BRIDGE", None)
        registry = start_registry(self.args.at_spi, env, self.spawn)
        self.start_pulse()
        self.ports, bookmarks = {}, []
        for name, bandwidth in SERVERS:
            root = self.root / name
            root.mkdir()
            port = free_port()
            murmur = Murmur(self.args.murmur, root, port)
            murmur.ini.write_text(murmur.ini.read_text().replace(
                "bandwidth=72000\n", f"bandwidth={bandwidth}\n" if bandwidth else ""))
            murmur.seed()
            murmur.start()
            self.servers[name] = murmur
            self.ports[name] = port
            pin = certificate_pin(self.args.binaries, "127.0.0.1", port, root / "probe")
            bookmarks.append(dict(id=str(uuid.uuid4()), name=name, host="127.0.0.1", port=port, username="Andi",
                                  certificate_sha256=pin, auto_connect=True, last_channel=1))
        # Nothing listens here: a bookmark that never connects.
        self.ports["Offline"] = free_port()
        bookmarks.append(dict(id=str(uuid.uuid4()), name="Offline", host="127.0.0.1", port=self.ports["Offline"],
                              username="Andi", auto_connect=False))
        self.profile = self.root / "profile"
        (self.profile / "config").mkdir(parents=True)
        (self.profile / "config/servers.json").write_text(json.dumps(bookmarks))
        (self.profile / "config/settings.json").write_text(json.dumps({
            "server_icons": False, "audio": {"echo_cancellation": "off"}, "appearance": {"theme": "dark"}}))
        (self.profile / "config/voice-quality.json").write_text(json.dumps({"version": 1, "voice_bitrate": 80_000}))
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

        self.press("push button", "Settings")
        self.find("dialog", "Settings")
        names = self.settings("80 kb/s", custom=True)
        self.results.append(dict(step="settings at a custom 80 kb/s", lines=names))
        self.shot("settings-custom-80")
        self.press("radio button", "128 kb/s")
        eventually(lambda: self.files()[1] == {"version": 1, "voice_bitrate": 128_000},
                   "voice-quality.json was not updated to 128 kb/s")
        names = self.settings("128 kb/s", custom=False)
        self.results.append(dict(step="settings at 128 kb/s", lines=names))
        self.shot("settings-128")
        print("  Settings:", *names, sep="\n    ", flush=True)
        self.shell.key("Escape")
        eventually(lambda: not any(role == "dialog" and name == "Settings" for _, _, role, name in self.nodes()),
                   "Settings did not close")

        self.connection("Stock", "48 of 128 kb/s (server limit)")
        self.connection("Tiny", "Not sending (server limit)")
        self.connection("Wide", "128 kb/s")
        self.connection("Slow", "8 of 128 kb/s (server limit)")
        self.connection("Offline", None)
        (self.args.output / "bitrate-list.json").write_text(json.dumps(self.results, indent=2) + "\n")
        print("Voice bitrate list: ok", flush=True)

    def close(self):
        for murmur in self.servers.values():
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
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output = args.output.resolve()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschl-bitrate-list-", dir=args.output) as temporary:
        test = BitrateList(args, Path(temporary))
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
