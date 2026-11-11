#!/usr/bin/env python3
"""Compare the pure-Rust Opus codec (opus-pure, in the media worker) with
reference libopus, and check that the worker survives hostile packets.

1. Decoder conformance: the same packets (from either encoder) decoded by
   libopus and by opus-pure must match closely.
2. Encoder and interoperability: a synthetic talker is coded at 64 kb/s along
   four paths (libopus→libopus, opus-pure→libopus, libopus→opus-pure,
   opus-pure→opus-pure). Each output is aligned to the input (best lag and gain)
   and its waveform SNR reported with the coding modes used. Waveform SNR is not
   a perceptual measure (libopus trades it for perceptual tools), so it only
   guards against a broken encoder: opus-pure must not fall 3 dB below libopus.
3. Robustness: thousands of mutated, truncated and random packets go to the
   worker's decoder; every request must be answered with finite audio or a
   rejection, matching libopus's accept/reject decision and length, and the
   worker must stay up.

libopus runs only in this test process, through ctypes, as a reference.
"""
import argparse
import ctypes
import json
import math
from pathlib import Path
import random
import struct
import subprocess
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from mumble_client import ReferenceOpus  # noqa: E402

RATE = 48000
FRAME = 960  # 20 ms


def talker(seconds, pitch=140.0, syllables=3.3, seed=0.4):
    """Harmonics of a gliding pitch with a formant, syllable envelope and pauses."""
    samples = []
    phase = 0.0
    for index in range(int(seconds * RATE)):
        t = index / RATE
        f0 = pitch * (1 + 0.12 * math.sin(2 * math.pi * 0.7 * t + seed))
        phase += f0 / RATE
        syllable = max(0.0, math.sin(2 * math.pi * syllables * t + seed))
        pause = 0.0 if (t + seed) % 2.3 < 0.4 else 1.0
        value = 0.0
        for harmonic in range(1, 13):
            formant = 1 / (1 + ((harmonic * f0 - 700) / 500) ** 2)
            value += math.sin(2 * math.pi * phase * harmonic) * (0.3 / harmonic + formant)
        samples.append(0.08 * syllable * pause * value)
    return samples


