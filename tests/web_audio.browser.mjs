// SPDX-License-Identifier: EUPL-1.2
// Actual browser audio/codec workers and TLS SDK on an owned loopback Murmur.
import assert from "node:assert/strict";
import {createServer} from "node:http";
import {createConnection} from "node:net";
import {readFile,writeFile} from "node:fs/promises";
import {resolve,extname,join} from "node:path";
import {pathToFileURL} from "node:url";
const [source,sdkSource,sdk,codec,relayUrl,port,wrapper,playwright,output]=process.argv.slice(2);
assert.ok(source&&sdkSource&&sdk&&codec&&relayUrl&&wrapper&&playwright&&output);
const relay=new URL(relayUrl);assert.equal(relay.hostname,"127.0.0.1");assert.equal(relay.protocol,"ws:");
assert.ok(Number(port)>1023&&Number(port)<65536);
const {chromium}=await import(pathToFileURL(playwright).href);
const csp="default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; worker-src 'self'; connect-src 'self'; base-uri 'none'; object-src 'none'; frame-ancestors 'none'";
let origin;
const routes={"/audio.js":join(source,"web/app/src/audio.js"),"/audio-protocol.js":join(source,"web/app/src/audio-protocol.js"),
  "/audio-codec.worker.js":join(source,"web/app/src/audio-codec.worker.js"),"/audio-worklet.js":join(source,"web/app/src/audio-worklet.js"),
  "/fixture.mjs":join(source,"tests/web_audio.fixture.mjs"),"/sdk.worker.js":join(sdkSource,"sdk.worker.js"),"/history.js":join(sdkSource,"history.js"),"/server-order.js":join(sdkSource,"server-order.js")};
