// SPDX-License-Identifier: EUPL-1.2
// Actual worker persistence logic; facade isolates network/codec work. Rust
// catalog invariants and real browser interactions have separate tests.
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import vm from "node:vm";
import test from "node:test";
async function worker() {
  let order=["a","b","c"], records=order.map(id=>({id})), persisted=JSON.stringify(records), blocked=true;
  const posts=[], pending=[], writes=[];
  const callbacks={};
  class Client {
    on_change(cb){callbacks.state=cb;} on_audio(){}
    poll(){return "[]";} timeline(){return "null";} voice(){return '{"server_id":"a","connection_generation":"9","voice_generation":"7"}';}
    voice_packets(){return "[]";} storage_requests(){return "[]";} tick(){return false;}
    servers(){return JSON.stringify(order.map(id=>({id,runtime_index:records.findIndex(record=>record.id===id)})));}
    bookmarks(){return JSON.stringify(order.map(id=>records.find(record=>record.id===id)));}
    restore_bookmarks(){}
    set_server_order(raw){
      const ids=JSON.parse(raw);
      assert.equal(new Set(ids).size,records.length);assert.equal(ids.length,records.length);
      assert.ok(ids.every(id=>records.some(record=>record.id===id)));
      const changed=JSON.stringify(order)!==raw;order=ids;return changed;
    }
    add_server(id){records.push({id});order.push(id);return records.length-1;}
  }
  const context=vm.createContext({onmessage:undefined,Uint8Array,queueMicrotask,performance:{now:()=>0},setInterval(){},postMessage(message){posts.push(message);}});
  const module=new vm.SourceTextModule(await readFile(new URL("../web/app/src/sdk.worker.js",import.meta.url),"utf8"),{context});
  await module.link(async specifier=>{
    if(specifier==="./sdk/nuschl_client_wasm.js")return new vm.SyntheticModule(["default","WebClient"],function(){this.setExport("default",async()=>{});this.setExport("WebClient",Client);},{context});
    if(specifier==="./server-order.js")return new vm.SourceTextModule(await readFile(new URL("../web/app/src/server-order.js",import.meta.url),"utf8"),{context});
    if(specifier==="./history.js")return new vm.SyntheticModule(["openHistory"],function(){this.setExport("openHistory",async()=>({bookmarks(value){
      if(value===undefined)return Promise.resolve(persisted);
      writes.push(value);
      if(!blocked){persisted=value;return Promise.resolve();}
      return new Promise((resolve,reject)=>pending.push({resolve(){persisted=value;resolve();},reject(){reject(new Error("quota"));}}));
    }}));},{context});
    throw new Error(specifier);
  });
  await module.evaluate();
  async function flush(){await new Promise(setImmediate);}
  async function send(data){await context.onmessage({data});await flush();}
  await send({method:"init",relay:"ws://127.0.0.1:1"});
  return {posts,writes,pending,send,flush,callbacks,get order(){return order;},get persisted(){return JSON.parse(persisted).map(record=>record.id);},release(){pending.shift().resolve();},fail(){pending.shift().reject();},unblock(){blocked=false;}};
}
test("order acknowledgment waits for the actual storage transaction; refresh does not duplicate it",async()=>{
  const w=await worker(), action=w.send({id:1,method:"set_server_order",args:[["c","a","b"]]});
  await w.flush();assert.equal(w.pending.length,1);assert.ok(!w.posts.some(post=>post.id===1));
  w.callbacks.state();await w.flush();assert.equal(w.writes.length,1);
  w.release();await action;
  assert.deepEqual(w.persisted,["c","a","b"]);
  assert.equal(w.posts.find(post=>post.id===1).value,true);
  assert.ok(w.posts.filter(post=>post.kind==="state").every(post=>post.voice.server_id==="a" && post.voice.connection_generation==="9" && post.voice.voice_generation==="7"),"ordering changed voice context");
});
test("failed storage restores the prior order and preserves a concurrent added bookmark",async()=>{
  const w=await worker(), action=w.send({id:2,method:"set_server_order",args:[["c","a","b"]]});
  await w.flush();await w.send({id:3,method:"add_server",args:["d"]});
  assert.equal(w.writes.length,1,"cache refresh queued a stale snapshot during the order transaction");
  w.unblock();w.fail();await action;await w.flush();
  assert.deepEqual(Array.from(w.order),["a","b","c","d"]);assert.deepEqual(w.persisted,["a","b","c","d"]);
  assert.match(w.posts.find(post=>post.id===2).error,/previous order was restored/);
});
test("overlapping order commands serialize their commits",async()=>{
  const w=await worker(), first=w.send({id:4,method:"set_server_order",args:[["c","a","b"]]}), second=w.send({id:5,method:"set_server_order",args:[["b","c","a"]]});
  await w.flush();assert.equal(w.pending.length,1);w.release();await first;await w.flush();
  assert.equal(w.pending.length,1);w.release();await second;
  assert.deepEqual(w.persisted,["b","c","a"]);
  assert.deepEqual(w.posts.filter(post=>post.id===4||post.id===5).map(post=>post.id),[4,5]);
});
