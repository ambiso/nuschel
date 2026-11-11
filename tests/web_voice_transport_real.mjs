// SPDX-License-Identifier: EUPL-1.2
// Actual production WASM through the private relay to stock Murmur. No audio devices.
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {join} from "node:path";
import {pathToFileURL} from "node:url";
const [sdk,relay,host,port] = process.argv.slice(2);
const bindings = await import(pathToFileURL(join(sdk,"nuschl_client_wasm.js")).href);
await bindings.default({module_or_path:await readFile(join(sdk,"nuschl_client_wasm_bg.wasm"))});
const a = new bindings.WebClient(relay);
const b = new bindings.WebClient(relay);
const ai = a.add_server("",host,Number(port),"voice-wasm-one",undefined);
const bi = b.add_server("",host,Number(port),"voice-wasm-two",undefined);
let received = [];
a.on_change(()=>{}); b.on_change(()=>{});
a.on_audio(()=>{a.voice_packets();});
b.on_audio(()=>{received.push(...JSON.parse(b.voice_packets()));});
a.connect(ai); b.connect(bi);
const timer=setInterval(()=>{a.tick(performance.now());b.tick(performance.now());},100);
function view(c) { return JSON.parse(c.voice()); }
function context(v) {return {server_id:v.server_id,connection_generation:v.connection_generation,voice_generation:v.voice_generation};}
async function until(predicate,reason) {
  const end=Date.now()+15000;
  for (;;) {
    a.poll();b.poll();
    if (predicate()) return;
    assert.ok(Date.now()<end,`${reason}; a=${a.voice()} b=${b.voice()}`);
    await new Promise(resolve=>setTimeout(resolve,20));
  }
}
function accept(c) {
  const s=JSON.parse(c.servers())[0];
  if (s.certificate) c.accept_certificate(0,s.certificate);
  return s.status === "connected";
}
function ready(c,capture,playback=true) {c.audio_ready(JSON.stringify({...context(view(c)),capture,playback}));}
function send(c,sequence) {c.voice_packet(JSON.stringify({...context(view(c)),sequence,end:false}),new Uint8Array([0xf8,0xff,0xfe]));}
try {
  await until(()=>{const ar=accept(a),br=accept(b);return ar&&br;},"TLS login failed");
  assert.equal(view(a),null);
  a.join(ai,0);b.join(bi,0);
  await until(()=>view(a)?.joined && view(b)?.joined && !view(a).deafened && !view(b).deafened,"voice join not acknowledged");
  assert.equal(view(a).muted,true);
  ready(a,true);ready(b,false);
  assert.throws(()=>send(a,"9007199254740993"),/authorized|muted/);
  a.toggle_mute(ai);
  await until(()=>!view(a).muted,"unmute not acknowledged");
  assert.equal(view(a).can_transmit,false);
  ready(a,true);
  assert.equal(view(a).can_transmit,true);
  const active=context(view(a));
  // Text navigation does not retarget voice, including private conversation browsing.
  const own=JSON.parse(a.servers())[0];
  const recipient=own.users.find(user=>user.name==="voice-wasm-two");
  a.open_direct(ai,recipient.session);a.poll();
  assert.deepEqual(context(view(a)),active);
  send(a,"9007199254740993");
  await until(()=>received.length>0,"stock Murmur did not forward tunneled Opus");
  assert.equal(received[0].sequence,"9007199254740993");
  assert.deepEqual(received[0].opus,[0xf8,0xff,0xfe]);
  assert.equal(received[0].speaker,own.own_session);
  assert.throws(()=>send(a,"9007199254740993"),/Old voice/);
  const stale=context(view(a));
  a.toggle_mute(ai);
  assert.equal(view(a).muted,true);
  assert.throws(()=>a.audio_ready(JSON.stringify({...stale,capture:true,playback:true})),/Stale/);
  assert.throws(()=>send(a,"9007199254740995"),/authorized|muted/);
  a.toggle_deaf(ai);
  for (let i=0;i<10;i++) {await new Promise(resolve=>setTimeout(resolve,20));a.poll();assert.equal(view(a).deafened,true);assert.equal(view(a).muted,true);}
  a.stop_audio();a.poll();assert.equal(view(a),null);
  assert.equal(JSON.parse(a.servers())[0].status,"connected");
  a.edit_server(ai,"Renamed local bookmark",host,Number(port),"voice-wasm-renamed",own.pin);
  assert.equal(JSON.parse(a.bookmarks()).length,1);
  assert.equal(JSON.parse(a.bookmarks())[0].username,"voice-wasm-renamed");
  a.disconnect(ai);a.poll();assert.equal(view(a),null);
  assert.throws(()=>a.audio_ready(JSON.stringify({...stale,capture:true,playback:true})),/Stale/);
  console.log(JSON.stringify({passed:true,transport:"TLS UDPTunnel",actual_wasm:sdk,received_packets:received.length,sequence:received[0].sequence,checks:["startup mute","join acknowledgment","readiness gate","private text browsing preserves route","stock Murmur Opus interchange","duplicate rejection","mute stale-work rejection","deaf implies mute","stop audio preserves chat","bookmark identity replacement","disconnect revocation"]}));
} finally {clearInterval(timer);a.disconnect(ai);b.disconnect(bi);}
