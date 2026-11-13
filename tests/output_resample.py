#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual playback-converter PCM versus Mumble's Speex quality-3 resampler.

Uses an explicitly invoked Rust test fixture, never devices or a live server.
Reference C library is test-only. Every sample comparison keeps requested gain.
"""
import argparse
import ctypes
import json
import math
import os
from pathlib import Path
import struct
import subprocess

from output_quality import amplitude, compare

RATE = 48000


def write_pcm(path, values):
    path.write_bytes(struct.pack(f'<{len(values)}f', *values))


def read_mix_dump(path, frames):
    """`mixer::mixer_fixture::dump` output: interleaved stereo little-endian f64
    frames of the wide mix, before the device's final clip."""
    raw = path.read_bytes()
    if len(raw) != frames * 2 * 8:
        raise ValueError(f'{path}: {len(raw)} bytes are not {frames} stereo f64 frames')
    values = struct.unpack(f'<{frames * 2}d', raw)
    if not all(math.isfinite(v) for v in values):
        raise ValueError(f'{path}: nonfinite sample')
    return values


def device_clip(values):
    """What a 48 kHz device plays from the wide mix: `device_sample` in
    playback.rs clamps to unit amplitude and converts to f32."""
    clipped = [max(-1., min(1., v)) for v in values]
    return struct.unpack(f'<{len(clipped)}f', struct.pack(f'<{len(clipped)}f', *clipped))


def read_pcm(path):
    raw = path.read_bytes()
    assert len(raw) % 8 == 0
    values = struct.unpack(f'<{len(raw) // 4}f', raw)
    assert all(math.isfinite(v) for v in values)
    return values


