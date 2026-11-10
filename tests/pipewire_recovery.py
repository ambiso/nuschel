#!/usr/bin/env python3
"""Live media worker against a private PipeWire session (no host devices).

Runs PipeWire, WirePlumber and pipewire-pulse with their own runtime directory
and D-Bus daemon. Only virtual nodes exist: WirePlumber's ALSA, Bluetooth and
camera monitors are disabled, and the test asserts that no hardware node
appeared. Checks:

1. Playback through the native PipeWire host reaches the speakers, with a
   raised quantum range (1024..2048, as in virtual machines).
2. PipeWire restart: the worker reports the failure; a replacement worker
   pinned to PipeWire (as the desktop supervisor does) tries no other audio
   server while PipeWire is down, and plays again once it is back.
3. A pipewire-pulse restart does not disturb a native PipeWire worker; a
   worker on the PulseAudio API recovers after pipewire-pulse restarts.
4. A PipeWire echo-cancel node as the default microphone is recognised:
   Nuschel does not stack its own canceller, routes only its own playback
   into the canceller's sink and leaves the session defaults alone. Device
   lists hold only real sources and sinks (no other applications' streams,
   sink monitors or virtual default entries).
"""
import argparse
import json
import math
import os
from pathlib import Path
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import threading
import time
import wave

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mumble_client import ReferenceOpus  # noqa: E402

VIRTUAL_NODES = """# A raised quantum range, as PipeWire applies in virtual machines (and some users
# configure): Nuschel must fit its buffer request into it.
context.properties = { default.clock.min-quantum = 1024 default.clock.max-quantum = 2048 }
context.objects = [
  { factory = adapter
    args = { factory.name = support.null-audio-sink node.name = "speakers" node.description = "Speakers"
             media.class = "Audio/Sink" audio.position = [ FL FR ] object.linger = true
             monitor.channel-volumes = true } }
  { factory = adapter
    args = { factory.name = support.null-audio-sink node.name = "microphone" node.description = "Microphone"
             media.class = "Audio/Source/Virtual" audio.position = [ MONO ] object.linger = true } }
]
"""
ECHO_CANCEL = """context.modules = [
  { name = libpipewire-module-echo-cancel
    args = {
      library.name = aec/libspa-aec-webrtc
      capture.props = { node.name = "echo-cancel-capture" target.object = "microphone" node.passive = true }
      source.props = { node.name = "echo-cancel-source" node.description = "Echo-Cancel Source" }
      sink.props = { node.name = "echo-cancel-sink" node.description = "Echo-Cancel Sink" }
      playback.props = { node.name = "echo-cancel-playback" target.object = "speakers" node.passive = true }
    }
  }
]
"""
NO_HARDWARE = """wireplumber.profiles = {
  main = {
    monitor.alsa = disabled
    monitor.alsa-midi = disabled
    monitor.bluez = disabled
    monitor.bluez-midi = disabled
    monitor.v4l2 = disabled
    monitor.libcamera = disabled
  }
}
"""


