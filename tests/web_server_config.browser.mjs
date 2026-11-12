// SPDX-License-Identifier: EUPL-1.2
// Real App/SDK worker with an owned TLS/protobuf fixture; no widget/state injection.
import assert from "node:assert/strict";
import {createServer} from "node:http";
import {createConnection} from "node:net";
import {readFile,writeFile,mkdir,rename,unlink} from "node:fs/promises";
import {resolve,extname,sep} from "node:path";
import {pathToFileURL} from "node:url";
const [site,relayUrl,port,wrapper,playwright,output]=process.argv.slice(2);
assert.ok(site&&relayUrl&&port&&wrapper&&playwright&&output);
const requireSendCap=process.env.NUSCHL_REQUIRE_BROWSER_SEND_CAP==="1";
const relay=new URL(relayUrl);assert.equal(relay.hostname,"127.0.0.1");assert.equal(relay.protocol,"ws:");
const directory=resolve(site);await mkdir(output,{recursive:true});let origin;
let csp="default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const types={".html":"text/html",".js":"text/javascript",".css":"text/css",".wasm":"application/wasm",".svg":"image/svg+xml",".json":"application/json",".txt":"text/plain"};
const requests=[],canaries=[];
const server=createServer(async(req,res)=>{
  if(req.url.includes("canary"))canaries.push(req.url);
  try {
    const pathname=decodeURIComponent(new URL(req.url,"http://127.0.0.1").pathname);
    const mounted=pathname.startsWith("/app/")?pathname.slice(4):pathname;
    const path=resolve(directory,"."+mounted);assert.ok(path===directory||path.startsWith(directory+sep));
    const target=extname(mounted)?path:resolve(path,"index.html");
    let body=await readFile(target);
    res.writeHead(200,{"Content-Type":types[extname(target)]||"application/octet-stream","Content-Security-Policy":csp,"Permissions-Policy":"camera=(), geolocation=(), microphone=(self)","Cross-Origin-Opener-Policy":"same-origin","Referrer-Policy":"no-referrer","X-Content-Type-Options":"nosniff"});res.end(body);
  }catch{res.writeHead(404);res.end();}
});
const tunnels=new Set();
server.on("upgrade",(req,socket,head)=>{
  if(req.url!=="/relay"){socket.destroy();return;}
  const upstream=createConnection({host:"127.0.0.1",port:Number(relay.port)},()=>{
    let headers=`GET ${relay.pathname} HTTP/1.1\r\n`;
    for(let i=0;i<req.rawHeaders.length;i+=2)headers+=`${req.rawHeaders[i]}: ${req.rawHeaders[i+1]}\r\n`;
    upstream.write(headers+"\r\n");if(head.length)upstream.write(head);socket.pipe(upstream).pipe(socket);
  });
  tunnels.add(socket);tunnels.add(upstream);socket.on("close",()=>{tunnels.delete(socket);upstream.destroy();});
  upstream.on("close",()=>{tunnels.delete(upstream);socket.destroy();});socket.on("error",()=>upstream.destroy());upstream.on("error",()=>socket.destroy());
});
await new Promise(done=>server.listen(0,"127.0.0.1",done));origin=`http://127.0.0.1:${server.address().port}`;
csp=csp.replace("connect-src 'self'",`connect-src 'self' ${origin.replace("http:","ws:")}`);
const {chromium}=await import(pathToFileURL(playwright).href);
const browser=await chromium.launch({headless:true,executablePath:wrapper});const contexts=[],pages=[],errors=[];
async function open(username,width,height){
  const context=await browser.newContext({viewport:{width,height},colorScheme:"dark"});contexts.push(context);
  context.on("request",request=>requests.push(request.url()));
  await context.route("**/*",route=>route.request().url().startsWith(origin+"/")?route.continue():route.abort());
  const page=await context.newPage();pages.push(page);page.on("pageerror",e=>errors.push(e.message));
  await page.addInitScript(()=>{
    window.__welcomeProof={violations:[],captures:0,documents:[],states:[],results:[],allResults:[]};
    document.addEventListener("securitypolicyviolation",e=>__welcomeProof.violations.push(e.violatedDirective));
    navigator.mediaDevices.getUserMedia=()=>{__welcomeProof.captures++;return Promise.reject(new Error("No physical capture in fixture."));};
    const Base=Worker;
    window.Worker=class extends Base{
      constructor(url,options){super(url,options);if(!String(url).includes("sdk.worker"))return;window.__welcomeSdk=this;this.addEventListener("message",({data})=>{
        if(data.kind==="state"){
          __welcomeProof.states.push(data.servers.map(s=>({id:s.id,token:s.welcome_token,status:s.status,keys:Object.keys(s)})));
          for(const u of data.updates||[])if(u.kind==="welcome")__welcomeProof.documents.push(u);
        }
        if(data.kind==="result")__welcomeProof.allResults.push(data);
        if(data.kind==="result"&&String(data.id).startsWith("fixture-"))__welcomeProof.results.push(data);
      });}
    };
  });
  await page.goto(`${origin}/app/#s=127.0.0.1:${port}&u=${username}`);
  if(width<=760)await page.getByRole("button",{name:"Open navigation",exact:true}).click();
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).click({timeout:20000});
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null,{timeout:20000});
  if(width<=760)await page.getByRole("button",{name:"Close navigation",exact:true}).click();
  await page.getByRole("region",{name:"Server welcome",exact:true}).waitFor();return page;
}
async function nav(page,action){
  if((await page.viewportSize()).width<=760)await page.getByRole("button",{name:"Open navigation",exact:true}).click();
  await action();
  if((await page.viewportSize()).width<=760)await page.keyboard.press("Escape");
}
async function viewport(page,width,height){
  await page.setViewportSize({width,height});
  await page.waitForFunction(()=>!!document.querySelector(".menu-button")===(innerWidth<=760));
  await page.waitForFunction(()=>Math.abs(document.querySelector("main").getBoundingClientRect().height-(visualViewport?.height||innerHeight))<=1);
  await page.evaluate(()=>new Promise(requestAnimationFrame));
}
try{
  const page=await open("config-browser",390,844),composer=page.getByLabel("Message",{exact:true});let seq=0,acks=0;
  async function configure(fields){
    const temporary=resolve(output,"next-control.json");await writeFile(temporary,JSON.stringify({seq:++seq,fields}));await rename(temporary,resolve(output,"control.json"));
    await page.locator("article").filter({hasText:`config-applied:${seq}`}).waitFor();
  }
  async function send(body,allowed){
    await composer.fill(body);const count=await page.evaluate(()=>__welcomeProof.allResults.length);
    await page.getByRole("button",{name:"Send",exact:true}).click();await page.waitForFunction(n=>__welcomeProof.allResults.length>n,count);
    const reply=await page.evaluate(()=>__welcomeProof.allResults.at(-1));assert.equal(!reply.error,allowed);
    if(allowed){await page.locator("article").filter({hasText:`wire-ack:${++acks}`}).waitFor();await page.waitForFunction(()=>document.querySelector("#message").value==="");}
    else {
      assert.equal(await composer.inputValue(),body,"rejection erased draft");assert.ok(await composer.isEditable());
      assert.match(reply.error,/Message exceeds this server's \d+-byte limit/);
      await page.getByRole("alert").filter({hasText:reply.error}).waitFor();
    }
  }
  await configure({3:1,4:128,5:256,6:12,7:0});await send("x".repeat(129),false);
  const diagnostics=[];
  async function diagnostic(id,body){
    const before=await page.evaluate(()=>__welcomeProof.allResults.length);
    await page.evaluate(({id,body})=>__welcomeSdk.postMessage({id,method:"send_text",args:[body]}),{id,body});
    await page.waitForFunction(n=>__welcomeProof.allResults.length>n,before);
    const reply=await page.evaluate(()=>__welcomeProof.allResults.at(-1));diagnostics.push(reply);return reply.error;
  }
  assert.match(await diagnostic("fixture-empty",""),/Choose a conversation/);
  assert.match(await diagnostic("fixture-global","x".repeat(256*1024+1)),/256 KiB client limit/);
  assert.equal(await composer.inputValue(),"x".repeat(129));

  await configure({1:24_000,6:24});assert.equal(await composer.inputValue(),"x".repeat(129));await send("x".repeat(129),false);
  await configure({4:256});assert.equal(await composer.inputValue(),"x".repeat(129));await send("x".repeat(129),true);
  await configure({4:16});await send("<&>\n",false);
  await configure({4:17});assert.equal(await composer.inputValue(),"<&>\n");await send("<&>\n",true);
  await configure({3:0,4:4});await send("<&>\n",true);
  await configure({4:0,5:0,6:0});await send("x".repeat(5001),true);
  await unlink(resolve(output,"control.json"));
  await nav(page,async()=>{await page.getByRole("button",{name:"Disconnect",exact:true}).click();assert.match(await diagnostic("fixture-disconnected","inactive"),/Connect before sending/);await page.getByRole("button",{name:"Connect",exact:true}).click();await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected");});
  await send("x".repeat(5001),false);await send("x".repeat(5000),true);
  const proof=await page.evaluate(()=>__welcomeProof);assert.equal(proof.captures,0);assert.deepEqual(proof.violations,[]);assert.deepEqual(errors,[]);assert.deepEqual(canaries,[]);
  await page.screenshot({path:resolve(output,"configuration-390.png")});
  await writeFile(resolve(output,"proof.json"),JSON.stringify({passed:true,site,csp,diagnostics,wire_acknowledgements:acks,checks:["independent private TLS/protobuf configuration", "actual App rejected128+1-byte body", "sparse bandwidth/max-user update preserves cap", "draft survives rejection and asynchronous cap increase", "escaped HTML/newline wire17-byte boundary", "plain UTF-8 wire4-byte boundary", "explicit zero enables above 5000 within app ceiling", "reconnect resets conservative5000 default", "no capture/CSP/canary"]},null,2));console.log("Configuration App proof passed");
}catch(error){await writeFile(resolve(output,"failure.json"),JSON.stringify({error:String(error),errors,canaries,pages:await Promise.all(pages.map(page=>page.evaluate(()=>({body:document.body.textContent,proof:__welcomeProof})).catch(()=>null)))},null,2));throw error;}
finally{for(const context of contexts)await context.close();await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(done=>server.close(done));}