class Speex:
    def __init__(self, library):
        self.lib = ctypes.CDLL(str(library))
        ptr, uint = ctypes.c_void_p, ctypes.c_uint32
        self.lib.speex_resampler_init.argtypes = [uint, uint, uint, ctypes.c_int, ctypes.POINTER(ctypes.c_int)]
        self.lib.speex_resampler_init.restype = ptr
        self.lib.speex_resampler_destroy.argtypes = [ptr]
        self.lib.speex_resampler_process_interleaved_float.argtypes = [ptr, ctypes.POINTER(ctypes.c_float),
            ctypes.POINTER(uint), ctypes.POINTER(ctypes.c_float), ctypes.POINTER(uint)]
        self.lib.speex_resampler_process_interleaved_float.restype = ctypes.c_int
        self.lib.speex_resampler_get_output_latency.argtypes = [ptr]
        self.lib.speex_resampler_get_output_latency.restype = ctypes.c_int

    def convert(self, pcm, rate, input_rate=RATE):
        if rate == input_rate:
            return list(pcm), 0  # Mumble bypasses SRC when rates match.
        error = ctypes.c_int()
        state = self.lib.speex_resampler_init(2, input_rate, rate, 3, ctypes.byref(error))
        assert state and error.value == 0, error.value
        try:
            source = (ctypes.c_float * len(pcm))(*pcm)
            capacity = len(pcm) // 2 * rate // input_rate + 4096
            output = (ctypes.c_float * (capacity * 2))()
            frames, produced = ctypes.c_uint32(len(pcm) // 2), ctypes.c_uint32(capacity)
            result = self.lib.speex_resampler_process_interleaved_float(state, source, ctypes.byref(frames),
                output, ctypes.byref(produced))
            assert result == 0 and frames.value == len(pcm) // 2, (result, frames.value)
            return list(output[:produced.value * 2]), self.lib.speex_resampler_get_output_latency(state)
        finally:
            self.lib.speex_resampler_destroy(state)


def tone(frequency, seconds=1):
    return [v for i in range(round(RATE * seconds)) for v in
            (.2 * math.sin(2 * math.pi * frequency * i / RATE),
             .13 * math.sin(2 * math.pi * frequency * i / RATE + .71))]


def level(pcm, frequency, rate):
    stable = pcm[rate // 5 * 2:rate // 5 * 8:2]
    return 20 * math.log10(max(amplitude(stable, frequency, rate) / .2, 1e-20))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--test-binary', required=True, type=Path)
    parser.add_argument('--engine', choices=['converter','sonora','speex-rust'], default='converter')
    parser.add_argument('--speex-library', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    parser.add_argument('--require-reference',action='store_true',help='gate this source-matched adapter against fixed-gain stereo reference PCM')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    speex = Speex(args.speex_library)
    cases = []
    for rate in [11025, 22050, 44100, 48000, 96000]:
        descriptions = [('passband-997', 997, 997), ('passband-high', .35 * min(RATE, rate), .35 * min(RATE, rate))]
        if rate < RATE:
            source = min(23000, rate * .75)
            descriptions.append(('alias', source, abs(source - rate * round(source / rate))))
        elif rate > RATE:
            descriptions.append(('image', 16800, RATE - 16800))
        if rate in (11025, 22050):
            descriptions.append(('clock-997', 997, 997))
        impulse = [0.] * (RATE * 2)
        impulse[4800 * 2], impulse[4800 * 2 + 1] = .2, -.13
        peak_positions = {}
        for name, frequency, measured in [*descriptions, ('impulse', 0, 0)]:
            seconds = 10 if name == 'clock-997' else 1
            source = impulse if name == 'impulse' else tone(frequency, seconds)
            input_path = args.output / f'{rate}-{name}-input.f32le'
            write_pcm(input_path, source)
            results = []
            for suffix, chunks in [('contiguous', '8192'), ('irregular', '1,127,511,37,960,256')]:
                output = args.output / f'{rate}-{name}-{suffix}.f32le'
                env = {**os.environ, 'NUSCHL_OUTPUT_INPUT': str(input_path), 'NUSCHL_OUTPUT_OUTPUT': str(output),
                       'NUSCHL_OUTPUT_RATE': str(rate), 'NUSCHL_OUTPUT_CHUNKS': chunks}
                command = ([str(args.test_binary), '--ignored', '--exact', 'live::output_fixture::dump']
                    if args.engine == 'converter' else [str(args.test_binary), str(input_path), str(output), str(rate), chunks])
                subprocess.run(command, env=env, capture_output=True, check=True, timeout=20)
                pcm = read_pcm(output)
                assert len(pcm) == rate * seconds * 2, (rate, len(pcm))
                results.append(pcm)
            assert results[0] == results[1], (rate, name, 'callback boundaries changed PCM')
            expected, latency = speex.convert(source, rate)
            if name == 'impulse':
                peak = max(range(len(results[0]) // 2), key=lambda i: abs(results[0][i * 2]))
                reference_peak = max(range(len(expected) // 2), key=lambda i: abs(expected[i * 2]))
                peak_positions = {'converter_peak_frame': peak, 'reference_peak_frame': reference_peak,
                    'source_impulse_time_seconds': .1, 'converter_delay_seconds': peak / rate - .1,
                    'reference_delay_seconds': reference_peak / rate - .1, 'reference_reported_delay_frames': latency}
            else:
                actual_db, reference_db = level(results[0], measured, rate), level(expected, measured, rate)
                cases.append({'rate': rate, 'case': name, 'source_frequency': frequency,
                    'measured_frequency': measured, 'converter_db': actual_db, 'reference_db': reference_db,
                    'delta_db': actual_db - reference_db, 'chunk_exact': True, 'duration_seconds': seconds,
                    'full_fixed_gain_reference': compare(expected[:len(results[0])],results[0])})
        cases.append({'rate': rate, 'case': 'delay', **peak_positions, 'chunk_exact': True})
    gated = [case['full_fixed_gain_reference'] for case in cases if 'full_fixed_gain_reference' in case]
    passed = all(case['fixed_gain_snr_db'] >= 100 and case['max_abs_error'] <= 2e-6 for case in gated)
    report = {'reference_gate_passed': passed, 'engine': args.engine, 'test_binary': str(args.test_binary), 'reference_library': str(args.speex_library),
        'reference_quality': 3, 'host_devices': False, 'gain': 1, 'cases': cases,
        'limits': ['Source-matched Speex resampler comparison, not full reference-Mumble output.',
                   'No subjective/perceptual or hardware-quality guarantee.']}
    (args.output / 'proof.json').write_text(json.dumps(report, indent=2) + '\n')
    print(json.dumps(report), flush=True)
    if args.require_reference and not passed:
        raise SystemExit(1)


if __name__ == '__main__':
    main()
