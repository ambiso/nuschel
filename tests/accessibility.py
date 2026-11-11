#!/usr/bin/env python3
"""Screen-reader view of the desktop client over AT-SPI (Linux).

Starts a private accessibility bus and registry inside the current D-Bus
session (or a private GNOME Shell with --gnome-shell), connects Nuschel to a
loopback stock Mumble server, then reads the accessibility tree exactly as
Orca would and checks that:

- the window, its title-bar controls, voice controls, channel tree, users,
  search and the message composer are exposed with roles and names;
- every interactive control has a name (no unlabeled buttons or fields);
- a control can be operated through AT-SPI alone: "Unmute" is invoked via the
  Action interface, the server sees the user unmuted, and the button becomes
  "Mute";
- with --gnome-shell (which has a clipboard): a pasted image shows in the
  composer's tray as a named list item, and its "Remove" button works through
  the Action interface;
- Settings' shortcut rows name each action's Record button, and recording is
  announced as a status that a screen reader reads out.

Uses busctl for D-Bus calls, so no Python bindings are needed.
"""
import argparse
from itertools import takewhile
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import uuid

sys.path.insert(0, str(Path(__file__).resolve().parent))
from local_murmur import Murmur, eventually, free_port  # noqa: E402
from mumble_client import Client, certificate_pin  # noqa: E402

# AT-SPI role numbers (atspi-constants.h).
ROLES = {23: "frame", 29: "label", 43: "push button", 54: "status bar", 61: "text", 62: "toggle button",
         65: "tree", 69: "window", 75: "application", 79: "entry", 91: "tree item", 27: "image",
         40: "password text", 16: "dialog", 33: "menu", 35: "menu item", 85: "section", 99: "grouping",
         31: "list", 32: "list item", 39: "panel"}
INTERACTIVE = {"push button", "toggle button", "text", "entry", "password text", "tree item", "menu item"}
# A 2×2 RGB PNG.
PNG = (b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR\0\0\0\x02\0\0\0\x02\x08\x02\0\0\0\xfd\xd4\x9as\0\0\0\x12IDATx"
       b"\x9cc\xf8\xcf\xc0\xc0\0\xc2\x0c\xff\x81\0\0\x1f\xee\x05\xfb\x0b\xd9h\x8b\0\0\0\0IEND\xaeB`\x82")


class Bus:
    def __init__(self, address, busctl):
        self.address = address
        self.busctl = busctl

    def __call__(self, *arguments):
        result = subprocess.run([self.busctl, f"--address={self.address}", "--json=short", *arguments],
                                capture_output=True, text=True, timeout=10)
        if result.returncode:
            raise RuntimeError(result.stderr.strip())
        return json.loads(result.stdout)["data"] if result.stdout.strip() else None


class Node:
    def __init__(self, bus, destination, path):
        self.bus, self.destination, self.path = bus, destination, path

    def call(self, interface, method, *arguments):
        return self.bus("call", self.destination, self.path, f"org.a11y.atspi.{interface}", method, *arguments)

    @property
    def name(self):
        return self.bus("get-property", self.destination, self.path, "org.a11y.atspi.Accessible", "Name")

    @property
    def role(self):
        number = self.call("Accessible", "GetRole")[0]
        return ROLES.get(number, f"role {number}")

    def children(self):
        return [Node(self.bus, destination, path) for destination, path in self.call("Accessible", "GetChildren")[0]]


def snapshot(application):
    """(depth, node, role, name) for the whole tree. Nodes that disappear while
    the UI updates are skipped together with their subtree."""
    entries = []

    def visit(node, depth):
        try:
            entries.append((depth, node, node.role, node.name))
            children = node.children() if depth < 60 else []
        except RuntimeError as error:
            if "Unknown object" in str(error):
                return
            raise
        for child in children:
            visit(child, depth + 1)
    visit(application, 0)
    return entries


