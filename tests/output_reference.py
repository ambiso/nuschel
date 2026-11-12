#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual clipped-device fallback versus independent C SRC and rational clocks.

Device-free ignored Rust fixtures only. The synthetic room uses independently
reconstructed heard output; no microphone/speaker/network/profile is opened.
"""
import argparse
import ctypes
import json
import os
from pathlib import Path
import subprocess

import numpy as np

from output_resample import Speex
from output_spectral import metrics, sources, snr_pass


class Reference(Speex):
    def input_latency(self, rate):
        if rate == 48000: return 0
        error = ctypes.c_int()
        state = self.lib.speex_resampler_init(2, rate, 48000, 3, ctypes.byref(error))
        assert state and error.value == 0
        try:
            self.lib.speex_resampler_get_input_latency.argtypes = [ctypes.c_void_p]
            self.lib.speex_resampler_get_input_latency.restype = ctypes.c_int
            return self.lib.speex_resampler_get_input_latency(state)
        finally:
            self.lib.speex_resampler_destroy(state)


def invoke(binary, name, environment):
    return subprocess.run([str(binary), '--ignored', '--exact', 'live::output_fixture::'+name],
        env=os.environ | environment, capture_output=True, check=True, timeout=60)


def render(binary, source, rate, chunks, root):
    root.mkdir(parents=True, exist_ok=True)
    before, after, reference = root/'input.f64le', root/'device.f32le', root/'reference.f32le'
    np.asarray(source, dtype='<f8').tofile(before)
    invoke(binary, 'dump', {'NUSCHL_OUTPUT_INPUT': str(before), 'NUSCHL_OUTPUT_OUTPUT': str(after),
        'NUSCHL_OUTPUT_REFERENCE': str(reference), 'NUSCHL_OUTPUT_FORMAT': 'f64le',
        'NUSCHL_OUTPUT_RATE': str(rate), 'NUSCHL_OUTPUT_CHUNKS': chunks})
    return (np.fromfile(after, dtype='<f4').reshape(-1, 2),
        np.fromfile(reference, dtype='<f4').reshape(-1, 2),
        json.loads(reference.with_suffix('.json').read_text()))


def clock(stamps, rate, input_latency):
    device = generated = 0
    filter_delay = round(input_latency*1e9/rate)
    for stamp in stamps:
        assert stamp['device_start'] == device
        phase = device*1_000_000_000//rate-generated*1_000_000_000//48000
        delay = max(0, filter_delay+phase)
        assert abs(stamp['reference_delay_ns']-delay) <= 1, stamp
        playback_time = 1_000_000_000+device*1_000_000_000//rate
        assert abs(stamp['reference_time_ns']-(playback_time-delay)) <= 1, stamp
        device += stamp['device_frames']
        # Independently count48k frames from rational device clock, never round
        # individual callback sizes. A one-frame error fails immediately.
        expected = (device*48000+rate-1)//rate
        assert stamp['reference_buffered_frames'] == expected-generated, stamp
        generated = expected
    return {'device_frames': device, 'generated_48k_frames': generated,
        'max_timestamp_error_ns': max(abs(s['reference_delay_ns']-(filter_delay+s['device_start']*1_000_000_000//rate-((s['device_start']*48000+rate-1)//rate)*1_000_000_000//48000)) for s in stamps),
        'callback_count': len(stamps), 'p95_pull_ns': int(np.percentile([s['pull_ns'] for s in stamps], 95)),
        'max_pull_ns': max(s['pull_ns'] for s in stamps)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--converter-test', type=Path, required=True)
    parser.add_argument('--speex-library', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--echo', action='store_true')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    c = Reference(args.speex_library)
    cases = []
    for name, source, _, _, gain in (sources()[0], sources()[-1]):
        source = source[:48000]*gain
        for rate in (11025, 44100, 48000, 96000, 192000):
            outputs = []
            for kind, chunks in (('whole', '8192'), ('fragmented', '1,127,511,37,960,256')):
                root = args.output/f'{name}-{rate}-{kind}'
                actual, reference, stamps = render(args.converter_test, source, rate, chunks, root)
                oracle, _ = c.convert(actual.reshape(-1).tolist(), 48000, input_rate=rate)
                expected = np.clip(np.asarray(oracle, dtype=np.float32).reshape(-1, 2), -1, 1)
                measured = metrics(expected, reference, 48000)
                assert snr_pass(measured, 100), (name, rate, kind, measured)
                timing = clock(stamps, rate, c.input_latency(rate))
                assert timing['generated_48k_frames'] == len(reference)
                outputs.append((actual, reference))
                cases.append({'name': name, 'rate': rate, 'chunks': kind, 'c_reference': measured, 'clock': timing, 'passed': True})
            assert np.array_equal(outputs[0][0], outputs[1][0]), 'device output depends on callback fragmentation'
            assert np.array_equal(outputs[0][1], outputs[1][1]), 'fallback reference depends on callback fragmentation'
    echoes = []
    if args.echo:
        source_file = args.output/'echo-source.f64le'
        invoke(args.converter_test, 'echo_source', {'NUSCHL_ECHO_SOURCE': str(source_file)})
        source = np.fromfile(source_file, dtype='<f8').reshape(-1, 2)
        assert np.max(abs(source)) > 1, 'exercise real local-effect overload'
        for rate in (48000, 44100):
            root = args.output/f'echo-{rate}'
            actual, reference, stamps = render(args.converter_test, source, rate, '127,256,37,960', root)
            heard, _ = c.convert(actual.reshape(-1).tolist(), 48000, input_rate=rate)
            heard = np.clip(np.asarray(heard, dtype=np.float32).reshape(-1, 2), -1, 1)
            # Independent room:40ms direct path plus55/73ms reflections. These
            # are physical fixed delays, not an alignment fitted to output.
            far = (heard[:, 0]+heard[:, 1])*.5
            mic = np.zeros(len(far), dtype=np.float32)
            for delay, level in ((1920, .028), (2640, .009), (3504, -.004)):
                mic[delay:] += far[:-delay]*level
            mic_path, result = root/'mic.f32le', root/'aec.json'
            mic.tofile(mic_path)
            invoke(args.converter_test, 'fallback_echo', {'NUSCHL_ECHO_REFERENCE': str(root/'reference.f32le'),
                'NUSCHL_ECHO_MIC': str(mic_path), 'NUSCHL_ECHO_RESULT': str(result)})
            echoes.append({'rate': rate, 'result': json.loads(result.read_text()),
                'room_truth': 'independent C reverse-SRC of actual clipped device PCM',
                'source_peak': float(np.max(abs(source))), 'clipped_device_samples': int(np.sum(abs(actual) == 1)),
                'clock': clock(stamps, rate, c.input_latency(rate))})
    report = {'passed': True, 'host_devices': False, 'network': False, 'cases': cases, 'echo': echoes,
        'limits': ['Direct production fallback/AEC pipeline with synthetic C room; not an opened CPAL fallback device',
            'Native worker/system-monitor private-Pulse proof is separate', 'No general hardware or non48k AEC reliability claim']}
    (args.output/'proof.json').write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps({'passed': True, 'cases': len(cases), 'echo': echoes}), flush=True)


if __name__ == '__main__':
    main()
