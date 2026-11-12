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
const wasm=codec.initSync({module:await readFile(join(directory,"nuschl_audio_wasm_bg.wasm"))});
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
async function fixture({loading=false,measure=false}={}) {
  let time=0,release;const queue=[],messages=[],ready=new Promise(resolve=>{release=resolve;});
  let decodes=0;
  class MeasuredPlayout {
    constructor(){this.inner=new codec.VoicePlayout();}
    static validate_packet(packet){codec.VoicePlayout.validate_packet(packet);}
    push(...args){return this.inner.push(...args);}
    render(){const samples=this.inner.render();if(this.inner.last_render_kind())decodes++;return samples;}
    gain(){return this.inner.gain();}free(){this.inner.free();}
  }
  const context=vm.createContext({...protocol,VoiceEncoder:codec.VoiceEncoder,VoicePlayout:measure?MeasuredPlayout:codec.VoicePlayout,
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
  return {send,port,remote,queue,messages,pump,decodes:()=>decodes,authorize,wanted,render,packet,load,now:value=>{time=value;},close:()=>send({kind:"close"})};
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

test("new RX packets interleaved with either barrier order survive without old-RX replay",async()=>{
 for(const mainFirst of [true,false]) {
  const f=await fixture();f.authorize();f.wanted();f.pump();
  f.packet(packets[0],0,4,8);f.packet(packets[1],2,4,8,"rx",2);
  if(mainFirst)f.authorize("new");else{f.wanted("new",2);f.pump();}
  if(!mainFirst)f.authorize("new");
  f.packet(packets[2],0,.25,20,"new",3);f.packet(packets[3],2,.5,20,"new",4);
  if(mainFirst){f.wanted("new",2);f.pump();}
  f.render(1,480,"new",2);f.pump();
  const reply=f.port.sent.at(-1);assert.equal(reply.speakers.length,1);assert.equal(reply.speakers[0].speaker,20);
  assert.equal(reply.speakers[0].volume,.25);
  // Disabled->enabled with the same RX also preserves freshly authorized data.
  f.authorize("new",false,"tx2",false);f.wanted("new",3,false);f.pump();
  f.authorize("new",false,"tx3",true);f.packet(packets[4],0,.75,30,"new",5);f.packet(packets[5],2,.75,30,"new",6);
  f.wanted("new",4,true);f.pump();f.render(2,480,"new",4);f.pump();
  assert.equal(f.port.sent.at(-1).speakers[0].speaker,30);
  assert.equal(f.port.sent.at(-1).speakers[0].volume,.75);f.close();
 }
});

test("late disabled barrier from the previous authorization cannot discard freshly reenabled RX",async()=>{
 const f=await fixture();f.authorize("old");f.wanted("old",1);f.pump();
 // Main revokes, then re-enables with its mandatory fresh receive epoch before
 // the old Worklet disable control is processed on the other port.
 f.authorize("old",false,"tx2",false);f.authorize("new",false,"tx3",true);
 f.packet(packets[0],0,.25,9,"new");f.packet(packets[1],2,.5,9,"new",2);
 f.wanted("old",2,false);f.wanted("new",3,true);f.pump();f.render(1,480,"new",3);f.pump();
 assert.equal(f.port.sent.at(-1).speakers.length,1);assert.equal(f.port.sent.at(-1).speakers[0].volume,.25);f.close();
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
  for(let speaker=0;speaker<16;speaker++)f.packet(new Uint8Array([0xf0,0xff,0xfe]),tick,1,speaker,"rx",tick*16+speaker+1);
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


test("16 active receivers have bounded actual-WASM memory and measured render work",async()=>{
 const f=await fixture(),cost=[];f.authorize();f.wanted();f.pump();
 const initialBytes=wasm.memory.buffer.byteLength;
 for(let tick=0;tick<120;tick++) {
  f.now(tick*10);
  for(let speaker=0;speaker<16;speaker++)f.packet(new Uint8Array([0xf0,0xff,0xfe]),tick,1,speaker,"rx",tick*16+speaker+1);
  const started=performance.now();f.render(tick+1,(tick+1)*480);f.pump();cost.push(performance.now()-started);
  const reply=f.port.sent.at(-1);assert.equal(reply.kind,"playout");assert.ok(reply.speakers.length<=16);
 }
 const memoryBytes=wasm.memory.buffer.byteLength;assert.ok(memoryBytes<256*1024*1024);
 cost.sort((a,b)=>a-b);
 console.log(JSON.stringify({active_receivers:16,renders:120,wasm_memory_before:initialBytes,wasm_memory_peak:memoryBytes,
  render_ms:{p50:cost[60],p95:cost[114],max:cost.at(-1)},scope:"Node actual-WASM silence packets, not a hardware/browser latency guarantee"}));
 f.close();
});

// Test-side immutable 10 ms stereo packet from the independent libopus fixture
// (tests/opus_playout.py, 64 kbit CELT restricted-lowdelay, phase control1).
const ten=Uint8Array.from(Buffer.from("f49fd589d2f97065736f5bac893b0dd89b7ae42f02e91d10084ff7ecf209a8d2a5f01797028cfe0ced9e50e9a43a58690fa7dce6e7506e2f9fffacb03022744e538a81509e53c0d117c0930970f24c91","hex"));
const long=Uint8Array.from([ten[0]|3,12,...Array.from({length:12},()=>Array.from(ten.slice(1))).flat()]);
test("realized startup includes duration policy plus one transport block; single120ms/end/restart remains complete",async()=>{
 const report=[];
 for(const [name,packet,spacing,ended,expected] of [["10ms",ten,1,false,1920],["20ms",packets[0],2,false,1440],
   ["120ms",long,12,false,6240],["single120ms-end",long,12,true,480]]) {
  const f=await fixture(),p=new Processor();p.port.onmessage({data:{kind:"playout-port",port:f.remote}});
  f.authorize();p.port.onmessage({data:{kind:"configure",epoch:"rx",enabled:true,clocked:true}});f.pump();
  let first=null,blocks=0;
  for(let tick=0;tick<40;tick++) {
   f.now(tick*10);
   if(ended?tick===0:tick%spacing===0)f.packet(packet,tick,.25,1,"rx",tick+1,ended);
   p.process([],[[new Float32Array(480),new Float32Array(480)]]);f.pump();
   const batch=f.port.sent.at(-1);
   if(batch?.kind==="playout"&&batch.speakers.length){first??=batch.start_frame;blocks++;}
  }
  assert.equal(first,expected,name);if(ended)assert.equal(blocks,12,"all120ms cached frames survive end");
  report.push({name,first_decoded_output_frame:first,receive_start_ms:first/48,decoded_blocks:blocks});
  if(ended) {
   f.packet(long,0,.5,1,"rx",100,true);let restarted=0;
   for(let tick=40;tick<55;tick++) {f.now(tick*10);p.process([],[[new Float32Array(480),new Float32Array(480)]]);f.pump();
    const batch=f.port.sent.at(-1);if(batch?.kind==="playout"&&batch.speakers.length){restarted++;assert.equal(batch.speakers[0].volume,.5);}}
   assert.equal(restarted,12);
  }
  f.close();
 }
 console.log(JSON.stringify({realized_startup:report,scope:"explicit480-frame clock; excludes sender packetization/device/browser scheduling"}));
});

test("worst permitted duration performs at most one120ms decode per16 speakers per render",async()=>{
 const f=await fixture();f.authorize();f.wanted();f.pump();
 for(let speaker=0;speaker<16;speaker++)f.packet(long,0,.25,speaker,"rx",speaker+1,true);
 const before=wasm.memory.buffer.byteLength,started=performance.now();f.render();f.pump();const elapsed=performance.now()-started;
 const reply=f.port.sent.at(-1);assert.equal(reply.speakers.length,16);
 assert.ok(reply.speakers.every(s=>s.samples.length===960&&s.samples.every(Number.isFinite)));
 for(let tick=1;tick<12;tick++){f.now(tick*10);f.render(tick+1,(tick+1)*480);f.pump();assert.equal(f.port.sent.at(-1).speakers.length,16);}
 f.render(13,6240);f.pump();assert.equal(f.port.sent.at(-1).speakers.length,0);
 console.log(JSON.stringify({worst_case:{speakers:16,packet_ms:120,first_render_ms:elapsed,wasm_before:before,wasm_after:wasm.memory.buffer.byteLength},
  scope:"bounded decode work, not a guaranteed10ms CPU deadline on every device"}));f.close();
});


test("non-silent independent10/20/120ms packets measure aligned16-speaker decode bursts",async()=>{
 const matrix=[];
 for(const duration of [1,2,12]) {
  const packet=duration===1?ten:Uint8Array.from([ten[0]|3,duration,...Array.from({length:duration},()=>Array.from(ten.slice(1))).flat()]);
  const f=await fixture({measure:true}),cost=[],decodeCost=[];f.authorize();f.wanted();f.pump();
  // Prebuffer enough for the inherited40ms minimum, then feed at exact cadence.
  const initial=Math.ceil(4/duration)+1;
  for(let index=0;index<initial;index++)for(let speaker=0;speaker<16;speaker++)f.packet(packet,index*duration,.25,speaker,"rx",index*16+speaker+1);
  for(let tick=0;tick<240;tick++) {
   f.now(tick*10);
   if(tick%duration===0)for(let speaker=0;speaker<16;speaker++)f.packet(packet,(initial+tick/duration)*duration,.25,speaker,"rx",1000+tick*16+speaker);
   const count=f.decodes(),start=performance.now();f.render(tick+1,(tick+1)*480);f.pump();const elapsed=performance.now()-start;cost.push(elapsed);
   const decoded=f.decodes()-count;assert.ok(decoded===0||decoded===16,"decode bursts remain aligned");
   if(decoded)decodeCost.push(elapsed);
   const batch=f.port.sent.at(-1);assert.equal(batch.kind,"playout");assert.equal(batch.speakers.length,16);
   assert.ok(batch.speakers.every(s=>s.samples.every(Number.isFinite)));
  }
  const stats=list=>{list.sort((a,b)=>a-b);return {p50:list[Math.floor(list.length*.5)],p95:list[Math.floor(list.length*.95)],max:list.at(-1),over10ms:list.filter(x=>x>10).length,over20ms:list.filter(x=>x>20).length};};
  matrix.push({duration_ms:duration*10,speakers:16,renders:cost.length,render_ms:stats(cost),aligned_decode_bursts:decodeCost.length,aligned_decode_ms:stats(decodeCost),wasm_memory:wasm.memory.buffer.byteLength});f.close();
 }
 console.log(JSON.stringify({non_silent_reference_packets:matrix,scope:"Node production worker+actualWASM, costs compared with10/20ms reserve; actual scheduling tested separately"}));
});
