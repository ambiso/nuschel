// SPDX-License-Identifier: EUPL-1.2
// Disposable Chromium UI/worker integration against a PRIVATE local relay/server.
// node tests/web_broker.browser.mjs SITE RELAY_URL HOST PORT SCREENSHOT [stress|authority]
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
const {actionProof}=await import(process.env.NUSCHL_ACTION_CONTEXT?pathToFileURL(process.env.NUSCHL_ACTION_CONTEXT):new URL("../web/app/src/action-context.js",import.meta.url));
const {legacyPeer}=await import(process.env.NUSCHL_LEGACY_PEER?pathToFileURL(process.env.NUSCHL_LEGACY_PEER):new URL("./web_broker_legacy_peer.mjs",import.meta.url));
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
async function open(username,fault="",width=1280,trust=true) {
 const context=await browser.newContext({viewport:{width,height:900}});contexts.push(context);
 context.on("request",request=>{if(!request.url().startsWith(origin+"/")&&!request.url().startsWith("data:"))forbidden.push(request.url());});
 await context.route("**/*",async route=>{
  const url=route.request().url();
  if(!url.startsWith(origin+"/")){await route.abort();return;}
  if(/\/content-worker-[^/]+\.js$/.test(new URL(url).pathname)&&fault) {
   if(fault==="stall") {await route.fulfill({contentType:"text/javascript",body:'postMessage({kind:"ready"});onmessage=()=>{};'});return;}
   if(fault==="slow") {
    // Every real codec result is held 2.5s: a slow but healthy serial worker.
    const response=await route.fetch();
    const prelude=`const originalPost=self.postMessage.bind(self);self.postMessage=(value,...rest)=>{if(value?.kind==="result")setTimeout(()=>originalPost(value,...rest),2500);else originalPost(value,...rest);};`;
    await route.fulfill({response,body:prelude+await response.text()});return;
   }
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
  const fixture=globalThis.brokerFixture={sdk:null,last:null,work:[],replies:[],pending:new Map(),workers:[],completions:[],codec:[],snapshots:0,snapshot_bytes:0,holdAck:false,heldAck:null,invalidateState:false,ackCount:0,appliedPublication:null,commands:new Map(),errors:[],fatals:[],requestIds:new Set(),requestResults:new WeakSet(),requestsOutstanding:0,requestsPeak:0,delayRequestResults:0,dropAck:false,droppedAck:null,dropRetire:false,retires:[],retireReplies:[],ackPosts:0,watch:new Map()};
  const NativeWorker=globalThis.Worker;
  globalThis.Worker=class extends NativeWorker {
   constructor(...args) {
    super(...args);fixture.workers.push(String(args[0]));
    if(String(args[0]).includes("content-worker-"))this.addEventListener("message",({data})=>{fixture.codec.push({kind:data?.kind,error:data?.error,result_bytes:typeof data?.result==="string"?data.result.length:null});if(fixture.codec.length>128)fixture.codec.shift();});
    if(!String(args[0]).includes("sdk.worker-"))return;
    this.fixtureSdk=true;fixture.sdk=this;
    this.addEventListener("message",event=>{const {data}=event;
     if(data?.kind==="result"&&fixture.requestIds.delete(data.id))fixture.requestResults.add(event);
     if(data?.kind==="state"){if(fixture.invalidateState&&data.servers.length){fixture.invalidateState=false;data.servers[0].runtime_index=data.servers.length;}fixture.last=data;fixture.snapshots++;fixture.snapshot_bytes+=new TextEncoder().encode(JSON.stringify(data)).length;}
     if(data?.kind==="content_work")for(const operation of data.operations){fixture.work.push(operation);if(fixture.work.length>128)fixture.work.shift();if(operation.kind==="render"&&typeof operation.source==="string"&&operation.source.startsWith("PRESSURE_PEER_")&&fixture.watch.size<16)fixture.watch.set(operation.source,{request:operation.request,value:null,error:null});}
     if(data?.kind==="fatal")fixture.fatals.push(data.message);
     if(data?.kind==="result"&&fixture.commands.get(data.id)==="content_retire")fixture.retireReplies.push({value:data.value??null,error:data.error??null});
     if(data?.kind==="result"&&fixture.commands.has(data.id)){if(data.error)fixture.errors.push({method:fixture.commands.get(data.id),error:data.error});fixture.commands.delete(data.id);}
     if(data?.kind==="result"&&fixture.pending.has(data.id))for(const entry of fixture.watch.values())if(entry.request===fixture.pending.get(data.id)){entry.value=data.value??null;entry.error=data.error??null;}
     if(data?.kind==="result"&&fixture.pending.has(data.id)) {
      fixture.replies.push({request:fixture.pending.get(data.id),value:data.value,error:data.error});fixture.pending.delete(data.id);
      if(fixture.replies.length>128)fixture.replies.shift();
     }
    });
   }
   // Fault injection at the App boundary only: hold content_request results so
   // their RPC slots stay outstanding. The SDK has already processed them.
   set onmessage(handler) {
    if(!this.fixtureSdk||typeof handler!=="function"){super.onmessage=handler;return;}
    super.onmessage=event=>{
     if(!fixture.requestResults.delete(event))return handler.call(this,event);
     const deliver=()=>{fixture.requestsOutstanding--;handler.call(this,event);};
     if(fixture.delayRequestResults)setTimeout(deliver,fixture.delayRequestResults);else deliver();
    };
   }
   get onmessage(){return super.onmessage;}
   postMessage(data,...rest) {
    if(data?.method==="state_applied")fixture.ackCount++;
    if(data?.method==="state_applied"&&fixture.holdAck){fixture.heldAck=data.args[0];return;}
    if(data?.method==="state_applied")fixture.appliedPublication=data.args[0];
    if(this.fixtureSdk&&data?.id!==undefined){fixture.commands.set(data.id,data.method);if(fixture.commands.size>256)throw Error("fixture command bound");}
    if(data?.method==="content_complete"){fixture.ackPosts++;fixture.pending.set(data.id,data.args[0]);fixture.completions.push({request:data.args[0],bytes:data.args[1]?.length});if(fixture.completions.length>128)fixture.completions.shift();}
    if(this.fixtureSdk&&data?.method==="content_request"&&data.id!==undefined){fixture.requestIds.add(data.id);fixture.requestsPeak=Math.max(fixture.requestsPeak,++fixture.requestsOutstanding);}
    if(this.fixtureSdk&&data?.method==="content_complete"&&fixture.dropAck) {
     // A lost ACK: the SDK never receives it, and the App sees a failed RPC.
     fixture.dropAck=false;fixture.droppedAck=data.args[0];
     queueMicrotask(()=>this.dispatchEvent(new MessageEvent("message",{data:{kind:"result",id:data.id,error:"Fixture dropped this content ACK before the SDK."}})));
     return;
    }
    if(this.fixtureSdk&&data?.method==="content_retire") {
     fixture.retires.push(data.args?.[0]);
     if(fixture.dropRetire) {
      // The lane retirement notice is also lost: only SDK deadlines remain.
      fixture.dropRetire=false;
      queueMicrotask(()=>this.dispatchEvent(new MessageEvent("message",{data:{kind:"result",id:data.id,error:"Fixture dropped this lane retirement before the SDK."}})));
      return;
     }
    }
    return super.postMessage(data,...rest);
   }
  };
 });
 page.on("pageerror",error=>console.error("browser error:",error.message));
 await page.goto(`${origin}${entry}#s=${host}:${port}&u=${username}`);
 if(width<=760)await page.getByRole("button",{name:"Open navigation",exact:true}).click();
 try { await page.getByRole("button",{name:"Trust this certificate",exact:true}).waitFor({timeout:20000}); }
 catch(error) { console.error("fixture-open-failure",username,await page.locator("body").innerText());await page.screenshot({path:screenshot.replace(".png",`-${username}-open-failure.png`)});throw error; }
 if(!trust)return page;
 await page.getByRole("button",{name:"Trust this certificate",exact:true}).click();
 await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null,{timeout:20000});
 if(width<=760)await page.getByRole("button",{name:"Close navigation",exact:true}).click();
 return page;
}


