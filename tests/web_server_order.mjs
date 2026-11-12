// SPDX-License-Identifier: EUPL-1.2
// Exact production App + WASM + IndexedDB, offline bookmarks only. Real Chromium
// mouse/touch gestures under production CSP; no fake SDK and no host devices.
import assert from "node:assert/strict";
import {createServer} from "node:http";
import {readFile,mkdir,writeFile} from "node:fs/promises";
import {resolve,extname,sep} from "node:path";
import {pathToFileURL} from "node:url";
const [site,output]=process.argv.slice(2);assert.ok(site&&output);
const directory=resolve(site),out=resolve(output);await mkdir(out,{recursive:true});
const types={".html":"text/html",".js":"text/javascript",".css":"text/css",".wasm":"application/wasm",".svg":"image/svg+xml"};
let csp="default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const server=createServer(async(req,res)=>{
 try {
  const path=decodeURIComponent(new URL(req.url,"http://127.0.0.1").pathname);
  if(path==="/seed.html"){res.writeHead(200,{"Content-Type":"text/html","Content-Security-Policy":csp});res.end("<!doctype html><title>Private seed</title>");return;}
  const mounted=path.startsWith("/app/")?path.slice(4):path,target=resolve(directory,"."+mounted);assert.ok(target===directory||target.startsWith(directory+sep));
  const file=extname(mounted)?target:resolve(target,"index.html");
  const bytes=await readFile(file);
  res.writeHead(200,{"Content-Type":types[extname(file)]||"application/octet-stream","Content-Security-Policy":csp,"Referrer-Policy":"no-referrer","Permissions-Policy":"camera=(), microphone=(), geolocation=()","Cross-Origin-Opener-Policy":"same-origin"});res.end(bytes);
 }catch{res.writeHead(404);res.end();}
});
await new Promise(done=>server.listen(0,"127.0.0.1",done));
const origin=`http://127.0.0.1:${server.address().port}`;csp=csp.replace("connect-src 'self'",`connect-src 'self' ${origin.replace("http:","ws:")}`);
const {chromium}=await import(pathToFileURL(process.env.NUSCHL_PLAYWRIGHT||"/nix/store/phc5gsqnm205r6hvlay77f7ia3w4bzzk-playwright-core-1.64.1/index.mjs"));
const browser=await chromium.launch({headless:true,executablePath:process.env.NUSCHL_CHROMIUM||"/nix/store/fqriy9vihl23l4md56qgiks8z0d2xdlw-chromium-156.0.8078.79/bin/chromium"});
const proofs=[];
async function until(predicate,message){const end=Date.now()+10000;while(Date.now()<end){if(await predicate())return;await new Promise(done=>setTimeout(done,50));}throw new Error(message);}
try {
 for(const [name,width,height,touch] of [["desktop",1440,960,false],["phone-320",320,640,true],["phone-390",390,844,true]]){
  const context=await browser.newContext({viewport:{width,height},hasTouch:touch,reducedMotion:touch?"reduce":"no-preference"});
  const page=await context.newPage(),errors=[],remote=[],sockets=[];
  page.on("pageerror",error=>errors.push(error.message));page.on("request",request=>{if(!request.url().startsWith(origin+"/"))remote.push(request.url());});page.on("websocket",socket=>sockets.push(socket.url()));
  await page.addInitScript(()=>{
   const proof=window.__orderProbe={state:null,commands:[],violations:[],microphoneCalls:0,pointers:[]};
   for(const kind of ["pointerdown","pointermove","pointerup","pointercancel","gotpointercapture","lostpointercapture"])document.addEventListener(kind,event=>{if(proof.pointers.length<80)proof.pointers.push({kind,target:event.target.className,id:event.pointerId,x:event.clientX,y:event.clientY});},true);
   document.addEventListener("securitypolicyviolation",event=>proof.violations.push(event.violatedDirective));
   if(navigator.mediaDevices)navigator.mediaDevices.getUserMedia=()=>{proof.microphoneCalls++;return Promise.reject(new Error("Ordering fixture has no devices"));};
   const Worker=window.Worker;window.Worker=class extends Worker{
    constructor(url,options){super(url,options);if(String(url).includes("sdk.worker"))this.addEventListener("message",({data})=>{if(data.kind==="state")proof.state=data;});}
    postMessage(message,...rest){if(message?.method)proof.commands.push({method:message.method,args:message.args});return super.postMessage(message,...rest);}
   };
  });
  await page.goto(origin+"/seed.html");
  const books=Array.from({length:12},(_,index)=>({id:`web:tester@127.0.0.1:${22000+index}`,name:index===0?"Alpha":index===1?"Beta":index===2?"Gamma":`Group ${index}`,host:"127.0.0.1",port:22000+index,username:"tester",certificate_sha256:null,cached_channels:[{id:0,parent:null,name:"Root",position:0},{id:1,parent:0,name:"Lounge",position:0}],tokens:[],auto_connect:false,icon:null,icon_checked_ms:0,last_channel:null,managed:false}));
  await page.evaluate(async books=>{
   const opening=indexedDB.open("nuschl-observed-history-v1",1);
   opening.onupgradeneeded=()=>{const db=opening.result,rows=db.createObjectStore("messages",{keyPath:"id",autoIncrement:true});rows.createIndex("scope_time",["message.server","message.channel","message.observed_at_ms","id"],{unique:true});db.createObjectStore("settings");};
   const db=await new Promise((done,fail)=>{opening.onsuccess=()=>done(opening.result);opening.onerror=()=>fail(opening.error);});
   const tx=db.transaction("settings","readwrite");tx.objectStore("settings").put(JSON.stringify(books),"bookmarks");await new Promise((done,fail)=>{tx.oncomplete=done;tx.onabort=tx.onerror=fail;});db.close();
  },books);
  const ids=books.map(book=>book.id),alpha=ids[0],beta=ids[1],gamma=ids[2];
  await page.goto(origin+"/app/");
  await page.waitForFunction(()=>__orderProbe.state?.servers?.length===12);
  const openDrawer=async()=>{if(touch&&!await page.getByRole("dialog",{name:"Server navigation"}).count())await page.getByRole("button",{name:"Open navigation",exact:true}).click();};
  await openDrawer();
  const browse=label=>page.getByRole("button",{name:`Browse ${label}`,exact:true});
  const saved=()=>page.evaluate(async()=>{
   const opening=indexedDB.open("nuschl-observed-history-v1",1),db=await new Promise(done=>opening.onsuccess=()=>done(opening.result));
   const request=db.transaction("settings").objectStore("settings").get("bookmarks");const raw=await new Promise(done=>request.onsuccess=()=>done(request.result));db.close();return JSON.parse(raw).map(book=>book.id);
  });
  const rendered=()=>page.locator(".server-scroll>.server-order-slot").evaluateAll(nodes=>nodes.map(node=>node.dataset.serverId));
  await browse("Beta").click();await page.waitForFunction(id=>__orderProbe.state?.timeline?.server===id,beta);
  await until(async()=>JSON.stringify(await saved())===JSON.stringify(ids),"initial catalog changed unexpectedly");
  let expected=[gamma,alpha,beta,...ids.slice(3)];
  let cdp;
  async function touchDrag(source,target,cancel=false){
   const a=await source.boundingBox(),b=await target.boundingBox();const ax=a.x+a.width/2,ay=a.y+a.height/2,bx=b.x+b.width/2,by=b.y+b.height/2;
   await cdp.send("Input.dispatchTouchEvent",{type:"touchStart",touchPoints:[{x:ax,y:ay,id:1}]});
   await cdp.send("Input.dispatchTouchEvent",{type:"touchMove",touchPoints:[{x:bx,y:by,id:1}]});await page.waitForTimeout(120);
   if(cancel)await page.keyboard.press("Escape");
   else {
    const ghost=page.locator(".server-order-ghost");await ghost.waitFor();const box=await ghost.boundingBox();assert.ok(Math.abs(box.x+box.width/2-bx)<2&&Math.abs(box.y+box.height/2-by)<2,"touch ghost is not aligned in the modal layer");
    await page.screenshot({path:resolve(out,`${name}-preview.png`)});
   }
   await cdp.send("Input.dispatchTouchEvent",{type:"touchEnd",touchPoints:[]});
  }
  if(touch){
   cdp=await context.newCDPSession(page);await page.getByRole("button",{name:"Reorder servers",exact:true}).click();
   await touchDrag(page.getByRole("button",{name:"Move Gamma; use arrow keys or drag",exact:true}),browse("Alpha"));
  }else await browse("Gamma").dragTo(browse("Alpha"));
  await until(async()=>JSON.stringify(await saved())===JSON.stringify(expected),"actual SDK/IndexedDB did not commit the drop");
  assert.deepEqual(await rendered(),expected);assert.equal(await page.evaluate(()=>__orderProbe.state.timeline.server),beta,"reorder changed selected conversation");
  assert.deepEqual(await page.evaluate(()=>__orderProbe.state.servers.slice(0,3).map(server=>server.runtime_index)),[2,0,1]);
  // Cancel a second changed preview without a storage command.
  const commits=await page.evaluate(()=>__orderProbe.commands.filter(command=>command.method==="set_server_order").length);
  if(touch)await touchDrag(page.getByRole("button",{name:"Move Gamma; use arrow keys or drag",exact:true}),browse("Alpha"),true);
  else {
   const a=await browse("Gamma").boundingBox(),b=await browse("Alpha").boundingBox();await page.mouse.move(a.x+a.width/2,a.y+a.height/2);await page.mouse.down();await page.mouse.move(b.x+b.width/2,b.y+b.height/2,{steps:8});await page.keyboard.press("Escape");await page.mouse.up();
  }
  await until(async()=>JSON.stringify(await rendered())===JSON.stringify(expected),"cancel did not restore visible order");
  assert.deepEqual(await saved(),expected);assert.equal(await page.evaluate(()=>__orderProbe.commands.filter(command=>command.method==="set_server_order").length),commits);
  // Commands after a display permutation must still address runtime index1.
  await page.getByRole("button",{name:"Lounge",exact:true}).click();await page.waitForFunction(id=>__orderProbe.state?.timeline?.server===id&&__orderProbe.state?.timeline?.channel===1,beta);
  assert.equal(await page.evaluate(()=>__orderProbe.commands.filter(command=>command.method==="open").at(-1).args[0]),1);
  await openDrawer();await page.locator("nav.servers .settings-rail").click();
  await page.getByRole("button",{name:"Move server earlier",exact:true}).click();expected=[gamma,beta,alpha,...ids.slice(3)];
  await until(async()=>JSON.stringify(await saved())===JSON.stringify(expected),"accessible earlier action failed");
  await page.getByRole("button",{name:"Move server later",exact:true}).click();expected=[gamma,alpha,beta,...ids.slice(3)];
  await until(async()=>JSON.stringify(await saved())===JSON.stringify(expected),"accessible later action failed");
  await page.getByRole("button",{name:"Close settings",exact:true}).click();
  await page.reload();await page.waitForFunction(()=>__orderProbe.state?.servers?.length===12);await openDrawer();
  assert.deepEqual(await rendered(),expected,"real App reload lost persisted order");
  // Both scroll limits preserve the active ring/badge paint space.
  for(const edge of ["first","last"]){
   const bounds=await page.locator(".server-scroll").evaluate((node,edge)=>{
    node.scrollTop=edge==="first"?0:node.scrollHeight;
    const slot=edge==="first"?node.querySelector(".server-order-slot"):node.querySelector(".server-order-slot:last-child"),button=slot.querySelector(".server-icon"),a=node.getBoundingClientRect(),b=button.getBoundingClientRect();
    return {viewport:{left:a.left,top:a.top,right:a.left+node.clientWidth,bottom:a.top+node.clientHeight},paint:{left:b.left-4,right:b.right+3,top:b.top-4,bottom:b.bottom+2}};
   },edge);
   const a=bounds.viewport,b=bounds.paint;assert.ok(b.left>=a.left&&b.right<=a.right&&b.top>=a.top&&b.bottom<=a.bottom,`${name}/${edge}: decoration clipped ${JSON.stringify(bounds)}`);
  }
  await page.screenshot({path:resolve(out,`${name}-reloaded.png`)});
  assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true);
  assert.equal(await page.evaluate(()=>__orderProbe.microphoneCalls),0);assert.deepEqual(await page.evaluate(()=>__orderProbe.violations),[]);assert.deepEqual(errors,[]);assert.deepEqual(remote,[]);assert.deepEqual(sockets,[]);
  proofs.push({name,width,height,touch,passed:true,actual_sdk:true,actual_indexeddb:true,drop:true,cancel:true,runtime_index_after_permutation:true,accessible_moves:true,reload:true,decoration_gutters:true,no_microphone:true,no_connections:true});
  await writeFile(resolve(out,"proof.json"),JSON.stringify({site,csp,proofs},null,2));await context.close();
 }
 console.log(JSON.stringify(proofs));
}catch(error){
 await writeFile(resolve(out,"failure.json"),JSON.stringify({error:String(error),contexts:await Promise.all(browser.contexts().map(async context=>Promise.all(context.pages().map(async page=>({url:page.url(),state:await page.evaluate(()=>({probe:window.__orderProbe,text:document.body.textContent,slots:[...document.querySelectorAll(".server-order-slot")].map(node=>node.dataset.serverId)})).catch(()=>null)})))))},null,2));
 for(const [index,context] of browser.contexts().entries())for(const page of context.pages())await page.screenshot({path:resolve(out,`failure-${index}.png`)}).catch(()=>{});
 throw error;
}finally{await browser.close();await new Promise(done=>server.close(done));}
