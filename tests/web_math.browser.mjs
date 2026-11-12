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
const retirementProof=process.env.NUSCHL_MATH_RETIREMENT_PROOF==="1";
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
    if(retirementProof&&/math-render-worker-[^/]+\.js$/.test(target)&&new URL(req.url,"http://127.0.0.1").searchParams.get("private_cold_math")==="yes")await new Promise(done=>setTimeout(done,2500));
    if(retirementProof&&/math-render-worker-[^/]+\.js$/.test(target))body=Buffer.concat([body,Buffer.from(`\nself.addEventListener("message",event=>{if(event.data?.__private_math_stall__!==true)return;console.log("PRIVATE_APP_MATH_STALL_BEGIN");const until=performance.now()+3500;while(performance.now()<until){};console.log("PRIVATE_APP_MATH_STALL_END");});`)]);
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
  const page=await context.newPage();pages.push(page);page.on("pageerror",e=>errors.push(e.message));
  await page.addInitScript(({coldMath})=>{
    window.__mathProof={jobs:0,workers:0,copied:null,violations:[],captures:0,packetKeys:[],terminated:0,ready:0};
    document.addEventListener("securitypolicyviolation",e=>__mathProof.violations.push(e.violatedDirective));
    Object.defineProperty(navigator,"clipboard",{value:{writeText:async text=>{__mathProof.copied=text;}}});
    navigator.mediaDevices.getUserMedia=()=>{__mathProof.captures++;return Promise.reject(new Error("Physical capture is forbidden in this fixture."));};
    const Base=window.Worker;
    window.Worker=class extends Base{
      constructor(url,options){const math=String(url).includes("math-render-worker");if(math&&coldMath){url=new URL(url,location.href);url.searchParams.set("private_cold_math","yes");}super(url,options);this.isMath=math;if(math){__mathProof.workers++;__mathProof.worker=this;this.addEventListener("message",({data})=>{if(data.kind==="ready")__mathProof.ready++;});}}
      postMessage(message,...rest){if(this.isMath&&message.tex!==undefined){__mathProof.jobs++;__mathProof.packetKeys.push(Object.keys(message).sort());}return super.postMessage(message,...rest);}
      terminate(){if(this.isMath)__mathProof.terminated++;return super.terminate();}
    };
  },{coldMath:retirementProof&&username==="math-phone"});
  await page.goto(`${origin}/app/#s=127.0.0.1:${port}&u=${username}`);
  if(width<=760)await page.getByRole("button",{name:"Open navigation",exact:true}).click();
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).click({timeout:20000});
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null,{timeout:20000});
  if(width<=760)await page.getByRole("button",{name:"Close navigation",exact:true}).click();
  return page;
}
async function expand(page){const more=page.getByRole("button",{name:"Show more",exact:true});for(let i=0;i<await more.count();i++)await more.nth(i).click();}
try{
  const sender=await open("math-sender",1280,800),phone=await open("math-phone",390,844);
  const fraction=String.raw`$$\frac{1}{\sqrt{x^2+1}}$$`,matrix=String.raw`$$\begin{pmatrix}a&b\\c&d\end{pmatrix}$$`;
  const wide="$$"+Array(100).fill("a").join("+")+"$$";
  const text=["Private formula proof",String.raw`Inline $a^2+b^2=c^2$ next.`,fraction,matrix,wide,
    "Literal `$code$` stays literal.","```tex",String.raw`$$\frac{not}{math}$$`,"```",
    String.raw`Malformed $$\notAnAvailableCommand{x}$$`,String.raw`Loop $$\def\loop{\loop}\loop$$`,
    String.raw`Resource $$\includegraphics{/canary-math}$$`,
    String.raw`Style $$\style{background:url(/canary-style)}{x}$$`,
    String.raw`Text $$\text{<img src=/canary-text>}$$`].join("\n");
  await sender.getByLabel("Message",{exact:true}).fill(text);await sender.getByRole("button",{name:"Send",exact:true}).click();
  let coldApp;
  for(const page of [sender,phone]){
    await page.locator("article").filter({hasText:"Private formula proof"}).waitFor({timeout:20000});await expand(page);
    if(retirementProof&&page===phone){
      await page.waitForFunction(()=>[...document.querySelectorAll(".math-render-notice")].some(node=>node.textContent.includes("source is shown")));
      await page.waitForFunction(()=>__mathProof.ready===1,null,{timeout:6000});
      const coldState=await page.evaluate(()=>({workers:__mathProof.workers,terminated:__mathProof.terminated,jobs:__mathProof.jobs,retry:document.querySelectorAll(".math-retry").length,source:document.querySelectorAll(".math-fallback").length}));
      assert.equal(coldState.workers,1);assert.equal(coldState.terminated,0);assert.ok(coldState.retry>0);assert.ok(coldState.source>0);
      for(const formula of await page.locator(".math-formula").all()){
        const retry=formula.getByRole("button",{name:"Render formula",exact:true});
        if(await retry.count()){await formula.locator("summary").click();await retry.click();await formula.locator("summary").click();}
      }
      coldApp={passed:true,worker_response_delay_ms:2500,original_job_deadline_ms:2000,source_fallback_retry_same_owner:coldState};
    }
    await page.waitForFunction(()=>document.querySelector("mfrac")&&document.querySelector("msqrt")&&document.querySelector("mtable")&&document.querySelector("msup"),null,{timeout:15000});
    await page.waitForFunction(()=>document.querySelectorAll(".math-render-notice").length>=2);
    assert.ok((await page.locator(".code-block").textContent()).includes(String.raw`$$\frac{not}{math}$$`));
    assert.equal(await page.locator(".code-block math").count(),0);
    assert.ok((await page.locator("article").textContent()).includes("`$code$`"));
    assert.equal(await page.locator("math [href],math [style],math [class],math [src],math img,math svg").count(),0);
  }
  const fractionBounds=await phone.locator("mfrac").first().evaluate(node=>[...node.children].map(child=>{const b=child.getBoundingClientRect();return{y:b.y,height:b.height,width:b.width};}));
  assert.ok(fractionBounds[0].y<fractionBounds[1].y&&fractionBounds.every(b=>b.height>0&&b.width>0),"fraction was not laid out above/below the division line");
  const fractionUI=phone.locator("[data-math-formula]").filter({has:phone.locator("mfrac")}).first();
  await fractionUI.locator("summary").click();await fractionUI.getByRole("button",{name:"Copy formula source",exact:true}).click();
  assert.equal(await phone.evaluate(()=>__mathProof.copied),fraction);
  // An unbalanced delimiter/brace stays literal in the shared content grammar;
  // keep it in its own message so it cannot pair with a later test delimiter.
  const malformed=String.raw`Original malformed source: $$\frac{$$`;
  await new Promise(done=>setTimeout(done,1100));
  await sender.getByLabel("Message",{exact:true}).fill(malformed);await sender.getByRole("button",{name:"Send",exact:true}).click();
  const literal=phone.locator("article").filter({hasText:"Original malformed source"});await literal.waitFor();
  assert.ok((await literal.textContent()).includes(malformed));assert.equal(await literal.locator("math").count(),0);
  const recovery=String.raw`After malformed input $z^2$ still renders.`;
  await new Promise(done=>setTimeout(done,1100));await sender.getByLabel("Message",{exact:true}).fill(recovery);await sender.getByRole("button",{name:"Send",exact:true}).click();
  await phone.locator("article").filter({hasText:"After malformed input"}).locator("msup").waitFor();
  const wideIndex=await phone.locator(".math-scroll:not([hidden])").evaluateAll(nodes=>nodes.findIndex(node=>node.scrollWidth>node.clientWidth+30));
  assert.ok(wideIndex>=0,"wide formula did not retain a bounded horizontal scroll region");
  await phone.locator(".math-scroll:not([hidden])").nth(wideIndex).focus();await phone.keyboard.press("ArrowRight");
  await phone.waitForFunction(index=>document.querySelectorAll(".math-scroll:not([hidden])")[index].scrollLeft>0,wideIndex);
  const geometries=[];
  for(const [width,height] of [[390,844],[320,640],[1280,800]]){
    await phone.setViewportSize({width,height});await fractionUI.scrollIntoViewIfNeeded();
    const layout=await phone.evaluate(()=>{
      const spans=[...document.querySelectorAll(".math-scroll:not([hidden])")];
      return {viewport:innerWidth,body:document.documentElement.scrollWidth,regions:spans.map(e=>{const b=e.getBoundingClientRect();return{left:b.left,right:b.right,width:b.width,scroll:e.scrollWidth,client:e.clientWidth};})};
    });
    assert.ok(layout.body<=width+1,"formula widened the page");
    for(const bounds of layout.regions)assert.ok(bounds.left>=-1&&bounds.right<=width+1,"formula scroll viewport escaped the screen");
    geometries.push({width,height,...layout});await phone.screenshot({path:resolve(output,`math-${width}.png`)});
  }
  const jobs=await phone.evaluate(()=>__mathProof.jobs);
  await phone.getByRole("button",{name:"Settings",exact:true}).first().click();
  await phone.getByLabel("Theme",{exact:true}).selectOption("light");await phone.getByLabel("Text size",{exact:true}).selectOption("20");
  await phone.getByRole("button",{name:"Close settings",exact:true}).click();
  await new Promise(done=>setTimeout(done,100));assert.equal(await phone.evaluate(()=>__mathProof.jobs),jobs,"appearance change reran formula parsers");
  await fractionUI.scrollIntoViewIfNeeded();await phone.screenshot({path:resolve(output,"math-light-20.png")});
  let retirement;
  if(retirementProof){
    const kept=await phone.locator("mfrac").count();assert.ok(kept>0);
    const draft="Unsent draft survives formula owner failure";await phone.getByLabel("Message",{exact:true}).fill(draft);
    const began=new Promise(done=>phone.on("console",message=>{if(message.text()==="PRIVATE_APP_MATH_STALL_BEGIN")done();}));
    await phone.evaluate(()=>__mathProof.worker.postMessage({__private_math_stall__:true}));await began;
    const source=String.raw`Retirement original source $$u^2+v^2=w^2$$`;
    await sender.getByLabel("Message",{exact:true}).fill(source);await sender.getByRole("button",{name:"Send",exact:true}).click();
    const stalled=phone.locator("article").filter({hasText:"Retirement original source"});await stalled.waitFor();
    await stalled.locator(".math-render-notice").filter({hasText:"unavailable"}).waitFor({timeout:5000});
    await stalled.locator("summary").click();await stalled.getByRole("button",{name:"Reload client",exact:true}).waitFor();
    assert.ok((await stalled.locator(".math-source-text").textContent()).includes("$$u^2+v^2=w^2$$"));
    assert.equal(await stalled.getByRole("button",{name:"Render formula",exact:true}).count(),0);
    assert.equal(await phone.locator("mfrac").count(),kept);assert.equal(await phone.getByLabel("Message",{exact:true}).inputValue(),draft);
    const before=await phone.evaluate(()=>({workers:__mathProof.workers,jobs:__mathProof.jobs,terminated:__mathProof.terminated}));
    assert.equal(before.workers,1);assert.equal(before.terminated,1);
    await new Promise(done=>setTimeout(done,1100));
    await sender.getByLabel("Message",{exact:true}).fill(String.raw`After retirement uncached $$q^7+r^8$$`);await sender.getByRole("button",{name:"Send",exact:true}).click();
    const later=phone.locator("article").filter({hasText:"After retirement uncached"});await later.locator(".math-render-notice").filter({hasText:"unavailable"}).waitFor();
    const after=await phone.evaluate(()=>({workers:__mathProof.workers,jobs:__mathProof.jobs,terminated:__mathProof.terminated}));assert.deepEqual(after,before);
    await phone.setViewportSize({width:320,height:640});await stalled.scrollIntoViewIfNeeded();await phone.screenshot({path:resolve(output,"math-retired-320.png")});
    retirement={passed:true,kept_fraction_nodes:kept,source_preserved:true,draft_preserved:true,retry_hidden:true,explicit_reload:true,before,after};
  }
  assert.deepEqual(errors,[]);assert.deepEqual(canaries,[]);
  assert.ok(requests.every(url=>url.startsWith(origin+"/")||url.startsWith(origin.replace("http:","ws:")+"/")));
  for(const page of pages){const state=await page.evaluate(()=>__mathProof);assert.deepEqual(state.violations,[]);assert.equal(state.captures,0);for(const keys of state.packetKeys)assert.deepEqual(keys,["display","id","tex"]);}
  const report={passed:true,site,csp,geometries,fractionBounds,formula_jobs:jobs,coldApp,retirement,checks:["real App/SDK/private Murmur text", "actual pinned Temml core Worker", "fraction/root/matrix/scripts", "inline and display", "code grammar stays literal", "original source copy without host clipboard", "macro/malformed recovery", "resource/style canaries inert", "320/390/1280 widths", "keyboard horizontal formula scroll", "font/theme native reflow without parser rerun", "no microphone calls"]};
  await writeFile(resolve(output,"proof.json"),JSON.stringify(report,null,2));console.log(JSON.stringify({passed:true,formula_jobs:jobs,checks:report.checks}));
}catch(error){
  await writeFile(resolve(output,"failure.json"),JSON.stringify({error:String(error),errors,canaries,states:await Promise.all(pages.map(page=>page.evaluate(()=>({text:document.body.textContent,proof:__mathProof})).catch(()=>null)))},null,2));throw error;
}finally{for(const context of contexts)await context.close();await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(done=>server.close(done));}
