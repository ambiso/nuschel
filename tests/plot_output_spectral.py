#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Create a compact shareable summary from real output_spectral.py artifacts."""
import argparse
import hashlib
import json
from pathlib import Path
import struct

import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
import numpy as np

from output_spectral import stft


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--evidence", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    report = json.loads((args.evidence/"proof.json").read_text())

    def read(relative):
        raw = (args.evidence/relative).read_bytes()
        assert hashlib.sha256(raw).hexdigest() == report['artifacts'][relative]['sha256']
        assert raw[:4] == b'RIFF' and raw[8:16] == b'WAVEfmt ' and raw[36:40] == b'data'
        assert struct.unpack('<H', raw[20:22])[0] == 3
        return np.frombuffer(raw[44:], dtype='<f4').reshape(-1, 2)

    def spectrum(values, rate):
        transformed, frequency, _ = stft(values, rate)
        return frequency/1000, 20*np.log10(np.maximum(np.sqrt(np.mean(abs(transformed[:, 0])**2, axis=0)), 1e-10))

    fig, axes = plt.subplots(2, 2, figsize=(12, 8), constrained_layout=True)
    actual = read('stereo-music/44100-actual.wav')
    expected = read('stereo-music/44100-reference.wav')
    for label, values, style in [('C reference', expected, '-'), ('Actual native', actual, '--'), ('Native minus C', actual-expected, '-')]:
        frequency, levels = spectrum(values, 44100)
        axes[0, 0].plot(frequency, levels, style, label=label, linewidth=1)
    axes[0, 0].set(title='Stereo music · actual 44.1kHz output · left channel', ylabel='RMS STFT amplitude/bin (dBFS)', xlabel='Frequency (kHz)', ylim=(-190, 0), xlim=(0, 22.05))
    axes[0, 0].legend(fontsize=8)
    source = read('speech-like/aligned-source.wav')
    decoded = read('speech-like/aligned-c-decoded.wav')
    for label, values in [('Original synthetic speech-like', source), ('C decoded minus input (lossy)', decoded-source)]:
        frequency, levels = spectrum(values, 48000)
        axes[0, 1].plot(frequency, levels, label=label, linewidth=1)
    axes[0, 1].set(title='Separate codec loss · independent C Audio 48kb/s', ylabel='RMS STFT amplitude/bin (dBFS)', xlabel='Frequency (kHz)', ylim=(-140, 0), xlim=(0, 22.05))
    axes[0, 1].legend(fontsize=8)
    names = [row['name'] for row in report['cases']]
    x = np.arange(len(names))
    for index, rate in enumerate((48000, 44100)):
        values = [next(r for r in row['rates'] if r['rate'] == rate)['stock_clip_order_diagnostic']['fixed_gain_snr_db'] for row in report['cases']]
        axes[1, 0].bar(x+(index-.5)*.35, values, width=.35, label=f'{rate}Hz')
    axes[1, 0].axhline(35, color='red', linestyle=':', label='35dB numerical gate')
    axes[1, 0].set(xticks=x, xticklabels=['Multiband', 'Transients', 'Speech-like', 'Music', 'Overload'], ylabel='Fixed-gain signal/error (dB)', ylim=(0, 210), title='C components in stock clip order vs actual native')
    axes[1, 0].legend(fontsize=8)
    actual = read('overload-staging/44100-actual.wav')
    expected = read('overload-staging/44100-reference.wav')
    start, end = round(.7*44100), round(.706*44100)
    time = np.arange(start, end)/44.1
    axes[1, 1].plot(time, expected[start:end, 0], label='Filter then clip (C)', linewidth=1)
    axes[1, 1].plot(time, actual[start:end, 0], label='Clip then filter (native)', linewidth=1, alpha=.8)
    axes[1, 1].set(title='Measured overload gap · 44.1kHz · left channel', xlabel='Fixed output clock (ms)', ylabel='Sample amplitude', ylim=(-1.1, 1.1))
    axes[1, 1].legend(fontsize=8)
    for ax in axes.flat: ax.grid(alpha=.2)
    fig.suptitle('Native receive/output spectral audit · synthetic/offline · fixed gain\nIdentical-packet implementation error is separate from lossy input/codec error', fontsize=13)
    fig.savefig(args.output, dpi=140)
    print(json.dumps({'output': str(args.output), 'sha256': hashlib.sha256(args.output.read_bytes()).hexdigest(), 'source_report_sha256': hashlib.sha256((args.evidence/'proof.json').read_bytes()).hexdigest()}))


if __name__ == '__main__':
    main()
