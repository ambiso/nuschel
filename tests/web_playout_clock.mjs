// SPDX-License-Identifier: EUPL-1.2
// Actual production processor, deterministic rendered-frame clock, no devices.
import test from "node:test";
import assert from "node:assert/strict";
const classes=new Map();
globalThis.sampleRate=48000;
globalThis.AudioWorkletProcessor=class {constructor(){this.port={sent:[],postMessage(value){this.sent.push(value);}};}};
globalThis.registerProcessor=(name,type)=>classes.set(name,type);
const {PLAYOUT_FRAME,PLAYOUT_RESERVE,MAX_SPEAKERS}=await import("../web/app/src/audio-worklet.js");
const Processor=classes.get("nuschl-playback");
class Port {
  constructor(){this.sent=[];this.closed=false;}
  postMessage(data){this.sent.push(data);}
  deliver(data){this.onmessage?.({data});}
  start(){}
  close(){this.closed=true;}
}
function fixture({order="port-first",rate=48000,enabled=true}={}) {
  globalThis.sampleRate=rate;
  const processor=new Processor(),port=new Port();
  const control=data=>processor.port.onmessage({data});
  const configure=(epoch="rx",active=enabled)=>control({kind:"configure",epoch,enabled:active,clocked:true});
  if(order==="port-first") {control({kind:"playout-port",port});configure();}
  else {configure();control({kind:"playout-port",port});}
  const current=()=>port.sent.findLast(value=>value.kind==="playout-configure");
  const ready=(value=current())=>port.deliver({...value,kind:"playout-ready"});
  const tick=(frames=128,mono=false)=>{
    const left=new Float32Array(frames),right=mono?null:new Float32Array(frames);
    processor.process([],[[left,...(right?[right]:[])]]);return {left,right};
  };
  const requests=()=>port.sent.filter(value=>value.kind==="render");
  const pending=()=>processor.clock.inFlight;
  const reply=(speakers=[],tuple=pending())=>port.deliver({...tuple,kind:"playout",speakers});
  return {processor,port,control,configure,current,ready,tick,requests,pending,reply};
}
function speaker(id=1,value=.25,volume=1) {
  return {speaker:id,samples:new Float32Array(960).fill(value),channels:2,frames:480,volume};
}
const silence=out=>assert.ok(out.left.every(value=>value===0)&&(!out.right||out.right.every(value=>value===0)));

test("both port/configure orders require the exact ready barrier before initial demand",()=>{
  for(const order of ["port-first","configure-first"]) {
    const f=fixture({order});silence(f.tick());assert.equal(f.requests().length,0);
    f.ready({...f.current(),clock_generation:f.current().clock_generation-1});
    f.tick();assert.equal(f.requests().length,0);
    f.ready();while(!f.pending())f.tick();assert.equal(f.requests().length,1);
    const request=f.pending();assert.equal(request.start_frame,960);assert.equal(request.frames,480);
    assert.equal(request.epoch,"rx");assert.ok(request.clock_generation>0);
  }
});

test("actual processor requests exact 10 ms cadence and keeps at most 20 ms reserve",()=>{
  const f=fixture();f.ready();let cursor=0;
  for(let quantum=0;quantum<100;quantum++) {
    const before=f.requests().length,out=f.tick();
    if(f.requests().length>before) {
      const request=f.pending();assert.ok(request.start_frame<=cursor+480);
      assert.ok(request.start_frame+480-cursor<=960);f.reply([speaker()]);
    }
    for(let i=0;i<128;i++)assert.equal(out.left[i],cursor+i<480?0:.25);
    cursor+=128;
  }
  const requests=f.requests();assert.ok(requests.length>20);
  for(let i=0;i<requests.length;i++) {
    assert.equal(requests[i].start_frame,480+i*480);
    assert.equal(requests[i].request,i+1);
  }
  assert.equal(f.processor.clock.left.length,PLAYOUT_RESERVE);
  assert.equal(f.processor.clock.right.length,PLAYOUT_RESERVE);
});

