// SPDX-License-Identifier: EUPL-1.2
// No session, credentials, endpoint or storage capability crosses this boundary.
// Codec/WASM work stays here, never in the UI or AudioWorklet callback.
import init,{VoiceEncoder,VoicePlayout} from "./audio-codec/nuschl_audio_wasm.js";
import {FRAME,MAX_PACKET,MAX_STREAMS,decimal} from "./audio-protocol.js";
let encoder=null,epoch="",enabled=false,receiveEpoch="",playback=false,loaded=false;
let pendingConfigure=null,port=null,wanted=null,lastRequest=0,lastStart=null,lastDemand=null;
const speakers=new Map();
let burst=0,created=0,windowStart=performance.now();
const validEpoch=value=>typeof value==="string"&&value.length>0&&value.length<=200;
const positive=value=>Number.isSafeInteger(value)&&value>0;
function freeSpeakers() {
  for(const state of speakers.values())state.receiver.free();
  speakers.clear();lastDemand=null;
}
function expire(now) {
  for(const [speaker,state] of speakers)if(now-state.last>2000){state.receiver.free();speakers.delete(speaker);}
}
function ready() {
  if(!loaded||!wanted||!port)return;
  port.postMessage({kind:"playout-ready",epoch:wanted.epoch,enabled:wanted.enabled&&playback&&wanted.epoch===receiveEpoch,clock_generation:wanted.clock_generation});
}
function configure(data) {
  if(!validEpoch(data.epoch)||!validEpoch(data.receive_epoch)||typeof data.capture!=="boolean"||typeof data.playback!=="boolean"||
      data.capture&&(!Number.isInteger(data.bitrate)||data.bitrate<6000||data.bitrate>128000))throw new Error("Invalid media configuration");
  if(!loaded){pendingConfigure=data;return;}
  // TX policy changes never destroy an independently authorized receive decoder.
  if(data.epoch!==epoch||data.capture!==enabled||enabled&&data.bitrate!==encoderBitrate) {
    encoder?.free();encoder=null;epoch=data.epoch;enabled=data.capture;
    if(enabled){encoder=new VoiceEncoder();encoder.set_bitrate(data.bitrate);}
    encoderBitrate=data.bitrate;
  }
  if(receiveEpoch!==data.receive_epoch||playback!==data.playback)freeSpeakers();
  receiveEpoch=data.receive_epoch;playback=data.playback;ready();
}
let encoderBitrate=0;
function direct(data) {
  if(!data||typeof data!=="object")return;
  if(data.kind==="playout-configure") {
    if(!validEpoch(data.epoch)||typeof data.enabled!=="boolean"||!positive(data.clock_generation))return;
    if(wanted&&data.clock_generation<wanted.clock_generation)return;
    if(wanted&&data.clock_generation===wanted.clock_generation) {
      if(data.epoch===wanted.epoch&&data.enabled===wanted.enabled)ready();
      return;
    }
    // Main may authorize/queue a new RX before this port's barrier arrives.
    // It already flushed the previous authorization: do not discard fresh data
    // merely because another port's old wanted tuple still exists.
    if(wanted&&data.epoch===receiveEpoch&&(!data.enabled||
        wanted.epoch===data.epoch&&wanted.enabled===data.enabled))freeSpeakers();
    wanted={epoch:data.epoch,enabled:data.enabled,clock_generation:data.clock_generation};
    lastStart=null;ready();return;
  }
  if(data.kind!=="render"||!loaded||!playback||!wanted?.enabled||wanted.epoch!==receiveEpoch||
      data.epoch!==receiveEpoch||data.clock_generation!==wanted.clock_generation||!positive(data.request)||data.request<=lastRequest||
      !Number.isSafeInteger(data.start_frame)||data.start_frame<0||data.start_frame>Number.MAX_SAFE_INTEGER-480||data.start_frame%480!==0||data.frames!==480||
      lastStart!==null&&data.start_frame!==lastStart+480)return;
  lastRequest=data.request;lastStart=data.start_frame;
  try {
    const now=performance.now();
    // Audio clock is authoritative. After a worker scheduling gap, discard
    // old decode/packet caches; never run a wall-clock catch-up burst.
    if(lastDemand!==null&&now-lastDemand>100)freeSpeakers();
    lastDemand=now;expire(now);
    const output=[],transfers=[];
    for(const [speaker,state] of speakers) {
      const samples=state.receiver.render();
      if(!samples.length)continue;
      if(!(samples instanceof Float32Array)||samples.length!==960||!samples.every(Number.isFinite))throw new Error("Invalid playout PCM");
      const volume=state.receiver.gain();
      if(!Number.isFinite(volume)||volume<0||volume>4)throw new Error("Invalid playout gain");
      output.push({speaker,samples,channels:2,frames:480,volume});transfers.push(samples.buffer);
    }
    port.postMessage({kind:"playout",epoch:data.epoch,clock_generation:data.clock_generation,request:data.request,start_frame:data.start_frame,frames:480,speakers:output},transfers);
  } catch(error) {
    // A fatal render error cannot release backpressure into more decode work.
    playback=false;freeSpeakers();ready();
    postMessage({kind:"playout-error",epoch:receiveEpoch,message:String(error?.message||error).slice(0,200)});
  }
}
function packet(data) {
  if(!playback||data.epoch!==receiveEpoch) {
    if(validEpoch(data.epoch)&&positive(data.job))postMessage({kind:"packet-queued",epoch:data.epoch,job:data.job});
    return;
  }
  try {
    if(!Number.isInteger(data.speaker)||data.speaker<0||data.speaker>0xffffffff||!positive(data.job)||
        !(data.opus instanceof Uint8Array)||data.opus.length<1||data.opus.length>MAX_PACKET||!decimal(data.sequence)||
        BigInt(data.sequence)>((1n<<63n)-1n)||typeof data.end!=="boolean"||!Number.isFinite(data.volume)||data.volume<0||data.volume>4)throw new Error("Invalid encoded packet");
    // Full packing validation precedes speaker creation and any timing/gain state.
    VoicePlayout.validate_packet(data.opus);
    const now=performance.now();expire(now);
    if(now-windowStart>=1000){windowStart=now;burst=0;created=0;}
    if(++burst>2000)throw new Error("Media admission budget exhausted");
    let state=speakers.get(data.speaker);
    if(!state) {
      if(speakers.size>=MAX_STREAMS||created>=MAX_STREAMS)throw new Error("Too many active speakers");
      created++;state={receiver:new VoicePlayout(),last:now,count:0,window:now};speakers.set(data.speaker,state);
    }
    if(now-state.window>=1000){state.window=now;state.count=0;}
    if(++state.count>200)throw new Error("Speaker admission budget exhausted");
    state.receiver.push(BigInt(data.sequence),data.opus,data.end,data.volume,BigInt(Math.floor(now*1000)));
    state.last=now;
    postMessage({kind:"packet-queued",epoch:receiveEpoch,job:data.job});
  } catch(error) {
    postMessage({kind:"media-error",operation:"decode",epoch:receiveEpoch,job:data.job,message:String(error?.message||error).slice(0,200)});
  }
}
// Register before asynchronous WASM initialization. Only the most recent
// configuration and one transferred port are retained while loading.
onmessage=({data})=>{
  if(!data||typeof data!=="object")return;
  try {
    if(data.kind==="playout-port") {
      if(port||!data.port||typeof data.port.postMessage!=="function"){data.port?.close();return;}
      port=data.port;port.onmessage=({data})=>direct(data);port.start?.();return;
    }
    if(data.kind==="configure"){configure(data);return;}
    if(data.kind==="close"){encoder?.free();encoder=null;freeSpeakers();port?.close();port=null;close();return;}
    if(!loaded)return;
    if(data.kind==="decode"){packet(data);return;}
    if(data.epoch!==epoch) {
      if(data.kind==="encode"&&validEpoch(data.epoch)&&positive(data.job))
        postMessage({kind:"media-error",operation:"encode",epoch:data.epoch,job:data.job,message:"Capture epoch revoked"});
      return;
    }
    if(data.kind==="encode") {
      if(!enabled||!encoder||!positive(data.job)||!(data.samples instanceof Float32Array)||data.samples.length!==FRAME)throw new Error("Invalid capture frame");
      const opus=encoder.encode(data.samples);
      postMessage({kind:"encoded",epoch,job:data.job,opus},[opus.buffer]);
    }
  } catch(error) {
    postMessage({kind:"media-error",operation:data.kind,epoch,job:data.job,message:String(error?.message||error).slice(0,200)});
  }
};
await init();loaded=true;
if(pendingConfigure){const pending=pendingConfigure;pendingConfigure=null;configure(pending);}
postMessage({kind:"ready"});ready();
setInterval(()=>{expire(performance.now());postMessage({kind:"heartbeat"});},500);
