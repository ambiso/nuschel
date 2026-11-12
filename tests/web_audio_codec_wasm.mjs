// SPDX-License-Identifier: EUPL-1.2
// Test the actual wasm-bindgen artifact used by the isolated codec worker.
// No browser, device, network, model weights or server credentials are used.
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {resolve,join} from "node:path";
import {pathToFileURL} from "node:url";
const directory=resolve(process.argv[2]||"");
assert.ok(process.argv[2],"usage: node tests/web_audio_codec_wasm.mjs CODEC_DIRECTORY");
const {initSync,VoiceEncoder,VoiceDecoder}=await import(pathToFileURL(join(directory,"nuschl_audio_wasm.js")).href);
initSync({module:await readFile(join(directory,"nuschl_audio_wasm_bg.wasm"))});
const encoder=new VoiceEncoder(),decoder=new VoiceDecoder();
const results=[];
try {
  for(const bitrate of [128000,24000,6000,48000,128000]) {
    encoder.set_bitrate(bitrate);
    let bytes=0;
    for(let frame=0;frame<8;frame++) {
      const samples=Float32Array.from({length:960},(_,i)=>Math.sin(2*Math.PI*997*(frame*960+i)/48000)*0.3);
      const packet=encoder.encode(samples);
      assert.ok(packet instanceof Uint8Array&&packet.length>0&&packet.length<=Math.floor(bitrate/400));
      const output=decoder.decode(packet);
      assert.equal(output.length,1920);assert.ok(output.every(Number.isFinite));bytes+=packet.length;
    }
    results.push({bitrate,frames:8,bytes});
  }
  for(const invalid of [0,5999,128001,0xffffffff])assert.throws(()=>encoder.set_bitrate(invalid));
  console.log(JSON.stringify({passed:true,actual_codec:directory,results}));
} finally {encoder.free();decoder.free();}
