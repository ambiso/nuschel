// SPDX-License-Identifier: EUPL-1.2
// Owned SDK worker retirement under an actual17s busy loop and15s RPC deadline. Requires a separately
// confined Chromium wrapper with synthetic microphone and no host sound devices.
import assert from "node:assert/strict";
import {createServer} from "node:http";
import {createConnection} from "node:net";
import {readFile,mkdir,writeFile} from "node:fs/promises";
import {resolve,extname,sep} from "node:path";
import {pathToFileURL} from "node:url";
const RPC_LIMIT=128, RPC_ACK_RESERVE=4, RPC_ORDINARY_LIMIT=RPC_LIMIT-RPC_ACK_RESERVE;
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
    let body=await readFile(target);
    // Fixture-only ordinary event task; never ship this listener. Keeping the
    // busy loop outside inspector evaluation distinguishes native retirement.
    if (/sdk\.worker-[^/]+\.js$/.test(target)) body=Buffer.concat([body,Buffer.from(`
self.addEventListener("message",event=>{if(event.data?.__rpc_stall_test__!==true)return;
 console.log("PRIVATE_SDK_STALL_BEGIN:"+Date.now());const until=performance.now()+17000;
 while(performance.now()<until){};
 console.log("PRIVATE_SDK_STALL_END:"+Date.now());postMessage({kind:"rpc-fault-tail"});
});
`)]);
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
async function open(username,width=1200,height=800) {
  const context=await browser.newContext({viewport:{width,height},permissions:["microphone"]});contexts.push(context);
  const page=await context.newPage();
  page.on("pageerror",e=>errors.push(e.message));
  page.on("request",r=>{if(!r.url().startsWith(origin+"/"))remote.push(r.url());});
  await page.addInitScript(()=>{
    const proof=window.__voiceProof={terminations:0,pendingRpc:new Map(),peakRpc:0,peakOrdinaryRpc:0,messagesAfterRetirement:0,tracks:[],voice:null,packets:0,states:0,commands:[],microphoneCalls:0,violations:[],contexts:[],gains:[]};
    document.addEventListener("securitypolicyviolation",e=>proof.violations.push(e.violatedDirective));
    const Context=window.AudioContext;
    window.AudioContext=class extends Context {
      constructor(options){super({...options,sinkId:{type:"none"}});proof.contexts.push(this);}
      createGain(){const gain=super.createGain();proof.gains.push(gain);return gain;}
    };
    const getUserMedia=navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
    navigator.mediaDevices.getUserMedia=async constraints=>{proof.microphoneCalls++;const stream=await getUserMedia(constraints);proof.tracks.push(...stream.getTracks());return stream;};
    const Worker=window.Worker;
    window.Worker=class extends Worker {
      constructor(url,options){super(url,options);if(String(url).includes("sdk.worker")){
        this.__ownedSdk=true;proof.sdk=this;
        this.addEventListener("message",({data})=>{
          if(proof.terminations)proof.messagesAfterRetirement++;
          if(data.kind==="result")proof.pendingRpc.delete(data.id);
          if(data.kind==="state"){proof.voice=data.voice;proof.timeline=data.timeline;proof.servers=data.servers;proof.states++;}
          if(data.kind==="voice_packet")proof.packets++;
        });
      }}
      terminate(){if(this.__ownedSdk){proof.terminations++;proof.terminatedAt=performance.now();proof.terminatedWall=Date.now();proof.pendingAtRetirement=proof.pendingRpc.size;}return super.terminate();}
      postMessage(message,...rest){if(message?.method){proof.commands.push(message.method);if(this.__ownedSdk&&Number.isSafeInteger(message.id)){proof.pendingRpc.set(message.id,message.method);proof.peakRpc=Math.max(proof.peakRpc,proof.pendingRpc.size);proof.peakOrdinaryRpc=Math.max(proof.peakOrdinaryRpc,[...proof.pendingRpc.values()].filter(method=>method!=="content_complete").length);}(proof.commandTimes??=[]).push({method:message.method,wall:Date.now()});}if(message?.method==="send_text")proof.lastSentText=message.args[0];return super.postMessage(message,...rest);}
    };
    proof.measure=()=>{
      const context=proof.contexts.at(-1),gain=proof.gains.at(-1);
      if(!context||!gain)return null;
      if(!proof.analyser){proof.analyser=context.createAnalyser();proof.analyser.fftSize=4096;gain.connect(proof.analyser);}
      const samples=new Float32Array(4096);proof.analyser.getFloatTimeDomainData(samples);
      return {rms:Math.sqrt(samples.reduce((sum,v)=>sum+v*v,0)/samples.length),finite:samples.every(Number.isFinite),sink:{type:context.sinkId?.type}};
    };
  });
  await page.goto(`${origin}/app/#s=127.0.0.1:${port}&u=${username}`);
  if (width <= 760) {
    await page.getByRole("button",{name:"Open navigation",exact:true}).click();
  }
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).click({timeout:20000});
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null,{timeout:20000});
  if (width <= 760) await page.getByRole("button",{name:"Close navigation",exact:true}).click();
  return page;
}
async function cache(page) {
 return page.evaluate(async()=>{
  const db=await new Promise((resolve,reject)=>{const request=indexedDB.open("nuschl-observed-history-v1",1);request.onsuccess=()=>resolve(request.result);request.onerror=reject;});
  try {
   const transaction=db.transaction(["settings","messages"],"readonly");
   const read=request=>new Promise((resolve,reject)=>{request.onsuccess=()=>resolve(request.result);request.onerror=reject;});
   const bookmarks=read(transaction.objectStore("settings").get("bookmarks"));
   const messages=read(transaction.objectStore("messages").getAll());
   return {bookmarks:await bookmarks,messages:await messages};
  }finally{db.close();}
 });
}
try {
 const page=await open("rpc-stall-private");
 await page.getByRole("button",{name:"Join voice",exact:true}).click();
 await page.waitForFunction(()=>__voiceProof.voice?.joined);
 await page.getByRole("button",{name:"Unmute microphone",exact:true}).click();
 await page.waitForFunction(()=>__voiceProof.voice?.can_transmit&&__voiceProof.tracks.some(t=>t.readyState==="live"));
 const editor=page.getByLabel("Message",{exact:true}),historyText="RPC history sentinel";
 await editor.fill(historyText);await page.getByRole("button",{name:"Send",exact:true}).click();
 await page.locator("article").filter({hasText:historyText}).waitFor();
 const before=await cache(page);assert.ok(before.bookmarks&&before.messages.length);
 const draft="Unsent draft survives SDK retirement\nwith a second line";await editor.fill(draft);
 const sdk=page.workers().find(worker=>worker.url().includes("sdk.worker"));assert.ok(sdk);
 let closed=false,finished=false,nativeClosedAt=0;const closedPromise=new Promise(resolve=>sdk.once("close",()=>{closed=true;nativeClosedAt=performance.now();resolve();}));
 const started=new Promise(resolve=>page.on("console",message=>{
  const text=message.text();if(text.startsWith("PRIVATE_SDK_STALL_")){workerEvents.push({text,node_wall:Date.now()});
   if(text.startsWith("PRIVATE_SDK_STALL_BEGIN:"))resolve();if(text.startsWith("PRIVATE_SDK_STALL_END:"))finished=true;}
 }));
 await page.evaluate(()=>__voiceProof.sdk.postMessage({__rpc_stall_test__:true}));
 await started;const start=performance.now();
 // First allow the real codec's two sends to fill, then issue140 actual DOM
 // click events while SDK processing is blocked. External post/ACK observation
 // measures the ordinary inbox; four of the 128 total slots stay reserved for
 // content completion ACKs. The production adapter is not mocked or shortened.
 await new Promise(resolve=>setTimeout(resolve,120));
 const audioInbox=await page.evaluate(()=>[...__voiceProof.pendingRpc.values()].filter(method=>method==="voice_packet").length);
 assert.equal(audioInbox,2);
 const serverName=await page.evaluate(()=>__voiceProof.servers[0].name);
 await page.getByRole("button",{name:`Browse ${serverName}`,exact:true}).evaluate(button=>{for(let index=0;index<140;index++)button.click();});
 const fullInbox=await page.evaluate(()=>({current:__voiceProof.pendingRpc.size,peak:__voiceProof.peakRpc,ordinary:[...__voiceProof.pendingRpc.values()].filter(method=>method!=="content_complete").length,acks:[...__voiceProof.pendingRpc.values()].filter(method=>method==="content_complete").length,peakOrdinary:__voiceProof.peakOrdinaryRpc}));
 assert.equal(fullInbox.ordinary,RPC_ORDINARY_LIMIT);assert.equal(fullInbox.peakOrdinary,RPC_ORDINARY_LIMIT);
 assert.ok(fullInbox.acks<=RPC_ACK_RESERVE);assert.equal(fullInbox.current,fullInbox.ordinary+fullInbox.acks);
 assert.ok(fullInbox.current<=RPC_LIMIT&&fullInbox.peak<=RPC_LIMIT);
 await page.getByRole("button",{name:"Reload client",exact:true}).waitFor({timeout:22000});
 const elapsed=performance.now()-start;
 assert.ok(elapsed>=14500&&elapsed<17000,`actual15s retirement elapsed ${elapsed}`);
 const terminal=await page.evaluate(()=>({terminations:__voiceProof.terminations,tracks:__voiceProof.tracks.map(t=>t.readyState),commands:__voiceProof.commands.length,messagesAfterRetirement:__voiceProof.messagesAfterRetirement,pendingAtRetirement:__voiceProof.pendingAtRetirement,violations:__voiceProof.violations}));
 assert.equal(terminal.terminations,1);assert.ok(terminal.tracks.length&&terminal.tracks.every(state=>state==="ended"));
 await page.getByRole("region",{name:"Voice controls"}).waitFor({state:"detached"});
 assert.equal(await page.locator(".server-connection .status").textContent(),"Unavailable");
 assert.equal(await editor.inputValue(),draft);assert.equal(await editor.isEnabled(),true);assert.equal(await editor.isEditable(),false);
 const selection=await editor.evaluate(node=>{node.focus();node.select();return node.selectionEnd-node.selectionStart;});assert.equal(selection,draft.length);
 assert.equal(await page.getByRole("button",{name:"Send",exact:true}).isDisabled(),true);
 await page.getByRole("button",{name:`Browse ${serverName}`,exact:true}).click();
 assert.equal(await page.evaluate(()=>__voiceProof.commands.length),terminal.commands,"closed lane posted a new command");
 assert.deepEqual(await cache(page),before,"retirement changed bookmarks/certificate/history");
 // Worker.terminate() returns void and does not acknowledge physical exit.
 // Retain the observed tail/close timing; permanent logical retirement still
 // prevents new requests and application acceptance during that grace period.
 await closedPromise;
 const afterTail=await page.evaluate(()=>({commands:__voiceProof.commands.length,messagesAfterRetirement:__voiceProof.messagesAfterRetirement,inbox:__voiceProof.pendingRpc.size,ordinary:[...__voiceProof.pendingRpc.values()].filter(method=>method!=="content_complete").length,acks:[...__voiceProof.pendingRpc.values()].filter(method=>method==="content_complete").length}));
 assert.equal(afterTail.commands,terminal.commands);assert.equal(afterTail.messagesAfterRetirement,0);
 assert.equal(afterTail.ordinary,RPC_ORDINARY_LIMIT,"an old tail cannot return ordinary results to the retired owner");
 assert.ok(afterTail.acks<=RPC_ACK_RESERVE);assert.ok(afterTail.inbox<=RPC_LIMIT);
 assert.equal(afterTail.inbox,terminal.pendingAtRetirement,"an old tail cannot return results to the retired owner");
 // Also deliver already-queued stale callbacks explicitly: browser disposal
 // dropping messages alone does not prove App's owned-worker identity guard.
 await page.evaluate(()=>{
  __voiceProof.sdk.dispatchEvent(new MessageEvent("message",{data:{kind:"ready",capabilities:["voice"]}}));
  __voiceProof.sdk.dispatchEvent(new MessageEvent("message",{data:{kind:"state",servers:__voiceProof.servers,timeline:__voiceProof.timeline,voice:__voiceProof.voice,updates:[]}}));
  __voiceProof.sdk.dispatchEvent(new MessageEvent("message",{data:{kind:"result",id:1,value:true}}));
 });
 assert.equal(await page.locator(".server-connection .status").textContent(),"Unavailable");
 assert.equal(await page.getByRole("region",{name:"Voice controls"}).count(),0);
 assert.equal(await editor.inputValue(),draft);assert.equal(await page.getByRole("button",{name:"Send",exact:true}).isDisabled(),true);
 assert.equal(await page.evaluate(()=>__voiceProof.commands.length),terminal.commands);
 assert.deepEqual(errors,[]);assert.deepEqual(remote,[]);assert.deepEqual(terminal.violations,[]);
 await page.screenshot({path:resolve(out,"unavailable-draft.png")});
 const report={passed:true,site,csp,actual_stall_ms:17000,rpc_deadline_ms:15000,observed_retirement_ms:elapsed,observed_native_close_ms:nativeClosedAt-start,audio_rpc_inbox:audioInbox,sdk_rpc_inbox:fullInbox,sdk_rpc_budget:{total:RPC_LIMIT,ordinary:RPC_ORDINARY_LIMIT,content_ack_reserve:RPC_ACK_RESERVE},
  messages_after_retirement:afterTail.messagesAfterRetirement,three_explicit_stale_callbacks_ignored:true,worker_closed:closed,worker_stall_completed:finished,worker_events:workerEvents,terminations:terminal.terminations,microphone_tracks:terminal.tracks,
  draft_retained_and_selectable:true,cache_unchanged:true,new_send_denied:true,
  checks:["actual packaged App and owned SDK worker","fixture-appended ordinary-message17s block; original packaged SDK prefix unmodified","15s logical retirement/termination request before synthetic rejection","physical shutdown timing recorded separately","no auto restart/consent","tracks stopped","Unavailable status/no voice controls","read-only selectable draft","bookmarks/certificate/history unchanged","new command denied","stale ready/state/result callbacks cannot revive old owner","production CSP/no foreign requests"]};
 await writeFile(resolve(out,"proof.json"),JSON.stringify(report,null,2));console.log(JSON.stringify(report));
}catch(error){
 await writeFile(resolve(out,"failure.json"),JSON.stringify({error:String(error),errors,remote,workerEvents,states:await Promise.all(contexts.map(async context=>context.pages()[0]?.evaluate(()=>({text:document.body.textContent,terminations:__voiceProof.terminations,commands:__voiceProof.commands,commandTimes:__voiceProof.commandTimes,terminatedWall:__voiceProof.terminatedWall,tracks:__voiceProof.tracks.map(t=>t.readyState)})).catch(()=>null)))},null,2));throw error;
}finally{
 for(const context of contexts)await context.close();await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(done=>server.close(done));
}
