// SPDX-License-Identifier: EUPL-1.2
// Touch behaviour of the packaged App (#202): voice toggles, drawer slide, swipes.
// Real App/SDK/worker over the owned relay and Murmur; started by tests/web_touch.py.
// node tests/web_touch.browser.mjs ENGINE SITE RELAY_URL MURMUR_PORT WRAPPER PLAYWRIGHT OUTPUT
import assert from "node:assert/strict";
import {createServer} from "node:http";
import {createConnection} from "node:net";
import {readFile,writeFile,mkdir} from "node:fs/promises";
import {resolve,extname,sep} from "node:path";
import {pathToFileURL} from "node:url";
const [engine,site,relayUrl,port,wrapper,playwright,output]=process.argv.slice(2);
assert.ok(["chromium","firefox"].includes(engine)&&site&&relayUrl&&port&&wrapper&&playwright&&output);
const relay=new URL(relayUrl);assert.equal(relay.hostname,"127.0.0.1");assert.equal(relay.protocol,"ws:");
assert.ok(Number(port)>1023&&Number(port)<65536);
const directory=resolve(site),out=resolve(output);await mkdir(out,{recursive:true});
let csp="default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const types={".html":"text/html",".js":"text/javascript",".css":"text/css",".wasm":"application/wasm",".svg":"image/svg+xml",".json":"application/json",".txt":"text/plain"};
const server=createServer(async(req,res)=>{
  try {
    const pathname=decodeURIComponent(new URL(req.url,"http://127.0.0.1").pathname);
    const mounted=pathname.startsWith("/app/")?pathname.slice(4):pathname;
    const path=resolve(directory,"."+mounted);assert.ok(path===directory||path.startsWith(directory+sep));
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
  tunnels.add(socket);tunnels.add(upstream);socket.on("close",()=>{tunnels.delete(socket);upstream.destroy();});
  upstream.on("close",()=>{tunnels.delete(upstream);socket.destroy();});socket.on("error",()=>upstream.destroy());upstream.on("error",()=>socket.destroy());
});
await new Promise(done=>server.listen(0,"127.0.0.1",done));
const origin=`http://127.0.0.1:${server.address().port}`;
csp=csp.replace("connect-src 'self'",`connect-src 'self' ${origin.replace("http:","ws:")}`);
const pw=await import(pathToFileURL(playwright).href);
const sleep=ms=>new Promise(done=>setTimeout(done,ms));
const firefoxPrefs={"media.navigator.streams.fake":true,"media.navigator.permission.disabled":true};
const browser=engine==="chromium"?await pw.chromium.launch({headless:true,executablePath:wrapper})
  :await pw.firefox.launch({headless:true,executablePath:wrapper,firefoxUserPrefs:firefoxPrefs});
const errors=[],remote=[],proof={engine,version:browser.version(),cases:[]};
const width=390,height=844;

// Touch input through the engine's own pipeline. Playwright exposes only tap();
// Firefox's Juggler has a touch command that Playwright's server session reaches.
async function touchInput(page) {
  if(engine==="chromium"){
    const cdp=await page.context().newCDPSession(page);
    return (type,x,y)=>cdp.send("Input.dispatchTouchEvent",{type,touchPoints:type==="touchEnd"?[]:[{x,y}]});
  }
  const session=page._connection.toImpl(page).delegate.rawTouchscreen._client;
  return (type,x,y)=>session.send("Page.dispatchTouchEvent",{type,touchPoints:[{x,y}],modifiers:0});
}
async function swipe(touch,from,to,{steps=12,interval=16,lift=true}={}) {
  await touch("touchStart",from.x,from.y);
  for(let i=1;i<=steps;i++){await sleep(interval);await touch("touchMove",from.x+(to.x-from.x)*i/steps,from.y+(to.y-from.y)*i/steps);}
  if(lift){await sleep(interval);await touch("touchEnd",to.x,to.y);}
}
// Started before an action: pauses the animations it starts under `selector` at
// `fraction` of their duration so a screenshot shows a frame mid-way; resume()
// lets them finish.
function freeze(page,selector,fraction=.5) {
  return page.evaluate(async([selector,fraction])=>{
    const deadline=performance.now()+3000;
    while(performance.now()<deadline){
      const running=document.getAnimations().filter(a=>a.effect?.getTiming().duration>0&&a.playState==="running"&&a.effect.target?.closest?.(selector));
      if(running.length){
        window.__frozen=running;
        return running.map(a=>{const d=a.effect.getTiming().duration;a.pause();a.currentTime=d*fraction;return {target:a.effect.target.tagName+"."+a.effect.target.getAttribute("class"),property:a.transitionProperty||"keyframes",duration:d};});
      }
      await new Promise(done=>requestAnimationFrame(done));
    }
    return [];
  },[selector,fraction]);
}
const resume=page=>page.evaluate(()=>{for(const a of window.__frozen||[])a.play();window.__frozen=[];});
// Svelte starts a transition's animation a frame after the change; wait for it to begin, then end.
const settled=async page=>{
  await page.evaluate(()=>new Promise(done=>requestAnimationFrame(()=>requestAnimationFrame(()=>requestAnimationFrame(done)))));
  await page.waitForFunction(()=>!document.getAnimations().some(a=>a.playState==="running"&&a.effect?.getTiming().duration>0),null,{timeout:5000});
};
const drawer=page=>page.evaluate(()=>{const d=document.querySelector("dialog.navigation-drawer");if(!d)return null;const r=d.getBoundingClientRect();return {open:d.open,left:Math.round(r.left),width:Math.round(r.width),inline:d.style.transform};});
const shot=(page,name)=>page.screenshot({path:resolve(out,`${name}.png`)});
async function toggleState(page,label) {
  return page.evaluate(label=>{
    const button=document.querySelector(`.voice-actions button[aria-label="${label}"]`);
    const strike=button?.querySelector(".strike"),s=strike&&getComputedStyle(strike),b=getComputedStyle(button);
    return {label,pressed:button?.getAttribute("aria-pressed"),hovered:button?.matches(":hover"),background:b.backgroundColor,color:b.color,
      strikeOffset:s?.strokeDashoffset,strikeVisibility:s?.visibility,transitions:document.getAnimations().filter(a=>a.effect?.target===strike&&a.effect.getTiming().duration>0).length};
  },label);
}

async function run(motion) {
  const reduced=motion==="reduce",tag=`${engine}-${reduced?"reduced":"motion"}`;
  const context=await browser.newContext({viewport:{width,height},deviceScaleFactor:2,hasTouch:true,...(engine==="chromium"?{isMobile:true,permissions:["microphone"]}:{}),colorScheme:"dark",reducedMotion:motion});
  context.on("request",request=>{if(!request.url().startsWith(origin+"/"))remote.push(request.url());});
  await context.route("**/*",route=>route.request().url().startsWith(origin+"/")?route.continue():route.abort());
  const page=await context.newPage();page.on("pageerror",error=>errors.push(`${tag}: ${error.message}`));
  await page.addInitScript(()=>{window.__csp=[];document.addEventListener("securitypolicyviolation",e=>window.__csp.push(e.violatedDirective));});
  try { await scenario(page,tag,reduced,motion); }
  catch(error) { await shot(page,`${tag}-failure`).catch(()=>{}); throw error; }
  finally { await context.close(); }
}

async function scenario(page,tag,reduced,motion) {
  const touch=await touchInput(page),record={motion,steps:[]};
  const step=(name,data={})=>{record.steps.push({name,...data});};
  // No gesture may leave the client (history swipes, pull to refresh).
  const navigations=[];page.on("framenavigated",frame=>{if(frame===page.mainFrame())navigations.push(frame.url());});
  await page.goto(`${origin}/app/#s=127.0.0.1:${port}&u=touch-${engine}-${reduced?"r":"m"}`);
  record.media=await page.evaluate(()=>({hover:matchMedia("(hover: hover)").matches,coarse:matchMedia("(pointer: coarse)").matches,reduced:matchMedia("(prefers-reduced-motion: reduce)").matches}));
  assert.equal(record.media.reduced,reduced);

  // The menu button opens the drawer with a slide; the certificate prompt lives in it.
  let frozen=!reduced&&freeze(page,"dialog.navigation-drawer");
  await page.getByRole("button",{name:"Open navigation",exact:true}).tap();
  if(!reduced){step("menu-open",{frozen:await frozen,drawer:await drawer(page)});await shot(page,`${tag}-01-menu-open-mid`);await resume(page);}
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).tap({timeout:20000});
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null,{timeout:20000});
  await settled(page);
  frozen=!reduced&&freeze(page,"dialog.navigation-drawer");
  await page.getByRole("button",{name:"Close navigation",exact:true}).tap();
  if(!reduced){
    frozen=await frozen;assert.ok(frozen.length,"close button: no slide-out");
    const mid=await drawer(page);assert.ok(mid?.open&&mid.left<-20&&mid.left>-mid.width+20,`close button: mid frame ${JSON.stringify(mid)}`);
    step("close-button",{frozen,drawer:mid});await shot(page,`${tag}-02-close-button-mid`);await resume(page);
  }
  await page.locator("dialog.navigation-drawer").waitFor({state:"detached"});
  assert.equal(await page.evaluate(()=>document.activeElement?.getAttribute("aria-label")),"Open navigation","close button: focus not returned");

  // Synthetic text so the timeline scrolls, plus a code block wider than the screen.
  const composer=page.getByLabel("Message",{exact:true});
  for(let i=1;i<=12;i++){
    await composer.fill(`Synthetic touch line ${i}\nsecond line ${i}`);
    await page.getByRole("button",{name:"Send",exact:true}).tap();
    await page.waitForFunction(()=>document.querySelector("#message").value==="",null,{timeout:10000});
  }
  await composer.fill("```text\nlet wide = \""+"0123456789 abcdefghij ".repeat(12)+"\";\n```");
  await page.getByRole("button",{name:"Send",exact:true}).tap();
  await page.waitForFunction(()=>document.querySelector("#message").value==="",null,{timeout:10000});
  await page.locator("article pre").last().waitFor();
  await page.evaluate(()=>document.activeElement?.blur());

  // Voice: joined self-muted, so the microphone starts struck.
  await page.getByRole("button",{name:"Join voice",exact:true}).tap();
  await page.getByRole("button",{name:"Deafen",exact:true}).waitFor({timeout:20000});
  await page.waitForFunction(()=>!document.querySelector('.voice-actions button[aria-label="Deafen"]').disabled);
  // Until audio is ready the status may still read "Sound off"; it does not affect the toggles.
  await page.waitForFunction(()=>!/Sound off|Joining/.test(document.querySelector(".voice-description small")?.textContent),null,{timeout:3000}).catch(()=>{});
  await settled(page);
  const voiceBar=page.getByRole("region",{name:"Voice controls"});
  const before={mic:await toggleState(page,"Unmute microphone"),deafen:await toggleState(page,"Deafen")};
  assert.equal(before.mic.pressed,"true");assert.equal(before.mic.strikeVisibility,"visible");
  assert.equal(before.deafen.pressed,"false");assert.equal(before.deafen.strikeVisibility,"hidden");
  step("voice-before",before);await shot(page,`${tag}-03-voice-before`);await voiceBar.screenshot({path:resolve(out,`${tag}-03-voice-before-bar.png`)});

  // Tap Deafen: pressed red tint, and the headphones' strike draws in.
  frozen=!reduced&&freeze(page,".voice-actions");
  await page.getByRole("button",{name:"Deafen",exact:true}).tap();
  await page.locator('.voice-actions button[aria-label="Enable sound"][aria-pressed="true"]').waitFor();
  if(!reduced){
    frozen=await frozen;
    assert.ok(frozen.some(a=>a.property==="stroke-dashoffset"&&a.duration===180),`deafen: strike not animated ${JSON.stringify(frozen)}`);
    step("deafen-mid",{frozen,state:await toggleState(page,"Enable sound")});
    await voiceBar.screenshot({path:resolve(out,`${tag}-04-deafen-mid-bar.png`)});await resume(page);
  } else assert.equal((await toggleState(page,"Enable sound")).transitions,0,"reduced motion: strike animated");
  await page.waitForFunction(()=>!document.querySelector('.voice-actions button[aria-label="Enable sound"]').disabled);
  await settled(page);
  const deafened=await toggleState(page,"Enable sound");
  assert.equal(deafened.strikeVisibility,"visible");assert.equal(parseFloat(deafened.strikeOffset),0);
  assert.notEqual(deafened.background,before.deafen.background,"deafen: pressed looks unpressed");
  step("deafened",{state:deafened,mic:await toggleState(page,"Unmute microphone")});
  await shot(page,`${tag}-05-deafened`);await voiceBar.screenshot({path:resolve(out,`${tag}-05-deafened-bar.png`)});
  const marker=page.locator('.channel-tree .own-user span[title="Deafened"] svg.struck');
  // The own row in the channel list shows the struck headphones (checked in the drawer below).

  // Tap again: restored, the strike draws out.
  frozen=!reduced&&freeze(page,".voice-actions");
  await page.getByRole("button",{name:"Enable sound",exact:true}).tap();
  await page.locator('.voice-actions button[aria-label="Deafen"][aria-pressed="false"]').waitFor();
  if(!reduced){step("undeafen-mid",{frozen:await frozen});await voiceBar.screenshot({path:resolve(out,`${tag}-06-undeafen-mid-bar.png`)});await resume(page);}
  await page.waitForFunction(()=>!document.querySelector('.voice-actions button[aria-label="Deafen"]').disabled);
  await settled(page);
  const restored=await toggleState(page,"Deafen");
  assert.equal(restored.strikeVisibility,"hidden");assert.equal(restored.background,before.deafen.background);
  step("restored",{state:restored});await voiceBar.screenshot({path:resolve(out,`${tag}-07-restored-bar.png`)});

  // Unmute (fake stream only): the microphone's strike draws out.
  await page.getByRole("button",{name:"Unmute microphone",exact:true}).tap();
  const unmuted=await page.locator('.voice-actions button[aria-label="Mute microphone"][aria-pressed="false"]').waitFor({timeout:10000}).then(()=>true,()=>false);
  if(unmuted){await page.waitForFunction(()=>!document.querySelector('.voice-actions button[aria-label="Mute microphone"]').disabled);await settled(page);
    const state=await toggleState(page,"Mute microphone");assert.equal(state.strikeVisibility,"hidden");step("unmuted",{state});
    await voiceBar.screenshot({path:resolve(out,`${tag}-08-unmuted-bar.png`)});
    await page.getByRole("button",{name:"Mute microphone",exact:true}).tap();await page.getByRole("button",{name:"Unmute microphone",exact:true}).waitFor();
  } else step("unmuted",{skipped:"the microphone did not become available"});
  await page.locator(".messages").evaluate(node=>{node.scrollTop=node.scrollHeight;});
  await settled(page);

  // Swipe right on the timeline: the drawer slides in.
  const messages=await page.locator(".messages").boundingBox();
  const y=Math.round(messages.y+messages.height*.35);
  // A short, slow drag is not a swipe.
  await swipe(touch,{x:120,y},{x:150,y},{steps:10,interval:40});await sleep(300);
  assert.equal(await drawer(page),null,"short drag opened the drawer");step("short-drag",{opened:false});
  frozen=!reduced&&freeze(page,"dialog.navigation-drawer");
  await swipe(touch,{x:60,y},{x:300,y:y+8});
  if(!reduced){
    frozen=await frozen;assert.ok(frozen.length,"swipe: no slide-in");
    const mid=await drawer(page);assert.ok(mid.open&&mid.left<-20&&mid.left>-mid.width+20,`swipe open: mid frame ${JSON.stringify(mid)}`);
    step("swipe-open-mid",{frozen,drawer:mid});await shot(page,`${tag}-09-swipe-open-mid`);await resume(page);
  } else {
    const now=await drawer(page);assert.ok(now?.open&&now.left===0,`reduced: drawer not immediately open ${JSON.stringify(now)}`);
    assert.equal(await page.evaluate(()=>document.getAnimations().filter(a=>a.effect?.getTiming().duration>0).length),0,"reduced: drawer animated");
  }
  await settled(page);
  const open=await drawer(page);assert.ok(open?.open&&open.left===0,`swipe open: ${JSON.stringify(open)}`);
  assert.equal(await marker.count(),0,"own row shows deafened after restoring");
  step("swipe-open",{drawer:open});await shot(page,`${tag}-10-swipe-open`);

  // Swipe left on the drawer: it follows the finger, then slides out.
  const dy=Math.round(height*.6);
  await swipe(touch,{x:300,y:dy},{x:160,y:dy+6},{lift:false});await sleep(50);
  const dragging=await drawer(page);
  if(!reduced)assert.ok(dragging.left<=-120&&dragging.left>=-150,`drag: drawer did not follow ${JSON.stringify(dragging)}`);
  else assert.equal(dragging.left,0,"reduced: drawer followed the finger");
  step("drag-follow",{drawer:dragging});await shot(page,`${tag}-11-drag-follow`);
  frozen=!reduced&&freeze(page,"dialog.navigation-drawer");
  await touch("touchEnd",160,dy+6);
  if(!reduced){
    frozen=await frozen;assert.ok(frozen.length,"swipe close: no slide-out");
    const mid=await drawer(page);assert.ok(mid.left<dragging.left,`swipe close continued backwards ${JSON.stringify(mid)}`);
    step("swipe-close-mid",{frozen,drawer:mid});await shot(page,`${tag}-12-swipe-close-mid`);await resume(page);
  }
  await page.locator("dialog.navigation-drawer").waitFor({state:"detached"});
  assert.equal(await page.evaluate(()=>document.activeElement?.getAttribute("aria-label")),"Open navigation","swipe close: focus not returned");
  step("swipe-closed",{});await shot(page,`${tag}-13-swipe-closed`);

  // A partial drag springs back and the drawer stays.
  await swipe(touch,{x:60,y},{x:300,y:y+8});await settled(page);
  await swipe(touch,{x:300,y:dy},{x:270,y:dy},{steps:10,interval:40});await settled(page);
  const kept=await drawer(page);assert.ok(kept?.open&&kept.left===0,`partial drag closed: ${JSON.stringify(kept)}`);step("partial-drag",{drawer:kept});
  // Backdrop tap closes it, also with the slide.
  frozen=!reduced&&freeze(page,"dialog.navigation-drawer");
  await touch("touchStart",width-10,height/2);await touch("touchEnd",width-10,height/2);
  if(!reduced){frozen=await frozen;assert.ok(frozen.length,"backdrop: no slide-out");step("backdrop-mid",{frozen,drawer:await drawer(page)});await resume(page);}
  await page.locator("dialog.navigation-drawer").waitFor({state:"detached"});

  // From the very left edge (within 24 px) the swipe works too; Escape closes it.
  await swipe(touch,{x:12,y},{x:260,y});await settled(page);
  assert.ok((await drawer(page))?.open,"edge swipe did not open");step("edge-swipe",{opened:true});
  frozen=!reduced&&freeze(page,"dialog.navigation-drawer");
  await page.keyboard.press("Escape");
  if(!reduced){frozen=await frozen;step("escape-mid",{frozen});assert.ok(frozen.length,"Escape: no slide-out");await resume(page);}
  await page.locator("dialog.navigation-drawer").waitFor({state:"detached"});
  assert.equal(await page.evaluate(()=>document.activeElement?.getAttribute("aria-label")),"Open navigation","Escape: focus not returned");

  // Vertical scrolling stays scrolling.
  const top=await page.locator(".messages").evaluate(node=>node.scrollTop);
  await swipe(touch,{x:200,y:Math.round(messages.y+80)},{x:212,y:Math.round(messages.y+messages.height-40)});await sleep(400);
  const scrolled=await page.locator(".messages").evaluate(node=>node.scrollTop);
  assert.equal(await drawer(page),null,"vertical drag opened the drawer");
  step("vertical-drag",{opened:false,scrollTopBefore:top,scrollTopAfter:scrolled});
  if(engine==="chromium")assert.ok(scrolled<top,"vertical drag did not scroll the timeline");

  // A sideways drag on a wide code block scrolls the code instead.
  const pre=page.locator("article pre").last();
  await pre.evaluate(node=>{node.scrollIntoView({block:"center",behavior:"instant"});node.scrollLeft=node.scrollWidth;});await sleep(300);
  const box=await pre.boundingBox(),left=await pre.evaluate(node=>node.scrollLeft);
  assert.ok(left>0&&await pre.evaluate((node,[x,y])=>node.contains(document.elementFromPoint(x,y)),[box.x+30,box.y+box.height/2]),"the drag does not start on the code");
  await swipe(touch,{x:Math.round(box.x+30),y:Math.round(box.y+box.height/2)},{x:Math.round(box.x+box.width-20),y:Math.round(box.y+box.height/2)});await sleep(400);
  const after=await pre.evaluate(node=>node.scrollLeft);
  assert.equal(await drawer(page),null,"code block drag opened the drawer");
  step("code-drag",{opened:false,scrollLeftBefore:left,scrollLeftAfter:after});
  if(engine==="chromium")assert.ok(after<left,"code block did not scroll sideways");
  await shot(page,`${tag}-14-code-drag`);

  // A swipe that starts in the composer stays with the text field.
  const field=await composer.boundingBox();
  await swipe(touch,{x:Math.round(field.x+20),y:Math.round(field.y+field.height/2)},{x:Math.round(field.x+260),y:Math.round(field.y+field.height/2)});await sleep(300);
  assert.equal(await drawer(page),null,"composer drag opened the drawer");step("composer-drag",{opened:false});

  // Deafened marker in the channel list uses the struck headphones.
  await page.getByRole("button",{name:"Deafen",exact:true}).tap();
  await page.locator('.voice-actions button[aria-label="Enable sound"]:not(:disabled)').waitFor();
  await page.getByRole("button",{name:"Open navigation",exact:true}).tap();await settled(page);
  await marker.waitFor();step("deafened-marker",{struck:await marker.count()});
  await page.locator(".channel-tree .own-user").screenshot({path:resolve(out,`${tag}-15-deafened-marker.png`)});
  await page.getByRole("button",{name:"Close navigation",exact:true}).tap();
  await page.locator("dialog.navigation-drawer").waitFor({state:"detached"});

  await sleep(500);assert.equal(navigations.length,1,`${tag}: a gesture navigated: ${JSON.stringify(navigations)}`);
  assert.deepEqual(await page.evaluate(()=>window.__csp),[],`${tag}: CSP violations`);
  proof.cases.push(record);
}

try {
  for(const motion of ["no-preference","reduce"])await run(motion);
  assert.deepEqual(errors,[],"browser errors");assert.deepEqual(remote,[],"remote requests");
  await writeFile(resolve(out,"proof.json"),JSON.stringify({...proof,origin,errors,remote,passed:true},null,2)+"\n");
  console.log(`web touch (${engine} ${proof.version}): voice toggles, drawer slide, swipes and reduced motion passed.`);
} catch(error) {
  await writeFile(resolve(out,"proof.json"),JSON.stringify({...proof,origin,errors,remote,passed:false,failure:String(error?.stack||error)},null,2)+"\n");
  throw error;
} finally {
  await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(done=>server.close(done));
}
