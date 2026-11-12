// SPDX-License-Identifier: EUPL-1.2
// Disposable Chromium UI/worker integration against a PRIVATE local relay/server.
// node tests/web_browser.mjs SITE RELAY_URL HOST PORT [SCREENSHOT]
import { createServer } from "node:http";
import { createConnection } from "node:net";
import { readFile } from "node:fs/promises";
import { resolve, extname, sep } from "node:path";
import { pathToFileURL } from "node:url";
import assert from "node:assert/strict";
const [root, relayUrl, host, port, screenshot] = process.argv.slice(2);
assert.ok(root && relayUrl && host === "127.0.0.1" && Number(port)>1023,"private fixture arguments required");
const relay = new URL(relayUrl);
assert.ok(relay.hostname === "127.0.0.1" && relay.protocol === "ws:","private loopback relay required");
const { chromium } = await import(pathToFileURL(process.env.NUSCHL_PLAYWRIGHT));
const directory = resolve(root);
const entry = await readFile(resolve(directory,"app/index.html")).then(()=>"/app/").catch(()=>"/");
const types={".html":"text/html",".js":"text/javascript",".wasm":"application/wasm",".css":"text/css",".svg":"image/svg+xml"};
const server=createServer(async(req,res)=>{
 try {
  const path=resolve(directory,"."+decodeURIComponent(new URL(req.url,"http://127.0.0.1").pathname));
  if (path!==directory && !path.startsWith(directory+sep)) throw new Error("scope");
  const target=extname(path)?path:resolve(path,"index.html");
  const data=await readFile(target);
  res.writeHead(200,{"Content-Type":types[extname(target)]||"application/octet-stream","Content-Security-Policy":`default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self' ws://127.0.0.1:${server.address().port}; worker-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`,"Permissions-Policy":"camera=(),microphone=(),geolocation=()"});res.end(data);
 } catch {res.writeHead(404);res.end();}
});
const tunnels=new Set();
server.on("upgrade",(req,socket,head)=>{
 if(req.url!=="/relay"){socket.destroy();return;}
 const upstream=createConnection({host:"127.0.0.1",port:Number(relay.port)},()=>{
  let headers=`GET ${relay.pathname} HTTP/1.1\r\n`;
  for(let i=0;i<req.rawHeaders.length;i+=2)headers+=`${req.rawHeaders[i]}: ${req.rawHeaders[i+1]}\r\n`;
  upstream.write(headers+"\r\n");if(head.length)upstream.write(head);
  socket.pipe(upstream).pipe(socket);
 });
 tunnels.add(socket);tunnels.add(upstream);
 socket.on("close",()=>{tunnels.delete(socket);upstream.destroy();});upstream.on("close",()=>{tunnels.delete(upstream);socket.destroy();});
 socket.on("error",()=>upstream.destroy());upstream.on("error",()=>socket.destroy());
});
await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));
const origin=`http://127.0.0.1:${server.address().port}`;
const browser=await chromium.launch({headless:true,executablePath:process.env.NUSCHL_CHROMIUM});
const contexts=[];
const forbidden=[];
async function open(username,fault="",width=1280) {
 const context=await browser.newContext({viewport:{width,height:900}});contexts.push(context);
 context.on("request",request=>{if(!request.url().startsWith(origin+"/")&&!request.url().startsWith("data:"))forbidden.push(request.url());});
 await context.route("**/*",async route=>{
  const url=route.request().url();
  if(!url.startsWith(origin+"/")){await route.abort();return;}
  if(/\/content-worker-[^/]+\.js$/.test(new URL(url).pathname)&&fault) {
   if(fault==="stall") {await route.fulfill({contentType:"text/javascript",body:'postMessage({kind:"ready"});onmessage=()=>{};'});return;}
   if(fault==="delay"||fault==="expire") {
    const response=await route.fetch();
    const prelude=`let delayed=false;const originalPost=self.postMessage.bind(self);self.postMessage=(value,...rest)=>{if(!delayed&&value?.kind==="result"&&typeof value.result==="string"&&(value.result.includes("stale-source-canary")||value.result.includes("expiry-source-canary-0"))){delayed=true;globalThis.fixtureDelayed=true;setTimeout(()=>originalPost(value,...rest),3000);}else originalPost(value,...rest);};`;
    await route.fulfill({response,body:prelude+await response.text()});return;
   }
  }
  await route.continue();
 });
 const page=await context.newPage();
 await page.addInitScript(()=>{
  globalThis.captureCalls=0;navigator.mediaDevices.getUserMedia=()=>{captureCalls++;return Promise.reject(new Error("No capture in broker fixture"));};
  const fixture=globalThis.brokerFixture={sdk:null,last:null,work:[],replies:[],pending:new Map(),workers:[],completions:[],codec:[],snapshots:0,snapshot_bytes:0};
  const NativeWorker=globalThis.Worker;
  globalThis.Worker=class extends NativeWorker {
   constructor(...args) {
    super(...args);fixture.workers.push(String(args[0]));
    if(String(args[0]).includes("content-worker-"))this.addEventListener("message",({data})=>{fixture.codec.push({kind:data?.kind,error:data?.error,result_bytes:typeof data?.result==="string"?data.result.length:null});if(fixture.codec.length>128)fixture.codec.shift();});
    if(!String(args[0]).includes("sdk.worker-"))return;
    fixture.sdk=this;
    this.addEventListener("message",({data})=>{
     if(data?.kind==="state"){fixture.last=data;fixture.snapshots++;fixture.snapshot_bytes+=new TextEncoder().encode(JSON.stringify(data)).length;}
     if(data?.kind==="content_work")for(const operation of data.operations){fixture.work.push(operation);if(fixture.work.length>128)fixture.work.shift();}
     if(data?.kind==="result"&&fixture.pending.has(data.id)) {
      fixture.replies.push({request:fixture.pending.get(data.id),value:data.value,error:data.error});fixture.pending.delete(data.id);
      if(fixture.replies.length>128)fixture.replies.shift();
     }
    });
   }
   postMessage(data,...rest) {
    if(data?.method==="content_complete"){fixture.pending.set(data.id,data.args[0]);fixture.completions.push({request:data.args[0],bytes:data.args[1]?.length});if(fixture.completions.length>128)fixture.completions.shift();}
    return super.postMessage(data,...rest);
   }
  };
 });
 page.on("pageerror",error=>console.error("browser error:",error.message));
 await page.goto(`${origin}${entry}#s=${host}:${port}&u=${username}`);
 if(width<=760)await page.getByRole("button",{name:"Open navigation",exact:true}).click();
 try { await page.getByRole("button",{name:"Trust this certificate",exact:true}).waitFor({timeout:20000}); }
 catch(error) { console.error("fixture-open-failure",username,await page.locator("body").innerText());await page.screenshot({path:screenshot.replace(".png",`-${username}-open-failure.png`)});throw error; }
 await page.getByRole("button",{name:"Trust this certificate",exact:true}).click();
 await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null,{timeout:20000});
 if(width<=760)await page.getByRole("button",{name:"Close navigation",exact:true}).click();
 return page;
}


