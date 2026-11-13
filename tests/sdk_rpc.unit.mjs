// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {SdkRpc,SdkUnavailableError,SDK_RPC_LIMIT,SDK_RPC_TIMEOUT,SDK_STARTUP_TIMEOUT} from "../web/app/src/sdk-rpc.js";
import {BrowserAudio} from "../web/app/src/audio.js";
const turn=()=>new Promise(resolve=>setImmediate(resolve));
class Clock {
  constructor(){this.jobs=new Map();this.next=0;}
  setTimeout(callback,delay){assert.ok([SDK_RPC_TIMEOUT,SDK_STARTUP_TIMEOUT].includes(delay));const id=++this.next;this.jobs.set(id,callback);return id;}
  clearTimeout(id){this.jobs.delete(id);}
  expire(){for(const [id,callback] of [...this.jobs])if(this.jobs.delete(id))callback();}
}
class Worker {
  constructor(){this.messages=[];this.terminated=0;}
  postMessage(value){assert.equal(this.terminated,0);this.messages.push(value);}
  terminate(){this.terminated++;}
}
function fixture(onClose=()=>{}) {
  const worker=new Worker(),clock=new Clock();
  const rpc=new SdkRpc(worker,{clock,onClose:reason=>{assert.equal(worker.terminated,1,"termination precedes audio revocation");onClose(reason);}});
  rpc.initialize("ws://127.0.0.1/relay");assert.equal(rpc.acceptReady(),true);worker.messages.length=0;
  return {worker,clock,rpc};
}
test("128 unresolved commands remain globally bounded until real ACK or owned worker death",async()=>{
 const {worker,clock,rpc}=fixture();const promises=[];
 for(let index=0;index<SDK_RPC_LIMIT;index++)promises.push(rpc.request("voice_packet",[]).catch(error=>error));
 await assert.rejects(rpc.request("browse_server",[0]),/busy/);assert.equal(worker.messages.length,128);assert.equal(rpc.size,128);
 const first=worker.messages[0];assert.equal(rpc.accept({kind:"result",id:first.id+1000,value:true}),false);
 assert.equal(rpc.size,128);assert.equal(rpc.accept({kind:"result",id:first.id,value:true}),true);
 promises.push(rpc.request("browse_server",[]).catch(error=>error));assert.equal(rpc.size,128);
 assert.equal(rpc.accept({kind:"result",id:first.id,value:true}),false);assert.equal(rpc.size,128);
 clock.expire();assert.equal(worker.terminated,1);assert.equal(rpc.size,0);assert.equal(clock.jobs.size,0);
 const results=await Promise.all(promises);assert.equal(results[0],true);assert.ok(results.slice(1).every(error=>error instanceof SdkUnavailableError));
 await assert.rejects(rpc.request("join",[]),SdkUnavailableError);assert.equal(worker.messages.length,129);
});
test("timeout closes the worker and revokes audio before any synthetic rejection grants credits",async()=>{
 const trace=[];const {worker,clock,rpc}=fixture(()=>trace.push("revoke"));
 worker.terminate=()=>{worker.terminated++;trace.push("terminate");};
 const pending=rpc.request("voice_packet").catch(error=>{trace.push("reject");assert.ok(error instanceof SdkUnavailableError);});
 clock.expire();assert.deepEqual(trace,["terminate","revoke"]);await pending;assert.deepEqual(trace,["terminate","revoke","reject"]);
 rpc.close();clock.expire();assert.equal(worker.terminated,1);assert.equal(rpc.accept({kind:"result",id:1,value:true}),false);
});
test("actual command errors are transient and malformed or unsolicited results cannot settle requests",async()=>{
 const {worker,rpc}=fixture();const pending=rpc.request("send_text",["draft"]);const rejected=assert.rejects(pending,/permission/);
 for(const id of [null,0,-1,"1",1.5,NaN,Infinity,Number.MAX_SAFE_INTEGER+1])assert.equal(rpc.accept({kind:"result",id,error:"foreign"}),false);
 assert.equal(rpc.size,1);assert.equal(rpc.accept({kind:"result",id:1,error:"permission denied"}),true);await rejected;
 assert.equal(worker.terminated,0);assert.equal(rpc.closed,false);assert.equal(rpc.size,0);rpc.close();
});
test("post failure closes the owner before rejection; disposal is idempotent and silent",async()=>{
 const {worker,rpc}=fixture();worker.postMessage=()=>{throw new Error("clone failed");};
 await assert.rejects(rpc.request("voice_packet"),SdkUnavailableError);assert.equal(worker.terminated,1);
 const f=fixture(()=>assert.fail("unmounted UI callback"));const pending=f.rpc.request("older").catch(error=>error);
 f.rpc.close("Client closed.",{notify:false});assert.equal(f.worker.terminated,1);assert.ok(await pending instanceof SdkUnavailableError);f.rpc.close();
});
test("failed termination never pretends to acknowledge pending inbox work",async()=>{
 const {worker,clock,rpc}=fixture();let revoked=false;
 worker.terminate=()=>{worker.terminated++;throw new Error("broken host boundary");};rpc.onClose=()=>{revoked=true;};
 const pending=rpc.request("voice_packet");let settled=false;pending.finally(()=>{settled=true;});
 clock.expire();await turn();assert.equal(revoked,true);assert.equal(settled,false);assert.equal(rpc.size,1);assert.equal(rpc.closed,true);
 await assert.rejects(rpc.request("voice_packet"),SdkUnavailableError);
});
function voice(generation="1") {return {server_id:"private",connection_generation:generation,voice_generation:"1",channel_id:0,joined:true,muted:false,deafened:false,can_transmit:true,bitrate:48000};}
test("two SDK audio RPC credits survive20 epochs and250ms capture timeout until actual ACK/death",async()=>{
 let audio;const {worker,clock,rpc}=fixture(()=>audio.syncVoice(null));
 audio=new BrowserAudio({onPacket:packet=>rpc.request("voice_packet",[packet])});audio.syncVoice(voice());
 audio.sendPacket(0n,new Uint8Array([0xf8,0xff,0xfe]),false);audio.sendPacket(2n,new Uint8Array([0xf8,0xff,0xfe]),false);
 for(let index=0;index<20;index++){audio.syncVoice(voice(String(index+2)));audio.sendPacket(0n,new Uint8Array([0xf8,0xff,0xfe]),false);}
 assert.equal(worker.messages.length,2);assert.equal(audio.txJobs.size,2);assert.equal(rpc.size,2);
 // An ACK from an old route releases exactly its own global slot, never another.
 rpc.accept({kind:"result",id:1,value:true});await turn();assert.equal(audio.txJobs.size,1);
 audio.sendPacket(0n,new Uint8Array([0xf8,0xff,0xfe]),false);assert.equal(audio.txJobs.size,2);assert.equal(worker.messages.length,3);
 rpc.accept({kind:"result",id:1,value:true});await turn();assert.equal(audio.txJobs.size,2);
 await new Promise(resolve=>setTimeout(resolve,300));
 assert.equal(audio.transportStalled,true);assert.equal(audio.txJobs.size,2,"250ms is not an SDK ACK");
 audio.syncVoice(voice("50"));audio.sendPacket(0n,new Uint8Array([0xf8,0xff,0xfe]),false);assert.equal(worker.messages.length,3);
 clock.expire();assert.equal(worker.terminated,1);assert.equal(audio.voice,null);await turn();
 assert.equal(audio.txJobs.size,0);assert.equal(rpc.size,0);await audio.dispose();
});
test("content completion ACKs keep a small reserve so ordinary command pressure cannot refuse them",async()=>{
 const {worker,clock,rpc}=fixture();const ordinary=[];
 for(let index=0;index<SDK_RPC_LIMIT;index++)ordinary.push(rpc.request("content_request",[String(index+1)]).catch(error=>error));
 await assert.rejects(rpc.request("browse_server",[0]),/busy/);
 const acks=[];for(let index=0;index<4;index++)acks.push(rpc.request("content_complete",[String(index+1),""]).catch(error=>error));
 assert.equal(worker.messages.filter(message=>message.method==="content_complete").length,4,"ACKs reach the SDK at the ordinary limit");
 await assert.rejects(rpc.request("content_complete",["5",""]),/busy/,"the reserve itself is bounded");
 await assert.rejects(rpc.request("browse_server",[0]),/busy/,"ordinary work cannot borrow ACK slots");
 const ack=worker.messages.find(message=>message.method==="content_complete");
 assert.equal(rpc.accept({kind:"result",id:ack.id,value:"null"}),true);assert.equal(await acks[0],"null");
 acks.push(rpc.request("content_complete",["5",""]).catch(error=>error));
 assert.equal(worker.messages.filter(message=>message.method==="content_complete").length,5);
 clock.expire();assert.equal(rpc.size,0);await Promise.all(ordinary);await Promise.allSettled(acks);
});
