// SPDX-License-Identifier: EUPL-1.2
// Actual production WASM encoder, synthetic file PCM only. No devices/network.
import assert from "node:assert/strict";
import {readFile,writeFile} from "node:fs/promises";
import {join,resolve} from "node:path";
import {pathToFileURL} from "node:url";
const [directory,input,output,target]=process.argv.slice(2);
assert.ok(directory&&input&&output&&target,"CODEC INPUT.f32le OUTPUT.opus-frames BITRATE");
const bitrate=Number(target);assert.ok(Number.isInteger(bitrate)&&bitrate>=6000&&bitrate<=128000);
const codec=await import(pathToFileURL(join(resolve(directory),"nuschl_audio_wasm.js")).href);
codec.initSync({module:await readFile(join(directory,"nuschl_audio_wasm_bg.wasm"))});
const bytes=await readFile(input);assert.ok(bytes.length<=48000*4*30&&bytes.length%(960*4)===0);
const view=new DataView(bytes.buffer,bytes.byteOffset,bytes.length),encoder=new codec.VoiceEncoder();
const frames=[],costs=[];
try {
  encoder.set_bitrate(bitrate);const delay=encoder.lookahead_samples();
  for(let offset=0;offset<bytes.length;offset+=960*4){
    const pcm=Float32Array.from({length:960},(_,index)=>view.getFloat32(offset+index*4,true));
    assert.ok(pcm.every(value=>Number.isFinite(value)&&Math.abs(value)<=1));
    const start=performance.now(),packet=encoder.encode(pcm);costs.push((performance.now()-start)*1e6);
    assert.ok(packet.length>0&&packet.length<=Math.floor(bitrate/400));
    const size=Buffer.alloc(2);size.writeUInt16LE(packet.length);frames.push(size,Buffer.from(packet));
  }
  await writeFile(output,Buffer.concat(frames));costs.sort((a,b)=>a-b);
  console.log(JSON.stringify({actual_codec:directory,lookahead_samples:delay,frames:costs.length,
    encode_p50_ns:costs[Math.floor(costs.length*.5)],encode_p95_ns:costs[Math.floor(costs.length*.95)],encode_max_ns:costs.at(-1),
    timing_scope:"Node JS+WASM call, not physical browser hardware latency"}));
} finally {encoder.free();}
