#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Independent fixed-gain native Mixer reference; offline libopus only."""
import argparse,json,subprocess
from pathlib import Path
from opus_playout import scenarios,oracle,make_case
from output_quality import Reference,compare
from output_resample import read_pcm,write_pcm

def main():
 p=argparse.ArgumentParser(description=__doc__);p.add_argument('--probe',type=Path,required=True);p.add_argument('--opus-library',type=Path,required=True);p.add_argument('--output',type=Path,required=True);p.add_argument('--expect-failure',action='store_true');args=p.parse_args();args.output.mkdir(parents=True,exist_ok=True)
 reference=Reference(args.opus_library);cases=scenarios(reference)
 voice=cases[0]['packets'][:8]
 gains=[.25,.5,1.,0.,2.,.75,.125,1.,4.,0.]
 duplicate=make_case('stale-duplicate-controls',voice+[voice[0],voice[1]],[(i*2,i*20,i,i==7) for i in range(8)]+[(0,25,8,False),(2,75,9,False)],[(2+i*2,'packet',i,2) for i in range(8)],20,gains)
 cases.append(duplicate)
 control=dict(cases[0]);control['name']='constant-gain-control';control['gains']=[.5]*len(control['gains']);cases.append(control)
 results=[]
 for case in cases:
  pcm,gains,_=oracle(reference,case)
  expected=[max(-1.,min(1.,sample*gains[index//960])) for index,sample in enumerate(pcm)]
  commands=[];cursor=0;pushes=sorted(case['pushes'],key=lambda row:row[1])
  for tick in range(case['ticks']):
   while cursor<len(pushes) and pushes[cursor][1]<=tick*10:
    sequence,at,index,end=pushes[cursor];commands.append(f'P {sequence} {at*1000} {case["gains"][index]} {int(end)} {case["packets"][index].hex()}');cursor+=1
   commands.append(f'R {tick}')
  event=args.output/(case['name']+'.events');event.write_text('\n'.join(commands)+'\n')
  output=args.output/(case['name']+'.f32le');write_pcm(args.output/(case['name']+'-reference.f32le'),expected)
  run=subprocess.run([str(args.probe),str(event),str(output)],capture_output=True,text=True,check=True,timeout=20)
  metrics=compare(expected,read_pcm(output));results.append({'case':case['name'],'metrics':metrics,'timing':run.stdout.strip(),'passed':metrics['fixed_gain_snr_db']>=35})
 proof={'passed':all(row['passed'] for row in results),'fixed_gain':True,'delay_fitted':False,'reference':str(args.opus_library),'cases':results}
 (args.output/'proof.json').write_text(json.dumps(proof,indent=2)+'\n');print(json.dumps(proof))
 if args.expect_failure:
  assert not proof['passed'];assert next(row for row in results if row['case']=='constant-gain-control')['passed']
 else:assert proof['passed']
if __name__=='__main__':main()
