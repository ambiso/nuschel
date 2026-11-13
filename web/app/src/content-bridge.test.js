// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {DocumentCache,ContentBridge,contentDocument,retainDocument,observeDocument,contentFailed,contentUnavailable} from "./content-bridge.js";
const document={blocks:[{kind:"text",text:"safe",spans:[]}]};

test("global derivative credit follows pinned documents until actual release",()=>{
  const cache=new DocumentCache({maxBytes:3000,maxCount:2});
  assert.equal(cache.insert("1",document,1500),true);
  const release=cache.pin("1");
  assert.equal(cache.insert("2",document,1500),true);
  assert.equal(cache.insert("3",document,2000),false);
  assert.equal(cache.get("1"),document);
  assert.equal(cache.bytes,1500);
  release();release(); // release is idempotent
  assert.equal(cache.insert("3",document,2000),true);
  assert.equal(cache.get("1"),null);
  assert.equal(cache.bytes,2000);
});

test("untrusted charge, image source and derivative counts cannot bypass global bounds",()=>{
  const cache=new DocumentCache({maxBytes:3000,maxCount:1});
  assert.equal(cache.insert("1",document,1),false);
  assert.throws(()=>cache.insert("1",{blocks:[{kind:"image",width:1,height:1,rgba:"AAAAAA==",source:"https://outside.invalid"}]},1000));
  assert.equal(cache.insert("1",document,1000),true);
  const release=cache.pin("1");
  assert.equal(cache.insert("2",document,1000),false);
  assert.equal(cache.entries.size,1);
  release();
});

test("only the SDK accepted reply becomes visible, with no credential metadata in codec jobs",async()=>{
  const calls=[],requests=[];let finish,admission=true;
  const service={render(packet){calls.push(packet);return new Promise(resolve=>finish=resolve);},dispose(){}};
  const bridge=new ContentBridge(async(method,id,raw)=>{
    if(method==="content_request"){requests.push(id);if(admission instanceof Error)throw admission;return admission;}
    assert.equal(method,"content_complete");assert.equal(id,"7");
    assert.equal(JSON.parse(raw).plain_text,"safe");
    return JSON.stringify({token:"7",charge:1000,document,failed:false});
  },{service});
  try {
    assert.equal(bridge.handle({kind:"content_work",operations:[{kind:"render",request:"7",mode:"legacy",source:"<b>safe</b>"}]}),true);
    assert.deepEqual(calls,[{mode:"legacy",source:"<b>safe</b>"}]);
    assert.equal(contentDocument("7"),null);
    let own=0,other=0;
    const stopOwn=observeDocument("7",()=>own++),stopOther=observeDocument("8",()=>other++);
    finish({marked_text:"safe",plain_text:"safe",document});
    await new Promise(resolve=>setImmediate(resolve));
    assert.deepEqual(contentDocument("7"),document);
    assert.equal(own,2);assert.equal(other,1,"unrelated mounted row is not invalidated");
    stopOwn();stopOwn();stopOther();
    bridge.deny("7");assert.equal(own,2,"unmounted observer is released");assert.equal(other,1);
    const release=retainDocument("7");release();
    admission=false;bridge.retry("7");await new Promise(resolve=>setImmediate(resolve));
    assert.equal(contentFailed("7"),true);assert.equal(contentUnavailable("7"),false,"overload keeps explicit retry available");
    admission=new Error("Preview ticket owner exhausted");bridge.retry("7");await new Promise(resolve=>setImmediate(resolve));
    assert.equal(contentUnavailable("7"),true);assert.equal(contentUnavailable(),false,"other valid cached previews retain their owner");
    const count=requests.length;bridge.retry("7");await new Promise(resolve=>setImmediate(resolve));assert.equal(requests.length,count,"terminal refusal never retries or spins");
    assert.deepEqual(contentDocument("7"),document,"terminal rehydration never destroys a valid charged derivative");
    for(let index=100;index<800;index++)bridge.deny(String(index),index%2===0);
    assert.equal(bridge.denied.size,512,"terminal flags share the existing bounded negative table");
  } finally {bridge.dispose();}
  assert.deepEqual(contentDocument("7"),document);
  assert.throws(()=>new ContentBridge(()=>{}, {service}),/page-lifetime/);
});

