// SPDX-License-Identifier: EUPL-1.2
// Actual WASM + production worker/controller source, explicit clock, no devices.
import test from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import {readFile} from "node:fs/promises";
import {resolve,join} from "node:path";
import {pathToFileURL} from "node:url";
import * as protocol from "../web/app/src/audio-protocol.js";
const directory=resolve(process.argv[2]||"");
assert.ok(process.argv[2],"usage: node tests/web_playout_worker.mjs CODEC_DIRECTORY");
const codec=await import(pathToFileURL(join(directory,"nuschl_audio_wasm.js")));
codec.initSync({module:await readFile(join(directory,"nuschl_audio_wasm_bg.wasm"))});
const source=(await readFile(new URL("../web/app/src/audio-codec.worker.js",import.meta.url),"utf8")).replace(/^import .*;\n/gm,"");
const classes=new Map();globalThis.sampleRate=48000;
globalThis.AudioWorkletProcessor=class{constructor(){this.port={sent:[],postMessage(value){this.sent.push(value);}};}};
globalThis.registerProcessor=(name,type)=>classes.set(name,type);
await import("../web/app/src/audio-worklet.js");
const Processor=classes.get("nuschl-playback");
function pair(queue) {
  const a={sent:[],start(){},close(){this.closed=true;},postMessage(data){this.sent.push(data);queue.push(()=>b.onmessage?.({data}));}};
  const b={sent:[],start(){},close(){this.closed=true;},postMessage(data){this.sent.push(data);queue.push(()=>a.onmessage?.({data}));}};
  return [a,b];
}
async function fixture({loading=false}={}) {
  let time=0,release;const queue=[],messages=[],ready=new Promise(resolve=>{release=resolve;});
  const context=vm.createContext({...protocol,VoiceEncoder:codec.VoiceEncoder,VoicePlayout:codec.VoicePlayout,
    Float32Array,Uint8Array,performance:{now:()=>time},init:()=>ready,
    onmessage:null,postMessage:data=>messages.push(data),setInterval:()=>0,close:()=>{}});
  const initialized=new vm.Script(`(async()=>{${source}\n})()`).runInContext(context);
  const send=data=>context.onmessage({data});const [port,remote]=pair(queue);send({kind:"playout-port",port});
  const pump=()=>{for(let count=0;queue.length;count++){assert.ok(count<100,"bounded port traffic");queue.shift()();}};
  const authorize=(rx="rx",capture=false,epoch="tx",playback=true)=>send({kind:"configure",epoch,capture,bitrate:128000,receive_epoch:rx,playback});
  const wanted=(rx="rx",generation=1,enabled=true)=>remote.postMessage({kind:"playout-configure",epoch:rx,enabled,clock_generation:generation});
  const render=(request=1,start=480,rx="rx",generation=1)=>remote.postMessage({kind:"render",epoch:rx,clock_generation:generation,request,start_frame:start,frames:480});
  const packet=(opus,sequence=0,gain=.25,speaker=1,rx="rx",job=1,end=false)=>send({kind:"decode",epoch:rx,job,speaker,sequence:String(sequence),opus,end,volume:gain});
  const load=async()=>{release();await initialized;};
  if(!loading)await load();
  return {send,port,remote,queue,messages,pump,authorize,wanted,render,packet,load,now:value=>{time=value;},close:()=>send({kind:"close"})};
}
const encoder=new codec.VoiceEncoder();
const packets=Array.from({length:8},(_,frame)=>encoder.encode(Float32Array.from({length:960},(_,n)=>Math.sin(2*Math.PI*997*(frame*960+n)/48000)*.2)));
encoder.free();

test("both authorization orders survive WASM loading and require exact RX/G",async()=>{
 for(const mainFirst of [false,true]) {
  const f=await fixture({loading:true});
  if(mainFirst)f.authorize();f.wanted();f.pump();if(!mainFirst)f.authorize();
  assert.equal(f.port.sent.length,0);await f.load();f.pump();
  assert.ok(f.port.sent.some(m=>m.kind==="playout-ready"&&m.enabled&&m.epoch==="rx"&&m.clock_generation===1));
  f.packet(packets[0]);f.render(1,480,"stale");f.render(1,480,"rx",2);f.pump();
  assert.equal(f.port.sent.filter(m=>m.kind==="playout").length,0);
  f.render();f.pump();assert.equal(f.port.sent.filter(m=>m.kind==="playout").length,1);f.close();
 }
});

