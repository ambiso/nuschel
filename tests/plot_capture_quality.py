#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Plot actual before/after capture PCM against independent C mono SRC."""
import argparse
import hashlib
import json
from pathlib import Path
import matplotlib
matplotlib.use('Agg')
import matplotlib.pyplot as plt
import numpy as np
from capture_quality import MonoReference
from output_spectral import stft


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline',type=Path,required=True)
    parser.add_argument('--filtered',type=Path,required=True)
    parser.add_argument('--speex-library',type=Path,required=True)
    parser.add_argument('--output',type=Path,required=True)
    args = parser.parse_args()
    c = MonoReference(args.speex_library)
    fig, axes = plt.subplots(3,1,figsize=(11,10),constrained_layout=True)
    hashes={}
    for ax,(rate,name) in zip(axes,[(11025,'tone-high'),(44100,'tone-high'),(96000,'tone-alias')]):
        stem=f'{rate}-{name}'
        original=args.baseline/(stem+'-source.f32le')
        source=args.filtered/(stem+'-source.f32le')
        assert original.read_bytes()==source.read_bytes(), 'before/after source differs'
        reference,_=c.mono(np.fromfile(source,dtype='<f4'),rate)
        old=np.fromfile(args.baseline/(stem+'-whole.f32le'),dtype='<f4')
        actual=np.fromfile(args.filtered/(stem+'-whole.f32le'),dtype='<f4')
        for label,values,style in [('Before: linear',old,'-'),('After: filtered',actual,'-'),('Independent C quality3',reference,'--'),('After minus C',actual-reference,':')]:
            transformed,frequency,_=stft(np.repeat(values[9600:38400,None],2,axis=1),48000)
            levels=20*np.log10(np.maximum(np.sqrt(np.mean(abs(transformed[:,0])**2,axis=0)),1e-10))
            ax.plot(frequency/1000,levels,style,label=label,linewidth=1)
        ax.set(title=f'{rate}→48000Hz · '+('36kHz input above target Nyquist' if name=='tone-alias' else 'upper passband and resampling images'),
            xlabel='Frequency(kHz)',ylabel='RMS STFT amplitude/bin(dBFS)',xlim=(0,24),ylim=(-170,0))
        ax.grid(alpha=.2);ax.legend(fontsize=9)
        for p in [original,source,args.baseline/(stem+'-whole.f32le'),args.filtered/(stem+'-whole.f32le')]:
            hashes[str(p)]=hashlib.sha256(p.read_bytes()).hexdigest()
    fig.suptitle('Actual native capture converter · fixed gain · no fitted delay\nSteady0.2–0.8s spectrum; full-wave/clock tests are reported separately',fontsize=13)
    fig.savefig(args.output,dpi=140)
    print(json.dumps({'output':str(args.output),'sha256':hashlib.sha256(args.output.read_bytes()).hexdigest(),'input_hashes':hashes},indent=2))


if __name__=='__main__':main()
