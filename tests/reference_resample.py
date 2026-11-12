#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual untouched monitor-reference SRC spectral audit, no devices/servers."""
import argparse
import json
import math
from pathlib import Path
import subprocess
from output_quality import amplitude,compare
from output_resample import Speex,write_pcm,read_pcm


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--probe',required=True,type=Path)
    parser.add_argument('--speex-library',required=True,type=Path)
    parser.add_argument('--output',required=True,type=Path)
    parser.add_argument('--require-reference',action='store_true')
    args=parser.parse_args();args.output.mkdir(parents=True,exist_ok=True)
    reference=Speex(args.speex_library);rows=[]
    for source_rate in [11025,22050,44100,48000,96000]:
        for frequency in [997,.35*min(48000,source_rate)]:
            data=[v for i in range(source_rate) for v in (.2*math.sin(2*math.pi*frequency*i/source_rate),
                .13*math.sin(2*math.pi*frequency*i/source_rate+.71))]
            source=args.output/f'{source_rate}-{frequency}.f32le';write_pcm(source,data)
            values=[]
            for suffix,chunks in [('whole','8192'),('chunks','1,127,480,37,256')]:
                target=args.output/f'{source_rate}-{frequency}-{suffix}.f32le'
                subprocess.run([str(args.probe),str(source),str(target),str(source_rate),chunks],check=True,capture_output=True,timeout=10)
                values.append(read_pcm(target))
            assert values[0]==values[1]
            expected,_=reference.convert(data,48000,input_rate=source_rate)
            image=abs((source_rate-frequency+24000)%48000-24000)
            agreement=compare(expected,values[0])
            actual=values[0][19200:76800:2];expected=expected[19200:76800:2]
            db=lambda v,f:20*math.log10(max(amplitude(v,f,48000)/.2,1e-20))
            rows.append({'source_rate':source_rate,'target_rate':48000,'frequency':frequency,'chunk_exact':True,
                'full_fixed_gain_reference':agreement,'frames':len(values[0])//2,'carrier_db':db(actual,frequency),'reference_carrier_db':db(expected,frequency),
                'image_frequency':image if source_rate<48000 else None,'image_db':db(actual,image) if source_rate<48000 else None,'reference_image_db':db(expected,image) if source_rate<48000 else None})
    passed=all(r['full_fixed_gain_reference']['fixed_gain_snr_db']>=100 and r['full_fixed_gain_reference']['max_abs_error']<=2e-6 for r in rows)
    report={'reference_gate_passed':passed,'host_devices':False,'reference_quality':3,'rows':rows,
        'limits':['Reconstruction error is not proof of the cause of runtime AEC variability.','No microphone startup or timeline ordering changed.']}
    (args.output/'proof.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report))
    if args.require_reference and not passed:raise SystemExit(1)

if __name__=='__main__':main()
