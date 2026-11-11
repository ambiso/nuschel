#!/usr/bin/env python3
"""Managed configuration as a user and a screen reader meet it (Home Manager VM).

Starts Nuschl with the user's own profile, where managed.json (written by
programs.nuschl.settings and .servers) sets "enter_sends": false, F13 for push
to talk and a server named "Managed", while settings.json says "enter_sends":
true. A private accessibility bus and registry are started as in
tests/accessibility.py, and the checks are that:

- Settings shows both settings with their managed values, as set by the system
  configuration, and clicking the shortcut's Clear changes nothing;
- another setting still changes, and settings.json keeps the user's own value
  under the managed keys;
- the managed server is listed, and its form offers no removal.

AT-SPI clicks land at a control's centre like a pointer's, so only controls in
view are clicked.

Run inside a D-Bus session with a display: dbus-run-session -- xvfb-run ...
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

sys.path.insert(0, str(Path(__file__).resolve().parent))
from accessibility import ROLES, AccessibilityTest, Bus, Node, snapshot  # noqa: E402
from local_murmur import eventually  # noqa: E402

ROLES.setdefault(44, "radio button")

NOTE = "Set by your system configuration"
SERVER_NOTE = "Set by your system configuration: only the password can be entered here."
# The detail "Enter sends messages" shows while it is off.
NEWLINE_DETAIL = "Enter starts a new line; Ctrl/Cmd+Enter sends."
find = AccessibilityTest.find


def press(node):
    assert node.call("Action", "DoAction", "i", "0")[0] is not False


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("nuschl", type=Path, help="the nuschl executable")
    parser.add_argument("--at-spi", type=Path, required=True, help="at-spi2-core package (bus launcher, registry)")
    parser.add_argument("--config", type=Path, required=True, help="Nuschl's configuration folder")
    args = parser.parse_args()
    root = Path(tempfile.mkdtemp(prefix="nuschl-managed-"))
    processes = []

    def spawn(command, env, log):
        process = subprocess.Popen(command, env=env, stdout=(root / log).open("w"), stderr=subprocess.STDOUT,
                                   start_new_session=True)
        processes.append(process)
        return process

    try:
        env = dict(os.environ)
        env.pop("NO_AT_BRIDGE", None)
        if not env.get("XDG_RUNTIME_DIR"):
            (root / "runtime").mkdir(mode=0o700)
            env["XDG_RUNTIME_DIR"] = str(root / "runtime")
        busctl = shutil.which("busctl") or "/run/current-system/sw/bin/busctl"
        session = Bus(env["DBUS_SESSION_BUS_ADDRESS"], busctl)
        spawn([str(args.at_spi / "libexec/at-spi-bus-launcher")], env | {"GSETTINGS_BACKEND": "memory"},
              "bus-launcher.log")
        eventually(lambda: AccessibilityTest.try_call(session, "call", "org.a11y.Bus", "/org/a11y/bus",
                                                      "org.a11y.Bus", "GetAddress"),
                   "the accessibility bus did not start", timeout=15)
        session("set-property", "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status", "IsEnabled", "b", "true")
        a11y = Bus(session("call", "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress")[0], busctl)
        spawn([str(args.at_spi / "libexec/at-spi2-registryd")], env, "registry.log")
        registry = Node(a11y, "org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root")
        eventually(lambda: AccessibilityTest.try_call(registry.children), "the accessibility registry did not start",
                   timeout=15)

        settings_file = args.config / "settings.json"
        assert json.loads(settings_file.read_text())["enter_sends"] is True, "expected settings.json's own value"
        app = spawn([str(args.nuschl)], env, "desktop.log")

        def application():
            assert app.poll() is None, (root / "desktop.log").read_text()[-3000:]
            return next((node for node in registry.children() if node.name.lower() == "nuschl"), None)
        nuschl = eventually(application, "Nuschl did not register with the accessibility registry", timeout=60)

        def node(role, name):
            return next((node for _, node, r, n in snapshot(nuschl) if r == role and n == name), None)
        press(eventually(lambda: node("push button", "Settings"), "no Settings button", timeout=60))

        def managed(role, name):
            """The control in a group marked as set by the system, and the group's text."""
            nodes = snapshot(nuschl)
            for index, (depth, _, group_role, group_name) in enumerate(nodes):
                if group_name == NOTE and group_role != "label":
                    below = list(takewhile(lambda entry: entry[0] > depth, nodes[index + 1:]))
                    control = [node for _, node, r, n in below if r == role and n == name]
                    if control:
                        return control[0], {n for _, _, _, n in below}
            return None
        _, text = eventually(lambda: managed("toggle button", "Enter sends messages"),
                             "Enter sends messages is not shown as set by the system", timeout=20)
        assert NEWLINE_DETAIL in text and NOTE in text, text
        clear, text = eventually(lambda: managed("push button", "Clear Push to talk shortcut"),
                                 "the push-to-talk shortcut is not shown as set by the system", timeout=10)
        assert "F13" in text, text
        press(clear)
        time.sleep(1.5)
        _, text = eventually(lambda: managed("push button", "Clear Push to talk shortcut"),
                             "the managed shortcut row disappeared", timeout=10)
        assert "F13" in text, f"clicking Clear changed the managed shortcut: {text}"
        print("Settings: Enter sends messages and the push-to-talk key show as set by the system "
              "configuration, and clicking Clear changes nothing.", flush=True)

        press(find(snapshot(nuschl), "radio button", "Push to talk"))

        def saved():
            settings = json.loads(settings_file.read_text())
            return settings if settings.get("audio", {}).get("transmit") == "push_to_talk" else None
        settings = eventually(saved, "another setting was not saved", timeout=10)
        assert settings["enter_sends"] is True, f"settings.json lost its own value: {settings}"
        assert "push-to-talk" not in settings.get("shortcuts", {}), f"a managed key was saved: {settings}"
        print("Another setting changed and was saved; settings.json kept its own values under managed keys.",
              flush=True)
        press(find(snapshot(nuschl), "push button", "Done"))

        server = eventually(lambda: next((node for _, node, role, name in snapshot(nuschl)
                                          if role == "push button" and name.startswith("Managed;")), None),
                            "the managed server is not listed", timeout=10)
        press(server)
        eventually(lambda: node("label", "Managed"), "the managed server was not selected", timeout=10)
        press(find(snapshot(nuschl), "push button", "Server settings"))
        names = eventually(lambda: (lambda names: names if SERVER_NOTE in names else None)(
            {name for _, _, _, name in snapshot(nuschl)}), "the managed server's form has no note", timeout=10)
        assert "Remove saved server" not in names, "a managed server can be removed"
        press(find(snapshot(nuschl), "push button", "Cancel"))
        print("The managed server is listed; its form says so and offers no removal.", flush=True)
    except BaseException:
        for log in ("desktop.log", "bus-launcher.log", "registry.log"):
            if (root / log).exists():
                print(f"--- {log}\n" + (root / log).read_text()[-3000:], file=sys.stderr)
        raise
    finally:
        for process in reversed(processes):
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)


if __name__ == "__main__":
    main()
