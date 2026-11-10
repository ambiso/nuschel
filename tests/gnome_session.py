#!/usr/bin/env python3
"""A private, headless GNOME Shell for real-GPU Wayland UI tests.

GNOME draws no server-side decorations, so this exercises Nuschel's own title
bar, controls and resize edges exactly as on a GNOME desktop. The shell runs
on its own Wayland socket and D-Bus session in unsafe mode, which permits
screenshots and Clutter virtual input via `org.gnome.Shell.Eval`. Nothing is
shown on, or read from, the user's desktop session.
"""
import ast
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import tempfile
import time


def find_gnome_shell():
    found = shutil.which("gnome-shell")
    if found:
        return Path(found)
    for candidate in sorted(Path("/nix/store").glob("*-gnome-shell-[0-9]*/bin/gnome-shell"), reverse=True):
        return candidate
    raise FileNotFoundError("gnome-shell not found; pass --gnome-shell")


class GnomeSession:
    DISPLAY = "nuschel-wl"

    def __init__(self, shell=None, width=1440, height=900, log=None):
        self.shell = Path(shell) if shell else find_gnome_shell()
        self.width, self.height = width, height
        # Wayland socket paths are limited to 108 bytes, so keep this short.
        self.runtime = Path(tempfile.mkdtemp(prefix="nwl-", dir=os.environ.get("NUSCHEL_WL_TMP", "/tmp")))
        self.runtime.chmod(0o700)
        self.home = Path(tempfile.mkdtemp(prefix="nuschel-gnome-home-"))
        self.log = log
        self.process = None
        self.bus = None
        self.pointer_ready = False

    def environment(self):
        env = {key: value for key, value in os.environ.items()
               if key not in {"WAYLAND_DISPLAY", "DISPLAY", "DBUS_SESSION_BUS_ADDRESS"}}
        env.update(XDG_RUNTIME_DIR=str(self.runtime), XDG_CONFIG_HOME=str(self.home / "config"),
                   XDG_DATA_HOME=str(self.home / "data"), XDG_CACHE_HOME=str(self.home / "cache"),
                   XDG_STATE_HOME=str(self.home / "state"))
        return env

    def app_environment(self):
        env = self.environment()
        env.update(WAYLAND_DISPLAY=self.DISPLAY, DBUS_SESSION_BUS_ADDRESS=self.bus,
                   XDG_SESSION_TYPE="wayland", XDG_CURRENT_DESKTOP="GNOME")
        return env

    def start(self):
        bus_file = self.runtime / "bus-address"
        command = (f"echo \"$DBUS_SESSION_BUS_ADDRESS\" > {bus_file}; exec {self.shell} --headless --wayland "
                   f"--no-x11 --virtual-monitor {self.width}x{self.height} --wayland-display {self.DISPLAY} --unsafe-mode")
        self.process = subprocess.Popen(["dbus-run-session", "--", "bash", "-c", command], env=self.environment(),
                                        stdout=self.log or subprocess.DEVNULL, stderr=subprocess.STDOUT,
                                        start_new_session=True)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                raise RuntimeError("gnome-shell exited during startup")
            if bus_file.exists() and (self.runtime / self.DISPLAY).exists():
                self.bus = bus_file.read_text().strip()
                try:
                    if self.eval("Main.layoutManager._startingUp") == "false":
                        break
                except subprocess.CalledProcessError:
                    pass
            time.sleep(0.3)
        else:
            raise RuntimeError("gnome-shell did not become ready")
        self.eval("new imports.gi.Gio.Settings({schema_id: 'org.gnome.desktop.interface'})"
                  ".set_boolean('enable-hot-corners', false); Main.overview.hide();"
                  " Main.messageTray._bannerBin.hide(); 'ok'")

    def eval(self, script):
        result = subprocess.run(["gdbus", "call", "--session", "--dest", "org.gnome.Shell", "--object-path",
                                 "/org/gnome/Shell", "--method", "org.gnome.Shell.Eval", script],
                                env=self.environment() | {"DBUS_SESSION_BUS_ADDRESS": self.bus},
                                capture_output=True, text=True, check=True, timeout=15)
        output = result.stdout.strip()
        ok = output.startswith("(true,")
        literal = output[output.index(",") + 1:].strip()[:-1].strip()
        # gdbus prints a GVariant string literal; its escapes match Python's.
        text = ast.literal_eval(literal) if literal.startswith(("'", '"')) else literal
        if not ok:
            raise RuntimeError(f"Shell.Eval failed: {text}")
        try:
            value = json.loads(text)
        except json.JSONDecodeError:
            return text
        return value if isinstance(value, str) else json.dumps(value)

    def windows(self):
        script = ("JSON.stringify(global.get_window_actors().map(a => a.meta_window).map(w => {"
                  "const r = w.get_frame_rect(); return {title: w.get_title(), x: r.x, y: r.y, "
                  "width: r.width, height: r.height, maximized: w.is_maximized(), "
                  "minimized: w.minimized}; }))")
        return json.loads(self.eval(script))

    def window(self, title_prefix="Nuschel"):
        for entry in self.windows():
            if (entry["title"] or "").startswith(title_prefix):
                return entry
        return None

    def place(self, title_prefix, x, y, width, height):
        self.eval("const w = global.get_window_actors().map(a => a.meta_window)"
                  f".find(w => (w.get_title() || '').startsWith({json.dumps(title_prefix)}));"
                  f" if (w.is_maximized()) w.unmaximize(); w.move_resize_frame(true, {x}, {y}, {width}, {height});"
                  " w.activate(global.get_current_time()); 'ok'")

    def screenshot(self, path, crop=None):
        path = Path(path)
        subprocess.run(["gdbus", "call", "--session", "--dest", "org.gnome.Shell.Screenshot", "--object-path",
                        "/org/gnome/Shell/Screenshot", "--method", "org.gnome.Shell.Screenshot.Screenshot",
                        "false", "false", str(path)],
                       env=self.environment() | {"DBUS_SESSION_BUS_ADDRESS": self.bus},
                       capture_output=True, text=True, check=True, timeout=15)
        if crop:
            x, y, width, height = crop
            subprocess.run(["magick", str(path), "-crop", f"{width}x{height}+{x}+{y}", "+repage", str(path)],
                           check=True, timeout=15)
        return path

    def _pointer(self):
        if not self.pointer_ready:
            self.eval("global._nuschelPointer = imports.gi.Clutter.get_default_backend().get_default_seat()"
                      ".create_virtual_device(imports.gi.Clutter.InputDeviceType.POINTER_DEVICE);"
                      "global._nuschelKeyboard = imports.gi.Clutter.get_default_backend().get_default_seat()"
                      ".create_virtual_device(imports.gi.Clutter.InputDeviceType.KEYBOARD_DEVICE); 'ok'")
            self.pointer_ready = True

    def move(self, x, y):
        self._pointer()
        self.eval(f"global._nuschelPointer.notify_absolute_motion(imports.gi.GLib.get_monotonic_time(), {x}, {y}); 'ok'")

    def click(self, x, y, button=1, count=1):
        self.move(x, y)
        time.sleep(0.05)
        for _ in range(count):
            self.eval("const p = global._nuschelPointer; const t = imports.gi.GLib.get_monotonic_time();"
                      f" p.notify_button(t, {button}, imports.gi.Clutter.ButtonState.PRESSED);"
                      f" p.notify_button(t + 30000, {button}, imports.gi.Clutter.ButtonState.RELEASED); 'ok'")
            time.sleep(0.06)

    def drag(self, start, end, steps=12):
        self.move(*start)
        self.eval("global._nuschelPointer.notify_button(imports.gi.GLib.get_monotonic_time(), 1, imports.gi.Clutter.ButtonState.PRESSED); 'ok'")
        for step in range(1, steps + 1):
            x = start[0] + (end[0] - start[0]) * step / steps
            y = start[1] + (end[1] - start[1]) * step / steps
            self.move(x, y)
            time.sleep(0.02)
        self.eval("global._nuschelPointer.notify_button(imports.gi.GLib.get_monotonic_time(), 1, imports.gi.Clutter.ButtonState.RELEASED); 'ok'")

    def key(self, keysym_name):
        self._pointer()
        self.eval("const k = global._nuschelKeyboard; const t = imports.gi.GLib.get_monotonic_time();"
                  f" const v = imports.gi.Clutter.KEY_{keysym_name};"
                  " k.notify_keyval(t, v, imports.gi.Clutter.KeyState.PRESSED);"
                  " k.notify_keyval(t + 20000, v, imports.gi.Clutter.KeyState.RELEASED); 'ok'")

    def type(self, text):
        self._pointer()
        for character in text:
            if character == "\n":
                self.key("Return")
                continue
            self.eval("const k = global._nuschelKeyboard; const t = imports.gi.GLib.get_monotonic_time();"
                      f" const v = imports.gi.Clutter.unicode_to_keysym({ord(character)});"
                      " k.notify_keyval(t, v, imports.gi.Clutter.KeyState.PRESSED);"
                      " k.notify_keyval(t + 10000, v, imports.gi.Clutter.KeyState.RELEASED); 'ok'")

    def stop(self):
        if self.process and self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
        shutil.rmtree(self.runtime, ignore_errors=True)
        shutil.rmtree(self.home, ignore_errors=True)
