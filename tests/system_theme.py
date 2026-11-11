#!/usr/bin/env python3
"""Nuschel's System theme follows the desktop's light or dark preference live.

Development aid, like screenshots.py: a private headless GNOME Shell and a
fake XDG settings portal (org.freedesktop.portal.Settings, color-scheme only)
on its private session bus switch from dark to light and back while Nuschel
runs; the window's background is checked after each switch. The portal needs
dbus-next:

    nix build --impure --expr 'with (builtins.getFlake "nixpkgs").legacyPackages.x86_64-linux;
        python3.withPackages (ps: [ ps.dbus-next ])'
    nix shell nixpkgs#imagemagick -c python3 tests/system_theme.py target/debug \\
        --portal-python result/bin/python3 --output /tmp/system-theme
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time

SCHEMES = {"dark": 1, "light": 2}


def serve_portal(scheme):
    import asyncio
    from dbus_next import DBusError, Variant
    from dbus_next.aio import MessageBus
    from dbus_next.service import PropertyAccess, ServiceInterface, dbus_property, method, signal

    class Settings(ServiceInterface):
        def __init__(self):
            super().__init__("org.freedesktop.portal.Settings")
            self.scheme = SCHEMES[scheme]

        def value(self, namespace, key):
            if (namespace, key) == ("org.freedesktop.appearance", "color-scheme"):
                return Variant("u", self.scheme)
            raise DBusError("org.freedesktop.portal.Error.NotFound", "no such setting")

        @method()
        def Read(self, namespace: "s", key: "s") -> "v":  # noqa: F821 - D-Bus signatures
            return Variant("v", self.value(namespace, key))

        @method()
        def ReadOne(self, namespace: "s", key: "s") -> "v":  # noqa: F821
            return self.value(namespace, key)

        @method()
        def ReadAll(self, namespaces: "as") -> "a{sa{sv}}":  # noqa: F821
            return {"org.freedesktop.appearance": {"color-scheme": Variant("u", self.scheme)}}

        @signal()
        def SettingChanged(self, namespace, key, value) -> "ssv":  # noqa: F821
            return [namespace, key, value]

        @dbus_property(access=PropertyAccess.READ)
        def version(self) -> "u":  # noqa: F821
            return 2

    async def run():
        bus = await MessageBus().connect()
        settings = Settings()
        bus.export("/org/freedesktop/portal/desktop", settings)
        await bus.request_name("org.freedesktop.portal.Desktop")
        print("ready", flush=True)
        loop = asyncio.get_running_loop()
        while line := (await loop.run_in_executor(None, sys.stdin.readline)).strip():
            settings.scheme = SCHEMES[line]
            settings.SettingChanged("org.freedesktop.appearance", "color-scheme",
                                    Variant("u", settings.scheme))
            print("changed", flush=True)

    asyncio.run(run())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path, nargs="?")
    parser.add_argument("--portal-python", type=Path)
    parser.add_argument("--gnome-shell", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--serve-portal", choices=SCHEMES)
    args = parser.parse_args()
    if args.serve_portal:
        return serve_portal(args.serve_portal)

    sys.path.insert(0, str(Path(__file__).resolve().parent))
    from gnome_session import GnomeSession
    from local_murmur import eventually

    output = args.output or Path(tempfile.mkdtemp(prefix="nuschel-system-theme-"))
    output.mkdir(parents=True, exist_ok=True)
    shell = GnomeSession(args.gnome_shell, 1100, 760, log=(output / "gnome-shell.log").open("w"))
    app = portal = None
    try:
        shell.start()
        env = shell.app_environment()
        portal = subprocess.Popen([str(args.portal_python), str(Path(__file__).resolve()), "--serve-portal", "dark"],
                                  env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
                                  stderr=(output / "portal.log").open("w"))
        assert portal.stdout.readline().strip() == "ready", "the fake portal did not start"
        profile = Path(tempfile.mkdtemp(prefix="nuschel-system-theme-profile-"))
        (profile / "config").mkdir()
        (profile / "config/settings.json").write_text(json.dumps({"appearance": {"theme": "system"}}))
        app = subprocess.Popen([str(args.binaries / "nuschel"), "--profile-dir", str(profile)], env=env,
                               stdout=(output / "desktop.log").open("w"), stderr=subprocess.STDOUT)
        eventually(lambda: shell.window("Nuschel"), "Nuschel window did not appear")
        shell.place("Nuschel", 40, 40, 960, 640)
        time.sleep(3)

        def background(name):
            window = shell.window("Nuschel")
            path = shell.screenshot(output / f"{name}.png",
                                    crop=(window["x"], window["y"], window["width"], window["height"]))
            print(path, flush=True)
            # The welcome page's background, right of the server rail.
            pixel = subprocess.run(["magick", str(path), "-crop", "1x1+600+500", "-format", "%[fx:int(255*luminance)]",
                                    "info:"], capture_output=True, text=True, check=True).stdout
            return int(pixel)

        assert background("system-dark") < 64, "the System theme did not start dark"
        for scheme in ("light", "dark"):
            portal.stdin.write(scheme + "\n")
            portal.stdin.flush()
            assert portal.stdout.readline().strip() == "changed"
            time.sleep(1.5)
            luminance = background(f"system-then-{scheme}")
            assert (luminance > 192) == (scheme == "light"), f"{scheme}: background luminance {luminance}"
        print("System theme followed the portal: dark, light, dark")
    finally:
        for process in (app, portal):
            if process and process.poll() is None:
                process.terminate()
                process.wait(timeout=8)
        shell.stop()


if __name__ == "__main__":
    main()
