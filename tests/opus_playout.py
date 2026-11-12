#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Independent libopus oracle for real Rust/WASM sequence playout.

No fitted delay/gain. Actions and output start ticks are fixed fixture contracts,
not copied from the tested engine. Test-only ctypes; private/offline PCM only.
"""
import argparse
import ctypes
import json
import math
from pathlib import Path
import subprocess

from output_quality import Reference, compare
from output_resample import read_pcm, write_pcm

RATE=48000
TICK=480


class Decoder:
    def __init__(self, reference):
        self.lib=reference.lib
        error=ctypes.c_int()
        self.handle=self.lib.opus_decoder_create(RATE,2,ctypes.byref(error))
        assert self.handle and error.value==0
        self.ctl=ctypes.CFUNCTYPE(ctypes.c_int,ctypes.c_void_p,ctypes.c_int,ctypes.c_int)(("opus_decoder_ctl",self.lib))
        assert self.ctl(self.handle,4046,1)==0
    def decode(self, packet=None, frames=5760, fec=False):
        output=(ctypes.c_float*(frames*2))()
        count=self.lib.opus_decode_float(self.handle,packet,len(packet) if packet else 0,output,frames,int(fec))
        if count<0:raise ValueError(f'libopus decode{count}')
        return list(output[:count*2])
    def reset(self):assert self.ctl(self.handle,4028,0)==0
    def close(self):self.lib.opus_decoder_destroy(self.handle)


def signal(frames, channels):
    return [value for i in range(frames) for value in (
        [sum(.2/h*math.sin(2*math.pi*(123+9*math.sin(i/RATE*9))*h*i/RATE) for h in range(1,9))]
        if channels==1 else [.2*math.sin(2*math.pi*997*i/RATE),.15*math.sin(2*math.pi*2003*i/RATE)])]


def make_case(name, packets, pushes, operations, ticks, gains):
    # operations: (start_tick, kind, packet_index, duration_ticks). Missing gain
    # stays at the preceding known packet, never an unrelated future gain.
    return dict(name=name,packets=packets,pushes=pushes,operations=operations,ticks=ticks,gains=gains)


def scenarios(reference):
    voice=reference.encode(signal(24*960,1),1,24000,application=2048,bandwidth=1103,fec=True,packet_loss=25)
    gains=[.25+(i%4)*.25 for i in range(24)]
    pushes=[(i*2,i*20,i,False) for i in range(20) if i not in (9,13,14)]
    pushes=[(seq,15 if index==2 else 25 if index==1 else 195 if index==10 else 290 if index==15 else at,index,index==19) for seq,at,index,_ in pushes]
    pushes += [(28,400,14,False),(10,105,5,False)]
    ops=[(2+i*2,'fec' if i in (9,14) else 'plc' if i==13 else 'packet',10 if i==9 else 15 if i==14 else i,2) for i in range(20)]
    # Same live context, sequence reset after a drained terminator.
    pushes += [(0,500,20,False),(2,520,21,True)]
    ops += [(52,'reset',0,0),(52,'packet',20,2),(54,'packet',21,2)]
    fixed=make_case('loss-reorder-fec-plc-restart',voice,pushes,ops,58,gains)
    base=reference.encode(signal(48_000,2),2,64000,application=2051,frame_samples=480)
    durations=list(range(1,13))
    packets=[];at=0
    for count in durations:
        packets.extend(reference.aggregate(base[at:at+count],count));at+=count
    seqs=[];at=0
    for count in durations:seqs.append(at);at+=count
    var=make_case('10-through120ms',packets,[(seq,seq*10,i,i==len(durations)-1) for i,seq in enumerate(seqs)],
        [(3+seq,'packet',i,count) for i,(seq,count) in enumerate(zip(seqs,durations))],sum(durations)+8,[.5]*len(durations))
    long=reference.aggregate(base[:12],12)[0]
    short=reference.aggregate(base[12:14],2)[0]
    # The small lost interval MUST NOT expand to the preceding120 ms duration.
    gap=make_case('120ms-then-missing10ms', [long,short],[(0,0,0,False),(13,130,1,True)],
        [(13,'packet',0,12),(25,'fec',1,1),(26,'packet',1,2)],32,[.25,1.])
    ordered=make_case('ordered-FEC-enabled',voice,[(i*2,i*20,i,i==23) for i in range(24)],
        [(2+i*2,'packet',i,2) for i in range(24)],52,gains)
    return [ordered,fixed,var,gap]


def oracle(reference, case):
    decoder=Decoder(reference);expected=[0.]*(case['ticks']*TICK*2)
    gain_by_tick=[1.]*case['ticks'];size_by_tick=[0]*case['ticks'];gain=1.;fec_contrast=[]
    try:
        for tick,kind,index,duration in case['operations']:
            if kind=='reset':decoder.reset();continue
            packet=case['packets'][index]
            pcm=decoder.decode(None if kind=='plc' else packet,frames=duration*TICK if kind!='packet' else 5760,fec=kind=='fec')
            assert len(pcm)==duration*TICK*2,(case['name'],kind,len(pcm))
            if kind=='packet':gain=case['gains'][index]
            expected[tick*TICK*2:(tick+duration)*TICK*2]=pcm
            for at in range(tick,tick+duration):gain_by_tick[at]=gain;size_by_tick[at]=TICK*2
        return expected,gain_by_tick,size_by_tick
    finally:decoder.close()


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--opus-library',type=Path,required=True)
    parser.add_argument('--native-probe',type=Path)
    parser.add_argument('--wasm',type=Path)
    parser.add_argument('--output',type=Path,required=True)
    parser.add_argument('--expect-gap-failure',action='store_true')
    parser.add_argument('--expect-codec-failure',action='store_true',help='frozen-source diagnostic only; retains passed=false evidence')
    args=parser.parse_args();args.output.mkdir(parents=True,exist_ok=True)
    assert args.native_probe or args.wasm
    reference=Reference(args.opus_library);results=[]
    for case in scenarios(reference):
        expected,gain_expected,size_expected=oracle(reference,case)
        commands=[]
        pushes=sorted(case['pushes'],key=lambda item:item[1]);cursor=0
        for tick in range(case['ticks']):
            while cursor<len(pushes) and pushes[cursor][1]<=tick*10:
                seq,at,index,end=pushes[cursor]
                commands.append(f'P {seq} {at*1000} {case["gains"][index]} {int(end)} {case["packets"][index].hex()}')
                cursor+=1
            commands.append(f'R {tick}')
        source=args.output/(case['name']+'.events');source.write_text('\n'.join(commands)+'\n')
        write_pcm(args.output/(case['name']+'-reference.f32le'),expected)
        for engine in ['native','wasm']:
            if not getattr(args,'native_probe' if engine=='native' else 'wasm'):continue
            output=args.output/f'{case["name"]}-{engine}.f32le';metadata_path=output.with_suffix('.json')
            if engine=='native':
                result=subprocess.run([str(args.native_probe),str(source),str(output)],capture_output=True,text=True,check=True,timeout=20)
                metadata=[dict(zip(['tick','samples','kind','gain'],[int(a),int(b),int(c),float(d)])) for a,b,c,d in (line.split() for line in result.stdout.splitlines())]
                metadata_path.write_text(json.dumps(metadata)+'\n')
            else:
                subprocess.run(['node','tests/web_playout_probe.mjs',str(args.wasm),str(source),str(output),str(metadata_path)],check=True,timeout=20)
                metadata=json.loads(metadata_path.read_text())
            metrics=compare(expected,read_pcm(output))
            sizes=[row['samples'] for row in metadata];gains=[row['gain'] for row in metadata]
            sizes_match=sizes==size_expected
            gain_match=all(gains[i]==gain_expected[i] for i,size in enumerate(size_expected) if size)
            passed=metrics['fixed_gain_snr_db']>=35 and sizes_match and gain_match
            results.append({'case':case['name'],'engine':engine,'metrics':metrics,'sizes_match':sizes_match,'gain_match':gain_match,'passed':passed})
    proof={'reference':str(args.opus_library),'fixed_gain':True,'delay_fitted':False,'cases':results,'passed':all(row['passed'] for row in results)}
    (args.output/'proof.json').write_text(json.dumps(proof,indent=2)+'\n');print(json.dumps(proof),flush=True)
    for row in results:
        if args.expect_codec_failure and row['case'] in ('ordered-FEC-enabled','loss-reorder-fec-plc-restart'):assert not row['passed'],'known codec failure must reproduce'
        elif args.expect_gap_failure and row['case']=='120ms-then-missing10ms':assert not row['passed'],'frozen bug must reproduce'
        else:assert row['passed'],row


if __name__=='__main__':main()