// Fresh module realm per case: the production bridge is one page-lifetime owner.
let realm=0;
const fresh=()=>import(`./content-bridge.js?credit-case=${++realm}`);
const settle=()=>new Promise(resolve=>setImmediate(resolve));
const render=(request,source="x")=>({kind:"content_work",operations:[{kind:"render",request,mode:"plain",source}]});

test("an oversized codec reply is ACKed as a bounded failure, never forwarded",async()=>{
  const {ContentBridge}=await fresh();const acks=[];
  const service={render:async()=>({marked_text:"x".repeat(2*1024*1024+1),plain_text:"x",document}),dispose(){}};
  const bridge=new ContentBridge(async(method,id,raw)=>{acks.push({method,id,raw});return "null";},{service});
  bridge.handle(render("1"));await settle();
  assert.deepEqual(acks.map(({method,id,raw})=>({method,id,bytes:raw.length})),[{method:"content_complete",id:"1",bytes:0}]);
  assert.equal(bridge.pending.size,0);bridge.dispose();
});

test("an ACK that never reached the SDK keeps its credit and retires previews visibly",async()=>{
  const m=await fresh();const calls=[];
  const service={render:async packet=>{calls.push(packet.source);return {marked_text:"ok",plain_text:"ok",document};},dispose(){}};
  const bridge=new m.ContentBridge(async method=>{if(method==="content_complete")throw new Error("The client is busy. Try again shortly.");return true;},{service});
  bridge.handle(render("1","first"));await settle();
  assert.equal(bridge.pending.has("1"),true,"credit stays charged without a real ACK");
  assert.equal(m.contentUnavailable("9"),true,"terminal state is visible on every preview");
  // SDK-admitted work stays charged and unacknowledged; the SDK's own ACK
  // deadline then retires its lane with that credit still held.
  assert.equal(bridge.handle(render("2","second")),true);await settle();
  assert.equal(bridge.pending.has("2"),true);
  assert.deepEqual(calls,["first"],"a retired lane starts no further codec work");
  bridge.dispose();
});

test("rehydration keeps few SDK RPCs in flight and queued tokens are not denied",async()=>{
  const m=await fresh();const requests=[];
  const service={render:()=>new Promise(()=>{}),dispose(){}};
  const bridge=new m.ContentBridge((method,token)=>new Promise(resolve=>requests.push({method,token,resolve})),{service});
  const releases=[];for(let index=1;index<=100;index++)releases.push(m.retainDocument(String(index)));
  assert.ok(requests.length<=8,`at most 8 rehydration RPCs in flight, saw ${requests.length}`);
  assert.equal(m.contentFailed("100"),false,"a waiting row is not turned into a failure");
  releases[99]();
  for(let turn=0;turn<20&&requests.length<99;turn++){for(const request of requests.splice(0))request.resolve(true);await settle();}
  bridge.dispose();
});

test("admitted renders reach the serial codec one at a time; a queued cancel ACKs at once",async()=>{
  const m=await fresh();const started=[],finish=new Map(),acks=[];
  const service={render:packet=>new Promise(resolve=>{started.push(packet.source);finish.set(packet.source,resolve);}),dispose(){}};
  const bridge=new m.ContentBridge(async(method,id,raw)=>{acks.push([id,raw.length]);return "null";},{service});
  bridge.handle({kind:"content_work",operations:["1","2","3","4"].map(request=>({kind:"render",request,mode:"plain",source:`s${request}`}))});
  await settle();assert.deepEqual(started,["s1"],"one codec job at a time; later work waits in the bridge");
  bridge.handle({kind:"content_work",operations:[{kind:"cancel",request:"4"}]});await settle();
  assert.deepEqual(acks,[["4",0]],"queued cancellation is acknowledged without codec work");
  finish.get("s1")({marked_text:"ok",plain_text:"ok",document});await settle();
  assert.deepEqual(started,["s1","s2"]);assert.deepEqual(acks.map(([id])=>id),["4","1"]);
  finish.get("s2")({marked_text:"ok",plain_text:"ok",document});await settle();
  finish.get("s3")({marked_text:"ok",plain_text:"ok",document});await settle();
  assert.deepEqual(started,["s1","s2","s3"]);assert.equal(bridge.pending.size,0);bridge.dispose();
});

