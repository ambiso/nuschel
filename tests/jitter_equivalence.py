#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Compare frozen/native-adapter policy traces with exact floating-point bits.

The baseline is an explicit source file (e.g. jj file show -r abe83021
apps/media-worker/src/jitter.rs). No Cargo target or host devices are used.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--baseline',required=True,type=Path)
    parser.add_argument('--rustc',required=True,type=Path)
    parser.add_argument('--core-rlib',required=True,type=Path)
    parser.add_argument('--output',required=True,type=Path)
    parser.add_argument('--expect-correctness-change',action='store_true',help='reviewed gap-duration fix, not a retuned target')
    args=parser.parse_args()
    args.output.mkdir(parents=True,exist_ok=True)
    root=Path(__file__).resolve().parents[1]
    results=[]
    traces=[]
    for name,module in [('frozen',args.baseline),('shared-adapter',root/'apps/media-worker/src/jitter.rs')]:
        directory=args.output/name;directory.mkdir(exist_ok=True)
        source=module.read_bytes()
        (directory/'jitter_module.rs').write_bytes(source)
        (directory/'trace.rs').write_bytes((root/'tests/jitter_trace.rs').read_bytes())
        executable=directory/'trace'
        subprocess.run([str(args.rustc),'--edition=2024','--cfg=test','-O','--cap-lints=allow',str(directory/'trace.rs'),
            '--extern',f'nuschl_audio_jitter={args.core_rlib}','-o',str(executable)],check=True,timeout=30)
        trace=subprocess.check_output([str(executable)],timeout=20)
        (directory/'trace.txt').write_bytes(trace)
        traces.append(trace.decode().splitlines())
        results.append({'name':name,'source_sha256':hashlib.sha256(source).hexdigest(),
            'trace_sha256':hashlib.sha256(trace).hexdigest(),'pulls':trace.count(b'\n')})
    exact=results[0]['trace_sha256']==results[1]['trace_sha256']
    if args.expect_correctness_change:assert not exact,'expected reviewed correctness change missing'
    else:assert exact,'native policy changed'
    proof={'seed':'0x12345678','arrivals':10000,'exact_policy_trace':exact,'expected_correctness_change':args.expect_correctness_change,'cases':results,
        'core_rlib_sha256':hashlib.sha256(args.core_rlib.read_bytes()).hexdigest(),
        'first_difference':next(({'pull':i,'frozen':a,'candidate':b} for i,(a,b) in enumerate(zip(*traces)) if a!=b),None),
        'scope':'Policy equivalence only; independent reference codec/loss fixtures are separate.'}
    (args.output/'proof.json').write_text(json.dumps(proof,indent=2)+'\n')
    print(json.dumps(proof))


if __name__=='__main__':
    main()
