// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import {readFile} from "node:fs/promises";
import * as protocol from "./math-render-protocol.js";
const owner=(await readFile(new URL("./worker-owner.js",import.meta.url),"utf8")).replace(/^export /gm,"");
const source=(await readFile(new URL("./math-render-service.js",import.meta.url),"utf8"))
  .replace(/^import .*;\n/gm,"").replace(/^export \{.*\} from .*;\n/gm,"")
  .replaceAll("import.meta.url",'"https://fixture.invalid/math-render-service.js"').replace(/^export /gm,"");
const tree=text=>({tag:"math",attrs:{},children:[{tag:"mi",attrs:{},children:[{text}]}]});
const outcome=promise=>promise.then(value=>({value}),error=>({error}));
class Clock {
  now=0;next=0;jobs=new Map();
  setTimeout(callback,delay){const id=++this.next;this.jobs.set(id,{at:this.now+delay,callback});return id;}
  clearTimeout(id){this.jobs.delete(id);}
  advance(ms,{dispatch=true}={}){this.now+=ms;if(dispatch)for(const [id,job] of [...this.jobs])if(job.at<=this.now&&this.jobs.delete(id))job.callback();}
}
class Worker {
  sent=[];terminated=0;
  postMessage(data){this.sent.push(data);}
  terminate(){this.terminated++;}
  emit(data){this.onmessage?.({data});}
  ready(){this.emit({kind:"ready"});}
  finish(text="x",id=this.sent.at(-1).id){this.emit({kind:"result",id,tree:JSON.stringify(tree(text))});}
  error(id=this.sent.at(-1).id){this.emit({kind:"error",id,message:"This formula could not be rendered. Its source is available."});}
}
function fixture(options={}) {
  const workers=[],clock=new Clock();
  // Separate module realm per case, not an exported production reset function.
  const realm=vm.createContext({...protocol,TextEncoder,performance:{now:()=>clock.now},setTimeout:clock.setTimeout.bind(clock),clearTimeout:clock.clearTimeout.bind(clock)});
  new vm.Script(owner+"\n"+source+"\nglobalThis.Service=MathRenderService;").runInContext(realm);
  const Service=realm.Service;
  const workerFactory=()=>{const worker=new Worker();workers.push(worker);return worker;};
  return {service:new Service({workerFactory,...options}),workers,clock,Service};
}
test("all facades share eight jobs/32KiB and the first admitted factory, not unused constructors",async()=>{
  const f=fixture();f.service.dispose();let calls=0;const worker=new Worker();
  const service=new f.Service({workerFactory:()=>{calls++;return worker;}}),other=new f.Service({workerFactory:()=>assert.fail("replacement")});
  const jobs=Array.from({length:8},(_,i)=>outcome((i%2?other:service).render(String(i)+"x".repeat(4095))));
  assert.equal(service.bytes,32768);await assert.rejects(other.render("overflow"),/busy/);assert.equal(calls,1);assert.equal(f.workers.length,0);
  worker.ready();for(const job of jobs){worker.finish();assert.ok((await job).value);}
  assert.equal(service.bytes,0);service.dispose();other.dispose();assert.equal(worker.terminated,0);
});
test("active cancellation keeps its exact reservation until real ACK and cannot cache a discarded result",async()=>{
  const {service,workers,clock}=fixture();const abort=new AbortController();
  const first=outcome(service.render("old",false,{signal:abort.signal})),next=outcome(service.render("new"));workers[0].ready();clock.advance(50);abort.abort();
  assert.match((await first).error.message,/cancelled/);assert.equal(service.bytes,6);assert.equal(service.active.deadline,2000);assert.equal(workers[0].sent.length,1);
  workers[0].finish("discard");assert.equal(service.bytes,3);assert.equal(workers[0].sent.at(-1).tex,"new");workers[0].finish("new");await next;
  assert.equal(service.cacheEntries,1);const oldAgain=outcome(service.render("old"));assert.equal(workers[0].sent.length,3);workers[0].finish();await oldAgain;assert.equal(workers.length,1);
});
test("queued cancellation, facade disposal and startup cancellation never replace the healthy owner",async()=>{
  const {service,workers,Service}=fixture();const first=outcome(service.render("first"));service.dispose();assert.ok((await first).error);
  const other=new Service();const active=outcome(other.render("active"));const abort=new AbortController();
  const queued=outcome(other.render("never posted",false,{signal:abort.signal}));abort.abort();await queued;
  workers[0].ready();assert.equal(workers[0].sent.length,1);workers[0].finish();await active;
  other.dispose();const later=new Service();const reused=outcome(later.render("reuse"));workers[0].finish();await reused;
  assert.equal(workers.length,1);assert.equal(workers[0].terminated,0);
});
test("cold loading survives first 2s job expiry and later retry uses the same original 15s loader",async()=>{
  const {service,workers,clock,Service}=fixture();const first=outcome(service.render("cold"));clock.advance(2000);
  assert.match((await first).error.message,/deadline/);assert.equal(service.retired,false);assert.equal(service.bytes,0);
  const other=new Service({startupMs:1,workerFactory:()=>assert.fail("new owner")});const retry=outcome(other.render("later"));clock.advance(100);
  workers[0].ready();workers[0].finish();assert.ok((await retry).value);assert.equal(workers.length,1);assert.equal(workers[0].terminated,0);
});
test("late startup readiness loses even when timer dispatch is delayed and new options cannot extend the ceiling",async()=>{
  const {service,workers,clock,Service}=fixture({startupMs:100});const abort=new AbortController();
  const first=outcome(service.render("cancelled loader",false,{signal:abort.signal}));clock.advance(50);abort.abort();await first;
  const other=new Service({startupMs:15000});const next=outcome(other.render("new facade"));clock.advance(51,{dispatch:false});workers[0].ready();
  assert.equal((await next).error.code,"worker-unavailable");assert.equal(service.retired,true);assert.equal(workers[0].sent.length,0);assert.equal(workers[0].terminated,1);
});
test("new facades/jobs cannot extend canceled active deadline or manufacture replacement credits",async()=>{
  const {service,workers,clock,Service}=fixture({timeoutMs:25});const abort=new AbortController();
  const active=outcome(service.render("stall",false,{signal:abort.signal}));workers[0].ready();clock.advance(10);abort.abort();await active;
  const later=Array.from({length:20},()=>outcome(new Service({timeoutMs:2000}).render("later")));clock.advance(15);
  assert.ok((await Promise.all(later)).every(result=>result.error));assert.equal(service.retired,true);assert.equal(service.bytes,5);assert.equal(workers.length,1);assert.equal(workers[0].terminated,1);
  await assert.rejects(new Service().render("bypass"),error=>error.code==="worker-unavailable");
});
test("late exact result cannot beat absolute active deadline before its timer dispatch",async()=>{
  const {service,workers,clock}=fixture({timeoutMs:25});const first=outcome(service.render("late"));workers[0].ready();clock.advance(26,{dispatch:false});workers[0].finish();
  assert.equal((await first).error.code,"worker-unavailable");assert.equal(service.cacheEntries,0);assert.equal(service.retired,true);
});
test("ordinary parser errors and invalid input preserve healthy reuse without an unavailable error",async()=>{
  const {service,workers}=fixture();await assert.rejects(service.render("x".repeat(4097)));assert.equal(workers.length,0);
  const invalid=outcome(service.render("\\frac{"));workers[0].ready();workers[0].error();const failure=(await invalid).error;
  assert.equal(failure.code,undefined);assert.equal(service.retired,false);const next=outcome(service.render("valid"));workers[0].finish();assert.ok((await next).value);assert.equal(workers.length,1);
});
test("factory/post/trap/malformed/stale replies close one owner even across new facades",async()=>{
  for(const failure of ["factory","post","trap","malformed","stale"]){
    const {service,workers,Service}=fixture();if(failure==="factory")service.workerFactory=()=>{throw new Error("construction");};
    const first=outcome(service.render("first"));if(failure!=="factory"){
      if(failure==="post")workers[0].postMessage=()=>{throw new Error("clone");};workers[0].ready();
      if(failure==="trap")workers[0].onerror({preventDefault(){}});
      if(failure==="malformed")workers[0].emit({kind:"result",id:1,tree:'{"tag":"math","attrs":{"href":"/canary"},"children":[]}'});
      if(failure==="stale")workers[0].finish("wrong",7);
    }
    assert.equal((await first).error.code,"worker-unavailable");assert.equal(service.retired,true);await assert.rejects(new Service().render("new"),e=>e.code==="worker-unavailable");
    assert.equal(workers.length,failure==="factory"?0:1);
  }
});
test("throwing termination always settles cleanup but never releases the retired owner permit",async()=>{
  const {service,workers,clock,Service}=fixture({timeoutMs:25});const active=outcome(service.render("active")),next=outcome(service.render("queued"));workers[0].ready();
  workers[0].terminate=()=>{throw new Error("host terminate failure");};clock.advance(25);
  assert.equal((await active).error.code,"worker-unavailable");assert.equal((await next).error.code,"worker-unavailable");assert.equal(service.terminationFailed,true);
  assert.equal(service.active,null);assert.equal(service.queue.length,0);assert.equal(service.bytes,6);assert.equal(clock.jobs.size,0);
  workers[0].finish("late");assert.equal(service.cacheEntries,0);await assert.rejects(new Service().render("retry"),e=>e.code==="worker-unavailable");assert.equal(workers.length,1);
});
test("global immutable LRU survives disposal, counts duplicate keys once, and evicts below both caps",async()=>{
  const {service,workers,Service}=fixture();const a=outcome(service.render("same")),b=outcome(service.render("same"));workers[0].ready();workers[0].finish();await a;const size=service.cacheBytes;workers[0].finish();await b;
  assert.equal(service.cacheEntries,1);assert.equal(service.cacheBytes,size);const other=new Service();const cached=await other.render("same");assert.ok(Object.isFrozen(cached));assert.ok(Object.isFrozen(cached.children));
  service.dispose();assert.equal(other.cacheEntries,1);const before=workers[0].sent.length;await other.render("same");assert.equal(workers[0].sent.length,before);
  for(let i=0;i<80;i++){const next=outcome(other.render(`entry ${i}`));workers[0].finish();await next;}assert.equal(other.cacheEntries,64);assert.ok(other.cacheBytes<=2*1024*1024);
  // Near the maximum serialized reply, with bounded text and many inert rows.
  const large={tag:"math",attrs:{},children:Array.from({length:900},()=>({tag:"mrow",attrs:{},children:[]}))};
  assert.ok(JSON.stringify(large).length<65536);
  for(let i=0;i<80;i++){const next=outcome(other.render(`large ${i}`));workers[0].emit({kind:"result",id:workers[0].sent.at(-1).id,tree:JSON.stringify(large)});await next;}
  assert.ok(other.cacheEntries<64);assert.ok(other.cacheBytes<=2*1024*1024);assert.ok(other.cacheBytes>1900000);
});
test("retirement notifies mounted subscribers once; removals and throwing views cannot break cleanup or cached formulas",async()=>{
  const {service,workers,clock,Service}=fixture({timeoutMs:25});const cached=outcome(service.render("cached"));workers[0].ready();workers[0].finish("kept");const value=(await cached).value;
  let calls=0;const unsubscribe=service.onUnavailable(()=>calls++);unsubscribe();service.onUnavailable(()=>{throw new Error("bad view");});service.onUnavailable(()=>calls++);
  const stalled=outcome(service.render("stall"));clock.advance(25);assert.equal((await stalled).error.code,"worker-unavailable");assert.equal(calls,1);
  const other=new Service();other.onUnavailable(()=>calls++);assert.equal(calls,2);assert.equal(await other.render("cached"),value);assert.equal(workers.length,1);
  workers[0].finish("late");assert.equal(calls,2);assert.equal(other.cacheEntries,1);
});
test("diagnostics are frozen detached numeric snapshots without settlement/cache mutation",async()=>{
  const {service,workers}=fixture();const jobs=Array.from({length:8},(_,i)=>outcome(service.render(`source ${i}`)));
  const queued=service.queue;assert.ok(Object.isFrozen(queued));assert.ok(queued.every(Object.isFrozen));assert.deepEqual(Object.keys(queued[0]),["id","size","deadline"]);
  assert.throws(()=>queued.pop());assert.throws(()=>{queued[0].deadline=Infinity;});assert.equal(service.cache,undefined);
  workers[0].ready();const active=service.active;assert.ok(Object.isFrozen(active));assert.equal(active.resolve,undefined);assert.equal(active.client,undefined);assert.equal(active.packet,undefined);
  assert.throws(()=>{active.size=0;});await assert.rejects(service.render("overflow"),/busy/);for(const job of jobs){workers[0].finish();await job;}assert.equal(service.bytes,0);
});
test("duplicate readiness cannot reset deadlines or dispatch a second job; invalid timing options remain bounded",async()=>{
  const {service,workers,clock}=fixture({timeoutMs:NaN,startupMs:Infinity});const first=outcome(service.render("first")),next=outcome(service.render("second"));workers[0].ready();workers[0].ready();
  assert.equal(workers[0].sent.length,1);assert.equal(service.active.deadline,2000);clock.advance(2000);assert.equal((await first).error.code,"worker-unavailable");assert.ok((await next).error);assert.equal(workers.length,1);
});