async function rpc(page,method,...args) {
 return page.evaluate(({method,args})=>new Promise((resolve,reject)=>{
  const worker=brokerFixture.sdk,id=1_000_000+(globalThis.fixtureRpcNext=(globalThis.fixtureRpcNext||0)+1);
  const timer=setTimeout(()=>{worker.removeEventListener("message",listener);reject(new Error("fixture RPC timed out"));},5000);
  function listener({data}) {if(data?.kind!=="result"||data.id!==id)return;clearTimeout(timer);worker.removeEventListener("message",listener);if(data.error)reject(new Error(data.error));else resolve(data.value);}
  worker.addEventListener("message",listener);worker.postMessage({id,method,args});
 }),{method,args});
}
async function send(page,text) {
 await page.getByLabel("Message",{exact:true}).fill(text);
 await page.getByRole("button",{name:"Send",exact:true}).click();
 await page.waitForFunction(()=>document.querySelector("textarea")?.value==="");
}
const result={passed:false,artifact:root,stale:null,retired:null,expiry:null,recovery:null,stress:process.argv[7]==="stress",throughput:[]};
try {
 const stale=await open("broker-stale","delay");
 await send(stale,"stale-source-canary");
 await stale.waitForFunction(()=>brokerFixture.work.some(operation=>operation.kind==="render"&&operation.source==="stale-source-canary"));
 const original=await stale.evaluate(()=>({request:brokerFixture.work.find(operation=>operation.kind==="render"&&operation.source==="stale-source-canary").request,context:brokerFixture.last.timeline.content_context}));
 let activeCodec;
 for(let attempt=0;attempt<100;attempt++) {
  activeCodec=stale.workers().find(worker=>worker.url().includes("content-worker-"));
  if(activeCodec&&await activeCodec.evaluate(()=>globalThis.fixtureDelayed===true))break;
  await new Promise(resolve=>setTimeout(resolve,20));
 }
 assert.ok(activeCodec&&await activeCodec.evaluate(()=>globalThis.fixtureDelayed===true),"actual codec computed old source before delayed ACK");
 await rpc(stale,"disconnect",0);await rpc(stale,"connect",0);
 await stale.waitForFunction(original=>brokerFixture.last?.timeline?.content_context&&brokerFixture.last.timeline.content_context!==original.context&&brokerFixture.last.servers[0].status==="connected",original,{timeout:20000});
 await stale.waitForFunction(original=>brokerFixture.replies.some(reply=>reply.request===original.request&&reply.value==="null"),original,{timeout:15000});
 await stale.locator("article").filter({hasText:"stale-source-canary"}).waitFor({timeout:20000});
 result.stale=await stale.evaluate(()=>({content_workers:brokerFixture.workers.filter(url=>url.includes("content-worker-")).length,capture_calls:captureCalls,articles:document.querySelectorAll("article").length}));
 assert.equal(result.stale.content_workers,1);assert.equal(result.stale.capture_calls,0);result.stale.old_ticket_rejected=true;
 await stale.screenshot({path:screenshot.replace(".png","-stale.png")});

 const expiry=await open("broker-expiry","expire");
 for(let index=0;index<7;index++)await send(expiry,`expiry-source-canary-${index}`);
 let expiryCodec;
 for(let attempt=0;attempt<100;attempt++) {
  expiryCodec=expiry.workers().find(worker=>worker.url().includes("content-worker-"));
  if(expiryCodec&&await expiryCodec.evaluate(()=>globalThis.fixtureDelayed===true))break;
  await new Promise(resolve=>setTimeout(resolve,20));
 }
 assert.ok(expiryCodec&&await expiryCodec.evaluate(()=>globalThis.fixtureDelayed===true));
 const expirySdk=expiry.workers().find(worker=>worker.url().includes("sdk.worker-"));
 assert.ok(expirySdk);
 // Fault injection changes only the SDK deadline clock after authentication;
 // no source, SDK memory, keys or content-worker timer is read or modified.
 const expiredWork=await expiry.evaluate(()=>brokerFixture.work.filter(operation=>operation.kind==="render"&&operation.source.startsWith("expiry-source-canary-")));
 assert.ok(expiredWork.length>=1&&expiredWork.length<=4,"only actually posted jobs consume execution deadlines");
 await expirySdk.evaluate(()=>{const originalNow=performance.now.bind(performance);performance.now=()=>originalNow()+65000;});
 await rpc(expiry,"dismiss_notice",0); // one real refresh observes elapsed SDK deadline
 await expiry.waitForFunction(count=>document.querySelectorAll("article").length===7&&[...document.querySelectorAll("article button")].filter(node=>node.textContent==="Retry message preview").length===count,expiredWork.length,{timeout:10000});
 // Cursor identities had not acquired source/worker credit. They may now make
 // their FIRST admission; expired posted sources must never auto-resubmit.
 await expiry.waitForFunction(()=>brokerFixture.last.timeline.messages.every(message=>message.content_failed||!message.document.blocks.some(block=>block.kind==="text"&&block.text.startsWith("["))),null,{timeout:10000});
 const expiredSources=expiredWork.map(operation=>operation.source);
 await expiry.waitForTimeout(300);
 assert.equal(await expiry.evaluate(sources=>brokerFixture.work.filter(operation=>operation.kind==="render"&&sources.includes(operation.source)).length,expiredSources),expiredWork.length,"expired posted sources never automatically resubmit");
 await expiry.locator("article").first().getByRole("button",{name:"Retry message preview",exact:true}).click();
 await expiry.locator("article").first().filter({hasText:"expiry-source-canary-0"}).waitFor({timeout:10000});
 result.expiry=await expiry.evaluate(()=>({articles:document.querySelectorAll("article").length,content_workers:brokerFixture.workers.filter(url=>url.includes("content-worker-")).length,capture_calls:captureCalls}));
 assert.equal(result.expiry.articles,7);assert.equal(result.expiry.content_workers,1);assert.equal(result.expiry.capture_calls,0);
 await expiry.screenshot({path:screenshot.replace(".png","-expiry.png")});

 const failed=await open("broker-retired","stall");
 await send(failed,"retired-source-first");await send(failed,"retired-source-second");
 await failed.waitForFunction(()=>[...document.querySelectorAll("small")].filter(node=>node.textContent==="Preview unavailable. Reload to try again.").length>=2,null,{timeout:20000});
 assert.equal(await failed.getByRole("button",{name:"Retry message preview",exact:true}).count(),0);
 const before=await failed.evaluate(()=>brokerFixture.workers.filter(url=>url.includes("content-worker-")).length);
 await send(failed,"retired-source-third");
 await failed.waitForFunction(()=>document.querySelectorAll("article").length>=3);
 await failed.waitForFunction(()=>[...document.querySelectorAll("small")].filter(node=>node.textContent==="Preview unavailable. Reload to try again.").length>=3);
 result.retired=await failed.evaluate(()=>({content_workers:brokerFixture.workers.filter(url=>url.includes("content-worker-")).length,capture_calls:captureCalls,articles:document.querySelectorAll("article").length}));
 assert.equal(before,1);assert.equal(result.retired.content_workers,1);assert.equal(result.retired.capture_calls,0);
 assert.equal(await failed.locator("article").filter({hasText:"retired-source-third"}).count(),0,"retired codec never triggers privileged rendering fallback");
 await failed.screenshot({path:screenshot.replace(".png","-retired.png")});
 if(result.stress) {
  const recovery=await open("broker-recovery");
  const sdk=recovery.workers().find(worker=>worker.url().includes("sdk.worker-"));assert.ok(sdk);
  // Only the SDK realm's append transaction is failed. Main realm seeding is
  // confined to this fresh fixture origin; no host profile/database is opened.
  await sdk.evaluate(()=>{
   const original=IDBDatabase.prototype.transaction;
   IDBDatabase.prototype.transaction=function(names,mode,...rest){if(names==="messages"&&mode==="readwrite")throw new Error("fixture append failure");return original.call(this,names,mode,...rest);};
  });
  const canary="literal <em>UNSAVED_RECOVERY_CANARY</em>";
  await send(recovery,canary);
  await recovery.locator("article").filter({hasText:canary}).waitFor();
  const original=await recovery.evaluate(()=>({serial:brokerFixture.last.timeline.messages.at(-1).serial,status:brokerFixture.last.timeline.messages.at(-1).status,pin:brokerFixture.last.servers[0].pin}));
  assert.match(original.status,/not saved/i);
  const seeded=await recovery.evaluate(async({host,port,pin})=>{
   const bytes=await crypto.subtle.digest("SHA-256",new TextEncoder().encode(`${host}:${port}:${pin}`));
   const identity=[...new Uint8Array(bytes)].map(byte=>byte.toString(16).padStart(2,"0")).join("");
   const db=await new Promise((resolve,reject)=>{const request=indexedDB.open("nuschl-observed-history-v1");request.onsuccess=()=>resolve(request.result);request.onerror=()=>reject(request.error);});
   await new Promise((resolve,reject)=>{
    const tx=db.transaction("messages","readwrite"),store=tx.objectStore("messages");
    tx.oncomplete=resolve;tx.onerror=()=>reject(tx.error);
    for(let channel=0;channel<5;channel++)for(let row=0;row<900;row++)store.add({message:{server:identity,channel:String(channel),author:"seed-peer",observed_at_ms:channel*1000+row,body:`<b>cached ${channel}/${row}</b>`},outgoing:false,mode:"legacy"});
   });db.close();return 4500;
  },{host,port:Number(port),pin:original.pin});
  assert.equal(seeded,4500);
  await rpc(recovery,"disconnect",0);await rpc(recovery,"connect",0);
  await recovery.waitForFunction(()=>brokerFixture.last?.servers[0].status==="connected"&&brokerFixture.last.timeline.messages.length>=101,null,{timeout:20000});
  for(let channel=0;channel<5;channel++) {
   const metric=await recovery.evaluate(()=>({at:performance.now(),snapshots:brokerFixture.snapshots,bytes:brokerFixture.snapshot_bytes}));
   await rpc(recovery,"open",0,channel);
   await recovery.waitForFunction(channel=>brokerFixture.last?.timeline?.channel===channel&&!brokerFixture.last.timeline.loading&&brokerFixture.last.timeline.messages.length>=100,channel);
   for(let page=1;page<=8;page++) {
    await rpc(recovery,"older");
    await recovery.waitForFunction(({channel,page})=>brokerFixture.last?.timeline?.channel===channel&&!brokerFixture.last.timeline.loading&&brokerFixture.last.timeline.messages.length>=100*(page+1),{channel,page});
   }
   try { await recovery.waitForFunction(()=>brokerFixture.last.timeline.messages.every(message=>!message.content_failed&&!message.document.blocks.some(block=>block.kind==="text"&&block.text.startsWith("["))),null,{timeout:60000}); }
   catch(error) {
    const diagnostic=await recovery.evaluate(()=>({channel:brokerFixture.last?.timeline?.channel,count:brokerFixture.last?.timeline?.messages.length,missing:brokerFixture.last?.timeline?.messages.filter(message=>!message.content_token&&!message.content_failed).slice(0,4),work:brokerFixture.work.slice(-10).map(({kind,request})=>({kind,request})),reply:brokerFixture.replies.slice(-4)}));
    console.error("recovery-diagnostic",JSON.stringify(diagnostic));result.recovery_failure=diagnostic;throw error;
   }
   result.throughput.push(await recovery.evaluate(({channel,metric})=>({channel,messages:brokerFixture.last.timeline.messages.length,milliseconds:performance.now()-metric.at,snapshots:brokerFixture.snapshots-metric.snapshots,snapshot_bytes:brokerFixture.snapshot_bytes-metric.bytes}),{channel,metric}));
  }
  // Inactive row survives another real connection/lifetime change; then an
  // actual user reopen rebuilds its original-source lease, without IDB success.
  await rpc(recovery,"disconnect",0);await rpc(recovery,"connect",0);
  await recovery.waitForFunction(()=>brokerFixture.last?.servers[0].status==="connected",null,{timeout:20000});
  await rpc(recovery,"open",0,0);
  await recovery.waitForFunction(serial=>brokerFixture.last?.timeline?.channel===0&&brokerFixture.last.timeline.messages.some(message=>message.serial===serial),original.serial);
  let restored=recovery.locator("article").filter({hasText:canary});
  if(!await restored.count()) {
   const row=recovery.locator("article").filter({hasText:"broker-recovery"});
   const retry=row.getByRole("button",{name:"Retry message preview",exact:true});
   if(await retry.count())await retry.click();
  }
  try { await restored.waitFor({timeout:30000}); }
  catch(error) {
   const diagnostic=await recovery.evaluate(serial=>({canary:brokerFixture.last?.timeline?.messages.find(message=>message.serial===serial),count:brokerFixture.last?.timeline?.messages.length,unavailable:[...document.querySelectorAll("small")].filter(node=>node.textContent.includes("Preview unavailable")).length,retries:[...document.querySelectorAll("article button")].filter(node=>node.textContent==="Retry message preview").length,canary_work:brokerFixture.work.filter(operation=>operation.source?.includes("UNSAVED_RECOVERY_CANARY")),recent_replies:brokerFixture.replies.slice(-5),completions:brokerFixture.completions.slice(-8),codec:brokerFixture.codec.slice(-8)}),original.serial);
   console.error("canary-recovery-diagnostic",JSON.stringify(diagnostic));result.recovery_failure=diagnostic;await recovery.screenshot({path:screenshot.replace(".png","-recovery-failure.png")});throw error;
  }
  const after=await recovery.evaluate(serial=>{
   const message=brokerFixture.last.timeline.messages.find(message=>message.serial===serial);
   return {serial:message.serial,status:message.status,content_token:message.content_token,content_failed:message.content_failed,count:brokerFixture.last.timeline.messages.length,workers:brokerFixture.workers.filter(url=>url.includes("content-worker-")).length,capture_calls:captureCalls};
  },original.serial);
  assert.equal(after.serial,original.serial);assert.equal(after.status,original.status);assert.equal(after.count,901);assert.equal(after.workers,1);assert.equal(after.capture_calls,0);
  // Literal tags prove retained Plain mode, rather than guessed legacy HTML.
  assert.equal(await restored.locator("em").count(),0);
  const dbCount=await recovery.evaluate(()=>new Promise((resolve,reject)=>{const request=indexedDB.open("nuschl-observed-history-v1");request.onerror=()=>reject(request.error);request.onsuccess=()=>{const db=request.result,tx=db.transaction("messages"),count=tx.objectStore("messages").count();count.onsuccess=()=>resolve(count.result);tx.oncomplete=()=>db.close();};}));
  assert.equal(dbCount,4500,"unsaved live canary was never silently saved/replaced by fixture history");
  result.recovery={seeded,serial_preserved:true,unsaved_status_preserved:true,original_plain_mode_preserved:true,second_reconnect_inactive:true,articles:after.count,workers:after.workers,db_count:dbCount};
  await recovery.screenshot({path:screenshot.replace(".png","-recovery.png")});
 }
  assert.deepEqual(forbidden,[]);result.passed=true;result.forbidden=forbidden;
 console.log("Actual broker reconnect/source lease and permanent content-worker retirement passed.");
} finally {
 const {writeFile}=await import("node:fs/promises");await writeFile(screenshot.replace(".png",".json"),JSON.stringify(result,null,2));
 for(const context of contexts)await context.close();await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(resolve=>server.close(resolve));
}
