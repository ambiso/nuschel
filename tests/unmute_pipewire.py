#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Native PipeWire versus pipewire-pulse unmute on an owned virtual graph.

Hardware monitors are disabled and asserted absent. PCM is synthetic; no host
audio/session is connected. Optional upstream WebRTC AEC is PipeWire's real
module, with only the owned virtual source/sink as its endpoints.
"""

import argparse
import array
import json
import math
import subprocess
import sys
import threading
import time
from pathlib import Path
from types import SimpleNamespace

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mumble_client import ReferenceOpus
from pipewire_recovery import PipeWire, defaults
from unmute_latency import Worker

parser = argparse.ArgumentParser(description=__doc__)
for field in (
    "worker",
    "baseline-worker",
    "pipewire",
    "wireplumber",
    "dbus-daemon",
    "opus-library",
    "output",
):
    parser.add_argument("--" + field, type=Path, required=True)
parser.add_argument("--upstream-echo", action="store_true")
args = parser.parse_args()
root = args.output.resolve()
root.mkdir(parents=True, exist_ok=True, mode=0o700)
pw = PipeWire(args.pipewire, args.wireplumber, args.dbus_daemon, root)
(pw.config / "pipewire/pipewire.conf.d/10-nodes.conf").write_text("""
context.properties = { default.clock.rate = 48000 default.clock.quantum = 480 default.clock.min-quantum = 480 default.clock.max-quantum = 2048 }
context.objects = [
 { factory = adapter args = { factory.name = support.null-audio-sink node.name = speakers node.description = Speakers media.class = Audio/Sink audio.position = [ FL FR ] object.linger = true } }
 { factory = adapter args = { factory.name = support.null-audio-sink node.name = microphone node.description = Injection media.class = Audio/Sink audio.position = [ MONO ] object.linger = true } }
]
context.modules = [
 { name = libpipewire-module-loopback args = {
    capture.props = { node.name = synthetic.capture stream.capture.sink = true target.object = microphone node.passive = true }
    playback.props = { node.name = synthetic.mic node.description = SyntheticMicrophone media.class = Audio/Source audio.position = [ MONO ] }
 } }
]
""")
feeder = None
try:
    if args.upstream_echo:
        pw.enable_echo_cancel()
        cfg = pw.config / "pipewire/pipewire.conf.d/20-echo-cancel.conf"
        cfg.write_text(
            cfg.read_text().replace(
                'target.object = "microphone"', 'target.object = "synthetic.mic"'
            )
        )
    expected = ("speakers", "microphone", "synthetic.mic") + (
        ("echo-cancel-source", "echo-cancel-sink") if args.upstream_echo else ()
    )
    pw.up(expected=expected)
    for key, name in [
        ("default.configured.audio.sink", "speakers"),
        (
            "default.configured.audio.source",
            "echo-cancel-source" if args.upstream_echo else "synthetic.mic",
        ),
    ]:
        subprocess.run(
            [
                str(pw.pipewire / "bin/pw-metadata"),
                "-n",
                "default",
                "0",
                key,
                json.dumps({"name": name}),
                "Spa:String:JSON",
            ],
            env=pw.env,
            capture_output=True,
            check=True,
        )
    time.sleep(0.3)
    assert not [n for n, p in pw.nodes().items() if p.get("device.api")]
    feeder = subprocess.Popen(
        [
            str(pw.pipewire / "bin/pw-cat"),
            "--playback",
            "--raw",
            "--target",
            "microphone",
            "--latency",
            "10ms",
            "--rate",
            "48000",
            "--channels",
            "1",
            "--format",
            "f32",
            "-",
        ],
        env=pw.env,
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=(root / "feeder.log").open("wb"),
    )
    samples = array.array(
        "f",
        (
            0.12
            * (0.5 + 0.5 * math.sin(2 * math.pi * 4 * i / 48000))
            * sum(math.sin(2 * math.pi * 140 * h * i / 48000) / h for h in range(1, 13))
            for i in range(48000)
        ),
    ).tobytes()

    def feed():
        try:
            while feeder.poll() is None:
                feeder.stdin.write(samples)
                feeder.stdin.flush()
        except (BrokenPipeError, ValueError):
            pass

    threading.Thread(target=feed, daemon=True).start()
    ref = ReferenceOpus(args.opus_library)
    rows = []
    for label, binary in [("baseline", args.baseline_worker), ("current", args.worker)]:
        for backend in ["pipewire", "pulse"]:
            for mode in ["continuous", "push_to_talk", "voice_activity"]:
                p = SimpleNamespace(root=pw.runtime, socket=pw.runtime / "pulse/native")
                w = Worker(
                    Path(binary),
                    p,
                    root / f"{label}-{backend}-{mode}.stderr",
                    environment=dict(pw.env) | {"NUSCHL_AUDIO_BACKEND": backend},
                )
                try:
                    w.send(
                        {
                            "kind": "configure",
                            "bitrate": 128000,
                            "packet_loss_percent": 5,
                            "settings": {"transmit": mode},
                        }
                    )
                    w.send({"kind": "push_to_talk", "active": True})
                    w.send(
                        {
                            "kind": "set_state",
                            "epoch": 1,
                            "muted": False,
                            "deafened": False,
                        }
                    )
                    packets = lambda w=w: [
                        (t, e)
                        for t, e in w.events
                        if e["kind"] == "packet" and not e["end"]
                    ]
                    w.wait(lambda packets=packets: len(packets()) >= 6, timeout=12)
                    pcm = ref.decode([e["opus"] for t, e in packets()[:6]])
                    assert len(pcm) == 6 * 960
                    levels = [
                        math.sqrt(
                            sum(x * x for x in pcm[i * 960 : (i + 1) * 960]) / 960
                        )
                        for i in range(6)
                    ]
                    usable = next(
                        (i for i, level in enumerate(levels) if level > 0.001), None
                    )
                    assert usable is not None
                    row = {
                        "label": label,
                        "backend": backend,
                        "mode": mode,
                        "first_packet_ms": packets()[0][0] * 1000,
                        "first_usable_packet_ms": packets()[usable][0] * 1000,
                        "first_six_packet_rms": levels,
                        "rms": math.sqrt(sum(x * x for x in pcm) / len(pcm)),
                        "devices": next(
                            (e for t, e in w.events if e["kind"] == "devices"), None
                        ),
                        "log": str(w.log),
                    }
                    assert row["rms"] > 0.001 and all(math.isfinite(x) for x in pcm)
                    assert row["devices"]["input"].startswith(
                        "pipewire:" if backend == "pipewire" else "pulseaudio:"
                    )
                    if args.upstream_echo:
                        assert row["devices"]["echo"]["kind"] == "upstream", row[
                            "devices"
                        ]
                    rows.append(row)
                    print(json.dumps(row), flush=True)
                finally:
                    w.stop()
    (root / "proof.json").write_text(
        json.dumps(
            {
                "passed": True,
                "hardware_nodes": False,
                "defaults": defaults(pw),
                "quantum": 480,
                "upstream_echo": args.upstream_echo,
                "producer": "pw-cat --latency10ms f32 mono48k to owned injection sink",
                "measurements": rows,
            },
            indent=2,
        )
    )
finally:
    if feeder:
        feeder.terminate()
        feeder.wait(timeout=5)
        try:
            feeder.stdin.close()
        except BrokenPipeError:
            pass
    pw.close()
    if pw.dbus.poll() is None:
        pw.dbus.wait(timeout=5)