class Worker:
    """The media worker's codec mode: one encoder and one decoder per process."""

    def __init__(self, binary):
        self.process = subprocess.Popen([str(binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
        self.request_id = 0

    def call(self, payload):
        self.request_id += 1
        body = json.dumps({"version": 1, "request_id": self.request_id, "payload": payload}).encode()
        self.process.stdin.write(struct.pack(">I", len(body)) + body)
        self.process.stdin.flush()
        header = self.process.stdout.read(4)
        if len(header) < 4:
            raise AssertionError(f"media worker exited ({self.process.wait()}) on {payload['kind']}")
        envelope = json.loads(self.process.stdout.read(struct.unpack(">I", header)[0]))
        assert envelope["request_id"] == self.request_id
        return envelope["payload"]

    def encode(self, samples):
        packets = []
        for offset in range(0, len(samples) - FRAME + 1, FRAME):
            reply = self.call({"kind": "encode_opus", "samples": samples[offset:offset + FRAME]})
            assert reply["status"] == "packet", reply
            packets.append(reply["value"])
        return packets

    def decode(self, packets):
        out = []
        for packet in packets:
            reply = self.call({"kind": "decode_opus", "packet": list(packet)})
            assert reply["status"] == "samples", reply
            out.extend(reply["value"])
        return out

    def close(self):
        self.process.stdin.close()
        self.process.wait(timeout=10)


def reference_encode(reference, samples, bitrate):
    lib = reference.lib
    error = ctypes.c_int()
    encoder = lib.opus_encoder_create(RATE, 1, 2048, ctypes.byref(error))  # OPUS_APPLICATION_VOIP
    assert error.value == 0
    try:
        assert lib.opus_encoder_ctl(encoder, 4002, bitrate) == 0  # OPUS_SET_BITRATE
        packets = []
        for offset in range(0, len(samples) - FRAME + 1, FRAME):
            pcm = (ctypes.c_float * FRAME)(*samples[offset:offset + FRAME])
            output = ctypes.create_string_buffer(1275)
            count = lib.opus_encode_float(encoder, pcm, FRAME, output, 1275)
            assert count > 0
            packets.append(list(output.raw[:count]))
        return packets
    finally:
        lib.opus_encoder_destroy(encoder)


def snr(reference, decoded, max_lag=2000):
    """Waveform SNR after the best lag (codec delay) and least-squares gain."""
    best = -math.inf
    step = 8
    window = reference[RATE // 2: RATE // 2 + RATE]  # a one-second probe for the lag
    candidates = range(0, max_lag, step)

    def correlation(lag):
        return sum(a * b for a, b in zip(window, decoded[RATE // 2 + lag:]))
    lag = max(candidates, key=correlation)
    lag = max(range(max(0, lag - step), lag + step), key=correlation)
    pairs = list(zip(reference, decoded[lag:]))[RATE // 10:]  # skip codec start-up
    signal = sum(a * a for a, _ in pairs)
    gain = sum(a * b for a, b in pairs) / max(sum(b * b for _, b in pairs), 1e-12)
    noise = sum((a - gain * b) ** 2 for a, b in pairs)
    best = 10 * math.log10(signal / max(noise, 1e-12))
    return best, lag


def check_quality(binary, reference):
    samples = talker(6.0)
    bitrate = 64000  # the worker's codec mode encodes at 64 kb/s
    ref_packets = reference_encode(reference, samples, bitrate)
    worker = Worker(binary)
    pure_packets = worker.encode(samples)
    worker.close()
    paths = {}
    paths["libopus → libopus"] = reference.decode(ref_packets)
    paths["opus-pure → libopus"] = reference.decode(pure_packets)
    worker = Worker(binary)
    paths["libopus → opus-pure"] = worker.decode(ref_packets)
    worker.close()
    worker = Worker(binary)
    paths["opus-pure → opus-pure"] = worker.decode(pure_packets)
    worker.close()
    sizes = {name: sum(map(len, packets)) * 8 / 6.0 / 1000 for name, packets in
             (("libopus", ref_packets), ("opus-pure", pure_packets))}
    print(f"Bitrate at a 64 kb/s target: libopus {sizes['libopus']:.1f} kb/s, opus-pure {sizes['opus-pure']:.1f} kb/s")
    results = {}
    for name, decoded in paths.items():
        assert all(math.isfinite(value) for value in decoded), name
        results[name], lag = snr(samples, decoded)
        print(f"  {name:24s} SNR {results[name]:5.1f} dB (delay {lag} samples)", flush=True)
    baseline = results["libopus → libopus"]
    for name, value in results.items():
        assert value > baseline - 3, f"{name} is more than 3 dB worse than libopus ({value:.1f} vs {baseline:.1f})"
    print(f"  coding modes: libopus {modes(ref_packets)}, opus-pure {modes(pure_packets)}")
    for source in ("libopus", "opus-pure"):
        a, b = paths[f"{source} → libopus"], paths[f"{source} → opus-pure"]
        assert len(a) == len(b), (source, len(a), len(b))
        error = sum((x - y) ** 2 for x, y in zip(a, b))
        energy = sum(x * x for x in a)
        agreement = 10 * math.log10(energy / error) if error else math.inf
        peak = max(abs(x - y) for x, y in zip(a, b))
        print(f"Decoders agree on {source} packets: {agreement:.1f} dB (largest sample difference {peak:.2e})",
              flush=True)
        assert agreement > 40, f"opus-pure decodes {source} packets differently from libopus"


def check_decoder_modes(binary, reference):
    """Stock clients at lower bitrates send SILK and Hybrid packets; decode them both ways."""
    samples = talker(3.0)
    for bitrate in (10000, 20000, 32000):
        packets = reference_encode(reference, samples, bitrate)
        expected = reference.decode(packets)
        worker = Worker(binary)
        decoded = worker.decode(packets)
        worker.close()
        assert len(expected) == len(decoded)
        error = sum((x - y) ** 2 for x, y in zip(expected, decoded))
        energy = sum(x * x for x in expected)
        agreement = 10 * math.log10(energy / error) if error else math.inf
        print(f"libopus at {bitrate // 1000} kb/s {modes(packets)}: opus-pure decoder agrees to {agreement:.1f} dB",
              flush=True)
        assert agreement > 40, f"opus-pure decodes {bitrate} b/s packets differently from libopus"


def modes(packets):
    counts = {}
    for packet in packets:
        config = packet[0] >> 3
        mode = "SILK" if config < 12 else "Hybrid" if config < 16 else "CELT"
        counts[mode] = counts.get(mode, 0) + 1
    return counts


def check_hostile_packets(binary, reference, count):
    rng = random.Random(20261110)
    seeds = reference_encode(reference, talker(1.0), 64000)
    worker = Worker(binary)
    outcomes = {"samples": 0, "rejected": 0}
    lib = reference.lib
    error = ctypes.c_int()
    libopus = lib.opus_decoder_create(RATE, 1, ctypes.byref(error))
    assert error.value == 0
    agree = 0
    loudest = 0.0
    for index in range(count):
        packet = bytearray(rng.choice(seeds))
        kind = index % 4
        if kind == 0:  # bit flips
            for _ in range(rng.randint(1, 8)):
                packet[rng.randrange(len(packet))] ^= 1 << rng.randrange(8)
        elif kind == 1:  # truncation
            packet = packet[:rng.randint(1, len(packet))]
        elif kind == 2:  # random TOC (mode, bandwidth, frame count codes) on a real body
            packet[0] = rng.randrange(256)
        else:  # random bytes of random length
            packet = bytearray(rng.randrange(256) for _ in range(rng.randint(1, 1275)))
        reply = worker.call({"kind": "decode_opus", "packet": list(packet)})
        assert reply["status"] in outcomes, reply
        expected = (ctypes.c_float * 5760)()
        produced = lib.opus_decode_float(libopus, bytes(packet), len(packet), expected, 5760, 0)
        if reply["status"] == "samples":
            # Like libopus, garbage may decode outside [-1, 1]; the live mixer clamps.
            assert all(math.isfinite(value) for value in reply["value"])
            loudest = max(loudest, max(map(abs, reply["value"]), default=0.0))
            agree += produced == len(reply["value"])
        else:
            agree += produced < 0
        outcomes[reply["status"]] += 1
    reply = worker.call({"kind": "decode_opus", "packet": seeds[3]})
    assert reply["status"] == "samples", "the decoder did not recover after hostile packets"
    worker.close()
    lib.opus_decoder_destroy(libopus)
    print(f"Hostile packets: {count} decoded without a crash "
          f"({outcomes['samples']} produced audio, {outcomes['rejected']} rejected); "
          f"accept/reject and length agree with libopus for {agree * 100 / count:.1f}%; "
          f"loudest output {loudest:.1f}", flush=True)
    assert agree >= count * 0.95, "opus-pure diverges from libopus on malformed packets"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--hostile-packets", type=int, default=5000)
    args = parser.parse_args()
    reference = ReferenceOpus(args.opus_library)
    check_quality(args.binaries / "nuschl-media-worker", reference)
    check_decoder_modes(args.binaries / "nuschl-media-worker", reference)
    check_hostile_packets(args.binaries / "nuschl-media-worker", reference, args.hostile_packets)


if __name__ == "__main__":
    main()
