// SPDX-License-Identifier: EUPL-1.2
// Private test page only. The production module/SDK/codec/worklet are unchanged.
import {BrowserAudio} from "/audio.js";
const RealAudioContext=globalThis.AudioContext;
globalThis.AudioContext=class extends RealAudioContext {
  constructor(options){super({...options,sinkId:{type:"none"}});}
};
const config=await fetch("/fixture-config.json").then(response=>response.json());
const username=new URL(location).searchParams.get("username")||"browser-audio";
const worker=new Worker("/sdk.worker.js",{type:"module"});
let next=0,voice=null,servers=[],audioState={},analyser,receivedCount=0;
const pending=new Map(),received=[],sent=[],errors=[],events=[];
function request(method,args=[]) {
  const id=++next;
  if(method!=="voice_packet"&&method!=="audio_ready")events.push({kind:"command",id,method,localMuted:servers[0]?.muted,localDeafened:servers[0]?.deafened});
  return new Promise((resolve,reject)=>{
    const timer=setTimeout(()=>{pending.delete(id);reject(new Error("SDK command timeout: "+method));},10000);
    pending.set(id,{resolve:value=>{clearTimeout(timer);resolve(value);},reject:error=>{clearTimeout(timer);reject(error);}});
    worker.postMessage({id,method,args});
  });
}
const audio=new BrowserAudio({
  onPacket:packet=>{sent.push({sequence:packet.sequence,end:packet.end,length:packet.opus.length});return request("voice_packet",[packet]);},
  onReady:ready=>request("audio_ready",[ready]),
  onState:state=>{audioState=state;events.push({phase:state.phase,microphone:state.microphone,playback:state.playback});document.querySelector("output").textContent=JSON.stringify(state);},
});
let ready;
const initialized=new Promise(resolve=>ready=resolve);
let trusting=false;
worker.onmessage=({data})=>{
  if(data.kind==="ready"){ready();return;}
  if(data.kind==="result"){
    const job=pending.get(data.id);pending.delete(data.id);
    if(job){if(data.error)job.reject(new Error(data.error));else job.resolve(data.value);}return;
  }
  if(data.kind==="voice_packet"){
    receivedCount++;
    if(received.length<400)received.push({...data.packet,opus:Array.from(data.packet.opus)});
    audio.receive(data.packet);return;
  }
  if(data.kind==="state"){
    servers=data.servers;voice=data.voice;audio.syncVoice(voice);
    events.push({kind:"state",muted:servers[0]?.muted,deafened:servers[0]?.deafened,voice:voice?{...voice}:null});
    if(events.length>500)events.shift();
    const first=servers[0];
    if(first?.certificate&&!trusting){trusting=true;request("accept_certificate",[0,first.certificate]).catch(error=>errors.push(error.message));}
    return;
  }
  if(data.kind==="error")errors.push(data.message);
};
worker.postMessage({method:"init",relay:config.relay});
await initialized;
const id=await request("add_server",["",config.host,config.port,username]);
await request("connect",[id]);
const button=(name,handler)=>document.getElementById(name).addEventListener("click",()=>Promise.resolve(handler()).catch(error=>errors.push(error.message)));
button("join",()=>{audio.syncVoice(null);audio.activateGesture();return request("join",[id,0]);});
button("microphone",()=>{
  if(servers[0]&&!servers[0].muted){audio.setLocalPolicy({muted:true});return request("toggle_mute",[id]);}
  audio.activateGesture({microphone:true});
  if(servers[0]?.muted)return request("toggle_mute",[id]);
});
button("deafen",()=>{
  if(servers[0]?.deafened)audio.activateGesture();else audio.setLocalPolicy({muted:true,deafened:true});
  return request("toggle_deaf",[id]);
});
button("mute",()=>{audio.setLocalPolicy({muted:true});if(servers[0]&&!servers[0].muted)return request("toggle_mute",[id]);});
function measure(){
  if(!audio.ctx||!audio.output)return null;
  if(!analyser){analyser=audio.ctx.createAnalyser();analyser.fftSize=4096;audio.output.connect(analyser);}
  const samples=new Float32Array(analyser.fftSize);analyser.getFloatTimeDomainData(samples);
  return {rms:Math.sqrt(samples.reduce((sum,v)=>sum+v*v,0)/samples.length),
    finite:samples.every(Number.isFinite),samples:Array.from(samples),sink:{type:audio.ctx.sinkId?.type}};
}
window.fixture={request,audio,measure,received,sent,errors,events,
  snapshot:()=>({voice,servers,audio:audioState,received:receivedCount,sent:sent.length,errors:[...errors],events:events.slice(-100),sink:{type:audio.ctx?.sinkId?.type}}),
  close:async()=>{await audio.dispose();worker.terminate();},
};
document.getElementById("loaded").textContent="ready";
