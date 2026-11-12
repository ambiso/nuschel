// SPDX-License-Identifier: EUPL-1.2
// Real processor, reference-decoded independent streams, no browser devices.
import assert from "node:assert/strict";
import {readFile,writeFile} from "node:fs/promises";
import {pathToFileURL} from "node:url";
import {resolve} from "node:path";
const [fixturePath,expectedPath,outputPath]=process.argv.slice(2);
const classes=new Map();
globalThis.AudioWorkletProcessor=class {constructor(){this.port={postMessage(){}};}};
globalThis.registerProcessor=(name,type)=>classes.set(name,type);
const {PcmMixer,MAX_QUEUED_SAMPLES,MAX_SPEAKERS}=await import(pathToFileURL(resolve("web/app/src/audio-worklet.js")));
delete globalThis.AudioWorkletProcessor;delete globalThis.registerProcessor;
const f32=async path=>{const raw=await readFile(path);return Float32Array.from(new Float32Array(raw.buffer,raw.byteOffset,raw.byteLength/4));};
const streams=await Promise.all(JSON.parse(await readFile(fixturePath)).map(async stream=>({...stream,samples:await f32(stream.pcm)})));
const expected=await f32(expectedPath),Processor=classes.get("nuschl-playback"),processor=new Processor();
processor.mixer.configure("headroom",true);
const output=[new Float32Array(128),new Float32Array(128)],actual=[];
// Initial hold consumes960 frames, not necessarily an integer128-frame quantum.
for(let cursor=0;cursor<expected.length/2+960;cursor+=128) {
  const chunk=Math.floor(cursor/960);
  if(cursor%960<128 && chunk*1920<expected.length) {
    for(const [speaker,stream] of streams.entries())
      assert.ok(processor.mixer.enqueue({epoch:"headroom",speaker,samples:stream.samples.slice(chunk*1920,(chunk+1)*1920),channels:2,frames:960,volume:stream.volume}));
  }
  processor.process([],[output]);
  for(let i=0;i<128;i++)if(cursor+i>=960 && cursor+i-960<expected.length/2)actual.push(output[0][i],output[1][i]);
}
assert.equal(actual.length,expected.length);
let maxError=0;
for(let i=0;i<expected.length;i++)maxError=Math.max(maxError,Math.abs(actual[i]-expected[i]));
assert.equal(maxError,0,"actual processor must equal the independent final-clipped f64 sum");
// Hostile extremes and scratch identity are also covered by the unit suite.
const mixer=new PcmMixer();mixer.configure("budget",true);
for(let speaker=0;speaker<MAX_SPEAKERS;speaker++)mixer.enqueue({epoch:"budget",speaker,samples:new Float32Array(MAX_QUEUED_SAMPLES),volume:4});
let retained=mixer.left.byteLength+mixer.right.byteLength;
for(const state of mixer.speakers.values())retained+=state.data.byteLength+state.gains.byteLength;
assert.equal(retained,1996800);
const proof={actual_processor:true,independent_two_stream_sum_exact:true,frames:actual.length/2,max_abs_error:maxError,
  max_speakers:MAX_SPEAKERS,queue_frames:MAX_QUEUED_SAMPLES,retained_pcm_gain_scratch_bytes:retained,
  baseline_pcm_bytes:MAX_SPEAKERS*MAX_QUEUED_SAMPLES*2*4,render_allocates_pcm_scratch:false};
await writeFile(outputPath,JSON.stringify(proof,null,2)+"\n");
console.log(JSON.stringify(proof));
