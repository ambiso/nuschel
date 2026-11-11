#!/usr/bin/env python3
"""Relay-only setting: AT-SPI, persistence, server reconnect and worker revocation.

Private GNOME, Murmur and wormhole mailbox/relay only. The direct-capable offer
waits for pickup; no peer joins it before privacy revokes it, so no STUN runs.
"""
import argparse
import json
import os
from pathlib import Path
import tempfile
import uuid

from accessibility import snapshot, start_registry
from desktop_recovery import worker_pids
from gnome_session import GnomeSession
from local_murmur import Murmur, eventually, free_port
from mumble_client import Client, certificate_pin
from transfer import Wormhole
from voice_servers import VoiceTest, WIDTH, HEIGHT


class PrivacyTest(VoiceTest):
    def run(self):
        self.shell = GnomeSession(self.args.gnome_shell, WIDTH + 160, HEIGHT + 100,
                                  log=(self.args.output / "shell.log").open("w"))
        self.shell.start()
        env = self.shell.app_environment()
        env.pop("NO_AT_BRIDGE", None)
        # Overrides force relay mode independently of the setting, so this test
        # deliberately configures the private servers through settings.json.
        env.pop("NUSCHL_WORMHOLE_RENDEZVOUS", None)
        env.pop("NUSCHL_WORMHOLE_RELAY", None)
        registry = start_registry(self.args.at_spi, env, self.spawn)
        self.start_pulse()
        wormhole = Wormhole(self.args.python, self.root)
        try:
            directory = self.root / "murmur"
            directory.mkdir()
            murmur = Murmur(self.args.murmur, directory, free_port())
            self.murmurs.append(murmur)
            murmur.seed()
            murmur.start()
            pin = certificate_pin(self.args.binaries, "127.0.0.1", murmur.port, self.root / "probe")
            peer = Client(self.args.binaries, "127.0.0.1", murmur.port, pin, "PrivacyPeer", self.root / "peer")
            self.peers["Privacy"] = peer
            peer.send(kind="join", channel=1)
            peer.wait(lambda event: event["kind"] == "user" and event["user"]["session"] == peer.session
                      and event["user"]["channel"] == 1)
            profile = self.root / "profile"
            (profile / "config").mkdir(parents=True)
            (profile / "config/servers.json").write_text(json.dumps([{
                "id": str(uuid.uuid4()), "name": "Privacy", "host": "127.0.0.1", "port": murmur.port,
                "username": "Andi", "certificate_sha256": pin,
            }]))
            settings_path = profile / "config/settings.json"
            settings_path.write_text(json.dumps({
                "server_icons": False, "relay_only": False,
                "wormhole_rendezvous": wormhole.servers["rendezvous"],
                "wormhole_relay": wormhole.servers["relay"],
            }))
            offered = self.root / "private.txt"
            offered.write_text("A local privacy fixture.\n")
            app = self.app = self.spawn([
                str(self.args.binaries / "nuschl"), "--profile-dir", str(profile),
                "--offer", str(offered), f"mumble://Andi@127.0.0.1:{murmur.port}/Lobby",
            ], env | {"NUSCHL_AUDIO_BACKEND": "pulse"}, "desktop.log")
            eventually(lambda: app.poll() is None and self.shell.window("Nuschl"), "no desktop window")
            self.shell.place("Nuschl", 40, 40, WIDTH, HEIGHT)
            self.nuschl = eventually(lambda: next((node for node in registry.children()
                                                   if node.name.lower() == "nuschl"), None),
                                      "no desktop accessibility tree", timeout=45)
            joined = peer.wait(lambda event: event["kind"] == "user" and event["user"]["name"] == "Andi",
                               timeout=45)["user"]
            peer.wait(lambda event: event["kind"] == "text" and "wormhole" in event["body"], timeout=45)
            active = eventually(lambda: worker_pids(app, "nuschl-transfer-worker"), "no active offer worker")
            self.press("push button", "Settings")
            self.shell.wheel(680, 420, 100)
            control = eventually(lambda: next((node for _, node, role, name in snapshot(self.nuschl)
                                               if name == "Relay-only connections"
                                               and role in {"toggle button", "switch", "check box"}), None),
                                 "relay-only privacy is not a named accessible toggle")
            control.press()
            eventually(lambda: json.loads(settings_path.read_text()).get("relay_only") is True,
                       "privacy toggle was not persisted")
            eventually(lambda: all(not Path(f"/proc/{pid}").exists() for pid in active),
                       "privacy toggle left a direct-capable worker process alive", timeout=15)
            peer.wait(lambda event: event["kind"] == "user_removed" and event["session"] == joined["session"],
                      timeout=15)
            peer.wait(lambda event: event["kind"] == "user" and event["user"]["name"] == "Andi",
                      timeout=30)
            self.shot("relay-only")
            self.press("push button", "Done")
            # A fresh desktop reads the persisted opt-in and never starts the
            # revoked offer again. Managed precedence is covered by config tests.
            import signal
            os.killpg(app.pid, signal.SIGTERM)
            app.wait(timeout=15)
            app = self.app = self.spawn([
                str(self.args.binaries / "nuschl"), "--profile-dir", str(profile),
            ], env | {"NUSCHL_AUDIO_BACKEND": "pulse"}, "desktop-restarted.log")
            eventually(lambda: app.poll() is None and self.shell.window("Nuschl"), "desktop restart failed")
            self.nuschl = eventually(lambda: next((node for node in registry.children()
                                                   if node.name.lower() == "nuschl"), None),
                                      "restarted accessibility tree missing", timeout=30)
            self.press("push button", "Settings")
            control = eventually(lambda: next((node for _, node, _, name in snapshot(self.nuschl)
                                               if name == "Relay-only connections"), None),
                                 "privacy toggle missing after restart")
            assert control.call("Accessible", "GetState")[0][0] & (1 << 23), "privacy was not selected after reload"
            assert not worker_pids(app, "nuschl-transfer-worker"), "revoked transfer restarted"
            print("AT-SPI relay-only toggle persisted/reloaded, reconnected the server and killed the actual direct-capable worker.", flush=True)
        finally:
            wormhole.stop()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    for name in ("murmur", "at-spi", "pulse-bin", "opus-library", "gnome-shell", "python", "output"):
        parser.add_argument("--" + name, type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschl-relay-privacy-") as temporary:
        test = PrivacyTest(args, Path(temporary))
        try:
            test.run()
        finally:
            test.close()


if __name__ == "__main__":
    main()