class PipeWire:
    def __init__(self, pipewire, wireplumber, dbus_daemon, root):
        self.pipewire, self.wireplumber = Path(pipewire), Path(wireplumber)
        self.root = root
        self.runtime = root / "run"
        self.runtime.mkdir(mode=0o700, parents=True)
        self.config = root / "config"
        (self.config / "pipewire/pipewire.conf.d").mkdir(parents=True)
        (self.config / "pipewire/pipewire.conf.d/10-nodes.conf").write_text(VIRTUAL_NODES)
        (self.config / "wireplumber/wireplumber.conf.d").mkdir(parents=True)
        (self.config / "wireplumber/wireplumber.conf.d/10-no-hardware.conf").write_text(NO_HARDWARE)
        bus = f"unix:path={self.runtime}/bus"
        self.env = {"PATH": os.environ.get("PATH", ""), "HOME": str(root), "XDG_RUNTIME_DIR": str(self.runtime),
                    "XDG_CONFIG_HOME": str(self.config), "XDG_STATE_HOME": str(root / "state"),
                    "XDG_DATA_DIRS": f"{self.wireplumber}/share:{self.pipewire}/share",
                    "DBUS_SESSION_BUS_ADDRESS": bus, "PIPEWIRE_RUNTIME_DIR": str(self.runtime),
                    "PULSE_RUNTIME_PATH": str(self.runtime / "pulse")}
        # A private bus: WirePlumber must never talk to the desktop's session.
        self.dbus = subprocess.Popen([dbus_daemon, "--session", "--nofork", f"--address={bus}"], env=self.env,
                                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        self.processes = {}

    def enable_echo_cancel(self):
        (self.config / "pipewire/pipewire.conf.d/20-echo-cancel.conf").write_text(ECHO_CANCEL)

    def start(self, name):
        binary = {"pipewire": self.pipewire / "bin/pipewire", "pipewire-pulse": self.pipewire / "bin/pipewire-pulse",
                  "wireplumber": self.wireplumber / "bin/wireplumber"}[name]
        self.processes[name] = subprocess.Popen([str(binary)], env=self.env,
                                                stdout=(self.root / f"{name}.log").open("a"),
                                                stderr=subprocess.STDOUT, start_new_session=True)

    def stop(self, name):
        process = self.processes.pop(name, None)
        if process and process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=10)

    def nodes(self):
        result = subprocess.run([str(self.pipewire / "bin/pw-dump")], env=self.env, capture_output=True, text=True,
                                timeout=10)
        objects = json.loads(result.stdout or "[]")
        return {entry["info"]["props"].get("node.name"): entry["info"]["props"] for entry in objects
                if entry.get("type") == "PipeWire:Interface:Node"}

    def up(self, expected=("speakers", "microphone")):
        for name in ("pipewire", "wireplumber", "pipewire-pulse"):
            self.start(name)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                nodes = self.nodes()
                session_ready = any("'default.audio.sink'" in line for line in defaults(self))
                if set(expected) <= set(nodes) and (self.runtime / "pulse/native").exists() and session_ready:
                    hardware = [name for name, props in nodes.items() if props.get("device.api")]
                    assert not hardware, f"hardware nodes appeared in the private session: {hardware}"
                    time.sleep(0.5)
                    return
            except (json.JSONDecodeError, subprocess.TimeoutExpired):
                pass
            time.sleep(0.3)
        raise AssertionError("private PipeWire did not come up: " + (self.root / "pipewire.log").read_text()[-3000:])

    def down(self):
        for name in ("pipewire-pulse", "wireplumber", "pipewire"):
            self.stop(name)

    def record(self, seconds, path):
        """Record what reaches the speakers (their monitor) as 16-bit mono WAV."""
        recorder = subprocess.Popen([str(self.pipewire / "bin/pw-record"), "--target", "speakers",
                                     "-P", "{ stream.capture.sink = true }", "--format", "s16", "--rate", "48000",
                                     "--channels", "1", str(path)], env=self.env, stdout=subprocess.DEVNULL,
                                    stderr=subprocess.DEVNULL, start_new_session=True)
        return recorder

    def close(self):
        self.down()
        if self.dbus.poll() is None:
            os.killpg(self.dbus.pid, signal.SIGTERM)


class Worker:
    count = 0
    logs = Path(tempfile.gettempdir())

    def __init__(self, binary, pipewire, host=None, backend=None, muted=True):
        env = dict(pipewire.env) | {"PULSE_SERVER": f"unix:{pipewire.runtime}/pulse/native", "PIPEWIRE_DEBUG": "2"}
        if backend:
            env["NUSCHEL_AUDIO_BACKEND"] = backend
        Worker.count += 1
        self.log = Worker.logs / f"worker-{Worker.count}.log"
        self.process = subprocess.Popen([str(binary), "--live"], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        stderr=self.log.open("w"), env=env)
        self.events = []
        threading.Thread(target=self._read, daemon=True).start()
        self.send({"kind": "configure", "bitrate": 40000, "packet_loss_percent": 5, "settings": {}, "host": host})
        self.send({"kind": "set_state", "epoch": 1, "muted": muted, "deafened": False})

    def _read(self):
        while True:
            header = self.process.stdout.read(4)
            if len(header) < 4:
                return
            self.events.append(json.loads(self.process.stdout.read(struct.unpack(">I", header)[0]))["payload"])

    def send(self, payload):
        body = json.dumps({"version": 1, "request_id": 0, "payload": payload}).encode()
        self.process.stdin.write(struct.pack(">I", len(body)) + body)
        self.process.stdin.flush()

    def wait(self, predicate, message, timeout=20, since=0):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            for event in list(self.events)[since:]:
                if predicate(event):
                    return event
            time.sleep(0.05)
        statuses = [e.get("message") for e in self.events if e["kind"] == "status"][-5:]
        raise AssertionError(f"{message}; recent status: {statuses}")

    def ready(self, since=0, timeout=20):
        return self.wait(lambda e: e["kind"] == "status" and e["ready"], "audio did not become ready",
                         timeout=timeout, since=since)

    def stop(self):
        try:
            self.send({"kind": "shutdown"})
            self.process.wait(timeout=5)
        except (BrokenPipeError, subprocess.TimeoutExpired):
            self.process.kill()
            self.process.wait(timeout=5)


