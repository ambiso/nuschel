// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {PcmMixer,CaptureFrames,AUDIO_FRAME,MAX_QUEUED_SAMPLES,MAX_SPEAKERS} from "../web/app/src/audio-worklet.js";
import {BrowserAudio} from "../web/app/src/audio.js";
import {context} from "../web/app/src/audio-protocol.js";

test("real worklet capture credits bound a stalled codec and reject stale acknowledgments",()=>{
  const frames=[];const capture=new CaptureFrames((epoch,samples)=>frames.push({epoch,samples}));
  capture.configure("voice:1",true);
  capture.append(new Float32Array(AUDIO_FRAME*100).fill(0.25));
  assert.equal(frames.length,2);assert.equal(capture.cursor,0);
  assert.ok(frames.every(f=>f.samples.length===AUDIO_FRAME&&f.samples.every(v=>v===0.25)));
  capture.credit("old");capture.append(new Float32Array(AUDIO_FRAME));assert.equal(frames.length,2);
  capture.credit("voice:1");capture.append(new Float32Array(AUDIO_FRAME));assert.equal(frames.length,3);
  capture.configure("voice:2",false);capture.credit("voice:1");capture.append(new Float32Array(AUDIO_FRAME));
  assert.equal(frames.length,3);
});

test("partial capture from before a mute or route change cannot enter the next frame",()=>{
  const frames=[];const capture=new CaptureFrames((epoch,samples)=>frames.push({epoch,samples}));
  capture.configure("first",true);capture.append(new Float32Array(500).fill(0.75));
  capture.configure("second",true);capture.append(new Float32Array(AUDIO_FRAME).fill(-0.25));
  assert.equal(frames.length,1);assert.equal(frames[0].epoch,"second");
  assert.ok(frames[0].samples.every(v=>v===-0.25));
});

test("mixer preserves separate speaker PCM, clips finite output and has finite latency",()=>{
  const mixer=new PcmMixer();mixer.configure("current",true);
  mixer.enqueue({epoch:"current",speaker:1,samples:new Float32Array(MAX_QUEUED_SAMPLES).fill(0.25)});
  mixer.enqueue({epoch:"current",speaker:1,samples:new Float32Array(MAX_QUEUED_SAMPLES).fill(0.5)});
  mixer.enqueue({epoch:"current",speaker:2,samples:new Float32Array(128).fill(0.75)});
  const prebuffer=new Float32Array(AUDIO_FRAME);mixer.render(prebuffer);assert.ok(prebuffer.every(v=>v===0));
  const out=new Float32Array(128);mixer.render(out);assert.ok(out.every(v=>v===1));
  mixer.render(out);assert.ok(out.every(v=>v===0.5));
  for(let i=0;i<MAX_QUEUED_SAMPLES/128+1;i++)mixer.render(out);
  assert.ok(out.every(v=>v===0),"old samples cannot survive beyond bounded playout");
});

test("deafen/epoch changes clear all buffered sound, and malformed or extra speakers stay bounded",()=>{
  const mixer=new PcmMixer();mixer.configure("old",true);
  for(let i=0;i<MAX_SPEAKERS;i++)assert.equal(mixer.enqueue({epoch:"old",speaker:i,samples:new Float32Array(128).fill(0.1)}),true);
  assert.equal(mixer.enqueue({epoch:"old",speaker:MAX_SPEAKERS,samples:new Float32Array(128)}),false);
  assert.equal(mixer.enqueue({epoch:"old",speaker:0,samples:new Float32Array([NaN])}),false);
  assert.equal(mixer.enqueue({epoch:"old",speaker:0,samples:new Float32Array(MAX_QUEUED_SAMPLES+1)}),false);
  mixer.configure("deaf",false);const out=new Float32Array(128).fill(1);mixer.render(out);assert.ok(out.every(v=>v===0));
  assert.equal(mixer.speakers.size,0);
  mixer.configure("new",true);
  assert.equal(mixer.enqueue({epoch:"old",speaker:0,samples:new Float32Array(128).fill(1)}),false);
  mixer.render(out);assert.ok(out.every(v=>v===0));
});

function voice(generation="1") {
  return {server_id:"web:"+"u".repeat(128)+"@"+"a".repeat(253)+":64738",
    connection_generation:generation,voice_generation:"9007199254740993",channel_id:0,
    joined:true,muted:false,deafened:false,can_transmit:true,bitrate:128000};
}
const turn=()=>new Promise(resolve=>setImmediate(resolve));

