// SPDX-License-Identifier: EUPL-1.2
// Actual packaged SDK startup failure/first-ready recovery. Requires a separately
// confined Chromium wrapper with synthetic microphone and no host sound devices.
import assert from "node:assert/strict";
import {createServer} from "node:http";
import {createConnection} from "node:net";
import {readFile,mkdir,writeFile} from "node:fs/promises";
import {resolve,extname,sep} from "node:path";
import {pathToFileURL} from "node:url";
const [site,relayUrl,port,wrapper,output] = process.argv.slice(2);
assert.ok(site&&relayUrl&&port&&wrapper&&output);
const relay=new URL(relayUrl);
assert.equal(relay.hostname,"127.0.0.1");assert.equal(relay.protocol,"ws:");
assert.ok(Number(port)>1023&&Number(port)<65536);
const directory=resolve(site),out=resolve(output);await mkdir(out,{recursive:true});
let csp="default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const types={".html":"text/html",".js":"text/javascript",".css":"text/css",".wasm":"application/wasm",".svg":"image/svg+xml"};
const server=createServer(async(req,res)=>{
  try {
    const pathname=decodeURIComponent(new URL(req.url,"http://127.0.0.1").pathname);
    const mounted=pathname.startsWith("/app/")?pathname.slice(4):pathname;
    const path=resolve(directory,"."+mounted);
    assert.ok(path===directory||path.startsWith(directory+sep));
    const target=extname(mounted)?path:resolve(path,"index.html");
    let body=mounted==="/seed.html"?Buffer.from("<!doctype html><meta charset=utf-8><title>Owned cache seed</title>"):await readFile(target);
    res.writeHead(200,{"Content-Type":types[extname(target)]||"application/octet-stream","Content-Security-Policy":csp,"Permissions-Policy":"camera=(), geolocation=(), microphone=(self)","Cross-Origin-Opener-Policy":"same-origin","Referrer-Policy":"no-referrer","X-Content-Type-Options":"nosniff"});res.end(body);
  } catch {res.writeHead(404);res.end();}
});
const tunnels=new Set();
server.on("upgrade",(req,socket,head)=>{
  if(req.url!=="/relay"){socket.destroy();return;}
  const upstream=createConnection({host:"127.0.0.1",port:Number(relay.port)},()=>{
    let headers=`GET ${relay.pathname} HTTP/1.1\r\n`;
    for(let i=0;i<req.rawHeaders.length;i+=2)headers+=`${req.rawHeaders[i]}: ${req.rawHeaders[i+1]}\r\n`;
    upstream.write(headers+"\r\n");if(head.length)upstream.write(head);socket.pipe(upstream).pipe(socket);
  });
  tunnels.add(socket);tunnels.add(upstream);
  socket.on("close",()=>{tunnels.delete(socket);upstream.destroy();});upstream.on("close",()=>{tunnels.delete(upstream);socket.destroy();});
  socket.on("error",()=>upstream.destroy());upstream.on("error",()=>socket.destroy());
});
await new Promise(done=>server.listen(0,"127.0.0.1",done));
const origin=`http://127.0.0.1:${server.address().port}`;
csp=csp.replace("connect-src 'self'",`connect-src 'self' ${origin.replace("http:","ws:")}`);
const {chromium}=await import(pathToFileURL(process.env.NUSCHL_PLAYWRIGHT||"/nix/store/phc5gsqnm205r6hvlay77f7ia3w4bzzk-playwright-core-1.64.1/index.mjs").href);
const browser=await chromium.launch({headless:true,executablePath:wrapper});
const contexts=[],errors=[],remote=[],workerEvents=[];

