#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Actual private device-rate playback; assert negotiated rate, no host devices.

The monitor requests the SAME rate as the private sink/client, preventing the
server's rate converter from being mistaken for Nuschl's. Gain and LR separation
are measured independently of arbitrary fractional sample delay.
"""
import argparse
import json
import math
from pathlib import Path
import tempfile
from output_quality import Reference, PrivatePulse, fixture, playback, delay_only, amplitude
from output_resample import Speex
from media_live import check_echo_test, Worker
import time


def compare_echo(current, baseline, pulse, output):
    module=pulse.ctl('load-module','module-loopback','source=speakers.monitor','sink=mic',
        'latency_msec=30','adjust_time=0','sink_input_properties=media.name=room').strip()
    results=[]
    try:
        time.sleep(.5)
        room=next(entry['index'] for entry in pulse.ctl('list','sink-inputs',json_output=True)
            if entry['properties'].get('media.name')=='room')
        pulse.ctl('set-sink-input-volume',str(room),'30%')
        for label,binary in [('baseline',baseline),('current',current),('current',current),('baseline',baseline)]:
            time.sleep(.5)
            worker=Worker(binary,pulse)
            try:
                worker.send({'kind':'configure','bitrate':40000,'packet_loss_percent':5,
                    'settings':{'echo_cancellation':'on'}})
                worker.send({'kind':'preview','capture':True,'loopback':False})
                worker.wait(lambda e:e.get('kind')=='status' and e.get('ready'),'audio not ready')
                devices=worker.wait(lambda e:e.get('kind')=='devices','no reference report')
                assert devices.get('reference'),devices  # System-recorded path, never fallback.
                worker.send({'kind':'echo_test'})
                result=worker.wait(lambda e:e.get('kind')=='echo_test','no echo report',timeout=20)
                assert not any(e.get('kind')=='packet' for e in worker.events),'preview transmitted'
                passed=result['reduction_db']>=15 and result['transmitted']<=.25
                results.append({'worker':label,'binary':str(binary),'reference':devices['reference'],
                    'result':result,'passed':passed})
                (output/'paired-progress.json').write_text(json.dumps(results,indent=2)+'\n')
            finally:worker.stop()
    finally:pulse.ctl('unload-module',module)
    return {'seed':'EchoProbe0x12345678','same_daemon_and_loopback':True,'order':'baseline/current/current/baseline',
        'rate':44100,'capture_rate':48000,'host_devices':False,'results':results,
        'passed_current':all(r['passed'] for r in results if r['worker']=='current'),
        'passed_baseline':all(r['passed'] for r in results if r['worker']=='baseline')}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('worker','pulse-bin','opus-library','speex-library','output'):
        parser.add_argument('--'+name, required=True,type=Path)
    parser.add_argument('--compare-worker',type=Path,help='ABBA echo comparison on one unchanged private graph')
    parser.add_argument('--echo-only',action='store_true',help='run only identical private AEC graph for a frozen worker baseline')
    parser.add_argument('--echo',action='store_true',help='also prove private 44.1 kHz render/reference AEC loop')
    args = parser.parse_args()
    args.output.mkdir(parents=True,exist_ok=True)
    if args.compare_worker:
        with tempfile.TemporaryDirectory(prefix='paired-44100-',dir=args.output) as temporary:
            pulse=PrivatePulse(args.pulse_bin,Path(temporary),44100)
            try:report=compare_echo(args.worker,args.compare_worker,pulse,args.output)
            finally:pulse.stop()
        (args.output/'proof.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report),flush=True)
        if not report['passed_current']:raise SystemExit(1)
        return
    opus=Reference(args.opus_library);src=Speex(args.speex_library)
    packets=opus.encode(fixture(2),2);decoded=opus.decode(packets,2)
    cases=[]
    for rate in (() if args.echo_only else (22050,44100,96000)):
        with tempfile.TemporaryDirectory(prefix=f'rate-{rate}-',dir=args.output) as temporary:
            pulse=PrivatePulse(args.pulse_bin,Path(temporary),rate)
            try:
                specs=[]
                actual=playback(args.worker,pulse,packets,args.output/f'{rate}.f32le',monitor_rate=rate,specifications=specs)
            finally:pulse.stop()
        assert len(specs)==1 and f'{rate}Hz' in specs[0],(rate,specs)
        expected,delay=src.convert(decoded,rate)
        lag=delay_only(expected[::2],actual[::2],rate=rate)
        aligned=actual[lag*2:lag*2+len(expected)]
        begin,end=rate//3*2,len(expected)-rate//5*2
        expected,aligned=expected[begin:end],aligned[begin:end]
        gains=[]
        for channel,frequency in [(0,997),(1,2003)]:
            gains.append(20*math.log10(amplitude(aligned[channel::2],frequency,rate)/amplitude(expected[channel::2],frequency,rate)))
        left997=amplitude(aligned[::2],997,rate);right997=amplitude(aligned[1::2],997,rate)
        left2003=amplitude(aligned[::2],2003,rate);right2003=amplitude(aligned[1::2],2003,rate)
        separation=min(20*math.log10(left997/max(right997,1e-20)),20*math.log10(right2003/max(left2003,1e-20)))
        assert max(map(abs,gains))<.05,(rate,gains)
        assert separation>50,(rate,separation)
        cases.append({'rate':rate,'actual_client_sample_spec':specs[0], 'monitor_rate':rate,
            'fixed_gain_carrier_error_db':gains,'stereo_separation_db':separation,
            'reference_filter_delay_frames':delay,'passed':True})
    echo=None
    if args.echo or args.echo_only:
        with tempfile.TemporaryDirectory(prefix='echo-44100-',dir=args.output) as temporary:
            pulse=PrivatePulse(args.pulse_bin,Path(temporary),44100)
            try:check_echo_test(args.worker,pulse)
            finally:pulse.stop()
        echo={'rate':44100,'capture_rate':48000,'passed':True,'fixture':'existing independent loopback/probe assertions'}
    report={'echo':echo,'passed':True,'worker':str(args.worker),'host_devices':False,'cases':cases,
        'limits':['Low-band encoded carriers; broad passband/alias/chunk/clock are direct converter fixtures.',
            'Gain compared without fitting; fractional endpoint delay prevents a single integer-aligned PCM identity claim.',
            'No perceptual/hardware guarantee.']}
    (args.output/'proof.json').write_text(json.dumps(report,indent=2)+'\n');print(json.dumps(report),flush=True)

if __name__=='__main__':main()