class AccessibilityTest:
    def __init__(self, args, root):
        self.args, self.root = args, root
        self.processes = []
        self.shell = None

    def spawn(self, command, env, log):
        process = subprocess.Popen(command, env=env, stdout=(self.root / log).open("w"), stderr=subprocess.STDOUT,
                                   start_new_session=True)
        self.processes.append(process)
        return process

    def run(self):
        busctl = shutil.which("busctl") or "/run/current-system/sw/bin/busctl"
        if self.args.gnome_shell:
            from gnome_session import GnomeSession
            self.shell = GnomeSession(self.args.gnome_shell, 1440, 900, log=(self.root / "shell.log").open("w"))
            self.shell.start()
            env = self.shell.app_environment()
        else:
            env = dict(os.environ)
            assert env.get("DBUS_SESSION_BUS_ADDRESS"), "run inside a D-Bus session (dbus-run-session)"
        # NixOS sets NO_AT_BRIDGE=1 wherever its at-spi2-core service is off; a
        # screen-reader session never has it, and AccessKit honours it.
        env.pop("NO_AT_BRIDGE", None)
        # Login sessions always have a runtime directory (su does not create one);
        # the accessibility bus socket lives there.
        if not env.get("XDG_RUNTIME_DIR"):
            runtime = self.root / "runtime"
            runtime.mkdir(mode=0o700)
            env["XDG_RUNTIME_DIR"] = str(runtime)
        session = Bus(env["DBUS_SESSION_BUS_ADDRESS"], busctl)
        # The bus launcher owns org.a11y.Bus and starts the accessibility bus; the
        # registry tracks applications on it. Neither depends on D-Bus activation.
        # The launcher keeps IsEnabled in GSettings: the memory backend keeps it in
        # this process, so the test never writes the user's dconf database (and
        # still works where no dconf service exists, as in the VM).
        self.spawn([str(self.args.at_spi / "libexec/at-spi-bus-launcher")], env | {"GSETTINGS_BACKEND": "memory"},
                   "bus-launcher.log")
        eventually(lambda: self.try_call(session, "call", "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus",
                                         "GetAddress"), "the accessibility bus did not start", timeout=15)
        session("set-property", "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status", "IsEnabled", "b", "true")
        a11y = Bus(session("call", "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress")[0], busctl)
        self.spawn([str(self.args.at_spi / "libexec/at-spi2-registryd")], env, "registry.log")
        registry = Node(a11y, "org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root")
        eventually(lambda: self.try_call(registry.children), "the accessibility registry did not start", timeout=15)

        port = free_port()
        (self.root / "murmur").mkdir()
        murmur = Murmur(self.args.murmur, self.root / "murmur", port)
        murmur.seed()
        murmur.start()
        peer = None
        try:
            pin = certificate_pin(self.args.binaries, "127.0.0.1", port, self.root / "probe")
            peer = Client(self.args.binaries, "127.0.0.1", port, pin, "Ada Lovelace", self.root / "peer")
            peer.send(kind="join", channel=2)
            profile = self.root / "profile"
            (profile / "config").mkdir(parents=True)
            (profile / "config/servers.json").write_text(json.dumps([dict(
                id=str(uuid.uuid4()), name="Accessible server", host="127.0.0.1", port=port, username="Andi",
                certificate_sha256=pin, cached_channels=[])]))
            app = self.spawn([str(self.args.binaries / "nuschel"), "--profile-dir", str(profile),
                              f"mumble://Andi@127.0.0.1:{port}/Lobby/Development"],
                             env, "desktop.log")
            desktop = peer.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                                and e["user"]["channel"] == 2, timeout=40)["user"]

            def application():
                assert app.poll() is None, (self.root / "desktop.log").read_text()[-3000:]
                for node in registry.children():
                    if node.name.lower() == "nuschel":
                        return node
                return None
            try:
                nuschel = eventually(application, "Nuschel did not register with the accessibility registry",
                                     timeout=45)
            except AssertionError as error:
                names = [node.name for node in registry.children()]
                logs = "\n".join(f"--- {name}\n" + (self.root / name).read_text()[-1500:]
                                  for name in ("desktop.log", "bus-launcher.log", "registry.log")
                                  if (self.root / name).exists())
                variables = {key: value for key, value in env.items() if "BUS" in key or "DISPLAY" in key
                             or key.startswith(("AT_SPI", "NO_AT_BRIDGE", "XDG_RUNTIME"))}
                raise AssertionError(f"{error}; registered applications: {names}; accessibility bus {a11y.address}; "
                                     f"environment: {variables}\n{logs}")

            def ready():
                nodes = snapshot(nuschel)
                names = {name for _, _, _, name in nodes}
                return nodes if "Ada Lovelace" in " ".join(names) and "Send" in names else None
            nodes = eventually(ready, "the accessibility tree did not fill in", timeout=30)
            self.write_tree(nodes, "tree.txt")
            self.check_tree(nodes)
            print(f"Accessibility tree: {len(nodes)} nodes; window controls, voice controls, channel tree, "
                  "users, search and composer exposed with names.", flush=True)

            assert desktop["self_muted"], "the client should start muted"
            unmute = self.find(nodes, "push button", "Unmute")
            actions = unmute.call("Action", "GetActions")[0]
            assert actions, "the Unmute button offers no action"
            assert unmute.call("Action", "DoAction", "i", "0")[0] is not False
            peer.wait(lambda e: e["kind"] == "user" and e["user"]["session"] == desktop["session"]
                      and not e["user"]["self_muted"], timeout=15)
            eventually(lambda: any(role == "push button" and name == "Mute"
                                   for _, _, role, name in snapshot(nuschel)),
                       "the button did not change to Mute", timeout=10)
            print("Unmuted through the AT-SPI Action interface; the server saw it and the button became Mute.",
                  flush=True)
            self.check_shortcuts(nuschel)
            if self.shell:
                self.check_attachments(nuschel)
        finally:
            if peer:
                peer.close()
            murmur.stop()

    def check_attachments(self, nuschel):
        """Paste an image into the composer; remove it again through AT-SPI."""
        image = self.root / "pasted.png"
        image.write_bytes(PNG)
        self.shell.eval("const [, data] = imports.gi.GLib.file_get_contents(%s);"
                        " imports.gi.St.Clipboard.get_default().set_content(imports.gi.St.ClipboardType.CLIPBOARD,"
                        " 'image/png', new imports.gi.GLib.Bytes(data)); 'ok'" % json.dumps(str(image)))
        nodes = snapshot(nuschel)
        composer = next(node for _, node, role, name in nodes if role in ("text", "entry") and name == "Message")
        composer.call("Component", "GrabFocus")
        time.sleep(0.5)
        self.shell.chord("Control_L", "v")

        def attached():
            nodes = snapshot(nuschel)
            return nodes if any(role == "list item" and name == "Image Pasted image 1"
                                for _, _, role, name in nodes) else None
        nodes = eventually(attached, "the pasted image is not in the composer's tray", timeout=30)
        self.write_tree(nodes, "tree-attached.txt")
        self.find(nodes, "list", "1 attachment")
        unnamed = [role for _, _, role, name in nodes if role in INTERACTIVE and not name.strip()]
        assert not unnamed, f"interactive elements without a name: {unnamed}"
        remove = self.find(nodes, "push button", "Remove Pasted image 1")
        assert remove.call("Action", "DoAction", "i", "0")[0] is not False
        eventually(lambda: not any(name == "Remove Pasted image 1" for _, _, _, name in snapshot(nuschel)),
                   "the image stayed after Remove", timeout=10)
        print("A pasted image showed in the tray as a named item and was removed through AT-SPI.", flush=True)

    def check_shortcuts(self, nuschel):
        self.find(snapshot(nuschel), "push button", "Settings").call("Action", "DoAction", "i", "0")
        actions = ("Push to talk", "Toggle mute", "Toggle deafen", "Push to mute")

        def rows():
            nodes = snapshot(nuschel)
            names = {name for _, _, _, name in nodes}
            return nodes if all(f"Record {action} shortcut" in names for action in actions) else None
        nodes = eventually(rows, "the shortcut rows did not appear in Settings", timeout=15)
        self.write_tree(nodes, "settings-tree.txt")
        for action in actions:
            assert any(name == f"{action} shortcut" for _, _, _, name in nodes), f"no group for {action}"
        self.find(nodes, "push button", "Record Toggle mute shortcut").call("Action", "DoAction", "i", "0")

        def recording():
            nodes = snapshot(nuschel)
            cancel = [node for _, node, role, name in nodes
                      if role == "push button" and name == "Cancel recording Toggle mute shortcut"]
            # The prompt is a label inside a status, which a screen reader announces.
            announced = any(role == "status bar" and any(
                inner_depth > depth and name.startswith("Press the key")
                for inner_depth, _, _, name in takewhile(lambda entry: entry[0] > depth, nodes[index + 1:]))
                for index, (depth, _, role, _) in enumerate(nodes))
            return cancel[0] if cancel and announced else None
        cancel = eventually(recording, "recording a shortcut was not announced", timeout=10)
        cancel.call("Action", "DoAction", "i", "0")
        eventually(lambda: any(name == "Record Toggle mute shortcut" for _, _, _, name in snapshot(nuschel)),
                   "cancelling did not restore the Record button", timeout=10)
        self.find(snapshot(nuschel), "push button", "Done").call("Action", "DoAction", "i", "0")
        eventually(lambda: not any(name == "Done" for _, _, _, name in snapshot(nuschel)),
                   "Settings did not close", timeout=10)
        print("Settings: every shortcut row has a named Record button; recording is announced as a status.",
              flush=True)

    @staticmethod
    def try_call(function, *arguments):
        try:
            return function(*arguments) or True
        except RuntimeError:
            return None

    def write_tree(self, nodes, name):
        lines = ["  " * min(depth, 30) + f"{role}: {label!r}" for depth, _, role, label in nodes]
        (self.args.output / name).write_text("\n".join(lines) + "\n")

    @staticmethod
    def find(nodes, role, name):
        for _, node, node_role, node_name in nodes:
            if node_role == role and node_name == name:
                return node
        raise AssertionError(f"no {role} named {name!r}; have: "
                             f"{sorted({(r, n) for _, _, r, n in nodes if r != 'label'})}")

    def check_tree(self, nodes):
        assert any(role == "frame" and name == "Nuschel" for _, _, role, name in nodes), "no Nuschel frame"
        if self.args.gnome_shell:
            # GNOME has no server-side decorations, so Nuschel draws (and must name) its own.
            for button in ("Minimize", "Close"):
                self.find(nodes, "push button", button)
            assert any(role == "push button" and name in ("Maximize", "Restore") for _, _, role, name in nodes)
        for button in ("Add a server", "Disconnect", "Settings", "Unmute", "Deafen",
                       "Search local history", "Send"):
            self.find(nodes, "push button", button)
        self.find(nodes, "tree", "Channels")
        for item in ("Lobby", "Development"):
            self.find(nodes, "tree item", item)
        users = [name for _, _, role, name in nodes if role == "tree item" and name.startswith(("Ada Lovelace", "Andi"))]
        assert len(users) == 2, f"users missing from the channel tree: {users}"
        composer = [name for _, _, role, name in nodes if role in ("text", "entry")]
        assert "Message" in composer, f"the message composer is not exposed (text fields: {composer})"
        unnamed = [role for _, _, role, name in nodes if role in INTERACTIVE and not name.strip()]
        assert not unnamed, f"interactive elements without a name: {unnamed}"

    def close(self):
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
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--murmur", type=Path, required=True)
    parser.add_argument("--at-spi", type=Path, required=True, help="at-spi2-core package (bus launcher, registry)")
    parser.add_argument("--gnome-shell", type=Path, help="run inside a private headless GNOME Shell")
    parser.add_argument("--output", type=Path, default=Path(tempfile.gettempdir()))
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschel-a11y-") as temporary:
        test = AccessibilityTest(args, Path(temporary))
        try:
            test.run()
        finally:
            test.close()


if __name__ == "__main__":
    main()
