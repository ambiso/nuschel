// SPDX-License-Identifier: EUPL-1.2
// The packaged web app's voice bitrate on an owned Murmur 1.5 through the
// real relay, in a confined Chromium with a synthetic microphone: requested,
// effective and why, real frame sizes at the receiving browser, a live
// ServerConfig, a budget too small for voice, and persistence across reload.
import assert from "node:assert/strict";
import {createServer} from "node:http";
import {createConnection} from "node:net";
import {readFile,mkdir,writeFile} from "node:fs/promises";
import {resolve,extname,sep} from "node:path";
import {pathToFileURL} from "node:url";
const [site,relayUrl,port,wrapper,output,playwright,dbusAddress] = process.argv.slice(2);
assert.ok(site&&relayUrl&&port&&wrapper&&output&&playwright&&dbusAddress);
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
    const body=await readFile(target);
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
import {execFileSync} from "node:child_process";
const {chromium}=await import(pathToFileURL(playwright).href);
const browser=await chromium.launch({headless:true,executablePath:wrapper});
const contexts=[],errors=[],results=[];
const setBandwidth=value=>execFileSync("dbus-send",["--session","--print-reply","--dest=net.sourceforge.mumble.murmur","/",
  "net.sourceforge.mumble.Meta.setConf","int32:1","string:bandwidth",`string:${value}`],{env:{...process.env,DBUS_SESSION_BUS_ADDRESS:dbusAddress}});