test("deadline expiry never releases the single request or queues catch-up decode",()=>{
  const f=fixture();f.ready();f.tick();const pending=f.pending();
  for(let i=0;i<2000;i++)silence(f.tick());
  assert.equal(f.requests().length,1);assert.equal(f.pending(),pending);
  const stalls=f.processor.port.sent.filter(value=>value.kind==="playout-stalled");
  assert.equal(stalls.length,1);assert.equal(stalls[0].request,pending.request);
  assert.equal(stalls[0].clock_generation,pending.clock_generation);
  assert.equal(stalls[0].start_frame,pending.start_frame);
  f.reply([speaker()]);assert.equal(f.pending(),null);
  assert.ok(f.current().clock_generation>pending.clock_generation);
  for(let i=0;i<10;i++)silence(f.tick());assert.equal(f.requests().length,1);
  f.ready();while(!f.pending())f.tick();assert.equal(f.requests().length,2);
  assert.ok(f.pending().start_frame>=f.processor.clock.cursor-128);
});

test("future PCM stays on its absolute timestamp and partial late PCM is trimmed",()=>{
  const f=fixture();f.ready();f.tick();f.reply([speaker()]);
  silence(f.tick());silence(f.tick());
  const crossing=f.tick();assert.ok(crossing.left.slice(0,96).every(v=>v===0));
  assert.ok(crossing.left.slice(96).every(v=>v===.25));
  const late=fixture();late.ready();late.tick();
  for(let i=0;i<4;i++)late.tick(); // cursor640, block480..960 partly elapsed
  const data=speaker();for(let i=0;i<480;i++)data.samples[i*2]=data.samples[i*2+1]=i/1024;
  late.reply([data]);const out=late.tick();
  for(let i=0;i<128;i++)assert.equal(out.left[i],(160+i)/1024);
  assert.equal(late.current().clock_generation,late.requests()[0].clock_generation);
});

test("whole-late exact reply re-buffers with a generation barrier and no stale replay",()=>{
  const f=fixture();f.ready();f.tick();const old={...f.pending()};
  for(let i=0;i<7;i++)f.tick(); // cursor1024 > block end960
  f.reply([speaker()],old);const generation=f.current().clock_generation;
  assert.ok(generation>old.clock_generation);silence(f.tick());assert.equal(f.requests().length,1);
  f.ready({...f.current(),clock_generation:old.clock_generation});f.tick();assert.equal(f.requests().length,1);
  f.ready();while(!f.pending())silence(f.tick());assert.equal(f.requests().length,2);
  assert.ok(f.pending().start_frame>old.start_frame+480);
  f.reply([speaker(1,1)],old);assert.ok(f.pending());
});

test("wrong epoch, generation, request, start and duplicate replies cannot acknowledge demand",()=>{
  const f=fixture();f.ready();f.tick();const pending=f.pending();
  for(const patch of [{epoch:"old"},{clock_generation:pending.clock_generation+1},{request:pending.request+1},
    {start_frame:pending.start_frame+480}]) {
    f.reply([speaker()],{...pending,...patch});assert.equal(f.pending(),pending);
  }
  f.reply([speaker()]);assert.equal(f.pending(),null);
  f.reply([speaker(1,1)],pending);
  while(f.processor.clock.cursor<480)f.tick();
  assert.ok(f.tick().left.every(value=>value===.25));
});

test("RX revoke/reopen clears queued and outstanding PCM and demands a fresh barrier",()=>{
  const f=fixture();f.ready();f.tick();const old={...f.pending()};f.reply([speaker()]);
  f.configure("rx",false);silence(f.tick());
  f.configure("next",true);f.reply([speaker(1,1)],old);silence(f.tick());assert.equal(f.requests().length,1);
  f.ready({...f.current(),epoch:"rx"});f.tick();assert.equal(f.requests().length,1);
  f.ready();while(!f.pending())f.tick();assert.equal(f.requests().length,2);
  assert.equal(f.pending().epoch,"next");assert.ok(f.pending().clock_generation>old.clock_generation);
  const generation=f.current().clock_generation,pending=f.pending();
  f.configure("next",true);assert.equal(f.current().clock_generation,generation);assert.equal(f.pending(),pending);
});

test("disabled ready flushes PCM and preserves an outstanding credit",()=>{
  const f=fixture();f.ready();f.tick();const pending=f.pending();
  f.ready({...f.current(),enabled:false});for(let i=0;i<20;i++)silence(f.tick());
  assert.equal(f.pending(),pending);assert.equal(f.requests().length,1);
  f.ready();f.tick();assert.equal(f.requests().length,1);
});

