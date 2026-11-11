// SPDX-License-Identifier: EUPL-1.2
// Exercise the actual SDK Worker module with a deterministic transport facade;
// the Rust/TLS/route boundary is covered by client-wasm's private relay tests.
// node --experimental-vm-modules --test tests/web_voice_transport.mjs
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import vm from "node:vm";
import test from "node:test";

async function worker() {
  const posts = [];
  const counters = {poll:0,timeline:0,sent:0};
  let callbacks = {};
  let packets = [];
  let timer;
  let tickChanged = false;
  class Client {
    on_change(cb) { callbacks.state = cb; }
    on_audio(cb) { callbacks.audio = cb; }
    poll() { counters.poll++; return "[]"; }
    servers() { return "[]"; }
    timeline() { counters.timeline++; return "null"; }
    voice() { return "null"; }
    voice_packets() { const out = packets; packets = []; return JSON.stringify(out); }
    storage_requests() { return "[]"; }
    bookmarks() { return "[]"; }
    restore_bookmarks() {}
    tick() { return tickChanged; }
    voice_packet(raw,opus) {
      counters.sent++;
      const packet = JSON.parse(raw);
      assert.equal(packet.sequence,"9007199254740993");
      assert.ok(opus instanceof Uint8Array);
      assert.equal(packet.opus,undefined);
    }
    audio_ready() {}
  }
  const context = vm.createContext({
    onmessage:undefined,Uint8Array,queueMicrotask,
    setInterval(cb) { timer=cb; },performance:{now:()=>500},
    postMessage(message,transfers) { posts.push({message,transfers}); },
  });
  const module = new vm.SourceTextModule(await readFile(new URL("../web/app/src/sdk.worker.js",import.meta.url),"utf8"),{context});
  await module.link((specifier)=> {
    if (specifier === "./sdk/nuschl_client_wasm.js") return new vm.SyntheticModule(["default","WebClient"],function() {this.setExport("default",async()=>{});this.setExport("WebClient",Client);},{context});
    if (specifier === "./history.js") return new vm.SyntheticModule(["openHistory"],function() {this.setExport("openHistory",async()=>({bookmarks:async()=>"[]"}));},{context});
    throw new Error(`unexpected import: ${specifier}`);
  });
  await module.evaluate();
  async function send(data) { await context.onmessage({data}); await new Promise(setImmediate); }
  await send({method:"init",relay:"ws://private-fixture.invalid"});
  return {posts,counters,send,callbacks,
    queue(p) { packets.push(p); },
    async tick(changed) {tickChanged=changed;timer();await new Promise(setImmediate);},
  };
}

test("audio-only ingress and egress never serialize or post a timeline snapshot",async()=> {
  const w = await worker();
  const before = {...w.counters};
  w.posts.length=0;
  for (let i=0;i<48;i++) {
    w.queue({server_id:"s",connection_generation:"1",voice_generation:"2",speaker:7,sequence:"9007199254740993",opus:[1,2,3],end:false,volume:1});
    w.callbacks.audio();
  }
  await new Promise(setImmediate);
  assert.equal(w.posts.length,48);
  for (const {message,transfers} of w.posts) {
    assert.equal(message.kind,"voice_packet");
    assert.ok(message.packet.opus instanceof Uint8Array);
    assert.equal(transfers.length,1);
    assert.equal(transfers[0],message.packet.opus.buffer);
    assert.equal(message.packet.sequence,"9007199254740993");
  }
  w.posts.length=0;
  for (let i=0;i<100;i++) await w.send({method:"voice_packet",args:[{server_id:"s",connection_generation:"1",voice_generation:"2",sequence:"9007199254740993",opus:new Uint8Array([1,2,3]),end:false}]});
  assert.equal(w.counters.sent,100);
  assert.equal(w.counters.timeline,before.timeline);
  assert.equal(w.counters.poll,before.poll);
  assert.equal(w.posts.length,0);
});

test("idle ticks stay quiet; an actual state transition coalesces one snapshot",async()=> {
  const w = await worker();
  w.posts.length=0;
  for (let i=0;i<10;i++) await w.tick(false);
  assert.equal(w.posts.length,0);
  for (let i=0;i<20;i++) w.callbacks.state();
  await new Promise(setImmediate);
  assert.equal(w.posts.filter(p=>p.message.kind==="state").length,1);
  w.posts.length=0;
  await w.tick(true);
  assert.equal(w.posts.filter(p=>p.message.kind==="state").length,1);
});

test("oversized and non-byte audio fail before reaching the session",async()=> {
  const w=await worker();
  for (const opus of [new Uint8Array(961),[1,2,3],"raw",null]) {
    await w.send({id:9,method:"voice_packet",args:[{opus}]});
    assert.match(w.posts.at(-1).message.error,/Invalid voice packet/);
  }
  assert.equal(w.counters.sent,0);
});
