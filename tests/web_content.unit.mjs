// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import {readFile} from "node:fs/promises";
import * as protocol from "../web/app/src/content-protocol.js";
const {MAX_IMAGE_INPUT,validateDocument,validateReply}=protocol;
// Fresh test module realm, not an exported production reset/owner bypass.
const ownerSource=(await readFile(new URL("../web/app/src/worker-owner.js",import.meta.url),"utf8")).replace(/^export /gm,"");
const source=ownerSource+"\n"+(await readFile(new URL("../web/app/src/content-service.js",import.meta.url),"utf8"))
  .replace(/^import .*;\n/gm,"").replaceAll("import.meta.url",'"https://fixture.invalid/content-service.js"').replace(/^export /gm,"");
const rendered=text=>JSON.stringify({marked_text:text,plain_text:text,document:{blocks:[{kind:"text",text}]}});
class Clock {
  now=0;next=0;jobs=new Map();
  setTimeout(callback,delay){const id=++this.next;this.jobs.set(id,{at:this.now+delay,callback});return id;}
  clearTimeout(id){this.jobs.delete(id);}
  advance(ms,{dispatch=true}={}){this.now+=ms;if(dispatch)for(const [id,job] of [...this.jobs])if(job.at<=this.now&&this.jobs.delete(id))job.callback();}
}
class Worker {
  sent=[];terminated=0;
  postMessage(message){this.sent.push(message);}
  terminate(){this.terminated++;}
  emit(data){this.onmessage?.({data});}
  ready(){this.emit({kind:"ready"});}
  finish(text="done",id=this.sent.at(-1).id){this.emit({kind:"result",id,result:rendered(text)});}
  error(message="Invalid image",id=this.sent.at(-1).id){this.emit({kind:"error",id,message});}
}
function fixture(options={}) {
  const workers=[],clock=new Clock();
  const realm=vm.createContext({...protocol,Uint8Array,performance:{now:()=>clock.now},setTimeout:clock.setTimeout.bind(clock),clearTimeout:clock.clearTimeout.bind(clock)});
  new vm.Script(source+"\nglobalThis.ContentService=ContentService;").runInContext(realm);
  const Service=realm.ContentService;
  const workerFactory=()=>{const worker=new Worker();workers.push(worker);return worker;};
  return {service:new Service({workerFactory,...options}),workers,clock,Service};
}
const outcome=promise=>promise.then(value=>({value}),error=>({error}));