test("valid long bookmark identities and exact u64 generations survive the audio boundary",()=>{
  const value=voice();assert.ok(context(value));assert.equal(context(value).voice_generation,"9007199254740993");
  assert.equal(context({...value,server_id:"x".repeat(1025)}),null);
  assert.equal(context({...value,voice_generation:"18446744073709551616"}),null);
});

test("old transport promises cannot consume or release the next connection's audio credits",async()=>{
  const completions=[];const audio=new BrowserAudio({onPacket:()=>new Promise(resolve=>completions.push(resolve))});
  audio.syncVoice(voice("1"));audio.sendPacket(0n,new Uint8Array([0xf8,0xff,0xfe]),false);audio.sendPacket(2n,new Uint8Array([0xf8,0xff,0xfe]),false);
  audio.sendPacket(4n,new Uint8Array([0xf8,0xff,0xfe]),false);assert.equal(completions.length,2);
  audio.syncVoice(voice("2"));audio.sendPacket(0n,new Uint8Array([0xf8,0xff,0xfe]),false);
  assert.equal(audio.txJobs.size,2);assert.equal(completions.length,2,"epoch change cannot acknowledge old SDK inbox work");
  completions[0]();await turn();assert.equal(audio.txJobs.size,1);
  audio.sendPacket(0n,new Uint8Array([0xf8,0xff,0xfe]),false);assert.equal(completions.length,3);assert.equal(audio.txJobs.size,2);
  completions[1]();await turn();assert.equal(audio.txJobs.size,1);
  completions[2]();await turn();assert.equal(audio.txJobs.size,0);await audio.dispose();
});

test("a stalled transport closes capture instead of growing an unlimited RPC backlog",async()=>{
  let reply;const audio=new BrowserAudio({onPacket:()=>new Promise(resolve=>{reply=resolve;})});audio.syncVoice(voice());
  audio.sendPacket(0n,new Uint8Array([0xf8,0xff,0xfe]),false);
  await new Promise(resolve=>setTimeout(resolve,300));
  assert.equal(audio.transportStalled,true);assert.equal(audio.txJobs.size,1,"timeout is not an SDK acknowledgment");assert.equal(audio.phase,"transport-error");
  reply();await turn();assert.equal(audio.txJobs.size,0);await audio.dispose();
});

test("packet inbox stays bounded across stalls and rapid RX revoke/reopen until exact ACK",async()=>{
  const output=[],sent=[];const audio=new BrowserAudio();audio.syncVoice(voice());
  audio.ctx={state:"running",currentTime:0};audio.workerReady=true;audio.nodesReady=true;
  audio.worker={postMessage:message=>sent.push(message),terminate:()=>{}};
  audio.playbackNode={port:{postMessage:message=>output.push(message)},disconnect:()=>{}};
  const packet=()=>({...audio.voice,speaker:7,sequence:"0",opus:new Uint8Array([0xf8,0xff,0xfe]),end:false,volume:1});
  for(let index=0;index<16;index++)assert.equal(audio.receive(packet()),true);
  const first=sent.find(message=>message.kind==="decode");
  assert.equal(audio.receive(packet()),false);
  await new Promise(resolve=>setTimeout(resolve,150));
  assert.equal(audio.receive(packet()),false,"elapsed time cannot release inbox credit");
  for(let index=0;index<20;index++) {
    audio.syncVoice({...voice(),voice_generation:String(100+index*2),deafened:true});
    audio.syncVoice({...voice(),voice_generation:String(101+index*2)});
    assert.equal(audio.receive(packet()),false);
  }
  assert.equal(audio.decodeJobs.size,16);
  audio.workerMessage({kind:"packet-queued",epoch:audio.receiveEpoch,job:first.job});
  assert.equal(audio.decodeJobs.size,16,"wrong RX cannot ACK an old entry");
  audio.workerMessage({kind:"packet-queued",epoch:first.epoch,job:first.job});
  assert.equal(audio.decodeJobs.size,15);assert.equal(audio.receive(packet()),true);
  audio.workerMessage({kind:"packet-queued",epoch:first.epoch,job:first.job});
  assert.equal(audio.decodeJobs.size,16,"duplicate cannot ACK a newer job");
  assert.ok(output.every(message=>message.kind==="configure"),"PCM never crosses UI");
  audio.ctx=null;await audio.dispose();
});

