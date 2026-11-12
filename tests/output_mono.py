#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual negotiated mono-sink clip/fold check, independent reference libopus.

Stock Mumble35dce9e: AudioOutput.cpp430 sets mono weights0.5/0.5;
827 maps/sums un-clipped stereo and856 clips the final device output.
Only private Pulse/null sinks; explicit forced backend/muted microphone.
"""
import argparse
import json
import math
from pathlib import Path
import tempfile

from output_quality import Reference, PrivatePulse, playback, compare, delay_only, RATE


def fixture(loud):
    result = []
    for index in range(2 * RATE):
        phase = 2 * math.pi * 997 * index / RATE
        if index < RATE // 5:
            left = right = 0.
        elif index < RATE * 7 // 10:
            # Shared quiet marker survives either clip/fold order. Existing
            # delay_only's fixed correlation interval lies wholly inside it.
            left = right = .04 * (math.sin(phase) + .3 * math.sin(2 * math.pi * 613 * index / RATE))
        elif loud:
            left = .99 if math.sin(phase) >= 0 else -.99
            right = -left * (.825 / .99)
        else:
            left = .2 * math.sin(phase)
            right = -.1 * math.sin(phase)
        result.extend((left, right))
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--worker', type=Path, required=True)
    parser.add_argument('--pulse-bin', type=Path, required=True)
    parser.add_argument('--opus-library', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    parser.add_argument('--expect-mismatch', action='store_true', help='frozen baseline only; records the failure without pretending parity')
    args = parser.parse_args()
    args.output.mkdir(parents=True,exist_ok=True)
    reference = Reference(args.opus_library)
    cases=[]
    with tempfile.TemporaryDirectory(prefix='mono-',dir=args.output) as temporary:
        pulse=PrivatePulse(args.pulse_bin,Path(temporary),RATE,channels=1)
        try:
            sinks=pulse.ctl('list','sinks',json_output=True)
            for name, loud, volume in [('quiet-control',False,1.),('decoded-overshoot',True,1.),('opposed-with-gain',True,4.)]:
                source=fixture(loud)
                packets=reference.encode(source,2)
                decoded=reference.decode(packets,2,phase_inversion_disabled=True)
                assert max(map(len,packets))<=1024
                clip=lambda v:max(-1.,min(1.,v))
                expected=[clip((l+r)*.5*volume) for l,r in zip(decoded[::2],decoded[1::2])]
                old=[(clip(l*volume)+clip(r*volume))*.5 for l,r in zip(decoded[::2],decoded[1::2])]
                specifications=[]
                actual=playback(args.worker,pulse,packets,args.output/f'{name}.f32le',volume,
                    specifications=specifications,monitor_channels=1)
                assert specifications and all('1ch' in spec and '48000Hz' in spec for spec in specifications), specifications
                lag=delay_only(expected,actual)
                start,end=RATE*9//10,len(expected)-RATE//10
                observed=actual[lag:lag+len(expected)]
                assert len(observed)==len(expected)
                metrics=compare(expected[start:end],observed[start:end])
                old_match=compare(old[start:end],observed[start:end])
                cases.append({'name':name,'volume':volume,'decoded_peak':max(map(abs,decoded)),
                    'source_peak':max(map(abs,source)), 'delay_samples':lag,'negotiated':specifications,
                    'reference':metrics,'old_clip_before_fold':old_match,'passed':metrics['fixed_gain_snr_db']>=35})
        finally:
            pulse.stop()
    result={'worker':str(args.worker),'reference_library':str(args.opus_library),'phase_inversion_disabled':True,
        'stock_revision':'35dce9e4801fe52b2217f0d9f421d17b0aaf3bb0',
        'stock_order':'LR0.5/0.5 mapping and summation, then output clip',
        'sink_channels':1,'sink_rate':RATE,'sink_specifications':[sink['sample_specification'] for sink in sinks],
        'host_devices':False,'fixed_gain':1,'cases':cases,'passed':all(case['passed'] for case in cases),
        'limits':['Actual private48k mono device path; non48k SRC/clip-order proof remains separate.','No physical hardware or perceptual equivalence guarantee.']}
    (args.output/'proof.json').write_text(json.dumps(result,indent=2)+'\n')
    print(json.dumps(result),flush=True)
    if args.expect_mismatch:
        assert cases[0]['passed'] and any(case['reference']['fixed_gain_snr_db']<15 for case in cases[1:]),'baseline must expose real mismatch'
    else:
        assert result['passed'],result


if __name__=='__main__':
    main()
