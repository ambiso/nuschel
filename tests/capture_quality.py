#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual device-free native capture SRC versus independent C Speex quality3."""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import subprocess

import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np

from output_reference import Reference as Speex
from output_spectral import metrics, snr_pass, stft, write_wav


def level(pcm, frequency):
    stable = pcm[9600:38400]
    phase = 2*np.pi*frequency*np.arange(len(stable))/48000
    return 2*abs(np.sum(stable*np.exp(-1j*phase)))/len(stable)


class MonoReference(Speex):
    def mono(self, source, rate):
        error = ctypes.c_int()
        state = self.lib.speex_resampler_init(1, rate, 48000, 3, ctypes.byref(error))
        assert state and error.value == 0
        self.lib.speex_resampler_process_float.argtypes = [ctypes.c_void_p, ctypes.c_uint32,
            ctypes.POINTER(ctypes.c_float), ctypes.POINTER(ctypes.c_uint32),
            ctypes.POINTER(ctypes.c_float), ctypes.POINTER(ctypes.c_uint32)]
        self.lib.speex_resampler_process_float.restype = ctypes.c_int
        try:
            pcm = np.ascontiguousarray(source, dtype='<f4')
            output = np.empty(len(pcm)*48000//rate+4096, dtype='<f4')
            consumed, produced = ctypes.c_uint32(len(pcm)), ctypes.c_uint32(len(output))
            if rate == 48000: return pcm.copy(), 0
            status = self.lib.speex_resampler_process_float(state, 0,
                pcm.ctypes.data_as(ctypes.POINTER(ctypes.c_float)), ctypes.byref(consumed),
                output.ctypes.data_as(ctypes.POINTER(ctypes.c_float)), ctypes.byref(produced))
            assert status == 0 and consumed.value == len(pcm)
            return output[:produced.value].copy(), self.lib.speex_resampler_get_output_latency(state)
        finally:
            self.lib.speex_resampler_destroy(state)


def acceptance(measured, input_rms, *, stopband=False, alias_db=None, reference_alias_db=None):
    # Relative output SNR is ill-conditioned after a strong intentional stopband.
    # Preserve that raw diagnostic; use a stricter fixed-input error budget plus
    # two explicit attenuation rails ONLY for the declared36k->12k fixture.
    output_gate = snr_pass(measured, 100)
    input_snr = 20*np.log10(input_rms)-measured['rms_error_dbfs']
    maximum = measured['max_abs_error'] <= 2e-6
    rails = {'output_relative_100db_diagnostic': output_gate,
        'max_abs_error_2e_minus6_gate': maximum, 'input_referenced_snr_db': float(input_snr)}
    if stopband:
        rails.update({'input_relative_120db_gate': bool(input_snr >= 120),
            'alias_below_minus90db_gate': bool(alias_db <= -90),
            'alias_no_more_than_C_plus_half_db_gate': bool(alias_db <= reference_alias_db+0.5)})
        rails['passed'] = maximum and all(rails[key] for key in
            ('input_relative_120db_gate','alias_below_minus90db_gate','alias_no_more_than_C_plus_half_db_gate'))
    else: rails['passed'] = maximum and output_gate
    return rails


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--test-binary', type=Path, required=True)
    parser.add_argument('--speex-library', type=Path, required=True)
    parser.add_argument('--source-revision', required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--filtered', action='store_true', help='also require actual filtered capture timestamps and rational count')
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    c = MonoReference(args.speex_library)
    rows = []
    for rate in (11025, 22050, 44100, 48000, 96000):
        t = np.arange(rate)/rate
        frequencies = [('low', 997.), ('high', .35*min(rate, 48000))]
        if rate > 48000: frequencies.append(('alias', 36000.))
        for name, frequency, channels in [*(('tone-'+name, f, 1) for name, f in frequencies),
                                         ('impulse', 0., 1), ('initial-marker', 997., 1), ('channel-average2', 997., 2), ('channel-average6', 997., 6)]:
            if name == 'impulse':
                mono = np.zeros(rate); mono[rate//10] = .5
            elif name == 'initial-marker':
                mono = .2*np.where(t < .08, np.sin(2*np.pi*997*t), np.sin(2*np.pi*3000*t))
            else:
                mono = .2*np.sin(2*np.pi*frequency*t)
            weights = np.array([1., .5, -.25, .75, -.5, .2][:channels])
            source = np.asarray(mono[:, None]*weights, dtype='<f4')
            average = np.clip(source.astype(float).mean(axis=1), -1, 1).astype(np.float32)
            reference, latency = c.mono(average, rate)
            reference = np.clip(reference, -1, 1)
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
            clock_error = 0
            if args.filtered:
                for stamps in metadata:
                    generated = 0
                    filter_delay = round(c.input_latency(rate)*1e9/rate)
                    for stamp in stamps:
                        raw_capture = 1_000_000_000+stamp['device_start']*1_000_000_000//rate
                        assert stamp['raw_capture_time_ns'] == raw_capture
                        phase = stamp['device_start']*1_000_000_000//rate-generated*1_000_000_000//48000
                        expected_time = raw_capture-max(0, filter_delay+phase)
                        difference = abs(stamp['capture_time_ns']-expected_time)
                        assert difference <= 1, (stem, stamp, expected_time)
                        clock_error = max(clock_error, difference)
                        expected_count = ((stamp['device_start']+stamp['device_frames'])*48000+rate-1)//rate
                        assert stamp['queued'] == expected_count-generated, (stem, stamp)
                        generated = expected_count
                if name == 'impulse':
                    peak = int(np.argmax(abs(outputs[1])))
                    generated = 0
                    represented = None
                    for stamp in metadata[1]:
                        if generated <= peak < generated+stamp['queued']:
                            represented = stamp['capture_time_ns']+(peak-generated)*1_000_000_000//48000
                            break
                        generated += stamp['queued']
                    actual_instant = 1_000_000_000+(rate//10)*1_000_000_000//rate
                    assert represented is not None and abs(represented-actual_instant) <= 1_000_000_000//48000+2, (rate, represented, actual_instant)
            row = {'rate': rate, 'case': name, 'channels': channels, 'source_frequency': frequency,
                'frames': len(outputs[0]), 'chunk_bit_exact': True, 'full_fixed_gain_reference': measured,
                'reference_delay_output_frames': latency, 'max_timestamp_error_ns':clock_error if args.filtered else None, 'passed': snr_pass(measured, 100) and measured['max_abs_error'] <= 2e-6,
                'first_fragment_queued_frames': metadata[1][0]['queued'],
                'actual_initial_peak': int(np.argmax(abs(outputs[0]))) if name == 'impulse' else None,
                'reference_initial_peak': int(np.argmax(abs(reference))) if name == 'impulse' else None}
            if name == 'initial-marker':
                def power(values, frequency):
                    phase = 2*np.pi*frequency*np.arange(len(values))/48000
                    return float(abs(np.sum(values*np.exp(-1j*phase)))**2)
                beginning = outputs[0][latency:960]
                following = outputs[0][5760:9600]
                first_ratio = power(beginning,997)/max(power(beginning,3000),1e-30)
                later_ratio = power(following,3000)/max(power(following,997),1e-30)
                assert first_ratio > 20 and later_ratio > 20, (rate, first_ratio, later_ratio)
                row['initial_marker_power_ratio'] = first_ratio
                row['following_marker_power_ratio'] = later_ratio
            if name not in ('impulse','initial-marker'):
                folded = abs((rate-frequency+24000)%48000-24000)
                input_level = .2*abs(float(weights.mean()))
                db = lambda x: float(20*np.log10(max(x/input_level, 1e-15)))
                row.update({'carrier_db': db(level(outputs[0], frequency)), 'reference_carrier_db': db(level(reference, frequency)),
                    'image_or_alias_frequency': folded if rate < 48000 else 12000 if name == 'tone-alias' else None})
                if row['image_or_alias_frequency'] is not None:
                    f = row['image_or_alias_frequency']
                    row.update({'image_or_alias_db': db(level(outputs[0], f)), 'reference_image_or_alias_db': db(level(reference, f))})
            rails = acceptance(measured, float(np.sqrt(np.mean(average.astype(float)**2))),
                stopband=name == 'tone-alias', alias_db=row.get('image_or_alias_db'), reference_alias_db=row.get('reference_image_or_alias_db'))
            row['quality_gate'] = rails
            row['passed'] = rails['passed']
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
    report = {'passed': all(row['passed'] for row in rows), 'filtered_clock_gate':args.filtered, 'source_revision': args.source_revision,
        'host_devices': False, 'network': False, 'gain_fitted': False, 'delay_fitted': False,
        'all_output_relative_100db_diagnostics_passed':all(row['quality_gate']['output_relative_100db_diagnostic'] for row in rows),
        'reference': {'channels':1, 'library': str(args.speex_library), 'sha256': sha(args.speex_library), 'quality': 3,
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