test("stalled encoding stays globally bounded across TX epochs; stale/unsolicited ACK cannot grant capture credits",async()=>{
  const sent=[],credits=[];const audio=new BrowserAudio();audio.syncVoice(voice());
  audio.worker={postMessage:message=>sent.push(message),terminate:()=>{}};
  audio.captureNode={port:{postMessage:message=>credits.push(message)},disconnect:()=>{}};
  const frame=()=>({kind:"capture",epoch:audio.epoch,frame:0,samples:new Float32Array(960)});
  audio.captureRunning=true;audio.captureFrame(frame());audio.captureFrame(frame());
  const original=sent.filter(m=>m.kind==="encode");assert.equal(original.length,2);
  for(let index=0;index<20;index++) {
    audio.resetMedia();audio.captureRunning=true;audio.captureFrame(frame());audio.captureFrame(frame());
  }
  assert.equal(sent.filter(m=>m.kind==="encode").length,2);assert.equal(audio.encodeJobs.size,2);
  const before=credits.length;
  audio.workerMessage({kind:"encoded",epoch:audio.epoch,job:original[0].job,opus:new Uint8Array([0xf8,0xff,0xfe])});
  audio.workerMessage({kind:"encoded",epoch:audio.epoch,job:99999,opus:new Uint8Array([0xf8,0xff,0xfe])});
  assert.equal(credits.length,before);assert.equal(audio.encodeJobs.size,2);
  audio.workerMessage({kind:"encoded",epoch:original[0].epoch,job:original[0].job,opus:new Uint8Array([0xf8,0xff,0xfe])});
  assert.equal(credits.length,before,"old ACK cannot replenish current Worklet credits");
  assert.equal(audio.encodeJobs.size,1);audio.captureFrame(frame());assert.equal(audio.encodeJobs.size,2);
  audio.workerMessage({kind:"encoded",epoch:original[0].epoch,job:original[0].job,opus:new Uint8Array([0xf8,0xff,0xfe])});
  assert.equal(audio.encodeJobs.size,2);await audio.dispose();
});

test("mic mute and bitrate reset TX only, but deafen/route changes revoke RX",async()=>{
  const messages=[],playback=[];const audio=new BrowserAudio();audio.syncVoice(voice());
  audio.ctx={state:"running",currentTime:0};audio.workerReady=true;audio.nodesReady=true;
  audio.worker={postMessage:message=>messages.push(message),terminate:()=>{}};
  audio.playbackNode={port:{postMessage:message=>playback.push(message)},disconnect:()=>{}};
  audio.applyPolicy();messages.length=0;playback.length=0;
  const rx=audio.receiveEpoch,tx=audio.epoch;
  audio.setLocalPolicy({muted:true});assert.equal(audio.receiveEpoch,rx);assert.notEqual(audio.epoch,tx);
  audio.syncVoice({...voice(),voice_generation:"10",muted:true,bitrate:24000});
  assert.equal(audio.receiveEpoch,rx);assert.equal(playback.length,0);
  assert.ok(messages.every(message=>message.kind!=="configure"||message.receive_epoch===rx));
  audio.syncVoice({...voice(),voice_generation:"11",deafened:true});assert.notEqual(audio.receiveEpoch,rx);
  const deaf=audio.receiveEpoch;audio.syncVoice({...voice(),voice_generation:"12"});assert.notEqual(audio.receiveEpoch,deaf);
  const before=audio.receiveEpoch;audio.syncVoice({...voice(),channel_id:1});assert.notEqual(audio.receiveEpoch,before);
  assert.ok(playback.every(message=>message.clocked===true));
  audio.ctx=null;await audio.dispose();
});

test("every playback reenable has a new RX authorization even when old disable messages are delayed",async()=>{
  const messages=[],audio=new BrowserAudio();audio.syncVoice(voice());
  audio.ctx={state:"running",currentTime:0};audio.workerReady=true;audio.nodesReady=true;
  audio.worker={postMessage:m=>messages.push(m),terminate:()=>{}};
  audio.playbackNode={port:{postMessage:()=>{}},disconnect:()=>{}};
  audio.applyPolicy();const initial=audio.receiveEpoch;
  audio.ctx.state="suspended";audio.applyPolicy();
  audio.ctx.state="running";audio.applyPolicy();
  assert.notEqual(audio.receiveEpoch,initial);
  const latest=messages.filter(m=>m.kind==="configure").at(-1);
  assert.equal(latest.playback,true);assert.equal(latest.receive_epoch,audio.receiveEpoch);
  audio.ctx=null;await audio.dispose();
});

