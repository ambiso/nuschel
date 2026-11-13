#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual isolated musicbot role protocol, malformed input and fresh-owner recovery.

Only generated sine/WAV/JSON, never host audio. The companion Rust probe invokes
exactly nuschl-ipc's production musicbot sandbox helper and waits for actual exit.
"""
import argparse
import ctypes
import json
import math
from pathlib import Path
import struct
import subprocess

from fake_subsonic import wav


def request(sequence, **payload):
    value = json.dumps({"version": 1, "request_id": sequence, "payload": payload}, separators=(",", ":")).encode()
    return struct.pack(">I", len(value)) + value


def replies(data):
    values = []
    while data:
        assert len(data) >= 4
        size = struct.unpack(">I", data[:4])[0]
        assert 0 < size <= 65536 and len(data) >= 4 + size
        value = json.loads(data[4:4+size])
        assert value["version"] == 1
        values.append(value)
        data = data[4+size:]
    return values


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("probe", type=Path)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--opus-library", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    cases = []

    def run(role, data, success=True):
        result = subprocess.run([str(args.probe.resolve()), "--worker", str(args.binary.resolve()), role],
                                input=data, capture_output=True, timeout=10)
        assert (result.returncode == 0) == success, (role, result.returncode, result.stderr)
        assert len(result.stdout) <= 512 * 1024
        return replies(result.stdout)

    decode = request(1, kind="decode", mime="audio/wav", extension="wav", skip_frames=0)
    bad = run("decode", decode + b"not a media file")
    assert len(bad) == 1 and bad[0]["payload"]["kind"] == "error", bad
    assert len(bad[0]["payload"]["message"].encode()) <= 512
    cases.append("malformed media rejected by actual child")
    good = run("decode", decode + wav(0.04, rate=48000))
    assert len(good) == 3 and good[-1]["payload"]["kind"] == "done", good[-1:]
    for value in good[:-1]:
        assert value["request_id"] == 1 and value["payload"]["kind"] == "pcm"
        samples = value["payload"]["samples"]
        assert len(samples) == 1920 and all(math.isfinite(x) for x in samples)
    cases.append("fresh decode owner returns finite frames then EOF Done")
    assert not run("decode", struct.pack(">I", 65537), success=False)
    cases.append("oversized advertised IPC rejected before body")
    assert not run("decode", request(1, kind="clear"), success=False)
    cases.append("wrong role rejected")
    samples = [0.2 * math.sin(2*math.pi*997*(frame//2)/48000) for frame in range(1920)]
    encoded = run("encode", request(1, kind="encode", samples=samples, bitrate=96000)
                  + request(2, kind="clear")
                  + request(3, kind="encode", samples=samples, bitrate=96000))
    assert len(encoded) == 3 and encoded[1]["payload"]["kind"] == "ack"
    library = ctypes.CDLL(str(args.opus_library))
    library.opus_decoder_create.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.POINTER(ctypes.c_int)]
    library.opus_decoder_create.restype = ctypes.c_void_p
    library.opus_decode_float.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int,
                                         ctypes.POINTER(ctypes.c_float), ctypes.c_int, ctypes.c_int]
    library.opus_decoder_destroy.argtypes = [ctypes.c_void_p]
    error = ctypes.c_int()
    decoder = library.opus_decoder_create(48000, 2, ctypes.byref(error))
    assert decoder and error.value == 0
    try:
        for value in (encoded[0], encoded[2]):
            packet = bytes(value["payload"]["opus"])
            assert 0 < len(packet) <= 992
            output = (ctypes.c_float * 1920)()
            assert library.opus_decode_float(decoder, packet, len(packet), output, 960, 0) == 960
            assert all(math.isfinite(x) for x in output) and max(map(abs, output)) > 0.01
    finally:
        library.opus_decoder_destroy(decoder)
    cases.append("Audio encoder/reset interoperates with independent C libopus")
    controls = run("duck", request(1, kind="clear") + request(2, kind="forget", sender=42))
    assert len(controls) == 2 and all(value["payload"]["kind"] == "ack" for value in controls)
    cases.append("duck control acknowledgements")
    proof = {"cases": cases, "passed": len(cases), "binary": str(args.binary.resolve()),
             "scope": "actual isolated codec roles; synthetic input; not full service/player proof"}
    (args.output / "proof.json").write_text(json.dumps(proof, indent=2) + "\n")
    print(json.dumps(proof))


if __name__ == "__main__":
    main()
