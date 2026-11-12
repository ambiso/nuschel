#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual device-free native capture SRC versus independent C Speex quality3."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np

from output_resample import Speex
from output_spectral import metrics, snr_pass, stft, write_wav


def level(pcm, frequency):
    stable = pcm[9600:38400]
    phase = 2*np.pi*frequency*np.arange(len(stable))/48000
    return 2*abs(np.sum(stable*np.exp(-1j*phase)))/len(stable)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--test-binary', type=Path, required=True)
    parser.add_argument('--speex-library', type=Path, required=True)
    parser.add_argument('--source-revision', required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    c = Speex(args.speex_library)
    rows = []
    for rate in (11025, 22050, 44100, 48000, 96000):
        t = np.arange(rate)/rate
        frequencies = [('low', 997.), ('high', .35*min(rate, 48000))]
        if rate > 48000: frequencies.append(('alias', 36000.))
        for name, frequency, channels in [*(('tone-'+name, f, 1) for name, f in frequencies),
                                         ('impulse', 0., 1), ('channel-average2', 997., 2), ('channel-average6', 997., 6)]:
            if name == 'impulse':
                mono = np.zeros(rate); mono[rate//10] = .5
            else:
                mono = .2*np.sin(2*np.pi*frequency*t)
            weights = np.array([1., .5, -.25, .75, -.5, .2][:channels])
            source = np.asarray(mono[:, None]*weights, dtype='<f4')
            average = np.clip(source.astype(float).mean(axis=1), -1, 1).astype(np.float32)
            # Existing C helper uses2 identical channels: verify they remain
            # identical, then take one. There is no reference channel mixing.
            reference, latency = c.convert(np.repeat(average, 2).tolist(), 48000, input_rate=rate)
            reference = np.asarray(reference, dtype=np.float32).reshape(-1, 2)
            assert np.array_equal(reference[:, 0], reference[:, 1])
            reference = np.clip(reference[:, 0], -1, 1)
            stem = f'{rate}-{name}'
            before = args.output/(stem+'-source.f32le'); source.tofile(before)
            outputs = []
            metadata = []
            for kind, chunks in [('whole', '8192'), ('fragmented', '1,127,480,37,256')]:
                after = args.output/(stem+'-'+kind+'.f32le')
                env = os.environ | {'NUSCHL_CAPTURE_INPUT': str(before), 'NUSCHL_CAPTURE_OUTPUT': str(after),
                    'NUSCHL_CAPTURE_CHANNELS': str(channels), 'NUSCHL_CAPTURE_RATE': str(rate), 'NUSCHL_CAPTURE_CHUNKS': chunks}
                subprocess.run([str(args.test_binary), '--ignored', '--exact', 'live::capture_fixture::dump'],
                    env=env, capture_output=True, check=True, timeout=15)
                outputs.append(np.fromfile(after, dtype='<f4'))
                metadata.append(json.loads(after.with_suffix('.json').read_text()))
            assert np.array_equal(outputs[0], outputs[1]), (stem, 'callback fragmentation changes PCM')
            assert len(outputs[0]) == 48000, (stem, len(outputs[0]))
            measured = metrics(np.repeat(reference[:, None], 2, axis=1), np.repeat(outputs[0][:, None], 2, axis=1), 48000)
            row = {'rate': rate, 'case': name, 'channels': channels, 'source_frequency': frequency,
                'frames': len(outputs[0]), 'chunk_bit_exact': True, 'full_fixed_gain_reference': measured,
                'reference_delay_output_frames': latency, 'passed': snr_pass(measured, 100) and measured['max_abs_error'] <= 2e-6,
                'first_fragment_queued_frames': metadata[1][0]['queued'],
                'actual_initial_peak': int(np.argmax(abs(outputs[0]))) if name == 'impulse' else None,
                'reference_initial_peak': int(np.argmax(abs(reference))) if name == 'impulse' else None}
            if name != 'impulse':
                folded = abs((rate-frequency+24000)%48000-24000)
                input_level = .2*abs(float(weights.mean()))
                db = lambda x: float(20*np.log10(max(x/input_level, 1e-15)))
                row.update({'carrier_db': db(level(outputs[0], frequency)), 'reference_carrier_db': db(level(reference, frequency)),
                    'image_or_alias_frequency': folded if rate < 48000 else 12000 if name == 'tone-alias' else None})
                if row['image_or_alias_frequency'] is not None:
                    f = row['image_or_alias_frequency']
                    row.update({'image_or_alias_db': db(level(outputs[0], f)), 'reference_image_or_alias_db': db(level(reference, f))})
            rows.append(row)
            if name == 'tone-high' and rate in (11025, 44100, 96000):
                fig, ax = plt.subplots(figsize=(10, 4), constrained_layout=True)
                for label, pcm in [('C Speex quality3', reference), ('Actual capture converter', outputs[0]), ('Residual', outputs[0]-reference)]:
                    transformed, frequencies, _ = stft(np.repeat(pcm[:, None], 2, axis=1), 48000)
                    spectrum = 20*np.log10(np.maximum(np.sqrt(np.mean(abs(transformed[:, 0])**2, axis=0)), 1e-10))
                    ax.plot(frequencies/1000, spectrum, label=label, linewidth=1)
                ax.set(title=f'Actual capture {rate}→48000Hz · no fitted gain/delay', xlabel='Frequency(kHz)', ylabel='RMS STFT amplitude/bin(dBFS)', xlim=(0, 24), ylim=(-170, 0))
                ax.legend(); ax.grid(alpha=.2); fig.savefig(args.output/(stem+'.png'), dpi=140); plt.close(fig)
                write_wav(args.output/(stem+'-actual.wav'), np.repeat(outputs[0][:, None], 2, axis=1), 48000)
                write_wav(args.output/(stem+'-reference.wav'), np.repeat(reference[:, None], 2, axis=1), 48000)
    sha = lambda p: hashlib.sha256(Path(p).read_bytes()).hexdigest()
    report = {'passed': all(row['passed'] for row in rows), 'source_revision': args.source_revision,
        'host_devices': False, 'network': False, 'gain_fitted': False, 'delay_fitted': False,
        'reference': {'library': str(args.speex_library), 'sha256': sha(args.speex_library), 'quality': 3,
            'mumble_source': '35dce9e4801fe52b2217f0d9f421d17b0aaf3bb0/src/mumble/AudioInput.cpp:515,587',
            'scope': 'C SRC policy; not full Mumble capture/DSP/encoder or its PCM16 quantization'},
        'test_binary': {'path': str(args.test_binary), 'sha256': sha(args.test_binary)}, 'cases': rows,
        'artifacts': {p.name: {'bytes': p.stat().st_size, 'sha256': sha(p)} for p in args.output.iterdir() if p.is_file() and p.suffix in ('.png', '.wav')},
        'limits': ['Synthetic actual converter only; not microphone startup, native TX or hardware quality',
            'No fitted alignment; steady carrier/image metrics isolate filter response from whole-PCM group-delay differences']}
    (args.output/'proof.json').write_text(json.dumps(report, indent=2)+'\n')
    print(json.dumps({'passed': report['passed'], 'cases': len(rows), 'failed': sum(not row['passed'] for row in rows)}))
    if not report['passed']: raise SystemExit(1)


if __name__ == '__main__':
    main()