test("explicit undeafen restores acknowledged playback while microphone capture stays closed",async()=>{
  const originals=new Map(["isSecureContext","AudioContext","AudioWorkletNode"].map(key=>[key,Object.getOwnPropertyDescriptor(globalThis,key)]));
  Object.defineProperty(globalThis,"isSecureContext",{value:true,configurable:true});
  Object.defineProperty(globalThis,"AudioContext",{value:class{},configurable:true});
  Object.defineProperty(globalThis,"AudioWorkletNode",{value:class{},configurable:true});
  const ready=[];const audio=new BrowserAudio({onReady:value=>ready.push(value)});
  try {
    audio.syncVoice({...voice(),muted:true,deafened:true,can_transmit:false});
    audio.ctx={state:"running",currentTime:0,resume:()=>Promise.resolve(),close:()=>Promise.resolve()};
    audio.workerReady=true;audio.nodesReady=true;audio.initializing=Promise.resolve();
    audio.worker={postMessage:()=>{},terminate:()=>{}};
    audio.captureNode={port:{postMessage:()=>{}},disconnect:()=>{}};
    audio.playbackNode={port:{postMessage:()=>{}},disconnect:()=>{}};
    audio.setLocalPolicy({muted:true,deafened:true});
    assert.equal(audio.playbackEnabled(),false);
    await audio.activateGesture();
    assert.equal(audio.playbackEnabled(),false,"SDK deafen acknowledgment still gates playback");
    audio.syncVoice({...voice(),voice_generation:"9007199254740994",muted:true,deafened:false,can_transmit:false});
    assert.equal(audio.playbackEnabled(),true);assert.equal(audio.captureEnabled(),false);
    assert.equal(audio.consent,null);assert.equal(ready.at(-1).capture,false);assert.equal(ready.at(-1).playback,true);
  } finally {
    await audio.dispose();
    for(const [key,descriptor] of originals)if(descriptor)Object.defineProperty(globalThis,key,descriptor);else delete globalThis[key];
  }
});


test("authenticated route targets reach only the codec and budget updates invalidate queued media",async()=>{
  const messages=[];const audio=new BrowserAudio();audio.syncVoice(voice());
  audio.ctx={state:"running",currentTime:0};audio.workerReady=true;audio.nodesReady=true;
  audio.worker={postMessage:message=>messages.push(message),terminate:()=>{}};
  audio.captureNode={port:{postMessage:()=>{}},disconnect:()=>{}};
  audio.consent=context(audio.voice);
  audio.stream={getAudioTracks:()=>[{muted:false,readyState:"live"}],getTracks:()=>[]};
  audio.source={disconnect:()=>{}};
  audio.applyPolicy();
  assert.equal(messages.at(-1).bitrate,128000);
  audio.encodeJobs.set(1,{epoch:audio.epoch});const oldEpoch=audio.epoch;
  audio.syncVoice({...voice(),voice_generation:"9007199254740994",bitrate:24000});
  assert.notEqual(audio.epoch,oldEpoch);assert.equal(audio.encodeJobs.size,1,"unacknowledged old work retains its global inbox credit");
  audio.workerMessage({kind:"encoded",epoch:oldEpoch,job:1,opus:new Uint8Array([0xf8,0xff,0xfe])});
  assert.equal(audio.encodeJobs.size,0);
  assert.equal(messages.at(-1).bitrate,24000);
  assert.ok(audio.captureEnabled());
  audio.syncVoice({...voice(),voice_generation:"9007199254740995",bitrate:0,can_transmit:false});
  assert.equal(audio.captureEnabled(),false);assert.equal(audio.stream,null);assert.equal(audio.consent,null);
  assert.equal(audio.playbackEnabled(),true,"a tiny upload budget does not disable listening");
  audio.ctx=null;await audio.dispose();
});

test("missing and malformed encoder targets cannot open capture",async()=>{
  const audio=new BrowserAudio();
  for(const bitrate of [undefined,-1,5999,128001,Infinity,"128000"]) {
    audio.syncVoice({...voice(),bitrate});assert.equal(audio.voice.bitrate,0);assert.equal(audio.captureEnabled(),false);
  }
  await audio.dispose();
});


