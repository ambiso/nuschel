#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Unchanged unmute/privacy gates with actual private non48k capture streams."""
import argparse
import array
import json
import math
from pathlib import Path
import subprocess
import threading

from mumble_client import ReferenceOpus
from output_quality import PrivatePulse
import unmute_latency


def retarget_microphone(pulse, rate):
    """Reconfigure only the explicitly owned null microphone before workers."""
    modules = [line.split('\t') for line in pulse.ctl('list','short','modules').splitlines()]
    modules = [m for m in modules if len(m) >= 3 and m[1] == 'module-null-sink' and 'sink_name=mic' in m[2].split()]
    assert len(modules) == 1 and modules[0][0].isdigit()
    pulse.ctl('unload-module', modules[0][0])
    pulse.ctl('load-module', 'module-null-sink', 'sink_name=mic', f'rate={rate}', 'channels=1', 'channel_map=mono')
    pulse.ctl('set-default-source', 'mic.monitor')
    mic = next(s for s in pulse.ctl('list', 'sources', json_output=True) if s['name'] == 'mic.monitor')
    assert f'{rate}Hz' in mic['sample_specification']
    return mic


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--worker', type=Path, required=True)
    parser.add_argument('--baseline-worker', type=Path)
    parser.add_argument('--pulse-bin', type=Path, required=True)
    parser.add_argument('--opus-library', type=Path, required=True)
    parser.add_argument('--output', type=Path, required=True)
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    reference = ReferenceOpus(args.opus_library)
    original_worker = unmute_latency.Worker
    report = {'passed': False, 'host_devices': False, 'GUI_timing': False, 'rates': []}
    try:
        for rate in (44100, 96000):
            root = args.output/str(rate); root.mkdir()
            pulse = PrivatePulse(args.pulse_bin, root, 48000)
            snapshots = []
            feeder = None
            try:
                mic = retarget_microphone(pulse, rate)

                class ObservedWorker(original_worker):
                    def __init__(self, *arguments, **keywords):
                        super().__init__(*arguments, **keywords)
                        self.capture_specs = []
                        def inspect():
                            while self.process.poll() is None:
                                streams = pulse.ctl('list', 'source-outputs', json_output=True)
                                for stream in streams:
                                    if stream['source'] == mic['index']:
                                        spec = stream['sample_specification']
                                        if spec not in self.capture_specs: self.capture_specs.append(spec)
                                if self.process.poll() is None:
                                    threading.Event().wait(.005)
                        self.observer = threading.Thread(target=inspect, daemon=True)
                        self.observer.start()

                    def stop(self):
                        super().stop()
                        self.observer.join(timeout=2)
                        assert not self.observer.is_alive()
                        captured = any(e['kind'] == 'packet' and not e['end'] for _, e in self.events)
                        if captured:
                            assert self.capture_specs and all(f'{rate}Hz' in s for s in self.capture_specs), self.capture_specs
                        else:
                            assert not self.capture_specs, 'muted control opened a microphone'
                        snapshots.append({'worker_log': str(self.log), 'actual_capture_sample_specs': self.capture_specs, 'packets': captured})

                unmute_latency.Worker = ObservedWorker
                feeder = subprocess.Popen([str(args.pulse_bin/'pacat'), f'--server=unix:{pulse.socket}',
                    '--playback','--device=mic','--raw','--format=float32le',f'--rate={rate}','--channels=1',
                    '--latency-msec=20','--process-time-msec=10'], stdin=subprocess.PIPE,
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                samples = array.array('f', (.12*(.5+.5*math.sin(2*math.pi*4*i/rate))*sum(math.sin(2*math.pi*140*h*i/rate)/h for h in range(1,13)) for i in range(rate))).tobytes()
                def feed():
                    try:
                        while feeder.poll() is None:
                            feeder.stdin.write(samples); feeder.stdin.flush()
                    except (BrokenPipeError, ValueError): pass
                source_thread = threading.Thread(target=feed, daemon=True); source_thread.start()
                cases = {}
                for label, binary in [('baseline', args.baseline_worker), ('current', args.worker)]:
                    if binary is None: continue
                    folder = root/label; folder.mkdir()
                    # Original continuous/PTT/VAD onset, six usable packets,
                    # pacing, terminator/epoch and microphone-release gates.
                    cases[label] = unmute_latency.measure(binary,pulse,folder,reference,label == 'current')
                report['rates'].append({'capture_rate':rate, 'physical_devices':False,
                    'null_source_sample_spec':mic['sample_specification'], 'actual_stream_observations':snapshots,
                    'cases':cases})
            finally:
                unmute_latency.Worker = original_worker
                try:
                    if feeder is not None:
                        feeder.terminate(); feeder.wait(timeout=5)
                        source_thread.join(timeout=2)
                        try: feeder.stdin.close()
                        except BrokenPipeError: pass
                finally: pulse.stop()
            (args.output/'progress.json').write_text(json.dumps(report,indent=2)+'\n')
    finally:
        unmute_latency.Worker = original_worker
    report['passed'] = True
    report['limits'] = ['Private worker-command timing, not GUI clicks or physical hardware latency',
        'No network/Mumble server in this fixture; actual codec packets decoded independently',
        'Original microphone consent/startup/mute assertions retained']
    (args.output/'proof.json').write_text(json.dumps(report,indent=2)+'\n')
    print(json.dumps(report), flush=True)


if __name__ == '__main__': main()
