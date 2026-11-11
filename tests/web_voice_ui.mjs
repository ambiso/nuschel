// SPDX-License-Identifier: EUPL-1.2
// Exact packaged Svelte UI/SDK/codec under production CSP. Requires a separately
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
const {chromium}=await import(pathToFileURL(process.env.NUSCHL_PLAYWRIGHT||"/nix/store/phc5gsqnm205r6hvlay77f7ia3w4bzzk-playwright-core-1.64.1/index.mjs").href);
const browser=await chromium.launch({headless:true,executablePath:wrapper});
const contexts=[],errors=[],remote=[];
async function open(username,width=1200,height=800) {
  const context=await browser.newContext({viewport:{width,height},permissions:["microphone"]});contexts.push(context);
  const page=await context.newPage();
  page.on("pageerror",e=>errors.push(e.message));
  page.on("request",r=>{if(!r.url().startsWith(origin+"/"))remote.push(r.url());});
  await page.addInitScript(()=>{
    const proof=window.__voiceProof={voice:null,packets:0,states:0,commands:[],microphoneCalls:0,violations:[],contexts:[],gains:[]};
    document.addEventListener("securitypolicyviolation",e=>proof.violations.push(e.violatedDirective));
    const Context=window.AudioContext;
    window.AudioContext=class extends Context {
      constructor(options){super({...options,sinkId:{type:"none"}});proof.contexts.push(this);}
      createGain(){const gain=super.createGain();proof.gains.push(gain);return gain;}
    };
    const getUserMedia=navigator.mediaDevices.getUserMedia.bind(navigator.mediaDevices);
    navigator.mediaDevices.getUserMedia=constraints=>{proof.microphoneCalls++;return getUserMedia(constraints);};
    const Worker=window.Worker;
    window.Worker=class extends Worker {
      constructor(url,options){super(url,options);if(String(url).includes("sdk.worker")){
        this.addEventListener("message",({data})=>{
          if(data.kind==="state"){proof.voice=data.voice;proof.timeline=data.timeline;proof.states++;}
          if(data.kind==="voice_packet")proof.packets++;
        });
      }}
      postMessage(message,...rest){if(message?.method)proof.commands.push(message.method);if(message?.method==="send_text")proof.lastSentText=message.args[0];return super.postMessage(message,...rest);}
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
try {
  const sender=await open("ui-voice-one"),receiver=await open("ui-voice-two",390,844);
  const editor=sender.getByLabel("Message",{exact:true});
  await editor.fill("probe first line\nprobe second line");
  assert.equal(await editor.inputValue(),"probe first line\nprobe second line");
  await editor.fill("");
  for(const page of [sender,receiver]){
    assert.equal(await page.evaluate(()=>__voiceProof.microphoneCalls),0);
    await page.getByRole("button",{name:"Join voice",exact:true}).click();
    await page.waitForFunction(()=>__voiceProof.voice?.joined&&!__voiceProof.voice.deafened,null,{timeout:20000});
    assert.equal(await page.evaluate(()=>__voiceProof.microphoneCalls),0);
  }
  await sender.getByRole("button",{name:"Unmute microphone",exact:true}).click();
  await sender.waitForFunction(()=>__voiceProof.voice?.can_transmit,null,{timeout:20000});
  await receiver.waitForFunction(()=>__voiceProof.packets>=25&&__voiceProof.measure()?.rms>0.001,null,{timeout:20000});
  const audible=await receiver.evaluate(()=>__voiceProof.measure());assert.ok(audible.finite);assert.equal(audible.sink.type,"none");
  const trafficBefore=await receiver.evaluate(()=>({packets:__voiceProof.packets,states:__voiceProof.states}));
  await new Promise(resolve=>setTimeout(resolve,400));
  const trafficAfter=await receiver.evaluate(()=>({packets:__voiceProof.packets,states:__voiceProof.states}));
  assert.ok(trafficAfter.packets-trafficBefore.packets>=5,"continuous audio stopped unexpectedly");
  assert.ok(trafficAfter.states-trafficBefore.states<=3,"audio traffic redrew the SDK timeline per packet");
  await receiver.getByRole("button",{name:"Deafen",exact:true}).click();
  await receiver.waitForFunction(()=>__voiceProof.voice?.deafened&&__voiceProof.voice.muted);
  await new Promise(resolve=>setTimeout(resolve,150));
  const deaf=await receiver.evaluate(()=>__voiceProof.measure());assert.ok(deaf.rms<1e-8);
  await receiver.getByRole("button",{name:"Enable sound",exact:true}).click();
  await receiver.waitForFunction(()=>!__voiceProof.voice?.deafened&&__voiceProof.measure()?.rms>0.001);
  assert.equal(await receiver.evaluate(()=>__voiceProof.microphoneCalls),0);
  // Several actual UI events during a toggle must converge or remain honestly
  // deafened with an explicit way to resume sound; no stale UI intent reopens capture.
  await receiver.getByRole("button",{name:"Deafen",exact:true}).click({clickCount:4,delay:5});
  await new Promise(resolve=>setTimeout(resolve,200));
  if (await receiver.evaluate(()=>__voiceProof.voice?.deafened)) await receiver.getByRole("button",{name:"Enable sound",exact:true}).click();
  const resume=receiver.getByRole("button",{name:"Enable audio",exact:true});
  if (await resume.isVisible()) await resume.click();
  await receiver.waitForFunction(()=>!__voiceProof.voice?.deafened&&__voiceProof.measure()?.rms>0.001);
  assert.equal(await receiver.evaluate(()=>__voiceProof.microphoneCalls),0);
  const text="A real UI message\nwith a preserved newline";
  await sender.getByLabel("Message",{exact:true}).evaluate(node=>{
    window.__voiceProof.composer={tag:node.tagName,type:node.type,outer:node.outerHTML,events:[]};
    for(const kind of ["keydown","beforeinput","submit"]){
      (kind==="submit"?node.form:node).addEventListener(kind,event=>__voiceProof.composer.events.push({kind,key:event.key,ctrl:event.ctrlKey,meta:event.metaKey,data:event.data,value:node.value}),true);
    }
    node.addEventListener("input",event=>{
      const record={value:node.value,type:event.inputType,data:event.data};__voiceProof.composer.events.push(record);
      queueMicrotask(()=>record.after=node.value);
    },true);
  });
  await sender.getByLabel("Message",{exact:true}).fill(text);assert.equal(await sender.getByLabel("Message",{exact:true}).inputValue(),text);await sender.getByRole("button",{name:"Send",exact:true}).click();
  const incoming=receiver.locator("article").filter({hasText:"A real UI message"});
  await incoming.waitFor();
  assert.ok((await incoming.locator(".text").allTextContents()).join("\n").includes("with a preserved newline"));
  assert.equal(await sender.getByLabel("Message",{exact:true}).inputValue(),"");
  await sender.getByRole("button",{name:"Mute microphone",exact:true}).click();
  await sender.waitForFunction(()=>__voiceProof.voice?.muted&&!__voiceProof.voice.can_transmit);
  await new Promise(resolve=>setTimeout(resolve,200));const before=await receiver.evaluate(()=>__voiceProof.packets);
  await new Promise(resolve=>setTimeout(resolve,400));assert.equal(await receiver.evaluate(()=>__voiceProof.packets),before);
  await sender.getByRole("button",{name:"Stop audio",exact:true}).click();
  await sender.getByRole("region",{name:"Voice controls"}).waitFor({state:"detached"});
  assert.equal(await sender.locator(".server-connection .status").textContent(),"Connected");
  await sender.screenshot({path:resolve(out,"sender.png")});await receiver.screenshot({path:resolve(out,"receiver-phone.png")});
  for(const context of contexts)for(const page of context.pages())assert.deepEqual(await page.evaluate(()=>__voiceProof.violations),[]);
  assert.deepEqual(errors,[]);assert.deepEqual(remote,[]);
  const report={passed:true,site,csp,audio_only_packets:trafficAfter.packets-trafficBefore.packets,audio_only_state_snapshots:trafficAfter.states-trafficBefore.states,audible_rms:audible.rms,deaf_rms:deaf.rms,checks:["real packaged App/SDK/codec/worklet","startup no microphone","join ack","gesture unmute","synthetic Opus interchange","finite worklet PCM to null sink","deafen flush","undeafen without microphone","rapid stale-snapshot audio controls","SDK Sent composer clear","multiline text","mute stops packets","stop audio keeps chat","same-origin production CSP"]};
  await writeFile(resolve(out,"proof.json"),JSON.stringify(report,null,2));console.log(JSON.stringify(report));
} catch(error){
  await writeFile(resolve(out,"failure.json"),JSON.stringify({error:String(error),errors,remote,states:await Promise.all(contexts.map(async c=>c.pages()[0]?.evaluate(()=>({voice:__voiceProof.voice,timeline:__voiceProof.timeline,lastSentText:__voiceProof.lastSentText,composer:__voiceProof.composer,packets:__voiceProof.packets,commands:__voiceProof.commands,microphoneCalls:__voiceProof.microphoneCalls,violations:__voiceProof.violations,text:document.body.textContent})).catch(()=>null)))},null,2));throw error;
} finally {
  for(const context of contexts)await context.close();await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(done=>server.close(done));
}