test("all facades share eight jobs,16MiB and one first-factory owner",async()=>{
  const {service,workers,Service}=fixture();let otherCalls=0;
  const other=new Service({workerFactory:()=>{otherCalls++;return new Worker();}});
  const jobs=Array.from({length:8},(_,i)=>outcome((i%2?other:service).render({source:`source ${i}`})));
  await assert.rejects(other.render({source:"overflow"}),/busy/);
  assert.equal(workers.length,1);assert.equal(otherCalls,0);workers[0].ready();
  for(let i=0;i<8;i++){assert.equal(workers[0].sent.length,i+1);workers[0].finish(`reply ${i}`);assert.equal((await jobs[i]).value.plain_text,`reply ${i}`);}
  assert.equal(service.bytes,0);service.dispose();other.dispose();assert.equal(workers[0].terminated,0);
  const fresh=new Service();const reused=outcome(fresh.render({source:"new conversation"}));workers[0].finish("reused");
  assert.equal((await reused).value.plain_text,"reused");assert.equal(workers.length,1);fresh.dispose();
});
test("unused/disposed facades do not claim the lazy first factory or retain an idle client",async()=>{
  const f=fixture();f.service.dispose();let calls=0;const replacement=new Worker();
  const client=new f.Service({workerFactory:()=>{calls++;return replacement;},startupMs:50});
  const pending=outcome(client.render({source:"first admitted"}));assert.equal(f.workers.length,0);assert.equal(calls,1);
  replacement.ready();replacement.finish();assert.ok((await pending).value);client.dispose();
  const later=new f.Service({workerFactory:()=>assert.fail("new factory")});const next=outcome(later.render({source:"later"}));replacement.finish();assert.ok((await next).value);later.dispose();
});
test("diagnostic snapshots cannot mutate admission, source state or consumer settlement",async()=>{
  const {service,workers}=fixture();const jobs=Array.from({length:8},(_,i)=>outcome(service.render({source:`source ${i}`})));
  const queue=service.queue;assert.ok(Object.isFrozen(queue));assert.ok(queue.every(Object.isFrozen));
  assert.deepEqual(Object.keys(queue[0]),["id","size","deadline"]);
  assert.throws(()=>queue.pop());assert.throws(()=>{queue[0].deadline=Infinity;});
  await assert.rejects(service.render({source:"cannot manufacture capacity"}),/busy/);
  workers[0].ready();const active=service.active;assert.ok(Object.isFrozen(active));
  assert.deepEqual(Object.keys(active),["id","size","deadline"]);assert.equal(active.resolve,undefined);assert.equal(active.packet,undefined);assert.equal(active.client,undefined);
  assert.throws(()=>{active.size=0;});assert.equal(workers[0].sent.length,1);
  for(let i=0;i<8;i++){workers[0].finish(`reply ${i}`);assert.equal((await jobs[i]).value.plain_text,`reply ${i}`);}
  assert.equal(service.bytes,0);service.dispose();
});
test("active cancellation retains exact slot/bytes/deadline until real ACK then reuses same worker",async()=>{
  const {service,workers,clock}=fixture();const abort=new AbortController();
  const first=outcome(service.render({source:"old"},{signal:abort.signal})),next=outcome(service.render({source:"new"}));
  workers[0].ready();const id=workers[0].sent[0].id;clock.advance(10);abort.abort();
  assert.match((await first).error.message,/cancelled/);assert.equal(service.bytes,6);assert.equal(service.active.id,id);
  assert.equal(service.active.deadline,8000);assert.equal(workers[0].sent.length,1);assert.equal(workers[0].terminated,0);
  workers[0].finish("discarded old derivative",id);assert.equal(workers[0].sent.at(-1).source,"new");assert.equal(service.bytes,3);
  workers[0].finish("current");assert.equal((await next).value.plain_text,"current");assert.equal(workers.length,1);service.dispose();
});
test("cancelled active reservation still consumes global job and byte budget",async()=>{
  const {service,workers,Service}=fixture();const abort=new AbortController();
  const first=outcome(service.decodeImage(new Uint8Array(MAX_IMAGE_INPUT),{signal:abort.signal}));workers[0].ready();abort.abort();await first;
  const other=new Service();const second=outcome(other.decodeImage(new Uint8Array(MAX_IMAGE_INPUT)));
  await assert.rejects(other.decodeImage(new Uint8Array([1])),/busy/);assert.equal(service.bytes,16*1024*1024);
  workers[0].error();workers[0].error();await second;assert.equal(service.bytes,0);service.dispose();other.dispose();
});
test("queued cancellation sends no cancelled payload and does not affect another facade",async()=>{
  const {service,workers,Service}=fixture();const other=new Service();
  const first=outcome(service.render({source:"active"}));const abort=new AbortController();
  const cancelled=outcome(other.render({source:"never posted"},{signal:abort.signal}));workers[0].ready();abort.abort();
  assert.match((await cancelled).error.message,/cancelled/);assert.equal(workers[0].terminated,0);workers[0].finish();await first;
  assert.equal(workers[0].sent.length,1);service.dispose();other.dispose();
});
test("cancelAll/dispose are consumer-only and healthy active ACK enables future facades",async()=>{
  const {service,workers,Service}=fixture();const first=outcome(service.render({source:"old active"}));workers[0].ready();
  const other=new Service();const next=outcome(other.render({source:"other facade"}));service.dispose();
  assert.match((await first).error.message,/cancelled/);await assert.rejects(service.render({source:"closed"}),/closed/);
  workers[0].finish("discarded");workers[0].finish("other");assert.equal((await next).value.plain_text,"other");
  assert.equal(workers.length,1);assert.equal(workers[0].terminated,0);other.dispose();
});
test("cancellation during startup keeps one loader and its first-factory absolute deadline",async()=>{
  const {service,workers,clock,Service}=fixture({startupMs:20});const abort=new AbortController();
  const old=outcome(service.render({source:"cancel loading"},{signal:abort.signal}));clock.advance(10);abort.abort();await old;
  assert.equal(service.bytes,0);assert.equal(workers[0].terminated,0);
  const later=new Service({startupMs:15000,workerFactory:()=>assert.fail("replacement factory")});
  const next=outcome(later.render({source:"new consumer"}));clock.advance(5);workers[0].ready();workers[0].finish("same loader");
  assert.equal((await next).value.plain_text,"same loader");assert.equal(workers.length,1);service.dispose();later.dispose();
});
test("late readiness cannot win against absolute startup deadline when timer dispatch is delayed",async()=>{
  const {service,workers,clock,Service}=fixture({startupMs:20});const abort=new AbortController();
  const first=outcome(service.render({source:"old"},{signal:abort.signal}));clock.advance(10);abort.abort();await first;
  const nextService=new Service({startupMs:15000});const next=outcome(nextService.render({source:"new"}));
  clock.advance(11,{dispatch:false});workers[0].ready();assert.match((await next).error.message,/could not start/);
  assert.equal(service.retired,true);assert.equal(workers[0].terminated,1);assert.equal(workers[0].sent.length,0);
  await assert.rejects(new Service().render({source:"replacement"}),/unavailable/);assert.equal(workers.length,1);
});
test("new jobs/facades cannot extend a cancelled active deadline or create replacement capacity",async()=>{
  const {service,workers,clock,Service}=fixture({timeoutMs:25});const abort=new AbortController();
  const active=outcome(service.render({source:"stall"},{signal:abort.signal}));workers[0].ready();clock.advance(10);abort.abort();await active;
  const others=[];for(let i=0;i<20;i++){const facade=new Service({timeoutMs:8000});others.push(outcome(facade.render({source:"later"})));}
  clock.advance(15);assert.equal(service.retired,true);assert.equal(workers[0].terminated,1);assert.equal(workers.length,1);
  assert.equal(service.bytes,5,"retired active bytes remain reserved, not fresh credits");assert.ok((await Promise.all(others)).every(result=>result.error));
  await assert.rejects(new Service().render({source:"retry"}),/unavailable/);assert.equal(workers.length,1);
});
test("queued deadline is independent, including while the single loader is still starting",async()=>{
  const {service,workers,clock}=fixture({timeoutMs:10,startupMs:100});const first=outcome(service.render({source:"queued"}));
  clock.advance(10);assert.match((await first).error.message,/deadline/);assert.equal(service.retired,false);assert.equal(service.bytes,0);
  const next=outcome(service.render({source:"retry same loader"}));clock.advance(5);workers[0].ready();workers[0].finish();assert.ok((await next).value);assert.equal(workers.length,1);service.dispose();
});
test("late exact response loses deadline race even before its timer callback",async()=>{
  const {service,workers,clock}=fixture({timeoutMs:25});const job=outcome(service.render({source:"late"}));workers[0].ready();
  clock.advance(26,{dispatch:false});workers[0].finish();assert.match((await job).error.message,/deadline/);assert.equal(service.retired,true);assert.equal(workers[0].terminated,1);
});
test("ordinary invalid-image errors reuse the responsive realm without accepting a stale ID",async()=>{
  const {service,workers}=fixture();const invalid=outcome(service.decodeImage(new Uint8Array([1,2,3])));workers[0].ready();workers[0].error("Invalid image");
  assert.match((await invalid).error.message,/Invalid image/);const next=outcome(service.render({source:"valid"}));workers[0].finish("valid");assert.equal((await next).value.plain_text,"valid");
  const pending=outcome(service.render({source:"current"}));workers[0].finish("stale",1);
  assert.match((await pending).error.message,/invalid response/);assert.equal(service.retired,true);assert.equal(workers.length,1);
});
test("terminate throwing cannot interrupt cleanup or permit another owner",async()=>{
  const {service,workers,clock,Service}=fixture({timeoutMs:25});const first=outcome(service.render({source:"active"}));const next=outcome(service.render({source:"queued"}));
  workers[0].ready();workers[0].terminate=()=>{throw new Error("host failure");};clock.advance(25);
  assert.match((await first).error.message,/deadline/);assert.ok((await next).error);assert.equal(service.active,null);assert.equal(service.queue.length,0);
  assert.equal(service.terminationFailed,true);assert.equal(service.bytes,6);assert.equal(clock.jobs.size,0);
  workers[0].finish("late old");assert.equal(service.bytes,6);await assert.rejects(new Service().render({source:"bypass"}),/unavailable/);assert.equal(workers.length,1);
});
test("factory/post failures, traps and malformed replies retire the one shared owner",async()=>{
  for(const failure of ["factory","post","trap","malformed"]){
    const f=fixture();if(failure==="factory")f.service.workerFactory=()=>{throw new Error("constructor failed");};
    const first=outcome(f.service.render({source:"first"}));
    if(failure!=="factory"){
      if(failure==="post")f.workers[0].postMessage=()=>{throw new Error("clone failed");};
      f.workers[0].ready();
      if(failure==="trap")f.workers[0].emit({kind:"fatal"});
      if(failure==="malformed")f.workers[0].emit({kind:"result",id:1,result:'{"unexpected":true}'});
    }
    assert.ok((await first).error);assert.equal(f.service.retired,true);await assert.rejects(new f.Service().render({source:"retry"}),/unavailable/);
    assert.equal(f.workers.length,failure==="factory"?0:1);
  }
});
test("image admission copies only after shared byte budget and never detaches caller buffer",async()=>{
  const {service,workers,Service}=fixture();const bytes=new Uint8Array(MAX_IMAGE_INPUT),other=new Service();
  const first=outcome(service.decodeImage(bytes)),second=outcome(other.decodeImage(bytes));await assert.rejects(service.decodeImage(new Uint8Array([1])),/busy/);
  assert.equal(bytes.length,MAX_IMAGE_INPUT);workers[0].ready();assert.notEqual(workers[0].sent[0].bytes.buffer,bytes.buffer);
  workers[0].error();await first;workers[0].error();await second;assert.equal(service.bytes,0);service.dispose();other.dispose();
});
test("the receiving boundary rejects resource URLs, invalid pixels and split UTF8 spans",()=>{
  const pixel={kind:"image",width:1,height:1,rgba:"/wAA/w=="};assert.ok(validateDocument({blocks:[pixel]}));
  for(const image of [{...pixel,source:"https://invalid.example/canary"},{...pixel,width:4096},{...pixel,rgba:"/wAA/w=A"}])assert.throws(()=>validateDocument({blocks:[image]}));
  assert.throws(()=>validateDocument({blocks:[{kind:"text",text:"🦀",spans:[{start:1,end:4}]}]}));
  assert.throws(()=>validateDocument({blocks:[{kind:"text",text:"link",spans:[{start:0,end:4,link:"javascript://alert(1)"}]}]}));
  assert.throws(()=>validateReply("encode",JSON.stringify({html:'<img src="https://invalid.example/a" />',thumbnail:pixel}),10000));
  assert.throws(()=>validateReply("encode",JSON.stringify({html:'<img src="data:image/png;base64,AA\" onerror=\"bad" />',thumbnail:pixel}),10000));
  const valid=validateReply("render",rendered("first\n\nsecond 🦀"));assert.equal(valid.plain_text,"first\n\nsecond 🦀");
});
test("documents carry the line breaks between blocks as breaks, on a budget of their own",()=>{
  const text=value=>({kind:"text",text:value});
  validateDocument({blocks:[{kind:"break",count:2},text("a"),{kind:"break",count:0},{kind:"math",source:"$$x$$",tex:"x"},text("b"),{kind:"break",count:3}]});
  // 512 blocks and a break around each are within both budgets; one more of either is not.
  const full=[{kind:"break",count:1}];for(let i=0;i<512;i++)full.push(text("x"),{kind:"break",count:2});
  validateDocument({blocks:full});
  assert.throws(()=>validateDocument({blocks:[...full,text("y")]}),/Invalid/);
  for(const bad of [{kind:"break",count:-1},{kind:"break",count:1.5},{kind:"break",count:"2"},{kind:"break"},{kind:"break",count:1,extra:true},{kind:"break",count:protocol.MAX_SOURCE+1}])
    assert.throws(()=>validateDocument({blocks:[text("a"),bad,text("b")]}),/Invalid/,JSON.stringify(bad));
  // Never two in a row.
  assert.throws(()=>validateDocument({blocks:[text("a"),{kind:"break",count:2},{kind:"break",count:2},text("b")]}),/Invalid/);
  assert.equal(protocol.MAX_BREAKS,protocol.MAX_BLOCKS+1);
});
test("all breaks together ask for no more line breaks than a message holds",()=>{
  const text=value=>({kind:"text",text:value});
  // 512 one-letter blocks, a whole message's line breaks between each two.
  const hostile=[];for(let i=0;i<protocol.MAX_BLOCKS;i++){if(i)hostile.push({kind:"break",count:protocol.MAX_SOURCE});hostile.push(text("x"));}
  assert.throws(()=>validateDocument({blocks:hostile}),/Invalid/);
  const spread=extra=>({blocks:[{kind:"break",count:protocol.MAX_SOURCE/2},text("x"),{kind:"break",count:protocol.MAX_SOURCE/2+extra}]});
  validateDocument(spread(0));
  assert.throws(()=>validateDocument(spread(1)),/Invalid/);
});
test("serial queued renders get their retiring deadline at dispatch; queue wait is reject-only",async()=>{
  const {service,workers,clock}=fixture();
  // Four admitted jobs, each well inside its own 8s budget, on one serial worker.
  const jobs=Array.from({length:4},(_,i)=>outcome(service.render({source:`queued ${i}`})));
  workers[0].ready();
  for(let i=0;i<4;i++){
    assert.equal(workers[0].sent.length,i+1);assert.equal(service.active.deadline,clock.now+8000,"execution budget starts at dispatch");
    clock.advance(2500);assert.equal(service.retired,false,"queue time never retires a healthy running job");
    workers[0].finish(`reply ${i}`);assert.equal((await jobs[i]).value.plain_text,`reply ${i}`);
  }
  assert.equal(service.bytes,0);assert.equal(workers.length,1);assert.equal(workers[0].terminated,0);
  // A job that waited past its separate queue deadline is rejected without
  // retiring the healthy job that is running.
  const first=outcome(service.render({source:"first"})),running=outcome(service.render({source:"running"})),waiting=outcome(service.render({source:"waiting"}));
  clock.advance(7000);workers[0].finish("first done");assert.ok((await first).value);
  clock.advance(1000);assert.match((await waiting).error.message,/deadline/);assert.equal(service.retired,false);
  clock.advance(1000);workers[0].finish("running done");assert.equal((await running).value.plain_text,"running done");
  assert.equal(service.bytes,0);assert.equal(workers.length,1);
  service.dispose();
});
test("active cancellation reports the worker's actual release, never the early rejection",async()=>{
  const {service,workers,clock}=fixture();const active=new AbortController(),queued=new AbortController();
  const first=outcome(service.render({source:"active"},{signal:active.signal})),second=outcome(service.render({source:"queued"},{signal:queued.signal}));
  workers[0].ready();queued.abort();const waiting=(await second).error;assert.equal(waiting.workerReleased,undefined,"a queued job held no worker");
  active.abort();const cancelled=(await first).error;let released=false;cancelled.workerReleased.then(()=>{released=true;});
  clock.advance(100);await Promise.resolve();assert.equal(released,false);assert.equal(service.bytes,6);
  workers[0].finish("late real reply");await Promise.resolve();assert.equal(released,true);assert.equal(service.bytes,0);
  // Retirement is also an actual release of a cancelled active job.
  const stalled=new AbortController();const third=outcome(service.render({source:"stalled"},{signal:stalled.signal}));stalled.abort();
  let retiredRelease=false;(await third).error.workerReleased.then(()=>{retiredRelease=true;});
  clock.advance(8000);await Promise.resolve();assert.equal(service.retired,true);assert.equal(retiredRelease,true);
});
