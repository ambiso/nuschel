#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Original private AEC assertions with observed negotiated non48k capture."""
import argparse
import json
import subprocess
import threading
import time
from pathlib import Path
from capture_unmute import retarget_microphone
from output_quality import PrivatePulse
import media_live


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--worker', type=Path, required=True)
    parser.add_argument('--pulse-bin', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    original = media_live.Worker
    report = {'passed':False,'host_devices':False,'rows':[]}
    try:
        for rate in (44100,96000):
            root = args.output/str(rate);root.mkdir(mode=0o700)
            pulse = PrivatePulse(args.pulse_bin, root, 48000)
            workers = []
            feeder = None
            try:
                mic = retarget_microphone(pulse,rate)
                # Keep an owned zero-valued stream at the desired input rate.
                # Otherwise Pulse may retune an idle null mic to the48k room
                # stream before CPAL opens, silently bypassing the SRC under test.
                feeder = subprocess.Popen([str(args.pulse_bin/'pacat'), f'--server=unix:{pulse.socket}',
                    '--playback','--device=mic','--raw','--format=float32le',f'--rate={rate}','--channels=1',
                    '--latency-msec=20','--process-time-msec=10'],stdin=subprocess.PIPE,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
                silence = bytes(rate*4)
                def feed():
                    try:
                        while feeder.poll() is None: feeder.stdin.write(silence); feeder.stdin.flush()
                    except (BrokenPipeError,ValueError):pass
                source_thread = threading.Thread(target=feed,daemon=True); source_thread.start()
                time.sleep(.1)
                class Observed(original):
                    def __init__(self,*arguments,**keywords):
                        super().__init__(*arguments,**keywords)
                        self.specs=[];self.mode=None
                        workers.append(self)
                    def send(self,payload):
                        if payload.get('kind') == 'configure':self.mode=payload['settings']['echo_cancellation']
                        return super().send(payload)
                    def wait(self,*arguments,**keywords):
                        result=super().wait(*arguments,**keywords)
                        specs=[s['sample_specification'] for s in pulse.ctl('list','source-outputs',json_output=True) if s['source']==mic['index']]
                        assert specs and all(f'{rate}Hz' in s for s in specs),specs
                        self.specs=specs
                        if result.get('kind')=='echo_test':
                            report['rows'].append({'rate':rate,'actual_capture_specs':specs,'mode':self.mode,'result':result})
                            (args.output/'progress.json').write_text(json.dumps(report,indent=2)+'\n')
                        return result
                media_live.Worker=Observed
                # Existing best-of-two15dB/25%on and>50%off assertions unchanged.
                media_live.check_echo_test(args.worker,pulse)
            finally:
                media_live.Worker=original
                for worker in workers:
                    if worker.process.poll() is None:worker.stop()
                try:
                    if feeder is not None:
                        feeder.terminate();feeder.wait(timeout=5);source_thread.join(timeout=2)
                        try:feeder.stdin.close()
                        except BrokenPipeError:pass
                finally:pulse.stop()
    finally:media_live.Worker=original
    report['passed']=True
    report['limits']=['Synthetic private monitor/loopback, not physical devices','Original two-attempt convergence policy retained; every attempt preserved','Not a general non48k AEC reliability claim']
    (args.output/'proof.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report))


if __name__=='__main__':main()
