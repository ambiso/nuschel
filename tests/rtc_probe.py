#!/usr/bin/env python3
"""Worker-encoded tone through two loopback WebRTC peers, then worker/reference decoding."""

import argparse
import json
import math
from pathlib import Path
import subprocess

from workers import exchange
from mumble_client import ReferenceOpus
import quarantine


def check(binaries, opus_library, probe=None):
    # Deterministic PCM, never the host microphone. Codec implementations stay
    # outside the transport executable, including when this runs in the VM.
    samples = [0.25 * math.sin(2 * math.pi * 440 * i / 48000) for i in range(960)]
    encoded, = exchange(binaries / "nuschl-media-worker", [
        {"kind": "encode_opus", "samples": samples},
    ])
    assert encoded["status"] == "packet", encoded
    packet = bytes(encoded["value"])
    result = subprocess.run([
        str(probe or binaries / "nuschl-rtc-probe"), "--opus-hex", packet.hex(),
    ], capture_output=True, text=True, timeout=15, check=True)
    report = json.loads(result.stdout)
    assert report["audio_frames_each_way"] == 50, report
    assert all(0 < count <= 17 for count in report["video_frames_received"]), report
    assert sum(report["video_frames_received"]) + report["video_frames_dropped"] == 34, report
    assert report["max_encoded_delivery_ms"] <= 500, report
    assert report["video_frame_bytes"] > 1200, "video must span multiple RTP packets"
    received = bytes.fromhex(report.pop("received_opus_hex"))
    assert received == packet, "encoded audio changed in transport"
    decoded, = exchange(binaries / "nuschl-media-worker", [
        {"kind": "decode_opus", "packet": list(received)},
    ])
    assert decoded["status"] == "samples", decoded
    pcm = decoded["value"]
    assert len(pcm) == 960 and all(math.isfinite(value) for value in pcm)
    assert sum(value * value for value in pcm) / len(pcm) > 0.001
    if opus_library:
        reference = ReferenceOpus(opus_library).decode([received])
        assert len(reference) == 960 and all(math.isfinite(value) for value in reference)
        assert sum(value * value for value in reference) / len(reference) > 0.001
    print(json.dumps(report))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", type=Path, nargs="?")
    parser.add_argument("--opus-library", type=Path)
    parser.add_argument("--probe", type=Path, help="nuschl-rtc-probe, when not among the binaries")
    args = parser.parse_args()
    if args.binaries is None:
        metadata = subprocess.run(["cargo", "metadata", "--no-deps", "--format-version", "1"],
                                  capture_output=True, text=True, check=True)
        args.binaries = Path(json.loads(metadata.stdout)["target_directory"]) / "debug"
    with quarantine.step("vm-smoke/rtc-probe"):
        check(args.binaries, args.opus_library, args.probe)
