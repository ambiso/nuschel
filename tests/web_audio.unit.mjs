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
    joined:true,muted:false,deafened:false,can_transmit:true};
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
  assert.equal(audio.txJobs.size,1);assert.equal(completions.length,3);
  completions[0]();completions[1]();await turn();assert.equal(audio.txJobs.size,1);
  completions[2]();await turn();assert.equal(audio.txJobs.size,0);await audio.dispose();
});

test("a stalled transport closes capture instead of growing an unlimited RPC backlog",async()=>{
  const audio=new BrowserAudio({onPacket:()=>new Promise(()=>{})});audio.syncVoice(voice());
  audio.sendPacket(0n,new Uint8Array([0xf8,0xff,0xfe]),false);
  await new Promise(resolve=>setTimeout(resolve,300));
  assert.equal(audio.transportStalled,true);assert.equal(audio.txJobs.size,0);assert.equal(audio.phase,"transport-error");
  await audio.dispose();
});

test("late decoder replies are discarded while a current realtime frame reaches the worklet",async()=>{
  const output=[],sent=[];const audio=new BrowserAudio();audio.syncVoice(voice());
  audio.ctx={state:"running",currentTime:0};audio.workerReady=true;audio.nodesReady=true;
  audio.worker={postMessage:message=>sent.push(message),terminate:()=>{}};
  audio.playbackNode={port:{postMessage:message=>output.push(message)},disconnect:()=>{}};
  const packet={...voice(),speaker:7,sequence:"0",opus:new Uint8Array([0xf8,0xff,0xfe]),end:false,volume:1};
  assert.equal(audio.receive(packet),true);const current=sent.at(-1);
  audio.workerMessage({kind:"decoded",epoch:audio.epoch,job:current.job,speaker:7,samples:new Float32Array(AUDIO_FRAME),volume:1});
  assert.equal(output.length,1);
  assert.equal(audio.receive({...packet,sequence:"2"}),true);const late=sent.at(-1);
  await new Promise(resolve=>setTimeout(resolve,150));
  audio.workerMessage({kind:"decoded",epoch:audio.epoch,job:late.job,speaker:7,samples:new Float32Array(AUDIO_FRAME),volume:1});
  assert.equal(output.length,1);assert.equal(audio.decodeJobs.size,0);
  audio.ctx=null;await audio.dispose();
});
