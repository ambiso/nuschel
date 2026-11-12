// SPDX-License-Identifier: EUPL-1.2
// Actual WASM playout engine on explicit fixture clocks; no browser devices.
import assert from "node:assert/strict";
import {readFile,writeFile} from "node:fs/promises";
import {resolve,join} from "node:path";
import {pathToFileURL} from "node:url";
const [directory,eventsPath,outputPath,metadataPath]=process.argv.slice(2),root=resolve(directory);
const {initSync,VoicePlayout}=await import(pathToFileURL(join(root,"nuschl_audio_wasm.js")));
initSync({module:await readFile(join(root,"nuschl_audio_wasm_bg.wasm"))});
const receiver=new VoicePlayout(),output=[],metadata=[];
const events=(await readFile(eventsPath,"utf8")).trim().split("\n");assert.ok(events.length<=10000);
try {
 for(const line of events){
  const fields=line.trim().split(/\s+/);
  if(fields[0]==="P"||fields[0]==="B"){
   assert.equal(fields.length,6);assert.ok(/^(?:[0-9a-f]{2}){1,1024}$/.test(fields[5]));
   const push=()=>receiver.push(BigInt(fields[1]),Uint8Array.from(Buffer.from(fields[5],"hex")),fields[4]==="1",Number(fields[3]),BigInt(fields[2]));
   if(fields[0]==="B")assert.throws(push);else push();
  } else if(fields[0]==="R"||fields[0]==="E"){
   let samples;
   if(fields[0]==="E"){assert.throws(()=>receiver.render());samples=new Float32Array();}
   else samples=receiver.render();
   assert.ok(samples.length===0||samples.length===960);assert.ok(metadata.length<2000);
   metadata.push({tick:Number(fields[1]),samples:samples.length,kind:receiver.last_render_kind(),gain:receiver.gain()});
   output.push(Buffer.from(samples.length?samples.buffer:new Float32Array(960).buffer));
  } else if(fields[0]==="C")receiver.reset();
  else throw new Error("invalid fixture event");
 }
} finally {receiver.free();}
await writeFile(outputPath,Buffer.concat(output));await writeFile(metadataPath,JSON.stringify(metadata)+"\n");