async function rpc(page,method,...args) {
 const visible=await page.evaluate(()=>({servers:brokerFixture.last.servers,timeline:brokerFixture.last.timeline,voice:brokerFixture.last.voice}));
 const proof=actionProof(method,args,visible.servers,visible.timeline,visible.voice);
 return page.evaluate(({method,args,context})=>new Promise((resolve,reject)=>{
  const worker=brokerFixture.sdk,id=1_000_000+(globalThis.fixtureRpcNext=(globalThis.fixtureRpcNext||0)+1);
  const timer=setTimeout(()=>{worker.removeEventListener("message",listener);reject(new Error("fixture RPC timed out"));},5000);
  function listener({data}) {if(data?.kind!=="result"||data.id!==id)return;clearTimeout(timer);worker.removeEventListener("message",listener);if(data.error)reject(new Error(data.error));else resolve(data.value);}
  worker.addEventListener("message",listener);worker.postMessage({id,method,args,...(context===undefined?{}:{context})});
 }),{method,args,context:proof===undefined?undefined:JSON.stringify(proof)});
}
// Stock-protocol direct text (TextMessage session=2, message=5) from a fixture peer.
const wireVarint=value=>{const out=[];do{out.push((value&127)|(value>127?128:0));value=Math.floor(value/128);}while(value);return Buffer.from(out);};
function directText(peer,session,body){const text=Buffer.from(body);peer.send(11,Buffer.concat([wireVarint(2*8),wireVarint(session),wireVarint(5*8+2),wireVarint(text.length),text]));}
async function send(page,text) {
 await page.getByLabel("Message",{exact:true}).fill(text);
 await page.getByRole("button",{name:"Send",exact:true}).click();
 await page.waitForFunction(()=>document.querySelector("textarea")?.value==="");
}
const result={passed:false,artifact:root,stale:null,retired:null,expiry:null,slow:null,ack_signal:null,ack_loss:null,direct:null,recovery:null,pressure:null,reconnect:null,stress:process.argv[7]==="stress",throughput:[]};
let observer;
async function hold(page){
 await page.evaluate(()=>{brokerFixture.holdAck=true;brokerFixture.heldAck=null;});
 await rpc(page,"dismiss_notice",0);
 await page.waitForFunction(()=>brokerFixture.heldAck!==null);
 return page.evaluate(()=>brokerFixture.last.publication);
}
async function release(page){await page.evaluate(()=>{const id=brokerFixture.heldAck;brokerFixture.holdAck=false;brokerFixture.heldAck=null;brokerFixture.sdk.postMessage({method:"state_applied",args:[id]});});}
async function refusal(page,method,after){await page.waitForFunction(({method,after})=>brokerFixture.errors.slice(after).some(error=>error.method===method&&/view changed|view is stale|target changed/i.test(error.error)),{method,after});}
try {
 if(process.argv[7]==="authority"){
  observer=await legacyPeer(Number(port));
  const page=await open("broker-authority");
  await page.waitForFunction(()=>brokerFixture.last?.timeline?.channel===0&&brokerFixture.last.servers[0].channels.some(channel=>channel.id===1&&channel.instance!=="0"));
  const held=await hold(page);await rpc(page,"open",0,1);
  const draft="ORIGINAL_ROOT_DRAFT_STAYS_LOCAL";await page.getByLabel("Message",{exact:true}).fill(draft);
  let errors=await page.evaluate(()=>brokerFixture.errors.length);await page.getByRole("button",{name:"Send",exact:true}).click();await refusal(page,"send_text",errors);
  assert.equal(await page.getByLabel("Message",{exact:true}).inputValue(),draft);
  assert.equal(observer.texts.filter(message=>message.body.includes(draft)).length,0);
  assert.equal(await page.evaluate(()=>brokerFixture.last.publication),held,"no newer snapshot before ACK");
  await release(page);await page.waitForFunction(()=>brokerFixture.last.timeline.channel===1);
  await rpc(page,"open",0,0);await page.waitForFunction(()=>brokerFixture.last.timeline.channel===0);
  assert.equal(await page.getByLabel("Message",{exact:true}).inputValue(),draft,"original conversation retains its draft");
  await page.getByRole("button",{name:"Send",exact:true}).click();await page.waitForFunction(()=>document.querySelector("textarea").value==="");
  await page.locator("article").filter({hasText:draft}).waitFor();
  await hold(page);const previous=[...observer.users.values()].find(user=>user.name==="broker-authority")?.session;
  await rpc(page,"disconnect",0);await rpc(page,"connect",0);
  await observer.wait(()=>[...observer.users.values()].some(user=>user.name==="broker-authority"&&user.session!==previous),"fresh owned browser presence");
  errors=await page.evaluate(()=>brokerFixture.errors.length);await page.getByRole("button",{name:"Join voice",exact:true}).click();await refusal(page,"join",errors);
  const fresh=[...observer.users.values()].find(user=>user.name==="broker-authority");assert.equal(fresh.channel??0,0,"stale join did not move fresh presence");
  await release(page);await page.waitForFunction(()=>brokerFixture.last.servers[0].status==="connected");
  await rpc(page,"join",0,1);await observer.wait(()=>observer.users.get(fresh.session)?.channel===1,"fresh explicit join");
  await page.waitForFunction(()=>brokerFixture.last.voice?.joined);
  await rpc(page,"toggle_mute",0);await page.waitForFunction(()=>brokerFixture.last.servers[0].muted===false);
  await hold(page);await rpc(page,"toggle_mute",0);
  errors=await page.evaluate(()=>brokerFixture.errors.length);await page.getByRole("button",{name:"Mute microphone",exact:true}).click();await refusal(page,"toggle_mute",errors);
  await release(page);await page.waitForFunction(()=>brokerFixture.last.servers[0].muted===true);
  await observer.wait(()=>observer.users.get(fresh.session)?.muted===1,"stale mute must not unmute current presence");
  assert.equal(await page.evaluate(()=>captureCalls),0);
  const cert=await open("broker-certificate","",1280,false);await hold(cert);await rpc(cert,"connect",0);
  errors=await cert.evaluate(()=>brokerFixture.errors.length);
  await cert.getByRole("button",{name:"Trust this certificate",exact:true}).click();await refusal(cert,"accept_certificate",errors);
  await release(cert);await cert.waitForFunction(()=>brokerFixture.last.servers[0].certificate&&brokerFixture.last.servers[0].pin===null);
  await cert.getByRole("button",{name:"Trust this certificate",exact:true}).click();await cert.waitForFunction(()=>brokerFixture.last.servers[0].status==="connected");
  result.authority={send_draft_preserved:true,wrong_channel_received:false,reconnected_join_refused:true,stale_toggle_cannot_unmute:true,stale_certificate_refused:true,capture_calls:await page.evaluate(()=>captureCalls)};
  await page.screenshot({path:screenshot.replace(".png","-authority.png")});
  // Server allows unlimited text; this is below its bounded control frame cap,
  // but intentionally exceeds the browser's independent local preview bound.
  await rpc(page,"open",0,1);await page.waitForFunction(()=>brokerFixture.last.timeline.channel===1);
  const tooLarge="<p>"+"x".repeat(256*1024)+"</p>",beforeRows=await page.evaluate(()=>brokerFixture.last.timeline.messages.length);
  for(let index=0;index<4;index++)observer.message(1,tooLarge);
  const notice="Some incoming messages exceeded the local display limit and were not shown.";
  // Capture the exact applied SDK publication and its rendered count in ONE
  // browser task. A locator becoming visible is not a publication barrier: a
  // later snapshot can replace it before a separate count() RPC runs.
  const noticeBarrier=await page.waitForFunction(notice=>{
   const state=brokerFixture.last;
   if(state?.servers[0]?.notice!==notice||brokerFixture.appliedPublication!==state.publication)return null;
   const count=[...document.querySelectorAll(".notice > span")].filter(node=>node.textContent===notice).length;
   return count===1?{publication:state.publication,applied:brokerFixture.appliedPublication,notice:state.servers[0].notice,count}:null;
  },notice,{timeout:20000}).then(handle=>handle.jsonValue());
  assert.equal(noticeBarrier.count,1);
  assert.equal(noticeBarrier.notice,notice);
  assert.equal(noticeBarrier.applied,noticeBarrier.publication);
  assert.equal(await page.evaluate(()=>brokerFixture.work.filter(work=>work.source?.length>256*1024).length),0);
  assert.equal(await page.evaluate(()=>brokerFixture.last.timeline.messages.length),beforeRows,"oversized wire body is never admitted as a chat row");
  observer.message(1,"HEALTHY_AFTER_OVERSIZE");await page.locator("article").filter({hasText:"HEALTHY_AFTER_OVERSIZE"}).waitFor();
  await rpc(page,"open",0,0);await page.waitForFunction(()=>brokerFixture.last.timeline.channel===0);await rpc(page,"open",0,1);await page.waitForFunction(()=>brokerFixture.last.timeline.channel===1);
  assert.equal(await page.evaluate(()=>brokerFixture.work.filter(work=>work.source?.length>256*1024).length),0);
  result.permanent={wire_refusal_notice:true,notice_publication_barrier:noticeBarrier,oversized_bytes:Buffer.byteLength(tooLarge),worker_admissions:0,oversized_rows:0,healthy_following_message:true,broker_terminal_scope:"unit tests; normal wire/history paths enforce the source cap before broker admission"};
  const stalled=await open("broker-no-ack");await hold(stalled);const snapshots=await stalled.evaluate(()=>brokerFixture.snapshots);
  for(let index=0;index<8;index++)await rpc(stalled,"dismiss_notice",0);
  const completions=await stalled.evaluate(()=>brokerFixture.completions.length);
  await rpc(stalled,"send_text","HELD_STATE_CODEC_PROGRESS");
  await stalled.waitForFunction(before=>brokerFixture.work.some(work=>work.source==="HELD_STATE_CODEC_PROGRESS")&&brokerFixture.completions.length>before,completions);
  assert.equal(await stalled.evaluate(()=>brokerFixture.snapshots),snapshots);
  await stalled.getByRole("button",{name:"Reload client",exact:true}).waitFor({timeout:20000});
  assert.equal(await stalled.evaluate(()=>brokerFixture.workers.filter(url=>url.includes("sdk.worker-")).length),1);
  result.no_ack={visible_terminal:true,no_snapshot_backlog:true,codec_progress_while_state_held:true,worker_replacements:0};
  const badApply=await open("broker-apply-failure");const acks=await badApply.evaluate(()=>{brokerFixture.invalidateState=true;return brokerFixture.ackCount;});
  await rpc(badApply,"dismiss_notice",0).catch(()=>{});
  await badApply.getByRole("button",{name:"Reload client",exact:true}).waitFor();
  assert.equal(await badApply.evaluate(()=>brokerFixture.ackCount),acks,"failed App state application cannot ACK credit");
  assert.equal(await badApply.evaluate(()=>brokerFixture.workers.filter(url=>url.includes("sdk.worker-")).length),1);
  result.apply_failure={visible_terminal:true,no_failed_state_ack:true,worker_replacements:0};
 }else{
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
 // Posted jobs run one at a time; only the job holding the codec consumes its
 // progress budget. Followers start theirs after its real ACK and render.
 await expiry.waitForFunction(count=>document.querySelectorAll("article").length===7&&[...document.querySelectorAll("article button")].filter(node=>node.textContent==="Retry message preview").length===count,1,{timeout:10000});
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

 const stopped="[Message previews stopped because a preview job was not acknowledged. Reload this page to prepare previews again.]";
 // A stalled worker misses its execution deadline and the service retires. The
 // job that actually settled is ACKed and shows reload guidance; the lane then
 // retires once through the bounded notice, so waiting rows turn terminal.
 const failed=await open("broker-retired","stall");
 await send(failed,"retired-source-first");await send(failed,"retired-source-second");
 await failed.waitForFunction(text=>[...document.querySelectorAll("article")].filter(node=>[...node.querySelectorAll("small")].some(small=>small.textContent==="Preview unavailable. Reload to try again.")||node.textContent.includes(text)).length>=2,stopped,{timeout:20000});
 assert.equal(await failed.getByRole("button",{name:"Retry message preview",exact:true}).count(),0);
 const before=await failed.evaluate(()=>brokerFixture.workers.filter(url=>url.includes("content-worker-")).length);
 await send(failed,"retired-source-third");
 await failed.waitForFunction(()=>document.querySelectorAll("article").length>=3);
 await failed.waitForFunction(text=>[...document.querySelectorAll("article")].filter(node=>[...node.querySelectorAll("small")].some(small=>small.textContent==="Preview unavailable. Reload to try again.")||node.textContent.includes(text)).length>=3,stopped,{timeout:20000});
 result.retired=await failed.evaluate(text=>({content_workers:brokerFixture.workers.filter(url=>url.includes("content-worker-")).length,capture_calls:captureCalls,articles:document.querySelectorAll("article").length,unavailable_rows:[...document.querySelectorAll("small")].filter(node=>node.textContent==="Preview unavailable. Reload to try again.").length,terminal_rows:[...document.querySelectorAll("article")].filter(node=>node.textContent.includes(text)).length,retires:brokerFixture.retires,third_render_work:brokerFixture.work.filter(operation=>operation.kind==="render"&&operation.source==="retired-source-third").length}),stopped);
 assert.equal(before,1);assert.equal(result.retired.content_workers,1);assert.equal(result.retired.capture_calls,0);
 assert.ok(result.retired.unavailable_rows>=1,"the settled job shows reload guidance");
 assert.deepEqual(result.retired.retires,["undelivered"],"the service retirement reaches the SDK once");
 assert.equal(result.retired.third_render_work,0,"a retired lane posts no further codec work");
 assert.equal(await failed.locator("article").filter({hasText:"retired-source-third"}).count(),0,"retired codec never triggers privileged rendering fallback");
 await failed.screenshot({path:screenshot.replace(".png","-retired.png")});

 // Six renders on a slow serial worker (2.5s each, ~15s in total). Each job's
 // worker and SDK budgets start when it actually runs; queue wait never retires.
 const slow=await open("broker-slow","slow");
 const slowSources=Array.from({length:6},(_,index)=>`slow-source-canary-${index}`);
 const slowStart=await slow.evaluate(()=>performance.now());
 for(const source of slowSources)await send(slow,source);
 try { await slow.waitForFunction(sources=>sources.every(source=>(brokerFixture.last?.timeline?.messages??[]).some(message=>message.content_token&&!message.content_failed&&message.document.blocks.some(block=>block.kind==="text"&&block.text.includes(source)))),slowSources,{timeout:45000}); }
 catch(error) {
  const diagnostic=await slow.evaluate(()=>({messages:brokerFixture.last?.timeline?.messages.map(({serial,content_token,content_failed,document})=>({serial,content_token,content_failed,text:document.blocks.map(block=>block.text).join(" ").slice(0,120)})),unavailable:[...document.querySelectorAll("small")].map(node=>node.textContent),work:brokerFixture.work.slice(-12),codec:brokerFixture.codec.slice(-12)}));
  console.error("slow-diagnostic",JSON.stringify(diagnostic));result.slow_failure=diagnostic;await slow.screenshot({path:screenshot.replace(".png","-slow-failure.png")});throw error;
 }
 result.slow=await slow.evaluate(start=>({elapsed_ms:performance.now()-start,result_delay_ms:2500,renders:brokerFixture.codec.filter(entry=>entry.kind==="result").length,unavailable:[...document.querySelectorAll("small")].filter(node=>node.textContent.includes("Preview unavailable")).length,retries:[...document.querySelectorAll("article button")].filter(node=>node.textContent==="Retry message preview").length,content_workers:brokerFixture.workers.filter(url=>url.includes("content-worker-")).length,capture_calls:captureCalls}),slowStart);
 assert.equal(result.slow.unavailable,0,"slow healthy jobs never retire the codec");assert.equal(result.slow.retries,0);
 assert.ok(result.slow.renders>=6&&result.slow.elapsed_ms>=6*2500);assert.equal(result.slow.content_workers,1);assert.equal(result.slow.capture_calls,0);
 await slow.screenshot({path:screenshot.replace(".png","-slow.png")});

 // A content ACK the bridge cannot deliver: it keeps that credit and tells the
 // SDK explicitly, so waiting and new rows turn terminal without a clock shift.
 const signal=await open("broker-ack-signal");
 await send(signal,"ack-signal-before");
 await signal.waitForFunction(()=>(brokerFixture.last?.timeline?.messages??[]).some(message=>message.content_token&&!message.content_failed));
 await signal.evaluate(()=>{brokerFixture.dropAck=true;});
 await send(signal,"ack-signal-dropped");
 await signal.waitForFunction(()=>brokerFixture.droppedAck!==null&&brokerFixture.retireReplies.length>0,null,{timeout:10000});
 await signal.locator("article").filter({hasText:stopped}).first().waitFor({timeout:10000});
 await send(signal,"ack-signal-after");
 await signal.waitForFunction(text=>[...document.querySelectorAll("article")].filter(node=>node.textContent.includes(text)).length>=2,stopped,{timeout:10000});
 result.ack_signal=await signal.evaluate(()=>({dropped:brokerFixture.droppedAck,retires:brokerFixture.retires,retire_replies:brokerFixture.retireReplies,dropped_cancelled:brokerFixture.work.some(operation=>operation.kind==="cancel"&&operation.request===brokerFixture.droppedAck),after_render_work:brokerFixture.work.filter(operation=>operation.kind==="render"&&operation.source==="ack-signal-after").length,reload_client:[...document.querySelectorAll("button")].some(node=>node.textContent==="Reload client"),content_workers:brokerFixture.workers.filter(url=>url.includes("content-worker-")).length,sdk_workers:brokerFixture.workers.filter(url=>url.includes("sdk.worker-")).length,capture_calls:captureCalls}));
 assert.deepEqual(result.ack_signal.retires,["undelivered"]);assert.deepEqual(result.ack_signal.retire_replies,[{value:true,error:null}]);
 assert.equal(result.ack_signal.dropped_cancelled,true,"the undelivered job is cancelled, its credit held");assert.equal(result.ack_signal.after_render_work,0);
 assert.equal(result.ack_signal.reload_client,false);assert.equal(result.ack_signal.content_workers,1);assert.equal(result.ack_signal.sdk_workers,1);assert.equal(result.ack_signal.capture_calls,0);
 await signal.screenshot({path:screenshot.replace(".png","-ack-signal.png")});

 // Both the content ACK and the retirement notice are lost before the SDK:
 // credit stays held and the SDK's deadline backstop retires the lane. Only the
 // SDK deadline clock moves (past the 12s progress budget, then past the 15s
 // cancelled-ACK bound); no ACK is ever delivered for it.
 const lost=await open("broker-ack-loss");
 await send(lost,"ack-loss-before");
 await lost.waitForFunction(()=>(brokerFixture.last?.timeline?.messages??[]).some(message=>message.content_token&&!message.content_failed));
 await lost.evaluate(()=>{brokerFixture.dropAck=true;brokerFixture.dropRetire=true;});
 await send(lost,"ack-loss-dropped");
 await lost.waitForFunction(()=>brokerFixture.droppedAck!==null);
 const lostSdk=lost.workers().find(worker=>worker.url().includes("sdk.worker-"));assert.ok(lostSdk);
 await lostSdk.evaluate(()=>{const originalNow=performance.now.bind(performance);globalThis.fixtureShift=0;performance.now=()=>originalNow()+globalThis.fixtureShift;});
 await lostSdk.evaluate(()=>{globalThis.fixtureShift=13000;});await rpc(lost,"dismiss_notice",0);
 await lost.waitForFunction(()=>brokerFixture.work.some(operation=>operation.kind==="cancel"&&operation.request===brokerFixture.droppedAck));
 await lost.waitForFunction(()=>[...document.querySelectorAll("small")].some(node=>node.textContent==="Preview unavailable. Reload to try again."));
 await lostSdk.evaluate(()=>{globalThis.fixtureShift=29000;});await rpc(lost,"dismiss_notice",0);
 await send(lost,"ack-loss-after");
 await lost.locator("article").filter({hasText:stopped}).first().waitFor({timeout:10000});
 result.ack_loss=await lost.evaluate(()=>({dropped:brokerFixture.droppedAck,retires:brokerFixture.retires,retire_replies:brokerFixture.retireReplies,after_render_work:brokerFixture.work.filter(operation=>operation.kind==="render"&&operation.source==="ack-loss-after").length,errors:brokerFixture.errors.filter(error=>error.method==="content_complete").length,reload_client:[...document.querySelectorAll("button")].some(node=>node.textContent==="Reload client"),content_workers:brokerFixture.workers.filter(url=>url.includes("content-worker-")).length,sdk_workers:brokerFixture.workers.filter(url=>url.includes("sdk.worker-")).length,capture_calls:captureCalls}));
 assert.equal(result.ack_loss.after_render_work,0,"a retired lane posts no further codec work");
 assert.equal(result.ack_loss.errors,1);assert.equal(result.ack_loss.reload_client,false,"chat stays usable");
 assert.deepEqual(result.ack_loss.retires,["undelivered"]);assert.equal(result.ack_loss.retire_replies.length,1);assert.notEqual(result.ack_loss.retire_replies[0].error,null);
 assert.equal(result.ack_loss.content_workers,1);assert.equal(result.ack_loss.sdk_workers,1);assert.equal(result.ack_loss.capture_calls,0);
 await lost.screenshot({path:screenshot.replace(".png","-ack-loss.png")});

 // A selected direct conversation keeps its exact verified derivative across a
 // reconnect: connect changes the generation outside a poll, and a direct
 // conversation's instance never changes.
 const direct=await open("broker-dm");
 const dmPeer=await legacyPeer(Number(port),"broker-dm-peer");
 try {
  await dmPeer.wait(()=>[...dmPeer.users.values()].some(user=>user.name==="broker-dm"),"direct peer sees the browser user");
  directText(dmPeer,[...dmPeer.users.values()].find(user=>user.name==="broker-dm").session,"dm-reconnect-canary");
  await direct.waitForFunction(()=>(brokerFixture.last?.servers[0].channels??[]).some(channel=>channel.name.includes("broker-dm-peer")),null,{timeout:20000});
  const conversation=await direct.evaluate(()=>brokerFixture.last.servers[0].channels.find(channel=>channel.name.includes("broker-dm-peer")).id);
  await rpc(direct,"open",0,conversation);
  // Read the rendered row atomically from one settled snapshot.
  const before=await (await direct.waitForFunction(id=>{const timeline=brokerFixture.last?.timeline;if(timeline?.channel!==id||timeline.loading)return null;const message=timeline.messages.find(message=>message.content_token&&!message.content_failed&&message.document.blocks.some(block=>block.kind==="text"&&block.text.includes("dm-reconnect-canary")));return message?{channel:timeline.channel,instance:timeline.instance,serial:message.serial,token:message.content_token}:null;},conversation,{timeout:20000})).jsonValue();
  await rpc(direct,"disconnect",0);await rpc(direct,"connect",0);
  await direct.waitForFunction(()=>brokerFixture.last?.servers[0].status==="connected",null,{timeout:20000});
  try { await direct.waitForFunction(before=>brokerFixture.last?.timeline?.channel===before.channel&&brokerFixture.last.timeline.messages.find(message=>message.serial===before.serial)?.content_token===before.token,before,{timeout:10000}); }
  catch(error) {
   const diagnostic=await direct.evaluate(before=>({before,channel:brokerFixture.last?.timeline?.channel,instance:brokerFixture.last?.timeline?.instance,row:brokerFixture.last?.timeline?.messages.find(message=>message.serial===before.serial),retries:[...document.querySelectorAll("article button")].filter(node=>node.textContent==="Retry message preview").length}),before);
   console.error("direct-diagnostic",JSON.stringify(diagnostic));result.direct_failure=diagnostic;await direct.screenshot({path:screenshot.replace(".png","-direct-failure.png")});throw error;
  }
  result.direct=await direct.evaluate(before=>({channel:before.channel,instance_before:before.instance,instance_after:brokerFixture.last.timeline.instance,token:before.token,token_kept:brokerFixture.last.timeline.messages.find(message=>message.serial===before.serial).content_token===before.token,unavailable:[...document.querySelectorAll("small")].filter(node=>node.textContent.includes("Preview unavailable")).length,retries:[...document.querySelectorAll("article button")].filter(node=>node.textContent==="Retry message preview").length,content_workers:brokerFixture.workers.filter(url=>url.includes("content-worker-")).length,capture_calls:captureCalls}),before);
  assert.equal(result.direct.token_kept,true);assert.equal(result.direct.instance_after,result.direct.instance_before,"a direct conversation keeps its instance");
  assert.equal(result.direct.retries,0);assert.equal(result.direct.unavailable,0,"no preview-unavailable guidance either");assert.equal(result.direct.content_workers,1);assert.equal(result.direct.capture_calls,0);
  await direct.screenshot({path:screenshot.replace(".png","-direct.png")});
 } finally { dmPeer.socket.destroy(); }
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
  // Switching back to a busy conversation remounts ~900 rows. Hold every
  // rehydration result 2s at the App boundary while real codec work for peer
  // direct messages completes: those RPCs stay few, and every ACK reaches the SDK.
  observer=await legacyPeer(Number(port),"broker-pressure-peer");
  await observer.wait(()=>[...observer.users.values()].some(user=>user.name==="broker-recovery"),"pressure peer sees the browser user");
  const browserSession=[...observer.users.values()].find(user=>user.name==="broker-recovery").session;
  const peerSources=Array.from({length:4},(_,index)=>`PRESSURE_PEER_${index}`);
  const pressureErrors=await recovery.evaluate(()=>{brokerFixture.delayRequestResults=2000;brokerFixture.requestsPeak=brokerFixture.requestsOutstanding;return brokerFixture.errors.length;});
  const pressureStart=await recovery.evaluate(()=>({at:performance.now(),acks:brokerFixture.ackPosts}));
  await rpc(recovery,"open",0,0);
  await recovery.waitForFunction(()=>brokerFixture.last?.timeline?.channel===0&&!brokerFixture.last.timeline.loading&&brokerFixture.last.timeline.messages.length>=900);
  await recovery.waitForFunction(()=>brokerFixture.requestsOutstanding>0);
  const outstandingAtSend=await recovery.evaluate(()=>brokerFixture.requestsOutstanding);
  // Spaced below the stock server's default text flood limit.
  for(const source of peerSources){directText(observer,browserSession,source);await new Promise(resolve=>setTimeout(resolve,1100));}
  try {
   await recovery.waitForFunction(()=>brokerFixture.last.timeline.messages.every(message=>!message.content_failed&&!message.document.blocks.some(block=>block.kind==="text"&&block.text.startsWith("["))),null,{timeout:60000});
   await recovery.waitForFunction(sources=>sources.every(source=>{const entry=brokerFixture.watch.get(source);return entry&&entry.error===null&&typeof entry.value==="string"&&entry.value!=="null";}),peerSources,{timeout:30000});
  }
  catch(error) {
   const diagnostic=await recovery.evaluate(()=>({count:brokerFixture.last?.timeline?.messages.length,missing:brokerFixture.last?.timeline?.messages.filter(message=>!message.content_token).length,peak:brokerFixture.requestsPeak,errors:brokerFixture.errors.slice(-6),watch:[...brokerFixture.watch].map(([source,entry])=>({source,request:entry.request,error:entry.error,accepted:typeof entry.value==="string"&&entry.value!=="null"})),servers:brokerFixture.last?.servers[0].channels.filter(channel=>channel.direct).map(channel=>({id:channel.id,name:channel.name})),work:brokerFixture.work.slice(-10).map(({kind,request,source})=>({kind,request,source:source?.slice(0,40)}))}));
   console.error("pressure-diagnostic",JSON.stringify(diagnostic));result.pressure_failure=diagnostic;throw error;
  }
  result.pressure=await recovery.evaluate(({errors,start})=>({milliseconds:performance.now()-start.at,messages:brokerFixture.last.timeline.messages.length,delayed_request_ms:brokerFixture.delayRequestResults,peak_request_rpcs:brokerFixture.requestsPeak,acks_sent:brokerFixture.ackPosts-start.acks,ack_errors:brokerFixture.errors.slice(errors).filter(error=>error.method==="content_complete").length,request_errors:brokerFixture.errors.slice(errors).filter(error=>error.method==="content_request").length}),{errors:pressureErrors,start:pressureStart});
  result.pressure.peer_direct_renders_accepted=peerSources.length;result.pressure.request_rpcs_outstanding_at_send=outstandingAtSend;
  await recovery.evaluate(()=>{brokerFixture.delayRequestResults=0;});
  assert.ok(result.pressure.peak_request_rpcs<=8,`rehydration RPCs in flight ${result.pressure.peak_request_rpcs}`);
  assert.equal(result.pressure.ack_errors,0);assert.ok(result.pressure.acks_sent>=peerSources.length,"codec ACKs completed during held rehydration");
  assert.ok(outstandingAtSend>0,"held rehydration RPCs were outstanding when the peer texts arrived");
  // A verified rendered row keeps its exact derivative across a reconnect
  // while inactive, and (below) across a reconnect of the visible conversation.
  const derivative=await recovery.evaluate(serial=>brokerFixture.last.timeline.messages.find(message=>message.serial===serial)?.content_token??null,original.serial);
  assert.ok(derivative,"canary has a derivative before reconnect");
  await rpc(recovery,"open",0,4);await recovery.waitForFunction(()=>brokerFixture.last?.timeline?.channel===4);
  // Inactive row survives another real connection/lifetime change; then an
  // actual user reopen rebinds its verified derivative, without IDB success.
  await rpc(recovery,"disconnect",0);await rpc(recovery,"connect",0);
  await recovery.waitForFunction(()=>brokerFixture.last?.servers[0].status==="connected",null,{timeout:20000});
  await rpc(recovery,"open",0,0);
  await recovery.waitForFunction(serial=>brokerFixture.last?.timeline?.channel===0&&brokerFixture.last.timeline.messages.some(message=>message.serial===serial),original.serial);
  const restored=recovery.locator("article").filter({hasText:canary});
  try {
   await recovery.waitForFunction(({serial,token})=>brokerFixture.last.timeline.messages.find(message=>message.serial===serial)?.content_token===token,{serial:original.serial,token:derivative},{timeout:10000});
   await restored.waitFor({timeout:30000});
  }
  catch(error) {
   const diagnostic=await recovery.evaluate(serial=>({canary:brokerFixture.last?.timeline?.messages.find(message=>message.serial===serial),count:brokerFixture.last?.timeline?.messages.length,unavailable:[...document.querySelectorAll("small")].filter(node=>node.textContent.includes("Preview unavailable")).length,retries:[...document.querySelectorAll("article button")].filter(node=>node.textContent==="Retry message preview").length,canary_work:brokerFixture.work.filter(operation=>operation.source?.includes("UNSAVED_RECOVERY_CANARY")),recent_replies:brokerFixture.replies.slice(-5),completions:brokerFixture.completions.slice(-8),codec:brokerFixture.codec.slice(-8)}),original.serial);
   console.error("canary-recovery-diagnostic",JSON.stringify(diagnostic));result.recovery_failure=diagnostic;await recovery.screenshot({path:screenshot.replace(".png","-recovery-failure.png")});throw error;
  }
  const after=await recovery.evaluate(serial=>{
   const message=brokerFixture.last.timeline.messages.find(message=>message.serial===serial);
   return {serial:message.serial,status:message.status,content_token:message.content_token,content_failed:message.content_failed,count:brokerFixture.last.timeline.messages.length,workers:brokerFixture.workers.filter(url=>url.includes("content-worker-")).length,capture_calls:captureCalls};
  },original.serial);
  assert.equal(after.serial,original.serial);assert.equal(after.status,original.status);assert.equal(after.count,901);assert.equal(after.workers,1);assert.equal(after.capture_calls,0);
  assert.equal(after.content_token,derivative,"the same verified derivative, no re-render or retry");assert.equal(after.content_failed,false);
  result.reconnect={inactive_reconnect_token_kept:true,token:derivative};
  // Literal tags prove retained Plain mode, rather than guessed legacy HTML.
  assert.equal(await restored.locator("em").count(),0);
  const dbCount=await recovery.evaluate(()=>new Promise((resolve,reject)=>{const request=indexedDB.open("nuschl-observed-history-v1");request.onerror=()=>reject(request.error);request.onsuccess=()=>{const db=request.result,tx=db.transaction("messages"),count=tx.objectStore("messages").count();count.onsuccess=()=>resolve(count.result);tx.oncomplete=()=>db.close();};}));
  assert.equal(dbCount,4500,"unsaved live canary was never silently saved/replaced by fixture history");
  result.recovery={seeded,serial_preserved:true,unsaved_status_preserved:true,original_plain_mode_preserved:true,second_reconnect_inactive:true,articles:after.count,workers:after.workers,db_count:dbCount};
  // The visible conversation reconnects: its newest history page reloads, and
  // the verified unsaved row keeps the same derivative. Wait for an APPLIED
  // publication from the new lifetime in which that reloaded page has been
  // processed (the timeline no longer holds the 901 resident rows, nothing is
  // loading, and every reloaded row is readable), not merely for the canary.
  const selectedBefore=await recovery.evaluate(()=>({instance:brokerFixture.last.timeline.instance,messages:brokerFixture.last.timeline.messages.length}));
  await rpc(recovery,"disconnect",0);await rpc(recovery,"connect",0);
  await recovery.waitForFunction(()=>brokerFixture.last?.servers[0].status==="connected",null,{timeout:20000});
  try {
   result.reconnect.selected_reload=await recovery.waitForFunction(({serial,token,before})=>{
    const state=brokerFixture.last,timeline=state?.timeline;
    if(state?.servers[0].status!=="connected"||timeline?.channel!==0||timeline.loading||timeline.instance===before.instance||brokerFixture.appliedPublication!==state.publication)return null;
    if(timeline.messages.length===before.messages)return null;
    if(!timeline.messages.every(message=>!message.content_failed&&!message.document.blocks.some(block=>block.kind==="text"&&block.text.startsWith("["))))return null;
    return timeline.messages.find(message=>message.serial===serial)?.content_token===token?{publication:state.publication,instance:timeline.instance,messages:timeline.messages.length}:null;
   },{serial:original.serial,token:derivative,before:selectedBefore},{timeout:30000}).then(handle=>handle.jsonValue());
  } catch(error) {
   const diagnostic=await recovery.evaluate(({serial,before})=>({before,status:brokerFixture.last?.servers[0].status,channel:brokerFixture.last?.timeline?.channel,instance:brokerFixture.last?.timeline?.instance,loading:brokerFixture.last?.timeline?.loading,messages:brokerFixture.last?.timeline?.messages.length,applied:brokerFixture.appliedPublication,publication:brokerFixture.last?.publication,canary:brokerFixture.last?.timeline?.messages.find(message=>message.serial===serial)?.content_token,unreadable:brokerFixture.last?.timeline?.messages.filter(message=>message.content_failed||message.document.blocks.some(block=>block.kind==="text"&&block.text.startsWith("["))).length}),{serial:original.serial,before:selectedBefore});
   console.error("selected-reload-diagnostic",JSON.stringify(diagnostic));result.selected_reload_failure=diagnostic;throw error;
  }
  result.reconnect.selected_reconnect_token_kept=true;
  await recovery.screenshot({path:screenshot.replace(".png","-recovery.png")});
 }
 }
  assert.deepEqual(forbidden,[]);result.passed=true;result.forbidden=forbidden;
 console.log("Actual broker reconnect/source lease and permanent content-worker retirement passed.");
} finally {
 const {writeFile}=await import("node:fs/promises");await writeFile(screenshot.replace(".png",".json"),JSON.stringify(result,null,2));
 observer?.socket.destroy();
 for(const context of contexts)await context.close();await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(resolve=>server.close(resolve));
}
