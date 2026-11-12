#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Independent receive-output audit: private Pulse, reference libopus, fixed gain.

This reports observed fidelity gaps; --require-parity makes the measured LR/gain
requirements a gate. Reference ctypes is TEST-ONLY. No host devices or servers.
"""
import argparse
import ctypes
import json
import math
import os
from pathlib import Path
import signal
import struct
import subprocess
import tempfile
import time

from media_live import start
from mumble_client import ReferenceOpus

RATE = 48000
FRAME = 960


def rms(values):
    return math.sqrt(sum(v * v for v in values) / max(1, len(values)))


def amplitude(values, frequency, rate=RATE):
    """Phase-independent sinusoidal amplitude; no fitted amplitude is applied."""
    real = imaginary = 0.0
    for index, value in enumerate(values):
        phase = 2 * math.pi * frequency * index / rate
        real += value * math.cos(phase)
        imaginary += value * math.sin(phase)
    return 2 * math.hypot(real, imaginary) / max(1, len(values))


def compare(expected, observed):
    """Fixed unity-gain sample comparison. Caller aligns delay independently."""
    if len(expected) != len(observed) or not expected:
        raise ValueError("equal, nonempty aligned PCM required")
    if not all(math.isfinite(v) for v in (*expected, *observed)):
        raise ValueError("finite PCM required")
    energy = sum(v * v for v in expected)
    error = sum((a - b) ** 2 for a, b in zip(expected, observed))
    return {
        "samples": len(expected),
        "fixed_gain_snr_db": 10 * math.log10(max(energy, 1e-30) / max(error, 1e-30)),
        "rms_expected": rms(expected), "rms_observed": rms(observed),
        "diagnostic_gain": sum(a * b for a, b in zip(expected, observed)) / max(energy, 1e-30),
        "max_abs_error": max(abs(a - b) for a, b in zip(expected, observed)),
        "clipped_samples": sum(abs(v) >= 0.99999 for v in observed),
    }


def delay_only(expected, observed, limit=2 * RATE):
    """Integer delay search; no scaling, warping or channel-dependent alignment.

    A coarse quiet-onset marker avoids periodic-carrier correlation aliases.
    Refine only its bounded ±128-sample region using the actual waveform.
    """
    def onset(values):
        threshold = max(rms(values) * .04, 1e-6)
        for at in range(0, len(values) - 64, 64):
            if rms(values[at:at + 64]) > threshold:
                return at
        raise ValueError("no active alignment marker")
    coarse = onset(observed) - onset(expected)
    offset, width = RATE // 3, RATE // 4
    end = min(limit, len(observed) - offset - width)
    if end < 0 or len(expected) < offset + width:
        raise ValueError("recording shorter than alignment probe")
    candidates = range(max(0, coarse - 128), min(end, coarse + 128) + 1)
    if not candidates:
        raise ValueError("alignment marker outside delay budget")
    return max(candidates, key=lambda lag: sum(expected[i] * observed[i + lag]
               for i in range(offset, offset + width, 4)))


def fixture(channels, seconds=2):
    """Distinct LR carriers with envelope variation and a quiet onset marker."""
    data = []
    for index in range(int(seconds * RATE)):
        t = index / RATE
        envelope = 0 if t < .2 else .18 * (1 + .3 * math.sin(2 * math.pi * 3.1 * t))
        left = envelope * (math.sin(2 * math.pi * 997 * t) + .2 * math.sin(2 * math.pi * 371 * t))
        right = envelope * (math.sin(2 * math.pi * 2003 * t) + .2 * math.sin(2 * math.pi * 613 * t))
        data.extend((left, right) if channels == 2 else (left,))
    return data


def overshoot_fixture(seconds=2):
    """Full-scale legal source whose lossy decoding has finite >1 headroom."""
    data = []
    for index in range(int(seconds * RATE)):
        for frequency in (997, 2003):
            data.append(0. if index < RATE // 5 else .99 * (1 if math.sin(2 * math.pi * frequency * index / RATE) > 0 else -1))
    return data


class Reference:
    def __init__(self, library):
        self.lib = ReferenceOpus(library).lib

    def encode(self, samples, channels, bitrate=128000):
        error = ctypes.c_int()
        encoder = self.lib.opus_encoder_create(RATE, channels, 2049, ctypes.byref(error))  # APPLICATION_AUDIO
        if error.value:
            raise RuntimeError(f"libopus create: {error.value}")
        try:
            for request, value in [(4002, bitrate), (4006, 0), (4022, channels)]:  # bitrate,VBR,force channels
                if self.lib.opus_encoder_ctl(encoder, request, value):
                    raise RuntimeError(f"libopus control {request}")
            packets = []
            for offset in range(0, len(samples) - FRAME * channels + 1, FRAME * channels):
                pcm = (ctypes.c_float * (FRAME * channels))(*samples[offset:offset + FRAME * channels])
                output = ctypes.create_string_buffer(1275)
                count = self.lib.opus_encode_float(encoder, pcm, FRAME, output, len(output))
                if count <= 0:
                    raise RuntimeError(f"libopus encode: {count}")
                packets.append(bytes(output.raw[:count]))
            return packets
        finally:
            self.lib.opus_encoder_destroy(encoder)

    def aggregate(self, packets, count):
        """Reference-libopus repacketizer: legal120ms packets at six20ms frames."""
        ptr, integer = ctypes.c_void_p, ctypes.c_int
        self.lib.opus_repacketizer_create.restype = ptr
        self.lib.opus_repacketizer_destroy.argtypes = [ptr]
        self.lib.opus_repacketizer_init.argtypes = [ptr]
        self.lib.opus_repacketizer_init.restype = ptr
        self.lib.opus_repacketizer_cat.argtypes = [ptr, ptr, integer]
        self.lib.opus_repacketizer_cat.restype = integer
        self.lib.opus_repacketizer_out.argtypes = [ptr, ptr, integer]
        self.lib.opus_repacketizer_out.restype = integer
        repacketizer = self.lib.opus_repacketizer_create()
        if not repacketizer:
            raise RuntimeError("reference repacketizer allocation failed")
        try:
            result = []
            for at in range(0, len(packets) - count + 1, count):
                self.lib.opus_repacketizer_init(repacketizer)
                # libopus keeps references until out(); keep original bytes alive.
                for packet in packets[at:at + count]:
                    status = self.lib.opus_repacketizer_cat(repacketizer, packet, len(packet))
                    if status:
                        raise RuntimeError(f"reference repacketizer cat {status}")
                output = ctypes.create_string_buffer(1275)
                length = self.lib.opus_repacketizer_out(repacketizer, output, len(output))
                if length <= 0:
                    raise RuntimeError(f"reference repacketizer out {length}")
                result.append(output.raw[:length])
            return result
        finally:
            self.lib.opus_repacketizer_destroy(repacketizer)

    def decode(self, packets, channels):
        error = ctypes.c_int()
        decoder = self.lib.opus_decoder_create(RATE, channels, ctypes.byref(error))
        if error.value:
            raise RuntimeError(f"libopus create: {error.value}")
        try:
            samples = []
            for packet in packets:
                output = (ctypes.c_float * (5760 * channels))()
                count = self.lib.opus_decode_float(decoder, packet, len(packet), output, 5760, 0)
                if count <= 0:
                    raise RuntimeError(f"libopus decode: {count}")
                samples.extend(output[:count * channels])
            return samples
        finally:
            self.lib.opus_decoder_destroy(decoder)


class PrivatePulse:
    """Only two virtual null sinks. No module-udev-detect or host session bus."""
    def __init__(self, binaries, root, rate):
        self.bin, self.root = binaries, root
        self.socket = root / "pulse" / "native"
        self.socket.parent.mkdir(mode=0o700)
        script = root / "default.pa"
        script.write_text(f"load-module module-native-protocol-unix socket={self.socket} auth-anonymous=1\n"
                          f"load-module module-null-sink sink_name=speakers rate={rate} channels=2 channel_map=front-left,front-right\n"
                          "load-module module-null-sink sink_name=mic rate=48000 channels=1\n"
                          "set-default-sink speakers\nset-default-source mic.monitor\n")
        self.process = subprocess.Popen([str(binaries / "pulseaudio"), "-n", "--daemonize=no", "--use-pid-file=no",
                                         "--exit-idle-time=-1", f"--file={script}"],
            env={"PATH": os.environ.get("PATH", ""), "HOME": str(root), "XDG_RUNTIME_DIR": str(root),
                 "PULSE_RUNTIME_PATH": str(self.socket.parent), "PULSE_STATE_PATH": str(root / "state"),
                 "DBUS_SESSION_BUS_ADDRESS": f"unix:path={root}/no-bus"},
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        deadline = time.monotonic() + 10
        while not self.socket.exists():
            if self.process.poll() is not None or time.monotonic() >= deadline:
                self.stop()
                raise RuntimeError("private Pulse did not start")
            time.sleep(.02)

    def stop(self):
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            self.process.wait(timeout=5)


def playback(binary, pulse, packets, path, volume=1., packet_frames=FRAME):
    """Actual worker receive→jitter→mix→device→private monitor, muted microphone."""
    worker = start(binary, pulse, muted=True, settings={"echo_cancellation": "off"})
    recorder = None
    try:
        worker.wait(lambda e: e.get("kind") == "status" and e.get("ready"), "playback not ready")
        with path.open("wb") as sink:
            recorder = subprocess.Popen([str(pulse.bin / "parec"), f"--server=unix:{pulse.socket}",
                "--device=speakers.monitor", "--raw", "--format=float32le", "--rate=48000", "--channels=2",
                "--latency-msec=20"], stdout=sink, stderr=subprocess.DEVNULL, start_new_session=True)
            deadline = time.monotonic() + 5
            while path.stat().st_size < RATE // 4 * 2 * 4:
                if recorder.poll() is not None or time.monotonic() >= deadline:
                    raise RuntimeError("private monitor failed")
                time.sleep(.02)
            at = time.monotonic()
            for index, packet in enumerate(packets):
                time.sleep(max(0, at + index * packet_frames / RATE - time.monotonic()))
                worker.send({"kind": "playback", "sender": 7, "sequence": index * (packet_frames // 480),
                             "opus": list(packet), "end": index == len(packets) - 1, "volume": volume})
            time.sleep(.5)
    finally:
        if recorder and recorder.poll() is None:
            os.killpg(recorder.pid, signal.SIGTERM)
            recorder.wait(timeout=5)
        worker.stop()
    raw = path.read_bytes()
    if len(raw) % 8:
        raise RuntimeError("partial stereo monitor frame")
    return struct.unpack(f"<{len(raw) // 4}f", raw)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", type=Path, required=True)
    parser.add_argument("--pulse-bin", type=Path, required=True)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--require-parity", action="store_true")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    reference = Reference(args.opus_library)
    report = {"fixed_gain": 1.0, "host_devices": False, "cases": [], "limits": [
        "Private Pulse 48 kHz monitor only; device-rate resampling remains separate.",
        "No perceptual equivalence or physical device claim."]}
    with tempfile.TemporaryDirectory(prefix="private-pulse-", dir=args.output) as temporary:
        pulse = PrivatePulse(args.pulse_bin, Path(temporary), RATE)
        try:
            for name, channels, volume, source, duration in [
                ("mono", 1, 1., fixture(1), 20), ("stereo", 2, 1., fixture(2), 20),
                ("overshoot-attenuated", 2, .25, overshoot_fixture(), 20),
                ("stereo-120ms", 2, 1., fixture(2), 120),
            ]:
                packets = reference.encode(source, channels, bitrate=64000 if duration == 120 else 128000)
                if duration == 120:
                    packets = reference.aggregate(packets, 6)
                decoded = reference.decode(packets, 2)
                expected = [value * volume for value in decoded]
                (args.output / f"fixture-{name}.json").write_text(json.dumps({
                    "channels": channels, "rate": RATE, "frames_per_packet": RATE * duration // 1000,
                    "packets": [list(packet) for packet in packets],
                    "reference_stereo": decoded,
                    "reference_mono": reference.decode(packets, 1),
                }) + "\n")
                actual = playback(args.worker, pulse, packets, args.output / f"{name}.f32le", volume, RATE * duration // 1000)
                lag = delay_only(expected[::2], actual[::2])
                length = len(expected) // 2
                aligned = actual[2 * lag:2 * (lag + length)]
                # Exclude only fixed 200 ms initial silence and 100 ms tail, never fit gain.
                begin, end = RATE // 5 * 2, len(expected) - RATE // 10 * 2
                metrics = compare(expected[begin:end], aligned[begin:end])
                stable = aligned[begin:end]
                left997, right997 = amplitude(stable[::2], 997), amplitude(stable[1::2], 997)
                left2003, right2003 = amplitude(stable[::2], 2003), amplitude(stable[1::2], 2003)
                separation = min(20 * math.log10(max(left997, 1e-20) / max(right997, 1e-20)),
                                 20 * math.log10(max(right2003, 1e-20) / max(left2003, 1e-20)))
                passed = metrics["fixed_gain_snr_db"] >= 35 and (channels == 1 or separation >= 25)
                report["cases"].append({"name": name, "source_channels": channels, "requested_volume": volume,
                    "reference_decoder_peak": max(map(abs, decoded)), "packet_duration_ms": duration, "delay_samples": lag,
                    "max_packet_bytes": max(map(len, packets)), "fits_current_1024_byte_receiver": all(len(p) <= 1024 for p in packets),
                    "metrics": metrics, "stereo_separation_db": separation if channels == 2 else None,
                    "parity_gate_passed": passed})
        finally:
            pulse.stop()
    report["parity_gate_passed"] = all(case["parity_gate_passed"] for case in report["cases"])
    (args.output / "proof.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report), flush=True)
    if args.require_parity and not report["parity_gate_passed"]:
        raise SystemExit(1)


if __name__ == "__main__":
    main()