async function open(username,width,height,context=null) {
  context??=await browser.newContext({viewport:{width,height},permissions:["microphone"]});
  if(!contexts.includes(context))contexts.push(context);
  const page=await context.newPage();
  page.on("pageerror",e=>errors.push(e.message));
  await page.addInitScript(()=>{
    const proof=window.__bitrate={voice:null,sizes:[],sent:[],commands:[]};
    const Context=window.AudioContext;
    window.AudioContext=class extends Context{constructor(options){super({...options,sinkId:{type:"none"}});}};
    const Worker=window.Worker;
    window.Worker=class extends Worker{
      constructor(url,options){super(url,options);if(String(url).includes("sdk.worker")){
        this.addEventListener("message",({data})=>{
          if(data.kind==="state")proof.voice=data.voice;
          if(data.kind==="voice_packet")proof.sizes.push(data.packet.opus.length);
        });
      }}
      postMessage(message,...rest){
        if(message?.method)proof.commands.push([message.method,message.method==="voice_packet"?message.args[0].opus.length:message.args]);
        return super.postMessage(message,...rest);
      }
    };
  });
  await page.goto(`${origin}/app/#s=127.0.0.1:${port}&u=${username}`);
  if(width<=760)await page.getByRole("button",{name:"Open navigation",exact:true}).click();
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).click({timeout:20000});
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null,{timeout:20000});
  if(width<=760)await page.getByRole("button",{name:"Close navigation",exact:true}).click();
  return page;
}
// Frames the receiving browser decoded from the sender, after `settle` ms.
async function received(page,ms,settle=400){
  await page.waitForTimeout(settle);
  await page.evaluate(()=>{__bitrate.sizes.length=0;});
  await page.waitForTimeout(ms);
  return page.evaluate(()=>__bitrate.sizes.slice());
}
function check(step,sizes,allowance){
  assert.ok(sizes.length>=20,`${step}: only ${sizes.length} frames`);
  const largest=Math.max(...sizes),mean=sizes.reduce((a,b)=>a+b,0)*8*50/sizes.length;
  assert.ok(largest<=allowance,`${step}: ${largest} B over ${allowance}`);
  results.push({step,frames:sizes.length,largest,allowance,mean_bps:Math.round(mean)});
  console.log(`  ${step.padEnd(46)} ${String(sizes.length).padStart(3)} frames, largest ${largest}/${allowance} B, mean ${(mean/1000).toFixed(1)} kb/s`);
}
async function chooseBitrate(page,value){
  await page.getByRole("button",{name:"Settings",exact:true}).first().click();
  const select=page.getByLabel("Voice bitrate",{exact:true});
  await select.selectOption(String(value));
  return select;
}
async function closeSettings(page){await page.getByRole("button",{name:"Close settings",exact:true}).click();}
const line=page=>page.locator(".voice-description .voice-bitrate").textContent();
try {
  const sender=await open("web-bitrate-one",1200,800),receiver=await open("web-bitrate-two",390,844);
  // The default request, before any choice: nothing stored yet.
  assert.equal(await sender.evaluate(()=>localStorage.getItem("nuschl.voice.bitrate.v1")),null);
  for(const page of [sender,receiver]){
    await page.getByRole("button",{name:"Join voice",exact:true}).click();
    await page.waitForFunction(()=>__bitrate.voice?.joined&&!__bitrate.voice.deafened,null,{timeout:20000});
  }
  await sender.getByRole("button",{name:"Unmute microphone",exact:true}).click();
  await sender.waitForFunction(()=>__bitrate.voice?.can_transmit,null,{timeout:20000});
  await sender.waitForFunction(()=>__bitrate.voice?.requested_bitrate===128000&&__bitrate.voice.bitrate===128000);
  assert.equal(await line(sender),"Voice 128 kb/s");
  check("default request (128 kb/s)",await received(receiver,2000),320);
  // Choose 256 kb/s in Settings: stored in this browser, sent to the SDK.
  const select=await chooseBitrate(sender,256000);
  await sender.waitForFunction(()=>__bitrate.voice?.bitrate===256000&&__bitrate.voice.can_transmit,null,{timeout:20000});
  assert.equal(await sender.evaluate(()=>localStorage.getItem("nuschl.voice.bitrate.v1")),"256000");
  const why=await sender.locator(".voice-quality .voice-bitrate").textContent();
  assert.match(why,/: 256 kb\/s, as requested\./);
  await sender.locator(".voice-quality").screenshot({path:resolve(out,"web-settings-256.png")});
  await closeSettings(sender);
  assert.equal(await line(sender),"Voice 256 kb/s");
  check("requested 256 kb/s",await received(receiver,2000),640);
  // A live ServerConfig lowers the server's limit to 72 kb/s.
  setBandwidth(72000);
  await sender.waitForFunction(()=>__bitrate.voice?.bitrate===48000&&__bitrate.voice.bitrate_limit==="server_budget",null,{timeout:20000});
  await sender.waitForFunction(()=>__bitrate.voice?.can_transmit,null,{timeout:20000});
  assert.equal(await line(sender),"Voice 48 of 256 kb/s · server limit");
  assert.equal(await sender.locator(".voice-description .voice-bitrate").getAttribute("title"),
    "48 kb/s: this server allows 72 kb/s per user, 24 kb/s of which is kept for Mumble packet overhead.");
  await sender.screenshot({path:resolve(out,"web-sender-72k.png")});
  // Stock Murmur polices over its last 360 accepted packets: wait for it.
  let limited=[];
  for(let attempt=0;attempt<45&&limited.length<20;attempt++)limited=await received(receiver,2000,0);
  check("live limit 72 kb/s",limited,120);
  // Too small for voice: the browser stops sending, playback stays enabled.
  setBandwidth(29999);
  await sender.waitForFunction(()=>__bitrate.voice?.bitrate===0&&!__bitrate.voice.can_transmit,null,{timeout:20000});
  assert.match(await line(sender),/^Voice not sending \(requested 256 kb\/s\)/);
  assert.equal((await received(receiver,1500)).length,0);
  setBandwidth(558000);
  await sender.waitForFunction(()=>__bitrate.voice?.bitrate===256000,null,{timeout:20000});
  // A reload keeps the request (local storage only; no SDK key crosses).
  await sender.reload();
  // A trusted certificate may still be confirmed once per page load.
  await sender.getByRole("button",{name:"Trust this certificate",exact:true}).click({timeout:8000}).catch(()=>{});
  await sender.getByRole("button",{name:"Settings",exact:true}).first().click();
  assert.equal(await sender.getByLabel("Voice bitrate",{exact:true}).inputValue(),"256000");
  await sender.waitForFunction(()=>__bitrate.commands.some(([method,args])=>method==="set_voice_bitrate"&&args[0]===256000),null,{timeout:20000});
  await sender.locator(".voice-quality").screenshot({path:resolve(out,"web-settings-after-reload.png")});
  await receiver.screenshot({path:resolve(out,"web-receiver-phone.png")});
  assert.deepEqual(errors,[]);
  await writeFile(resolve(out,"web-bitrate.json"),JSON.stringify({results},null,2));
  console.log("Web voice bitrate: ok");
} finally {
  for(const context of contexts)await context.close();await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(done=>server.close(done));
}