test("TX configure preserves actual RX decoder/cache; RX revoke clears it",async()=>{
 const f=await fixture();f.authorize();f.wanted();f.pump();
 f.packet(packets[0],0,.25,1,"rx",1);f.packet(packets[1],2,.5,1,"rx",2);
 f.render();f.pump();const first=f.port.sent.at(-1);assert.equal(first.speakers.length,1);
 f.authorize("rx",true,"newTX");f.render(2,960);f.pump();
 assert.equal(f.port.sent.at(-1).speakers.length,1,"second half of packet survives TX reset");
 assert.equal(f.port.sent.at(-1).speakers[0].volume,.25);
 f.authorize("revoked",false,"tx3",false);f.packet(packets[2],4,1,1,"rx",3);
 assert.ok(f.messages.some(m=>m.kind==="packet-queued"&&m.epoch==="rx"&&m.job===3),"stale inbox work is still ACKed");
 f.wanted("fresh",2);f.authorize("fresh",false,"tx4");f.pump();f.render(3,1440,"fresh",2);f.pump();
 assert.equal(f.port.sent.at(-1).speakers.length,0);f.close();
});

test("framing rejection cannot allocate speakers; reordered gains remain packet-bound",async()=>{
 const f=await fixture();f.authorize();f.wanted();f.pump();
 for(let id=0;id<16;id++)f.packet(new Uint8Array([9,0]),0,4,id);
 assert.equal(f.messages.filter(m=>m.kind==="media-error").length,16);
 for(const [index,gain] of [[0,.25],[2,.75],[1,.5]])f.packet(packets[index],index*2,gain,40,"rx",20+index,index===2);
 for(let index=0;index<6;index++) {
  f.render(index+1,480*(index+1));f.pump();
  assert.equal(f.port.sent.at(-1).speakers[0].volume,[.25,.25,.5,.5,.75,.75][index]);
 }
 f.render(7,3360);f.pump();assert.equal(f.port.sent.at(-1).speakers.length,0);f.close();
});

test("exact request sequence rejects duplicate, stale, skipped and malformed work",async()=>{
 const f=await fixture();f.authorize();f.wanted();f.pump();f.render();f.pump();
 const before=f.port.sent.length;
 f.render();f.render(2,1440);f.render(3,-480);f.render(3,960,"rx",0);f.pump();assert.equal(f.port.sent.length,before);
 f.render(2,960);f.pump();assert.equal(f.port.sent.length,before+1);
 f.now(200);f.packet(packets[0],0);f.packet(packets[1],2);f.render(3,1440);f.pump();
 assert.equal(f.port.sent.at(-1).speakers.length,0,"stalled output cannot replay cached audio");f.close();
});

test("packet/speaker budgets admit 16 legitimate 100pps streams but bound churn",async()=>{
 const f=await fixture();f.authorize();f.wanted();f.pump();
 for(let tick=0;tick<100;tick++) {
  f.now(tick*10);
  for(let speaker=0;speaker<16;speaker++)f.packet(packets[tick%8],tick*2,1,speaker,"rx",tick*16+speaker+1);
 }
 assert.equal(f.messages.filter(m=>m.kind==="packet-queued").length,1600);
 assert.equal(f.messages.filter(m=>m.kind==="media-error").length,0);
 f.packet(packets[0],0,1,17,"rx",1601);assert.equal(f.messages.at(-1).kind,"media-error");f.close();
});

test("actual Worklet/direct worker/WASM pipeline has one reserve and exact stereo/gain PCM",async()=>{
 const f=await fixture(),p=new Processor();
 p.port.onmessage({data:{kind:"playout-port",port:f.remote}});
 f.authorize();p.port.onmessage({data:{kind:"configure",epoch:"rx",enabled:true,clocked:true}});f.pump();
 const oracle=new codec.VoicePlayout();
 for(const index of [0,2,1,3,4]) {
  f.packet(packets[index],index*2,.25+index*.125,7,"rx",index+1,index===4);
  oracle.push(BigInt(index*2),packets[index],index===4,.25+index*.125,0n);
 }
 const expected=new Float32Array(480*2+41*480*2);
 for(let tick=0;tick<40;tick++) {
  const samples=oracle.render(),gain=oracle.gain();
  for(let i=0;i<samples.length;i++)expected[480*2+tick*960+i]=Math.max(-1,Math.min(1,samples[i]*gain));
 }
 const actual=[];
 for(let quantum=0;quantum<154;quantum++) {
  f.now(quantum*128/48);const l=new Float32Array(128),r=new Float32Array(128);
  p.process([],[[l,r]]);for(let i=0;i<128;i++)actual.push(l[i],r[i]);f.pump();
 }
 assert.deepEqual(Float32Array.from(actual),expected.slice(0,actual.length));
 assert.equal(p.port.sent.filter(m=>m.kind==="playout-error").length,0);
 assert.ok(f.messages.every(m=>m.kind!=="decoded"),"no UI PCM traffic");
 const blocks=f.port.sent.filter(m=>m.kind==="playout");assert.ok(blocks.length>30);
 assert.ok(blocks.every(m=>m.speakers.length<=16));oracle.free();f.close();
});
