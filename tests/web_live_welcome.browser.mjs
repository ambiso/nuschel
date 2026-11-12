// SPDX-License-Identifier: EUPL-1.2
// No widget/state injection: real App, SDK, Murmur text, worker and native MathML.
import assert from "node:assert/strict";
import {createServer} from "node:http";
import {createConnection} from "node:net";
import {readFile,writeFile,mkdir} from "node:fs/promises";
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
    window.__welcomeProof={violations:[],captures:0,documents:[],states:[],results:[]};
    document.addEventListener("securitypolicyviolation",e=>__welcomeProof.violations.push(e.violatedDirective));
    navigator.mediaDevices.getUserMedia=()=>{__welcomeProof.captures++;return Promise.reject(new Error("No physical capture in fixture."));};
    const Base=Worker;
    window.Worker=class extends Base{
      constructor(url,options){super(url,options);if(!String(url).includes("sdk.worker"))return;window.__welcomeSdk=this;this.addEventListener("message",({data})=>{
        if(data.kind==="state"){
          __welcomeProof.states.push(data.servers.map(s=>({id:s.id,token:s.welcome_token,status:s.status,keys:Object.keys(s)})));
          for(const u of data.updates||[])if(u.kind==="welcome")__welcomeProof.documents.push(u);
        }
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
try {
  const page=await open("live-browser",390,844),welcome=page.getByRole("region",{name:"Server welcome",exact:true});
  assert.ok((await welcome.textContent()).includes("initial.test"));
  assert.ok(!(await page.textContent("body")).includes("must-not-show-before-sync"));
  async function command(value,ack=true){
    await page.getByLabel("Message",{exact:true}).fill(value);await page.getByRole("button",{name:"Send",exact:true}).click();
    if(ack)await page.locator("article").filter({hasText:"done:"+value}).waitFor();
  }
  const initial=await page.evaluate(()=>__welcomeProof.documents[0]);
  await welcome.getByRole("button",{name:"Dismiss server welcome",exact:true}).click();await welcome.waitFor({state:"detached"});
  await command("repeat");assert.equal(await welcome.count(),0);assert.equal(await page.evaluate(()=>__welcomeProof.documents.length),1);
  await command("absent");assert.equal(await welcome.count(),0);
  await command("changed");await welcome.waitFor();assert.ok((await welcome.textContent()).includes("changed.test"));assert.ok((await welcome.textContent()).includes("<em>tag</em>"));
  assert.ok(Number(await welcome.locator(".bold").first().evaluate(e=>getComputedStyle(e).fontWeight))>=700);
  const changed=await page.evaluate(()=>__welcomeProof.documents.at(-1));assert.notEqual(changed.token,initial.token);
  await page.evaluate(token=>__welcomeSdk.postMessage({id:"fixture-stale",method:"dismiss_welcome",args:[0,token]}),initial.token);
  await page.waitForFunction(()=>__welcomeProof.results.length===1);assert.match((await page.evaluate(()=>__welcomeProof.results[0])).error,/changed/);
  if(requireSendCap){
    // Independent known-failing diagnostic until negotiated TX caps are enforced.
    await page.evaluate(()=>__welcomeSdk.postMessage({id:"fixture-retained-limit",method:"send_text",args:["x".repeat(129)]}));
    await page.waitForFunction(()=>__welcomeProof.results.length===2);
    assert.ok((await page.evaluate(()=>__welcomeProof.results[1])).error,"browser accepted a body above the negotiated send limit");
  }
  for(const[width,height]of [[320,640],[390,844],[1280,800]]){
    await viewport(page,width,height);const dismiss=await welcome.getByRole("button",{name:"Dismiss server welcome",exact:true}).boundingBox();assert.ok(dismiss.x>=0&&dismiss.x+dismiss.width<=width&&dismiss.y+dismiss.height<=height);
    await page.screenshot({path:resolve(output,`live-${width}.png`)});
  }
  await command("empty");await welcome.waitFor({state:"detached"});
  await command("absent");assert.equal(await welcome.count(),0);
  await command("original");await welcome.waitFor();assert.ok((await welcome.textContent()).includes("initial.test"));
  await command("oversized");await welcome.getByText("[Server welcome exceeds the display limit]",{exact:true}).waitFor();
  await command("delayed",false);
  await nav(page,async()=>{await page.getByRole("button",{name:"Disconnect",exact:true}).click();await welcome.waitFor({state:"detached"});await page.getByRole("button",{name:"Connect",exact:true}).click();await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected");});
  await welcome.waitFor();await new Promise(done=>setTimeout(done,800));
  assert.ok((await welcome.textContent()).includes("initial.test"));assert.ok(!(await welcome.textContent()).includes("old-delayed.test"));
  const proof=await page.evaluate(()=>__welcomeProof);assert.equal(proof.captures,0);assert.deepEqual(proof.violations,[]);assert.equal(proof.documents.length,5);
  assert.deepEqual(errors,[]);assert.deepEqual(canaries,[]);assert.ok(requests.every(url=>url.startsWith(origin+"/")||url.startsWith(origin.replace("http:","ws:")+"/")));
  await writeFile(resolve(output,"proof.json"),JSON.stringify({passed:true,site,csp,send_cap_checked:requireSendCap,documents:proof.documents,checks:["owned TLS fixture independent protobuf framing, not stock server", "pre-sync source ignored", "dismissed exact repeat stays dismissed", "absent no change", "new source and old-token rejection", "empty revokes", "oversize plain fallback", "320/390/1280 fixed Dismiss", "delayed old connection cannot replace reconnect source", "no capture/remote fetch/CSP violation"]},null,2));console.log("Live welcome proof passed");
}catch(error){await writeFile(resolve(output,"failure.json"),JSON.stringify({error:String(error),errors,canaries,pages:await Promise.all(pages.map(page=>page.evaluate(()=>({body:document.body.textContent,proof:__welcomeProof})).catch(()=>null)))},null,2));throw error;}
finally{for(const context of contexts)await context.close();await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(done=>server.close(done));}
