// SPDX-License-Identifier: EUPL-1.2
// No session object, credentials, network endpoints or storage API is accepted.
// This is a separate JS/WASM realm, not a promise of a separate OS process.
import init,{VoiceEncoder,VoiceDecoder} from "./audio-codec/nuschl_audio_wasm.js";
import {FRAME,MAX_PACKET,MAX_PCM,MAX_STREAMS,decimal} from "./audio-protocol.js";
let encoder=null,epoch="",enabled=false;
const speakers=new Map();
let burst=0,windowStart=performance.now();
function expire(now) {
  for(const [speaker,old] of speakers)if(now-old.last>2000){old.decoder.free();speakers.delete(speaker);}
}
function free() {
  encoder?.free();encoder=null;
  for(const state of speakers.values())state.decoder.free();
  speakers.clear();
}
function configure(data) {
  if(data.capture&&(!Number.isInteger(data.bitrate)||data.bitrate<6000||data.bitrate>128000))throw new Error("Invalid voice bitrate");
  free();epoch=data.epoch;enabled=Boolean(data.capture);
  if(enabled){encoder=new VoiceEncoder();encoder.set_bitrate(data.bitrate);}
}
await init();
postMessage({kind:"ready"});
setInterval(()=>{expire(performance.now());postMessage({kind:"heartbeat"});},500);
onmessage=({data})=>{
  if(!data||typeof data!=="object")return;
  try {
    if(data.kind==="configure") {
      if(typeof data.epoch!=="string"||data.epoch.length>200)throw new Error("Invalid media epoch");
      configure(data);return;
    }
    if(data.kind==="close"){free();close();return;}
    if(data.epoch!==epoch)return;
    if(data.kind==="encode") {
      if(!enabled||!encoder||!(data.samples instanceof Float32Array)||data.samples.length!==FRAME)throw new Error("Invalid capture frame");
      // Bounds precede wasm-bindgen's argument copy into linear memory.
      const opus=encoder.encode(data.samples);
      postMessage({kind:"encoded",epoch,job:data.job,opus},[opus.buffer]);
    } else if(data.kind==="decode") {
      if(!Number.isInteger(data.speaker)||data.speaker<0||data.speaker>0xffffffff||
          !(data.opus instanceof Uint8Array)||data.opus.length<1||data.opus.length>MAX_PACKET||!decimal(data.sequence))throw new Error("Invalid encoded packet");
      const now=performance.now();
      expire(now);
      if(now-windowStart>=1000){windowStart=now;burst=0;}
      if(++burst>1000)throw new Error("Media decode budget exhausted");
      let state=speakers.get(data.speaker);
      if(!state) {
        if(speakers.size>=MAX_STREAMS)throw new Error("Too many active speakers");
        state={decoder:new VoiceDecoder(),sequence:null,last:now,count:0,window:now,ended:true};speakers.set(data.speaker,state);
      }
      if(now-state.window>=1000){state.window=now;state.count=0;}
      if(++state.count>200)throw new Error("Speaker decode budget exhausted");
      const sequence=BigInt(data.sequence);
      if(state.sequence!==null&&(sequence===state.sequence||!state.ended&&sequence<state.sequence))throw new Error("Repeated or stale voice packet");
      let samples;
      try {samples=state.decoder.decode(data.opus);}
      catch(error){state.decoder.free();speakers.delete(data.speaker);throw error;}
      if(!(samples instanceof Float32Array)||samples.length<1||samples.length>MAX_PCM)throw new Error("Invalid decoded duration");
      state.sequence=sequence;state.ended=Boolean(data.end);state.last=now;
      postMessage({kind:"decoded",epoch,job:data.job,speaker:data.speaker,samples,volume:data.volume},[samples.buffer]);
    }
  } catch(error) {
    postMessage({kind:"media-error",operation:data.kind,epoch,job:data.job,message:String(error?.message||error).slice(0,200)});
  }
};
