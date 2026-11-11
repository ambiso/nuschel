// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {PcmMixer,CaptureFrames,AUDIO_FRAME,MAX_QUEUED_SAMPLES,MAX_SPEAKERS} from "../web/app/src/audio-worklet.js";

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
