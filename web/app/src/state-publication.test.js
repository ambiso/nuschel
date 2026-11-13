// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {StatePublication,stateFrame,MAX_STATE_BYTES,MAX_STATE_EVENTS,MAX_STATE_EVENT_BYTES,STATE_ACK_MS} from "./state-publication.js";
import {SdkRpc} from "./sdk-rpc.js";
function fixture(){
 const timers=new Map(),posted=[],failures=[];let now=0,next=0,model=0,collected=0;
 const clock={now:()=>now,setTimeout(fn,delay){assert.equal(delay,STATE_ACK_MS);timers.set(++next,fn);return next;},clearTimeout(id){timers.delete(id);}};
 const flow=new StatePublication({clock,collect(id,updates){collected++;return stateFrame(id,{servers:"[]",timeline:JSON.stringify({version:model}),voice:"null"},updates);},post:packet=>posted.push(packet),fatal:message=>failures.push(message)});
 return {flow,timers,posted,failures,set model(value){model=value;},get collected(){return collected;},advance:value=>{now+=value;}};
}
test("one in-flight snapshot and latest dirty model, no queued snapshot copies",()=>{
 const f=fixture();f.flow.changed("[]");
 for(let i=1;i<=10000;i++){f.model=i;f.flow.changed("[]");}
 assert.equal(f.posted.length,1);assert.equal(f.collected,1);assert.equal(f.timers.size,1);
 f.flow.ack(1);assert.equal(f.posted.length,2);assert.equal(f.collected,2);assert.equal(f.posted[1].timeline.version,10000);
 assert.equal(f.timers.size,1);f.flow.ack(2);assert.equal(f.timers.size,0);assert.equal(f.posted.length,2);
});
test("old/duplicate ACK cannot release newer credit; future and late ACK retire permanently",()=>{
 const f=fixture();f.flow.changed("[]");f.flow.changed("[]");f.flow.ack(1);
 assert.equal(f.flow.ack(1),false);assert.equal(f.flow.inflight.id,2);
 assert.equal(f.flow.ack(3),false);assert.equal(f.failures.length,1);f.flow.ack(2);f.flow.changed("[]");assert.equal(f.posted.length,2);
 const g=fixture();g.flow.changed("[]");g.advance(STATE_ACK_MS);assert.equal(g.flow.ack(1),false);assert.equal(g.failures.length,1);
});
test("combined in-flight and pending FIFO event credit is bounded, never drops Sent/errors",()=>{
 const f=fixture();f.flow.changed(JSON.stringify([{kind:"sent",body:"original"}]));
 f.flow.changed(JSON.stringify(Array.from({length:MAX_STATE_EVENTS-1},(_,id)=>({kind:"error",id}))));
 assert.equal(f.posted.length,1);f.flow.ack(1);
 assert.equal(f.posted[1].updates.length,127);assert.equal(f.posted[1].updates[0].id,0);assert.equal(f.posted[1].updates.at(-1).id,126);
 const g=fixture();g.flow.changed(JSON.stringify(Array.from({length:MAX_STATE_EVENTS},()=>({kind:"sent"}))));g.flow.changed('[{"kind":"error"}]');
 assert.equal(g.failures.length,1);assert.equal(g.posted.length,1);assert.equal(g.flow.closed,true);
 const h=fixture();h.flow.changed(JSON.stringify([{kind:"sent",body:"x".repeat(MAX_STATE_EVENT_BYTES/2)}]));h.flow.changed(JSON.stringify([{kind:"error",body:"x".repeat(MAX_STATE_EVENT_BYTES/2)}]));assert.equal(h.failures.length,1);
});
test("no ACK and UI disposal do not manufacture new publication credit",()=>{
 const f=fixture();f.flow.changed("[]");const expire=[...f.timers.values()][0];
 for(let i=0;i<100;i++)f.flow.changed("[]");assert.equal(f.timers.size,1);
 expire();assert.equal(f.failures.length,1);assert.equal(f.flow.inflight.id,1);f.flow.ack(1);assert.equal(f.posted.length,1);
 const g=fixture();g.flow.changed("[]");g.flow.fail("UI disposed");g.flow.changed("[]");assert.equal(g.posted.length,1);assert.equal(g.timers.size,0);
});
test("snapshot size/invalid raw export and actual post failures are terminal",()=>{
 assert.throws(()=>stateFrame(1,{servers:"[]",timeline:JSON.stringify({text:"x".repeat(100)}),voice:"null"},[],100),/limit/);
 assert.throws(()=>stateFrame(1,{servers:"x".repeat(MAX_STATE_BYTES+1),timeline:"null",voice:"null"},[]),/limit/);
 assert.throws(()=>stateFrame(1,{servers:"not-json",timeline:"null",voice:"null"},[]));
 const f=fixture();f.flow.post=()=>{throw new Error("clone failed");};f.flow.changed("[]");assert.equal(f.failures.length,1);assert.equal(f.flow.closed,true);assert.equal(f.timers.size,0);
});
test("UI state ACK has independent exact-owner control credit and application failure closes RPC",async()=>{
 const messages=[],timers=new Map();let next=0,terminated=0;
 const worker={postMessage:value=>messages.push(value),terminate:()=>terminated++};
 const clock={setTimeout(fn){timers.set(++next,fn);return next;},clearTimeout:id=>timers.delete(id)};
 const rpc=new SdkRpc(worker,{clock});rpc.initialize("ws://127.0.0.1/relay");rpc.acceptReady();
 // A full aggregate table: 124 ordinary slots plus the four reserved ACK slots.
 const pending=[...Array.from({length:124},()=>rpc.request("content_request",["1"]).catch(error=>error)),...Array.from({length:4},(_,index)=>rpc.request("content_complete",[String(index+1),""]).catch(error=>error))];
 assert.equal(rpc.ackState(null),false);assert.equal(rpc.ackState(0),false);
 assert.equal(rpc.acceptState(1),true);assert.equal(rpc.size,128);assert.equal(rpc.ackState(1),true);assert.deepEqual(messages.at(-1),{method:"state_applied",args:[1]});assert.equal(rpc.size,128);assert.equal(rpc.ackState(1),false);
 assert.equal(rpc.acceptState(2),true);rpc.close("UI apply failed");assert.equal(rpc.ackState(2),false);assert.equal(terminated,1);await Promise.all(pending);
 const other=new SdkRpc(worker,{clock});other.initialize("ws://127.0.0.1/relay");other.acceptReady();assert.equal(other.acceptState(2),false);assert.equal(other.closed,true,"new owner cannot accept previous owner's sequence");
});

test("large roster changed notices coalesce one charged indication; payload events remain exact FIFO",()=>{
 const f=fixture(),changes=Array.from({length:10000},()=>({kind:"changed"}));
 f.flow.changed(JSON.stringify([...changes,{kind:"sent",body:"first"},{kind:"changed",extra:"must survive"},{kind:"error",message:"original"}]));
 assert.deepEqual(f.posted[0].updates,[{kind:"changed"},{kind:"sent",body:"first"},{kind:"changed",extra:"must survive"},{kind:"error",message:"original"}]);
 const original=JSON.stringify(f.posted[0]);
 f.flow.changed(JSON.stringify([...changes,{kind:"sent",body:"second"},...changes,{kind:"error",message:"second"}]));
 assert.equal(JSON.stringify(f.posted[0]),original,"inflight frame is immutable");
 assert.equal(f.flow.events.length,3);assert.equal(f.flow.inflight.events,4);
 f.flow.ack(1);assert.deepEqual(f.posted[1].updates,[{kind:"changed"},{kind:"sent",body:"second"},{kind:"error",message:"second"}]);
});