def play_and_measure(worker, pipewire, packets, path):
    recorder = pipewire.record(4, path)
    time.sleep(0.6)
    start = time.monotonic()
    for index, opus in enumerate(packets):
        time.sleep(max(0, start + index * 0.02 - time.monotonic()))
        worker.send({"kind": "playback", "sender": 9, "sequence": index * 2, "opus": opus,
                     "end": index == len(packets) - 1, "volume": 1.0})
    time.sleep(0.6)
    os.killpg(recorder.pid, signal.SIGINT)
    recorder.wait(timeout=5)
    with wave.open(str(path)) as recording:
        frames = recording.readframes(recording.getnframes())
    samples = struct.unpack("<%dh" % (len(frames) // 2), frames)
    return math.sqrt(sum(s * s for s in samples) / max(len(samples), 1)) / 32768


def check_restarts(binary, pipewire, packets, output):
    worker = Worker(binary, pipewire)
    worker.ready()
    devices = worker.wait(lambda e: e["kind"] == "devices", "no device report")
    assert devices["output"].startswith("pipewire:"), \
        f"native PipeWire was not used: {devices}\nworker log:\n{worker.log.read_text()[-3000:]}"
    level = play_and_measure(worker, pipewire, packets, output / "pipewire-before.wav")
    assert level > 0.02, f"playback did not reach the speakers (RMS {level:.4f})"
    print(f"Native PipeWire playback reached the speakers (RMS {level:.3f}).", flush=True)

    mark = len(worker.events)
    pipewire.down()
    worker.wait(lambda e: e["kind"] == "status" and not e["ready"], "the PipeWire failure was not reported",
                since=mark)
    worker.stop()
    # The desktop supervisor replaces the worker and pins it to the server that worked.
    worker = Worker(binary, pipewire, host="pipewire")
    time.sleep(3)
    attempts = [e["message"] for e in worker.events if e["kind"] == "status" and not e["ready"]]
    assert attempts, "the pinned worker did not report PipeWire as unavailable"
    assert not any("Alsa" in m or "PulseAudio" in m for m in attempts), \
        f"a pinned worker tried another audio server while PipeWire was down: {attempts}"
    mark = len(worker.events)
    pipewire.up()
    worker.ready(since=mark, timeout=30)
    level = play_and_measure(worker, pipewire, packets, output / "pipewire-after.wav")
    assert level > 0.02, f"playback did not resume after the PipeWire restart (RMS {level:.4f})"
    print(f"PipeWire restart: pinned worker waited for PipeWire only, then played again (RMS {level:.3f}).",
          flush=True)

    mark = len(worker.events)
    pipewire.stop("pipewire-pulse")
    time.sleep(1)
    pipewire.start("pipewire-pulse")
    time.sleep(2)
    failures = [e for e in worker.events[mark:] if e["kind"] == "status" and not e["ready"]]
    assert not failures, f"a pipewire-pulse restart disturbed the native PipeWire worker: {failures}"
    worker.stop()

    worker = Worker(binary, pipewire, backend="pulse")
    worker.ready()
    mark = len(worker.events)
    pipewire.stop("pipewire-pulse")
    worker.wait(lambda e: e["kind"] == "status" and not e["ready"], "the pipewire-pulse failure was not reported",
                since=mark)
    worker.stop()
    pipewire.start("pipewire-pulse")
    worker = Worker(binary, pipewire, backend="pulse", host="pulseaudio")
    worker.ready(timeout=30)
    level = play_and_measure(worker, pipewire, packets, output / "pulse-after.wav")
    assert level > 0.02, f"playback through pipewire-pulse did not resume (RMS {level:.4f})"
    worker.stop()
    print("pipewire-pulse restart: native worker undisturbed; PulseAudio-API worker recovered.", flush=True)


def defaults(pipewire):
    """The session's default sink and source (PipeWire 'default' metadata)."""
    result = subprocess.run([str(pipewire.pipewire / "bin/pw-metadata"), "-n", "default"], env=pipewire.env,
                            capture_output=True, text=True, timeout=10)
    return sorted(line.split("key:", 1)[1] for line in result.stdout.splitlines() if "key:'default.audio." in line)


def check_echo_cancel(binary, pipewire):
    # The user chose the canceller as their microphone (as in GNOME Settings).
    subprocess.run([str(pipewire.pipewire / "bin/pw-metadata"), "-n", "default", "0",
                    "default.configured.audio.source", '{ "name": "echo-cancel-source" }', "Spa:String:JSON"],
                   env=pipewire.env, check=True, capture_output=True, timeout=10)
    deadline = time.monotonic() + 10
    while not any("echo-cancel-source" in line and "'default.audio.source'" in line for line in defaults(pipewire)):
        assert time.monotonic() < deadline, f"the echo-cancel source did not become the default: {defaults(pipewire)}"
        time.sleep(0.2)
    before = defaults(pipewire)
    worker = Worker(binary, pipewire, muted=False)
    devices = worker.wait(lambda e: e["kind"] == "devices", "no device report")
    worker.ready()
    assert devices["echo"]["kind"] == "upstream", devices["echo"]
    assert devices["output"].endswith(":echo-cancel-sink"), devices["output"]
    inputs = [entry["id"] for entry in devices["inputs"]]
    outputs = [entry["id"] for entry in devices["outputs"]]
    # cpal's PipeWire host omits Audio/Source/Virtual nodes (the test microphone), see TODO.md.
    assert inputs == ["pipewire:echo-cancel-source"], f"the microphone list should hold only real sources: {inputs}"
    assert sorted(outputs) == ["pipewire:echo-cancel-sink", "pipewire:speakers"], \
        f"the speaker list should hold only real sinks: {outputs}"
    time.sleep(1)
    after = defaults(pipewire)
    worker.stop()
    assert before == after, f"the session defaults changed: {before} -> {after}"
    print("PipeWire echo-cancel recognised: no second canceller; only Nuschel's playback goes to its sink; "
          "session defaults untouched.", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--pipewire", type=Path, required=True)
    parser.add_argument("--wireplumber", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--dbus-daemon", default=shutil.which("dbus-daemon") or "dbus-daemon")
    parser.add_argument("--output", type=Path, default=Path(tempfile.gettempdir()))
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    Worker.logs = args.output
    packets = ReferenceOpus(args.opus_library).encode_tone(100)
    binary = args.binaries / "nuschel-media-worker"
    with tempfile.TemporaryDirectory(prefix="nuschel-pw-", dir=os.environ.get("NUSCHEL_SHORT_TMP")) as temporary:
        pipewire = PipeWire(args.pipewire, args.wireplumber, args.dbus_daemon, Path(temporary) / "plain")
        try:
            pipewire.up()
            check_restarts(binary, pipewire, packets, args.output)
        finally:
            pipewire.close()
        pipewire = PipeWire(args.pipewire, args.wireplumber, args.dbus_daemon, Path(temporary) / "echo")
        try:
            pipewire.enable_echo_cancel()
            pipewire.up(expected=("speakers", "microphone", "echo-cancel-source", "echo-cancel-sink"))
            check_echo_cancel(binary, pipewire)
        finally:
            pipewire.close()


if __name__ == "__main__":
    main()