test("clocked production rejects main-port PCM and close/replacement flush their owned port",()=>{
  const f=fixture();f.ready();
  f.control({kind:"playback",epoch:"rx",...speaker(),job:1});
  f.tick();assert.equal(f.processor.mixer.speakers.size,0);assert.equal(f.processor.port.sent.length,0);
  f.reply([speaker()]);const replacement=new Port();f.control({kind:"playout-port",port:replacement});
  assert.equal(f.port.closed,true);silence(f.tick());assert.equal(f.pending(),null);
  const configure=replacement.sent.at(-1);replacement.deliver({...configure,kind:"playout-ready"});while(!f.pending())f.tick();
  assert.ok(f.pending());f.control({kind:"close"});assert.equal(replacement.closed,true);
  assert.equal(f.pending(),null);silence(f.tick());assert.ok(f.processor.clock.left.every(v=>v===0));
});

test("matching malformed batches fail atomically with bounded main error",()=>{
  const cases=[()=>({speakers:Array.from({length:17},(_,i)=>speaker(i))}),
    ()=>({speakers:[speaker(),speaker()]}),()=>({frames:479,speakers:[]}),
    ()=>({speakers:[{...speaker(),speaker:-1}]}),()=>({speakers:[{...speaker(),speaker:2**32}]}),
    ()=>({speakers:[{...speaker(),channels:1}]}),()=>({speakers:[{...speaker(),frames:960}]}),
    ()=>({speakers:[{...speaker(),samples:new Float32Array(958)}]}),
    ...[NaN,Infinity,-Infinity].map(value=>()=>({speakers:[speaker(1,value)]})),
    ...[-1,4.01,NaN,Infinity].map(volume=>()=>({speakers:[speaker(1,.25,volume)]}))];
  for(const build of cases) {
    const f=fixture();f.ready();f.tick();const pending=f.pending(),bad=build();
    f.port.deliver({...pending,kind:"playout",...bad});
    silence(f.tick());assert.equal(f.processor.clock.failed,true);
    assert.ok(f.processor.clock.left.every(value=>value===0));
    assert.equal(f.processor.port.sent.filter(value=>value.kind==="playout-error").length,1);
    f.port.deliver({...pending,kind:"playout",...bad});
    assert.equal(f.processor.port.sent.filter(value=>value.kind==="playout-error").length,1);
  }
});

test("raw finite extremes sum in f64, preserve stereo gain and fold mono before clipping",()=>{
  for(const mono of [false,true]) {
    const f=fixture();f.ready();f.tick();
    const max=3.4028234663852886e38;
    f.reply(Array.from({length:MAX_SPEAKERS},(_,i)=>speaker(i,i%2?max:-max,4)));
    for(let i=0;i<10;i++)silence(f.tick(128,mono));
  }
  const f=fixture();f.ready();f.tick();const first=speaker(1,0,.5),second=speaker(2,0,1);
  for(let i=0;i<480;i++) {first.samples[i*2]=3;first.samples[i*2+1]=-2;second.samples[i*2]=-.25;second.samples[i*2+1]=.5;}
  f.reply([first,second]);while(f.processor.clock.cursor<480)f.tick(1);
  const stereo=f.tick(128);assert.ok(stereo.left.every(v=>v===1));assert.ok(stereo.right.every(v=>v===-.5));
  const mono=f.tick(128,true);assert.ok(mono.left.every(v=>v===.375));
});

test("actual 48 kHz rate is required even when the main context check was bypassed",()=>{
  const f=fixture({rate:44100});f.tick();assert.equal(f.requests().length,0);
  assert.equal(f.processor.clock.failed,true);
  assert.match(f.processor.port.sent[0].message,/48 kHz/);
});

test("variable output quanta allocate no PCM scratch and never burst after a long callback",()=>{
  const f=fixture();f.ready();f.tick();f.reply([speaker()]);
  const left=f.processor.clock.left,right=f.processor.clock.right;
  const outputs=[1,127,480,960,4096].map(size=>[new Float32Array(size),new Float32Array(size)]);
  const F32=globalThis.Float32Array,F64=globalThis.Float64Array;
  globalThis.Float32Array=new Proxy(F32,{construct(){throw Error("callback allocated f32");}});
  globalThis.Float64Array=new Proxy(F64,{construct(){throw Error("callback allocated f64");}});
  try {for(const output of outputs)f.processor.process([],[output]);}
  finally {globalThis.Float32Array=F32;globalThis.Float64Array=F64;}
  assert.equal(f.processor.clock.left,left);assert.equal(f.processor.clock.right,right);
  assert.equal(f.requests().length,2);assert.ok(f.pending());
  assert.equal(left.byteLength+right.byteLength,15360);
});