test("a cancelled running job holds the next job and its own ACK until the worker releases it",async()=>{
  const m=await fresh();const started=[],acks=[];let releaseWorker;
  const service={render(packet,{signal}){started.push(packet.source);return new Promise((resolve,reject)=>{
    if(packet.source==="s1")signal.addEventListener("abort",()=>{const error=new Error("Content processing was cancelled.");error.workerReleased=new Promise(done=>{releaseWorker=done;});reject(error);});
    else resolve({marked_text:"ok",plain_text:"ok",document});
  });},dispose(){}};
  const bridge=new m.ContentBridge(async(method,id,raw)=>{acks.push([id,raw.length]);return "null";},{service});
  bridge.handle({kind:"content_work",operations:[{kind:"render",request:"1",mode:"plain",source:"s1"},{kind:"render",request:"2",mode:"plain",source:"s2"}]});
  bridge.handle({kind:"content_work",operations:[{kind:"cancel",request:"1"}]});await settle();
  assert.deepEqual(started,["s1"],"the next job does not start while the cancelled one still holds the worker");
  assert.deepEqual(acks,[],"the SDK head stays the cancelled job until the actual release");
  assert.equal(bridge.pending.size,2);
  releaseWorker();await settle();
  assert.deepEqual(started,["s1","s2"]);assert.deepEqual(acks,[["1",0],["2",JSON.stringify({marked_text:"ok",plain_text:"ok",document}).length]]);
  assert.equal(bridge.pending.size,0);bridge.dispose();
});

test("a protocol-violating batch admits nothing, never throws, keeps credit, honours cancels and retires visibly",async()=>{
  const ok={marked_text:"ok",plain_text:"ok",document};
  const four=["1","2","3","4"].map(request=>({kind:"render",request,mode:"plain",source:`s${request}`}));
  const cases={
    beyond_window:[{kind:"cancel",request:"1"},{kind:"render",request:"5",mode:"plain",source:"s5"}],
    duplicate:[{kind:"cancel",request:"1"},{kind:"render",request:"2",mode:"plain",source:"again"}],
    bad_ticket:[{kind:"cancel",request:"1"},{kind:"render",request:"05",mode:"plain",source:"s5"}],
    unknown_kind:[{kind:"cancel",request:"1"},{kind:"rerender",request:"6"}],
  };
  for(const [name,operations] of Object.entries(cases)) {
    const m=await fresh();const started=[],signals=new Map(),calls=[];
    const service={render(packet,{signal}){started.push(packet.source);signals.set(packet.source,signal);return new Promise(()=>{});},dispose(){}};
    const bridge=new m.ContentBridge(async(method,...args)=>{calls.push([method,...args]);return "null";},{service});
    assert.equal(bridge.handle({kind:"content_work",operations:four}),true);await settle();
    assert.equal(bridge.handle({kind:"content_work",operations}),true,`${name}: consumed without throwing`);await settle();
    assert.equal(bridge.pending.size,4,`${name}: admitted credit stays held`);
    assert.equal(m.contentUnavailable("1"),true,`${name}: terminal state is visible`);
    assert.equal(signals.get("s1").aborted,true,`${name}: the batch's cancellation is honoured`);
    assert.deepEqual(started,["s1"],`${name}: nothing new runs`);
    assert.deepEqual(calls,[["content_retire","protocol"]],`${name}: one notice and no ACK`);
    assert.equal(bridge.handle({kind:"content_work",operations:[{kind:"render",request:"9",mode:"plain",source:"later"}]}),true);await settle();
    assert.deepEqual(calls,[["content_retire","protocol"]]);bridge.dispose();
  }
  const m=await fresh();const calls=[];
  const bridge=new m.ContentBridge(async(method,...args)=>{calls.push(method);return "null";},{service:{render:async()=>ok,dispose(){}}});
  assert.equal(bridge.handle({kind:"content_work",operations:"not a list"}),true);await settle();
  assert.equal(m.contentUnavailable("1"),true);assert.deepEqual(calls,["content_retire"]);bridge.dispose();
});