const server=createServer(async(req,res)=>{
  const path=new URL(req.url,"http://127.0.0.1").pathname;
  const headers={"Content-Security-Policy":csp,"Permissions-Policy":"microphone=(self), camera=()","X-Content-Type-Options":"nosniff"};
  try {
    let body,type="text/javascript";
    if(path==="/"){body='<!doctype html><meta charset="utf-8"><title>Private browser audio proof</title><button id="join">Join voice</button><button id="microphone">Microphone</button><button id="mute">Mute microphone</button><button id="deafen">Deafen</button><div id="loaded"></div><output></output><script type="module" src="/fixture.mjs"></script>';type="text/html";}
    else if(path==="/fixture-config.json"){body=JSON.stringify({relay:origin.replace("http:","ws:")+"/relay",host:"127.0.0.1",port:Number(port)});type="application/json";}
    else if(path.startsWith("/sdk/")||path.startsWith("/audio-codec/")){
      const name=path.split("/").at(-1);assert.ok(/^[a-zA-Z0-9_.]+$/.test(name));
      body=await readFile(join(path.startsWith("/sdk/")?sdk:codec,name));type=extname(name)===".wasm"?"application/wasm":"text/javascript";
    } else if(routes[path])body=await readFile(routes[path]);else throw new Error("unknown route");
    res.writeHead(200,{...headers,"Content-Type":type});res.end(body);
  }catch{res.writeHead(404,headers);res.end();}
});
const tunnels=new Set();
server.on("upgrade",(req,socket,head)=>{
  if(req.url!=="/relay"){socket.destroy();return;}
  const upstream=createConnection({host:"127.0.0.1",port:Number(relay.port)},()=>{
    let request=`GET ${relay.pathname} HTTP/1.1\r\n`;
    for(let i=0;i<req.rawHeaders.length;i+=2)request+=`${req.rawHeaders[i]}: ${req.rawHeaders[i+1]}\r\n`;
    upstream.write(request+"\r\n");if(head.length)upstream.write(head);socket.pipe(upstream).pipe(socket);
  });
  tunnels.add(socket);tunnels.add(upstream);socket.on("close",()=>{tunnels.delete(socket);upstream.destroy();});
  upstream.on("close",()=>{tunnels.delete(upstream);socket.destroy();});socket.on("error",()=>upstream.destroy());upstream.on("error",()=>socket.destroy());
});
await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));origin=`http://127.0.0.1:${server.address().port}`;
const browser=await chromium.launch({headless:true,executablePath:wrapper});
const contexts=[],violations=[],browserErrors=[],network=[];
async function open(username){
  const context=await browser.newContext({viewport:{width:1000,height:700},permissions:["microphone"]});contexts.push(context);
  context.on("request",request=>{if(!request.url().startsWith(origin+"/")&&!request.url().startsWith("data:"))network.push(request.url());});
  const page=await context.newPage();page.on("pageerror",error=>browserErrors.push(error.message));
  page.on("console",msg=>{if(msg.type()==="error")browserErrors.push(msg.text());});
  await page.addInitScript(()=>document.addEventListener("securitypolicyviolation",event=>{globalThis.policyViolations??=[];globalThis.policyViolations.push(event.violatedDirective);}));
  await page.goto(`${origin}/?username=${username}`);
  await page.waitForFunction(()=>window.fixture?.snapshot().servers[0]?.status==="connected",null,{timeout:20000});
  return page;
}
async function state(page){return page.evaluate(()=>fixture.snapshot());}
try {
  const a=await open("browser-audio-one"),b=await open("browser-audio-two");
  await a.getByRole("button",{name:"Join voice",exact:true}).click();await b.getByRole("button",{name:"Join voice",exact:true}).click();
  for(const page of [a,b])await page.waitForFunction(()=>fixture.snapshot().voice?.joined&&fixture.snapshot().audio.playback,null,{timeout:20000});
  assert.equal((await state(a)).voice.muted,true);assert.equal((await state(a)).audio.microphone,false);
  assert.equal((await state(b)).audio.microphone,false);
  await a.getByRole("button",{name:"Microphone",exact:true}).click();
  await a.waitForFunction(()=>fixture.snapshot().voice?.can_transmit&&fixture.snapshot().audio.microphone,null,{timeout:20000});
  await b.waitForFunction(()=>fixture.snapshot().received>=25,null,{timeout:20000});
  await b.waitForFunction(()=>fixture.measure()?.rms>0.001,null,{timeout:15000});
  const initial=await b.evaluate(()=>fixture.measure());assert.ok(initial.finite);assert.equal(initial.sink.type,"none");
  const captured=await b.evaluate(()=>fixture.received.filter(p=>!p.end).map(p=>p.opus).slice(0,50));
  await writeFile(join(output,"wire-opus.json"),JSON.stringify(captured));
  await b.getByRole("button",{name:"Deafen",exact:true}).click();
  await b.waitForFunction(()=>fixture.snapshot().voice?.deafened&&fixture.snapshot().voice.muted&&!fixture.snapshot().audio.playback);
  await new Promise(resolve=>setTimeout(resolve,150));
  const deaf=await b.evaluate(()=>fixture.measure());assert.ok(deaf.finite&&deaf.rms<1e-8,"deafened output retained sound");
  await b.getByRole("button",{name:"Deafen",exact:true}).click();
  await b.waitForFunction(()=>!fixture.snapshot().voice?.deafened&&fixture.snapshot().audio.playback);
  await b.waitForFunction(()=>fixture.measure()?.rms>0.001);assert.equal((await state(b)).audio.microphone,false);
  await a.getByRole("button",{name:"Microphone",exact:true}).click();
  await a.waitForFunction(()=>fixture.snapshot().voice?.muted&&!fixture.snapshot().audio.microphone);
  await new Promise(resolve=>setTimeout(resolve,150));const stopped=await b.evaluate(()=>fixture.snapshot().received);
  await new Promise(resolve=>setTimeout(resolve,350));assert.equal(await b.evaluate(()=>fixture.snapshot().received),stopped,"muted capture kept transmitting");
  // Real asynchronous permission result, deliberately delayed AFTER the browser
  // has supplied its fake track. A newer mute must stop rather than attach it.
  await a.evaluate(()=>{
    const original=navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
    fixture.originalCapture=original;
    navigator.mediaDevices.getUserMedia=async constraints=>{
      const stream=await original(constraints);fixture.delayedStream=stream;
      await new Promise(resolve=>fixture.releaseCapture=resolve);return stream;
    };
  });
  await a.getByRole("button",{name:"Microphone",exact:true}).click();
  await a.waitForFunction(()=>Boolean(fixture.releaseCapture));
  await a.getByRole("button",{name:"Mute microphone",exact:true}).click();
  await a.waitForFunction(()=>fixture.snapshot().voice?.muted&&fixture.snapshot().servers[0]?.muted);
  await a.evaluate(()=>{navigator.mediaDevices.getUserMedia=fixture.originalCapture;fixture.releaseCapture();});
  await a.waitForFunction(()=>fixture.delayedStream.getTracks().every(track=>track.readyState==="ended"));
  assert.equal((await state(a)).audio.microphone,false);
  // Resume once, then rapidly deafen/undeafen using actual browser button events.
  await a.getByRole("button",{name:"Microphone",exact:true}).click();
  await a.waitForFunction(()=>fixture.snapshot().voice?.can_transmit&&fixture.snapshot().audio.microphone);
  await a.getByRole("button",{name:"Deafen",exact:true}).click({clickCount:4,delay:30});
  await a.waitForFunction(()=>fixture.snapshot().voice?.muted&&!fixture.snapshot().voice.deafened&&!fixture.snapshot().audio.microphone);
  await new Promise(resolve=>setTimeout(resolve,200));const rapid=await b.evaluate(()=>fixture.snapshot().received);
  await new Promise(resolve=>setTimeout(resolve,250));assert.equal(await b.evaluate(()=>fixture.snapshot().received),rapid,"rapid policy changes leaked queued capture");
  // Disconnect/reconnect leaves the new generation muted and without old consent.
  const oldGeneration=(await state(a)).voice.connection_generation;
  const index=0;
  await a.evaluate(index=>fixture.request("disconnect",[index]),index);
  await a.waitForFunction(()=>fixture.snapshot().voice===null);
  await a.evaluate(index=>fixture.request("connect",[index]),index);
  await a.waitForFunction(()=>fixture.snapshot().servers[0]?.status==="connected");
  await a.getByRole("button",{name:"Join voice",exact:true}).click();
  await a.waitForFunction(()=>fixture.snapshot().voice?.joined&&fixture.snapshot().audio.playback);
  const reconnected=await state(a);assert.notEqual(reconnected.voice.connection_generation,oldGeneration);
  assert.equal(reconnected.voice.muted,true);assert.equal(reconnected.audio.microphone,false);
  await a.screenshot({path:join(output,"sender.png")});await b.screenshot({path:join(output,"receiver.png")});
  for(const page of [a,b])violations.push(...await page.evaluate(()=>globalThis.policyViolations||[]));
  assert.deepEqual(violations,[]);assert.deepEqual(network,[]);assert.deepEqual(browserErrors,[]);
  const report={passed:true,actual_sdk:sdk,actual_codec:codec,csp,permissions_policy:"microphone=(self), camera=()",
    initial_rms:initial.rms,deaf_rms:deaf.rms,received_packets:captured.length,sender:await state(a),receiver:await state(b),
    checks:["actual TLS UDPTunnel and Rust WASM Opus","startup mute","explicit fake microphone permission","actual AudioWorklet PCM","deafen flush","undeafen without mic","mute stops transmission","late permission result stopped","rapid real deafen toggles flush capture","reconnect revokes old consent","null audio sink","production CSP","same-origin requests only"]};
  await writeFile(join(output,"browser-proof.json"),JSON.stringify(report,null,2));console.log(JSON.stringify({passed:true,received_packets:captured.length,rms:initial.rms,deaf_rms:deaf.rms}));
} catch(error){
  await writeFile(join(output,"failure.json"),JSON.stringify({error:String(error),browserErrors,network,states:await Promise.all(contexts.map(async c=>c.pages()[0]?.evaluate(()=>window.fixture?.snapshot()).catch(()=>null)))},null,2));throw error;
} finally {
  for(const context of contexts){for(const page of context.pages())await page.evaluate(()=>window.fixture?.close()).catch(()=>{});await context.close();}
  await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(resolve=>server.close(resolve));
}
