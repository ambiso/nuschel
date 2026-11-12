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
   if(fault==="delay") {
    const response=await route.fetch();
    const prelude=`let delayed=false;const originalPost=self.postMessage.bind(self);self.postMessage=(value,...rest)=>{if(!delayed&&value?.kind==="result"&&typeof value.result==="string"&&value.result.includes("stale-source-canary")){delayed=true;globalThis.fixtureDelayed=true;setTimeout(()=>originalPost(value,...rest),1200);}else originalPost(value,...rest);};`;
    await route.fulfill({response,body:prelude+await response.text()});return;
   }
  }
  await route.continue();
 });
 const page=await context.newPage();
 await page.addInitScript(()=>{
  globalThis.captureCalls=0;navigator.mediaDevices.getUserMedia=()=>{captureCalls++;return Promise.reject(new Error("No capture in broker fixture"));};
  const fixture=globalThis.brokerFixture={sdk:null,last:null,work:[],replies:[],pending:new Map(),workers:[]};
  const NativeWorker=globalThis.Worker;
  globalThis.Worker=class extends NativeWorker {
   constructor(...args) {
    super(...args);fixture.workers.push(String(args[0]));
    if(!String(args[0]).includes("sdk.worker-"))return;
    fixture.sdk=this;
    this.addEventListener("message",({data})=>{
     if(data?.kind==="state")fixture.last=data;
     if(data?.kind==="content_work")for(const operation of data.operations){fixture.work.push(operation);if(fixture.work.length>128)fixture.work.shift();}
     if(data?.kind==="result"&&fixture.pending.has(data.id)) {
      fixture.replies.push({request:fixture.pending.get(data.id),value:data.value,error:data.error});fixture.pending.delete(data.id);
      if(fixture.replies.length>128)fixture.replies.shift();
     }
    });
   }
   postMessage(data,...rest) {
    if(data?.method==="content_complete")fixture.pending.set(data.id,data.args[0]);
    return super.postMessage(data,...rest);
   }
  };
 });
 page.on("pageerror",error=>console.error("browser error:",error.message));
 await page.goto(`${origin}${entry}#s=${host}:${port}&u=${username}`);
 if(width<=760)await page.getByRole("button",{name:"Open navigation",exact:true}).click();
 await page.getByRole("button",{name:"Trust this certificate",exact:true}).waitFor({timeout:20000});
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
const result={passed:false,artifact:root,stale:null,retired:null};
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
 assert.deepEqual(forbidden,[]);result.passed=true;result.forbidden=forbidden;
 console.log("Actual broker reconnect/source lease and permanent content-worker retirement passed.");
} finally {
 const {writeFile}=await import("node:fs/promises");await writeFile(screenshot.replace(".png",".json"),JSON.stringify(result,null,2));
 for(const context of contexts)await context.close();await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(resolve=>server.close(resolve));
}
