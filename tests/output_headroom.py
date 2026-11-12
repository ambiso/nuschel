#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Independent fixed-gain, two-stream mix check. TEST-ONLY reference libopus.

The native diagnostic runs the actual decoder/jitter/mixer on a virtual clock;
the JS diagnostic executes the real PlaybackProcessor without devices/network.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess

from output_quality import Reference, compare, overshoot_fixture
from output_resample import read_pcm, write_pcm


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--test-binary', required=True, type=Path)
    parser.add_argument('--opus-library', required=True, type=Path)
    parser.add_argument('--output', required=True, type=Path)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    reference = Reference(args.opus_library)
    source = overshoot_fixture(1)
    streams = []
    decoded = []
    for index, (sign, volume) in enumerate([(1, 3.), (-1, 2.5)]):
        packets = reference.encode([sign * value for value in source], 2)
        assert max(map(len, packets)) <= 1024
        pcm = reference.decode(packets, 2, phase_inversion_disabled=True)
        path = args.output / f'speaker-{index}.f32le'
        write_pcm(path, pcm)
        streams.append({'packets': [list(packet) for packet in packets], 'volume': volume, 'pcm': str(path.resolve())})
        decoded.append(pcm)
    expected = [max(-1., min(1., a * 3. + b * 2.5)) for a, b in zip(*decoded)]
    expected_path = args.output / 'independent-final-sum.f32le'
    write_pcm(expected_path, expected)
    # Independent old algorithm is a negative control, not a fitted reference.
    old = [max(-1., min(1., max(-1., min(1., a * 3.)) + max(-1., min(1., b * 2.5)))) for a, b in zip(*decoded)]
    fixture = args.output / 'fixture.json'
    fixture.write_text(json.dumps(streams))
    output = args.output / 'native.f32le'
    subprocess.run([str(args.test_binary), '--ignored', '--exact', 'mixer::mixer_fixture::dump'],
        env={**os.environ, 'NUSCHL_MIX_INPUT': str(fixture.resolve()), 'NUSCHL_MIX_OUTPUT': str(output.resolve())},
        capture_output=True, check=True, timeout=20)
    actual = read_pcm(output)
    measured = compare(expected, actual)
    negative = compare(expected, old)
    assert measured['fixed_gain_snr_db'] >= 40, measured
    assert negative['fixed_gain_snr_db'] < 10, negative
    browser_path = args.output / 'worklet-proof.json'
    subprocess.run(['node', 'tests/web_headroom.mjs', str(fixture), str(expected_path), str(browser_path)], check=True, timeout=30)
    result = {'reference': str(args.opus_library), 'fixed_gain': True, 'virtual_clock_no_loss': True,
              'source_legal_full_scale': True, 'source_peak': max(map(abs, source)),
              'decoded_stream_peaks': [max(map(abs, pcm)) for pcm in decoded], 'native': measured,
              'negative_old_per_speaker_clip': negative, 'worklet': json.loads(browser_path.read_text()),
              'limits': ['Component sum proof, not full live multispeaker/device or jitter/loss parity.',
                         'Explicit stock Mumble phase-inversion setting; full device/loss parity remains separate.']}
    (args.output / 'proof.json').write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps(result))


if __name__ == '__main__':
    main()