test("stereo worklet retains LR, mono sinks fold deliberately, and gain precedes clipping",()=>{
  const stereo=Float32Array.from({length:1920},(_,i)=>i%2===0?.2:-.3);
  const make=()=>{const mixer=new PcmMixer();mixer.configure("stereo",true);
    assert.ok(mixer.enqueue({epoch:"stereo",speaker:1,samples:stereo,channels:2,frames:960,volume:1}));
    mixer.render(new Float32Array(960),new Float32Array(960));return mixer;};
  const l=new Float32Array(128),r=new Float32Array(128);make().render(l,r);
  assert.ok(l.every(v=>Math.abs(v-.2)<1e-6));assert.ok(r.every(v=>Math.abs(v+.3)<1e-6));
  const mono=new Float32Array(128);make().render(mono);assert.ok(mono.every(v=>Math.abs(v+.05)<1e-6));
  const attenuated=new PcmMixer();attenuated.configure("gain",true);
  attenuated.enqueue({epoch:"gain",speaker:1,samples:new Float32Array(128).fill(2),volume:.25});
  attenuated.render(new Float32Array(960));attenuated.render(mono);assert.ok(mono.every(v=>v===.5));
  assert.equal(attenuated.enqueue({epoch:"gain",speaker:1,samples:new Float32Array(3),channels:2,frames:2}),false);
  assert.equal(attenuated.enqueue({epoch:"gain",speaker:1,samples:new Float32Array(2*(MAX_QUEUED_SAMPLES+1)),channels:2,frames:MAX_QUEUED_SAMPLES+1}),false);
});


test("legal 120 ms packets keep their complete tail across the initial hold",()=>{
  const mixer=new PcmMixer();mixer.configure("long",true);
  assert.ok(mixer.enqueue({epoch:"long",speaker:1,samples:new Float32Array(11520).fill(.25),channels:2,frames:5760}));
  mixer.render(new Float32Array(960));
  mixer.render(new Float32Array(4800));
  assert.ok(mixer.enqueue({epoch:"long",speaker:1,samples:new Float32Array(11520).fill(.5),channels:2,frames:5760}));
  const tail=new Float32Array(960);mixer.render(tail);assert.ok(tail.every(v=>v===.25));
  const next=new Float32Array(5760);mixer.render(next);assert.ok(next.every(v=>v===.5));
});

test("wide worklet sums opposing headroom before clipping, with gain frozen per frame",()=>{
  const mixer=new PcmMixer();mixer.configure("wide",true);
  for(const [speaker,value] of [[1,1.5],[2,-1.25]])
    assert.ok(mixer.enqueue({epoch:"wide",speaker,samples:new Float32Array(960).fill(value)}));
  const left=new Float32Array(960),right=new Float32Array(960);
  mixer.render(left,right);mixer.render(left,right);
  assert.ok(left.every(v=>v===.25));assert.deepEqual(left,right);
  // Finite f32 ×4 must not first overflow into f32, nor turn cancellation NaN.
  mixer.configure("wide",true);
  const max=3.4028234663852886e38;
  for(let speaker=0;speaker<MAX_SPEAKERS;speaker++)
    assert.ok(mixer.enqueue({epoch:"wide",speaker,samples:new Float32Array(960).fill(speaker%2?max:-max),volume:4}));
  mixer.render(left,right);mixer.render(left,right);
  assert.ok(left.every(v=>v===0));assert.deepEqual(left,right);
  mixer.configure("wide",true);
  assert.ok(mixer.enqueue({epoch:"wide",speaker:1,samples:new Float32Array(960).fill(max),volume:4}));
  mixer.render(left);mixer.render(left);assert.ok(left.every(v=>v===1));
  for(const value of [NaN,Infinity,-Infinity])
    assert.equal(mixer.enqueue({epoch:"wide",speaker:1,samples:new Float32Array(960).fill(value)}),false);
  mixer.configure("wide",true);
  for(const volume of [.25,.5])assert.ok(mixer.enqueue({epoch:"wide",speaker:1,samples:new Float32Array(128).fill(2),volume}));
  mixer.render(new Float32Array(960));
  const changing=new Float32Array(256);mixer.render(changing);
  assert.ok(changing.slice(0,128).every(v=>v===.5));assert.ok(changing.slice(128).every(v=>v===1));
  const scratch=[mixer.left,mixer.right];
  mixer.render(left,right);assert.equal(mixer.left,scratch[0]);assert.equal(mixer.right,scratch[1]);
  const tooLarge=new Float32Array(MAX_QUEUED_SAMPLES+1).fill(1);mixer.render(tooLarge);assert.ok(tooLarge.every(v=>v===0));
});
