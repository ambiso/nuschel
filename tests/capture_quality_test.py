#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Negative controls use actual baseline/current PCM and independent C mono SRC."""
import argparse
from pathlib import Path
import numpy as np
from capture_quality import MonoReference, acceptance, level
from output_spectral import metrics


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline', type=Path, required=True)
    parser.add_argument('--filtered', type=Path, required=True)
    parser.add_argument('--speex-library', type=Path, required=True)
    args = parser.parse_args()
    c = MonoReference(args.speex_library)
    source = np.fromfile(args.filtered/'96000-tone-alias-source.f32le', dtype='<f4')
    reference, _ = c.mono(source, 96000)
    input_rms = float(np.sqrt(np.mean(source.astype(float)**2)))
    reference_db = 20*np.log10(level(reference,12000)/.2)
    def check(pcm):
        measured = metrics(np.repeat(reference[:,None],2,axis=1),np.repeat(pcm[:,None],2,axis=1),48000)
        return acceptance(measured,input_rms,stopband=True,alias_db=float(20*np.log10(max(level(pcm,12000)/.2,1e-15))),reference_alias_db=reference_db)
    actual = np.fromfile(args.filtered/'96000-tone-alias-whole.f32le',dtype='<f4')
    current = check(actual)
    assert current['passed'] and not current['output_relative_100db_diagnostic'], current
    original = check(np.fromfile(args.baseline/'96000-tone-alias-whole.f32le',dtype='<f4'))
    assert not original['passed'] and not original['alias_below_minus90db_gate']
    injected = check(actual+.2e-3*np.sin(2*np.pi*12000*np.arange(48000)/48000))
    assert not injected['passed'] and not injected['input_relative_120db_gate'] and not injected['alias_no_more_than_C_plus_half_db_gate']
    ordinary_source = np.fromfile(args.filtered/'44100-tone-low-source.f32le',dtype='<f4')
    ordinary, _ = c.mono(ordinary_source,44100)
    zero = metrics(np.repeat(ordinary[:,None],2,axis=1),np.zeros((len(ordinary),2)),48000)
    assert not acceptance(zero,float(np.sqrt(np.mean(ordinary_source.astype(float)**2))))['passed']
    print('4 real-PCM negative controls passed: original alias, injected leakage, silence, retained strict diagnostic')


if __name__ == '__main__': main()