async function snapshotCache(page) {
 return page.evaluate(async()=>{
  const db=await new Promise((resolve,reject)=>{const q=indexedDB.open("nuschl-observed-history-v1",1);q.onsuccess=()=>resolve(q.result);q.onerror=reject;});
  try{const tx=db.transaction(["settings","messages"],"readonly");const read=q=>new Promise((resolve,reject)=>{q.onsuccess=()=>resolve(q.result);q.onerror=reject;});
   const bookmarks=read(tx.objectStore("settings").get("bookmarks")),messages=read(tx.objectStore("messages").getAll());return {bookmarks:await bookmarks,messages:await messages};
  }finally{db.close();}
 });
}
async function openCase(mode) {
 const context=await browser.newContext({viewport:{width:mode==="constructor"?320:1000,height:800}});contexts.push(context);
 const held=[];if(mode==="blocked")await context.route(/nuschl_client_wasm_bg[^/]*\.wasm(?:\?|$)/,route=>held.push(route));
 const page=await context.newPage();page.on("pageerror",error=>errors.push(error.message));
 page.on("request",request=>{if(!request.url().startsWith(origin+"/"))remote.push(request.url());});
 await page.addInitScript(({mode})=>{
  const proof=window.__startupProof={attempted:0,created:0,initAttempts:0,initForwarded:0,commands:[],terminated:0,microphones:0,ready:0,violations:[]};
  document.addEventListener("securitypolicyviolation",event=>proof.violations.push(event.violatedDirective));
  const NativeWorker=window.Worker;
  window.Worker=class extends NativeWorker {
   constructor(url,options){const sdk=String(url).includes("sdk.worker");if(sdk){proof.attempted++;if(mode==="constructor")throw new DOMException("Fixture worker construction failure","SecurityError");}
    super(url,options);this.ownedSdk=sdk;if(sdk){proof.created++;proof.sdk=this;this.addEventListener("message",({data})=>{if(data.kind==="ready")proof.ready++;});}}
   postMessage(data,...rest){if(this.ownedSdk){if(data.method==="init"){proof.initAttempts++;if(mode==="post")throw new DOMException("Fixture init clone failure","DataCloneError");proof.initForwarded++;}
    else if(data.method)proof.commands.push(data.method);}return super.postMessage(data,...rest);}
   terminate(){if(this.ownedSdk){proof.terminated++;proof.retiredAt=performance.now();}return super.terminate();}
  };
  const getUserMedia=navigator.mediaDevices?.getUserMedia?.bind(navigator.mediaDevices);
  if(getUserMedia)navigator.mediaDevices.getUserMedia=constraints=>{proof.microphones++;return getUserMedia(constraints);};
 },{mode});
 await page.goto(origin+"/seed.html");
 await page.evaluate(async({mode,port})=>{
  const db=await new Promise((resolve,reject)=>{const q=indexedDB.open("nuschl-observed-history-v1",1);q.onupgradeneeded=()=>{const rows=q.result.createObjectStore("messages",{keyPath:"id",autoIncrement:true});rows.createIndex("scope_time",["message.server","message.channel","message.observed_at_ms","id"],{unique:true});q.result.createObjectStore("settings");};q.onsuccess=()=>resolve(q.result);q.onerror=reject;});
  try{const tx=db.transaction(["settings","messages"],"readwrite");tx.objectStore("settings").put(JSON.stringify(mode==="constructor"?[]:[{id:"startup-cache",name:"Offline startup cache",host:"127.0.0.1",port:Number(port),username:"fixture",certificate_sha256:"a".repeat(64),cached_channels:[],tokens:[],auto_connect:false,icon_checked_ms:0,last_channel:null,managed:false}]),"bookmarks");
   tx.objectStore("messages").add({message:{server:"b".repeat(64),channel:"0",author:"Fixture",observed_at_ms:1,body:"Saved before startup"},outgoing:false});await new Promise((resolve,reject)=>{tx.oncomplete=resolve;tx.onerror=reject;});
  }finally{db.close();}
 },{mode,port});
 const saved=await snapshotCache(page);
 const hash=mode==="ready"?`#s=127.0.0.1:${port}&u=startup-ready-private`:"";
 await page.goto(origin+"/app/"+hash);
 return {page,context,held,saved};
}
const cases=[];
try {
 for(const mode of ["constructor","post","blocked"]) {
  const {page,held,saved}=await openCase(mode);
  let draft="";
  if(mode==="blocked") {
   await page.waitForFunction(()=>__startupProof.initForwarded===1);
   draft="Pending server setup survives startup failure";
   await page.getByRole("button",{name:"Add a server",exact:true}).click();
   await page.getByLabel("Display name",{exact:false}).fill(draft);
   await page.waitForFunction(()=>document.querySelector('[role="alert"]')?.textContent.includes("finish starting"),null,{timeout:35000});
   assert.equal(await page.getByLabel("Display name",{exact:false}).inputValue(),draft);
   await page.getByRole("button",{name:"Close server form",exact:true}).click();
  }
  const reload=page.getByRole("button",{name:"Reload client",exact:true});await reload.waitFor({timeout:35000});
  const proof=await page.evaluate(()=>({attempted:__startupProof.attempted,created:__startupProof.created,initAttempts:__startupProof.initAttempts,initForwarded:__startupProof.initForwarded,commands:__startupProof.commands,terminated:__startupProof.terminated,retiredAt:__startupProof.retiredAt,microphones:__startupProof.microphones,ready:__startupProof.ready,violations:__startupProof.violations}));
  assert.equal(proof.commands.length,0);assert.equal(proof.microphones,0);assert.deepEqual(proof.violations,[]);
  assert.equal(proof.created,mode==="constructor"?0:1);assert.equal(proof.terminated,mode==="constructor"?0:1);
  if(mode==="blocked"){assert.ok(held.length>=1,"actual SDK WASM fetch must be blocked");assert.equal(proof.initForwarded,1);assert.ok(proof.retiredAt>=30000&&proof.retiredAt<34000);}
  assert.equal(await page.locator(".server-connection").count(),0,"recovery must work without a selected server");
  assert.deepEqual(await snapshotCache(page),saved);
  await page.evaluate(()=>{__startupProof.sdk?.dispatchEvent(new MessageEvent("message",{data:{kind:"ready",capabilities:["voice"]}}));});
  await reload.waitFor();assert.equal(await page.evaluate(()=>__startupProof.commands.length),0);
  const bounds=await reload.boundingBox();const viewport=page.viewportSize();assert.ok(bounds.x>=0&&bounds.x+bounds.width<=viewport.width&&bounds.y>=0&&bounds.y+bounds.height<=viewport.height);
  await page.screenshot({path:resolve(out,mode+"-recovery.png")});cases.push({mode,...proof,wasm_requests_held:held.length,cache_unchanged:true,setup_draft_preserved:mode==="blocked"?true:null,setup_draft_scope:mode==="blocked"?"entered before watchdog; retained across retirement":"not applicable: synchronous failure before entering a draft",seeded_servers:mode==="constructor"?0:1,global_reload_in_bounds:true});
 }
 const {page}=await openCase("ready");
 await page.getByRole("button",{name:"Trust this certificate",exact:true}).click({timeout:20000});
 await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null,{timeout:20000});
 const before=await page.evaluate(()=>__startupProof.commands.slice());assert.equal(before.filter(method=>method==="connect").length,1);
 await page.evaluate(()=>{for(let i=0;i<3;i++)__startupProof.sdk.dispatchEvent(new MessageEvent("message",{data:{kind:"ready",capabilities:[]}}));});
 await page.waitForTimeout(200);const after=await page.evaluate(()=>({commands:__startupProof.commands,terminated:__startupProof.terminated,microphones:__startupProof.microphones,violations:__startupProof.violations}));
 assert.equal(after.commands.filter(method=>method==="connect").length,1);assert.equal(after.commands.filter(method=>method==="add_server").length,before.filter(method=>method==="add_server").length);
 assert.equal(after.terminated,0);assert.equal(after.microphones,0);assert.deepEqual(after.violations,[]);
 assert.deepEqual(errors,[]);assert.deepEqual(remote,[]);
 cases.push({mode:"ready",duplicate_ready:3,connect_count:1,no_repeat_init_or_auto_connect:true,microphones:0});
 const report={passed:true,site,csp,startup_deadline_ms:30000,cases,checks:["actual packaged App/SDK","real WASM-loader stall without ordinary RPC","constructor/init-post failure with zero servers","global320px recovery","cache/certificate/history retained","setup draft retained","late/duplicate ready ignored","one invite auto-connect","no microphone/host devices","production CSP/no remote requests"]};
 await writeFile(resolve(out,"proof.json"),JSON.stringify(report,null,2));console.log(JSON.stringify(report));
}catch(error){
 await writeFile(resolve(out,"failure.json"),JSON.stringify({error:String(error),errors,remote,states:await Promise.all(contexts.map(async context=>context.pages()[0]?.evaluate(()=>({text:document.body.textContent,commands:__startupProof.commands,created:__startupProof.created,terminated:__startupProof.terminated,init:__startupProof.initForwarded})).catch(()=>null)))},null,2));throw error;
}finally{
 for(const context of contexts)await context.close();await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(done=>server.close(done));
}
