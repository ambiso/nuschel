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
async function fontPreference(){
  const page=await open("font-browser",1280,800);
  await page.getByLabel("Message",{exact:true}).fill("Actual message font preference");
  await page.getByRole("button",{name:"Send",exact:true}).click();
  const message=page.locator("article").filter({hasText:"Actual message font preference"}).locator(".text");
  await message.waitFor();const sizes=[];
  for(const size of [14,16,20]){
    await page.getByRole("button",{name:"Settings",exact:true}).first().click();
    await page.getByLabel("Text size",{exact:true}).selectOption(String(size));
    await page.getByRole("button",{name:"Close settings",exact:true}).click();
    await page.waitForFunction(expected=>getComputedStyle(document.documentElement).fontSize===`${expected}px`,size);
    const measured=await message.evaluate(e=>({root:getComputedStyle(document.documentElement).fontSize,text:parseFloat(getComputedStyle(e).fontSize)}));
    assert.equal(measured.root,`${size}px`);assert.ok(Math.abs(measured.text-size*.9)<.02);sizes.push({size,...measured});
  }
  await page.reload();
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null,{timeout:20000});
  await message.waitFor();assert.equal(await page.evaluate(()=>getComputedStyle(document.documentElement).fontSize),"20px");
  assert.ok(Math.abs(Number(await message.evaluate(e=>parseFloat(getComputedStyle(e).fontSize)))-18)<.02);
  const bounds=[];
  for(const[width,height]of [[320,640],[390,844],[1280,800]]){
    await page.setViewportSize({width,height});
    const layout=await page.evaluate(()=>{const box=e=>{const b=e.getBoundingClientRect();return{x:b.x,right:b.right,y:b.y,bottom:b.bottom,height:b.height}};return{width:innerWidth,pageWidth:document.documentElement.scrollWidth,panel:box(document.querySelector(".server-welcome")),dismiss:box(document.querySelector(".server-welcome>header button")),composer:box(document.querySelector(".composer"))};});
    assert.ok(layout.pageWidth<=width);assert.ok(layout.dismiss.x>=0&&layout.dismiss.right<=width&&layout.dismiss.bottom<=layout.panel.bottom);assert.ok(layout.composer.bottom<=height+1);bounds.push(layout);
    await page.screenshot({path:resolve(output,`welcome-real-20px-${width}.png`)});
  }
  return{sizes,persisted_after_reload:true,bounds};
}
try{
  const page=await open("welcome-browser",390,844),welcome=page.getByRole("region",{name:"Server welcome",exact:true});
  const bold=welcome.locator(".bold").filter({hasText:"something.de"});await bold.waitFor();
  assert.equal(await bold.textContent(),"something.de");assert.ok(Number(await bold.evaluate(e=>getComputedStyle(e).fontWeight))>=700);
  const text=await welcome.textContent();assert.ok(text.includes("<em>literal.test</em>"));assert.ok(!text.includes("bsomething.de/b"));assert.ok(!text.includes("hiddenWelcome"));
  assert.equal(await welcome.locator("img,script,iframe,svg").count(),0);
  const link=welcome.getByRole("link",{name:"linked.test",exact:true});assert.equal(await link.getAttribute("href"),"https://linked.test/");
  assert.equal(await link.getAttribute("referrerpolicy"),"no-referrer");assert.ok(Number(await link.evaluate(e=>getComputedStyle(e).fontWeight))>=700);
  assert.ok(await welcome.locator('pre[aria-label="rust code"] span').count()>1);
  const more=welcome.getByRole("button",{name:"Show more",exact:true});if(await more.count())await more.click();
  assert.ok((await welcome.textContent()).includes("Welcome line 34"));
  assert.ok((await welcome.locator(".typed-document").textContent()).includes("\n\n"));
  const geometries=[];
  for(const [width,height]of [[320,640],[390,844],[1280,800],[900,600]]){
    await page.setViewportSize({width,height});
    const body=welcome.getByRole("region",{name:"Welcome message",exact:true});await body.focus();await page.keyboard.press("End");
    await page.waitForFunction(()=>document.querySelector(".server-welcome-body").scrollTop>0);
    const geometry=await page.evaluate(()=>{
      const box=e=>{const b=e.getBoundingClientRect();return{x:b.x,y:b.y,right:b.right,bottom:b.bottom,width:b.width,height:b.height}};
      return{width:innerWidth,height:innerHeight,pageWidth:document.documentElement.scrollWidth,panel:box(document.querySelector(".server-welcome")),dismiss:box(document.querySelector(".server-welcome>header button")),body:box(document.querySelector(".server-welcome-body")),composer:box(document.querySelector(".composer"))};
    });
    assert.ok(geometry.pageWidth<=width+1);assert.ok(geometry.panel.height<=height*.32+2);
    assert.ok(geometry.dismiss.x>=0&&geometry.dismiss.right<=width&&geometry.dismiss.y>=0&&geometry.dismiss.bottom<=height);
    assert.ok(geometry.composer.bottom<=height+1&&geometry.composer.height>50);geometries.push(geometry);
    await page.screenshot({path:resolve(output,`welcome-${width}.png`)});
  }
  await page.setViewportSize({width:390,height:844});
  await nav(page,async()=>{await page.getByRole("button",{name:"Lobby",exact:true}).click();});
  await nav(page,async()=>{await page.getByRole("button",{name:"Root",exact:true}).click();});
  const before=await page.evaluate(()=>__welcomeProof.documents.length);assert.equal(before,1,"ordinary browsing resent welcome document");
  const original=await page.evaluate(()=>__welcomeProof.documents[0]);
  await welcome.getByRole("button",{name:"Dismiss server welcome",exact:true}).focus();await page.keyboard.press("Enter");await welcome.waitFor({state:"detached"});
  await page.getByLabel("Message",{exact:true}).fill("Draft preserved while welcome is dismissed");
  await nav(page,async()=>{
    await page.getByRole("button",{name:"Disconnect",exact:true}).click();
    await welcome.waitFor({state:"detached"});
    await page.getByRole("button",{name:"Connect",exact:true}).click();
    await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected");
  });
  await welcome.waitFor();assert.equal(await page.getByLabel("Message",{exact:true}).inputValue(),"Draft preserved while welcome is dismissed");
  const next=await page.evaluate(()=>__welcomeProof.documents.at(-1));assert.notEqual(next.token,original.token);
  // Real compiled SDK command validates an old UI token after reconnect.
  await page.evaluate(token=>__welcomeSdk.postMessage({id:"fixture-stale-dismiss",method:"dismiss_welcome",args:[0,token]}),original.token);
  await page.waitForFunction(()=>__welcomeProof.results.length===1);assert.match((await page.evaluate(()=>__welcomeProof.results[0])).error,/changed/);
  assert.equal(await welcome.count(),1);assert.equal(await welcome.locator(".bold").filter({hasText:"something.de"}).count(),1);
  await page.screenshot({path:resolve(output,"welcome-reconnected-390.png")});
  await page.getByRole("button",{name:"Open navigation",exact:true}).click();
  await page.getByRole("button",{name:"Settings",exact:true}).first().click();
  await page.getByLabel("Text size",{exact:true}).selectOption("20");
  await page.getByRole("button",{name:"Edit server bookmark",exact:true}).click();
  const longName="Long server éλ".repeat(8);
  await page.getByLabel("Display name",{exact:false}).fill(longName);
  await page.getByLabel("Your name",{exact:false}).fill("welcome-replacement");
  await page.getByRole("button",{name:"Connect",exact:true}).click();
  await welcome.waitFor();
  await page.waitForFunction(()=>__welcomeProof.documents.length===3);
  await page.setViewportSize({width:320,height:640});
  const longBounds=await welcome.evaluate(e=>{const box=n=>{const b=n.getBoundingClientRect();return{x:b.x,right:b.right,y:b.y,bottom:b.bottom,height:b.height}};return{panel:box(e),dismiss:box(e.querySelector("header button")),body:box(e.querySelector(".server-welcome-body"))};});
  assert.ok(longBounds.dismiss.x>=0&&longBounds.dismiss.right<=320&&longBounds.dismiss.bottom<longBounds.panel.bottom);
  assert.ok(longBounds.panel.height<=640*.32+2);
  await page.screenshot({path:resolve(output,"welcome-long-name-20px-320.png")});
  await page.evaluate(token=>__welcomeSdk.postMessage({id:"fixture-replaced-dismiss",method:"dismiss_welcome",args:[0,token]}),next.token);
  await page.waitForFunction(()=>__welcomeProof.results.length===2);
  assert.match((await page.evaluate(()=>__welcomeProof.results[1])).error,/changed/);
  assert.equal(await welcome.count(),1);
  const preferenceFonts=await page.evaluate(()=>({
    selected:Number(JSON.parse(localStorage.getItem("nuschl.appearance.v1")).fontSize),
    root:getComputedStyle(document.documentElement).fontSize,body:getComputedStyle(document.body).fontSize,
    welcome:getComputedStyle(document.querySelector(".server-welcome .text")).fontSize
  }));
  // An explicit CSS root-font stress, not a claim of physical mobile/browser zoom.
  await page.evaluate(()=>document.documentElement.style.fontSize="32px");
  const rootFontStress=await welcome.evaluate(e=>{const b=e.getBoundingClientRect(),d=e.querySelector("header button").getBoundingClientRect();return{root:getComputedStyle(document.documentElement).fontSize,pageWidth:document.documentElement.scrollWidth,panelBottom:b.bottom,dismiss:{x:d.x,right:d.right,bottom:d.bottom}};});
  assert.equal(rootFontStress.root,"32px");assert.ok(rootFontStress.pageWidth<=320);
  assert.ok(rootFontStress.dismiss.x>=0&&rootFontStress.dismiss.right<=320&&rootFontStress.dismiss.bottom<=rootFontStress.panelBottom);
  await page.screenshot({path:resolve(output,"welcome-root-font-stress-320.png")});
  const fontPreferenceProof=await fontPreference();
  const proof=await page.evaluate(()=>__welcomeProof);assert.equal(proof.documents.length,3);assert.equal(proof.captures,0);assert.deepEqual(proof.violations,[]);
  for(const state of proof.states)for(const server of state)assert.ok(!server.keys.includes("welcome")&&!server.keys.includes("document"));
  assert.deepEqual(errors,[]);assert.deepEqual(canaries,[]);
  assert.ok(requests.every(url=>url.startsWith(origin+"/")||url.startsWith(origin.replace("http:","ws:")+"/")));
  await writeFile(resolve(output,"proof.json"),JSON.stringify({passed:true,site,csp,geometries,longBounds,preferenceFonts,rootFontStress,fontPreferenceProof,document_deliveries:proof.documents.length,documents:proof.documents,checks:["private stock Murmur→real TLS/WASM SDK→typed App", "bold exact domain and safe explicit link", "escaped tags/newlines/highlighted code", "inert script/remote+same-origin images", "single document delivery per connection", "320/390/900/1280 bounded panel/fixed Dismiss/reachable composer", "keyboard body scroll+Dismiss", "reconnect refresh and stale token refusal", "draft preserved", "no capture/remote requests/CSP violations"]},null,2));
  console.log("Welcome App proof passed");
}catch(error){await writeFile(resolve(output,"failure.json"),JSON.stringify({error:String(error),errors,canaries,pages:await Promise.all(pages.map(page=>page.evaluate(()=>({body:document.body.textContent,proof:__welcomeProof})).catch(()=>null)))},null,2));throw error;}
finally{for(const context of contexts)await context.close();await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(done=>server.close(done));}