test("a remote exception before the SDK reaches the job keeps credit and retires visibly with one bounded notice",async()=>{
  const m=await fresh();const calls=[],started=[];
  const service={render:async packet=>{started.push(packet.source);return {marked_text:"ok",plain_text:"ok",document};},dispose(){}};
  const bridge=new m.ContentBridge(async(method,...args)=>{
    calls.push([method,...args]);
    if(method==="content_complete")throw new Error("Invalid content request."); // remote, before any Rust call
    return true;
  },{service});
  bridge.handle(render("1","first"));await settle();
  assert.equal(bridge.pending.has("1"),true,"a remote error is not the job's ACK");
  assert.equal(m.contentUnavailable("2"),true);
  assert.deepEqual(calls.map(([method,arg])=>[method,arg]),[["content_complete","1"],["content_retire","undelivered"]]);
  bridge.handle(render("2","second"));await settle();
  assert.deepEqual(started,["first"]);assert.equal(calls.length,2,"no further ACK or notice");bridge.dispose();
});

test("a falsy or malformed normal completion never installs content and keeps the job's credit",async()=>{
  const valid={token:"1",charge:1000,document,failed:false};
  for(const raw of [null,undefined,"",false,0,"false","1","[1,2,3,4]","{}",JSON.stringify({token:"1"}),JSON.stringify({...valid,extra:true}),JSON.stringify({...valid,charge:"1000"}),JSON.stringify({...valid,document:null}),"x".repeat(2*1024*1024+1)]) {
    const m=await fresh();const calls=[];
    const service={render:async()=>({marked_text:"ok",plain_text:"ok",document}),dispose(){}};
    const bridge=new m.ContentBridge(async(method,...args)=>{calls.push(method);return method==="content_complete"?raw:true;},{service});
    bridge.handle(render("1"));await settle();
    assert.equal(m.contentDocument("1"),null,`no install from ${String(raw).slice(0,24)}`);
    assert.equal(bridge.pending.has("1"),true);assert.equal(m.contentUnavailable("1"),true);
    assert.deepEqual(calls,["content_complete","content_retire"]);bridge.dispose();
  }
  const m=await fresh();
  const bridge=new m.ContentBridge(async()=>"null",{service:{render:async()=>({marked_text:"ok",plain_text:"ok",document}),dispose(){}}});
  bridge.handle(render("1"));await settle();
  assert.equal(bridge.pending.size,0,"a normal null result is the ACK");assert.equal(m.contentUnavailable("1"),false);bridge.dispose();
});

test("a refused retirement notice is bounded: no retry and no extra credit",async()=>{
  const m=await fresh();const calls=[];
  const service={render:()=>new Promise(()=>{}),dispose(){}};
  const bridge=new m.ContentBridge(async method=>{calls.push(method);throw new Error("The client is busy. Try again shortly.");},{service});
  bridge.handle({kind:"content_work",operations:[{kind:"render",request:"x"}]});await settle();
  bridge.handle({kind:"content_work",operations:[{kind:"render",request:"y"}]});await settle();
  assert.deepEqual(calls,["content_retire"]);assert.equal(m.contentUnavailable("1"),true);assert.equal(bridge.pending.size,0);bridge.dispose();
});
