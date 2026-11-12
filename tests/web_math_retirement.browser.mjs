// SPDX-License-Identifier: EUPL-1.2
// Real built Temml worker + source scheduling adapter, isolated page per owner.
import assert from "node:assert/strict";
import {createServer} from "node:http";
import {readFile,writeFile,mkdir,readdir} from "node:fs/promises";
import {join,resolve,sep,extname} from "node:path";
import {pathToFileURL} from "node:url";
const [source,site,wrapper,playwright,output]=process.argv.slice(2);assert.ok(source&&site&&wrapper&&playwright&&output);
await mkdir(output,{recursive:true});
const workers=(await readdir(join(site,"assets"))).filter(name=>/^math-render-worker-.*\.js$/.test(name));assert.equal(workers.length,1);
const workerPath="/app/assets/"+workers[0],requests=[],canaries=[];
const csp="default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const paths=new Map([...["worker-owner.js","math-render-service.js","math-render-protocol.js"].map(name=>["/"+name,join(source,"web/app/src",name)]),["/fixture.mjs",join(source,"tests/web_math_retirement.fixture.mjs")]]);
const server=createServer(async(req,res)=>{
  const url=new URL(req.url,"http://127.0.0.1");if(url.pathname.includes("canary"))canaries.push(req.url);
  try{
    let body,type="text/javascript";
    if(url.pathname==="/"){type="text/html";body=`<!doctype html><meta charset="utf-8"><title>Private formula owner proof</title><body data-worker="${workerPath}?cold=${url.searchParams.get("cold")==="yes"?"yes":"no"}"><main><div id="formula" aria-label="Validated formula"></div><div id="canaries"></div><output id="result"></output></main><script type="module" src="/fixture.mjs"></script>`;}
    else if(paths.has(url.pathname))body=await readFile(paths.get(url.pathname));
    else if(url.pathname.startsWith("/app/assets/")){
      const path=resolve(site,"."+url.pathname.slice(4));assert.ok(path.startsWith(resolve(site)+sep));body=await readFile(path);
      if(url.pathname===workerPath){if(url.searchParams.get("cold")==="yes")await new Promise(done=>setTimeout(done,2500));
        body=Buffer.concat([body,Buffer.from(`\nself.addEventListener("message",event=>{if(![120,3500].includes(event.data?.__private_math_stall__))return;console.log("PRIVATE_MATH_STALL_BEGIN");const until=performance.now()+event.data.__private_math_stall__;while(performance.now()<until){};console.log("PRIVATE_MATH_STALL_END");});`)]);
      }
      if(extname(path)===".wasm")type="application/wasm";
    }else{res.writeHead(404);res.end();return;}
    res.writeHead(200,{"Content-Type":type,"Content-Security-Policy":csp,"Permissions-Policy":"camera=(), microphone=(), geolocation=()","X-Content-Type-Options":"nosniff","Cache-Control":"no-store"});res.end(body);
  }catch{res.writeHead(500);res.end();}
});
await new Promise(done=>server.listen(0,"127.0.0.1",done));const origin=`http://127.0.0.1:${server.address().port}`;
const {chromium}=await import(pathToFileURL(playwright).href);const browser=await chromium.launch({headless:true,executablePath:wrapper});const contexts=[],reports=[];
try{
 for(const mode of ["cold","normal","throw"]){
  const context=await browser.newContext({viewport:{width:390,height:844}});contexts.push(context);const page=await context.newPage(),errors=[];
  context.on("request",request=>requests.push(request.url()));page.on("pageerror",e=>errors.push(e.message));
  await page.addInitScript(({mode})=>{
    const proof=globalThis.__mathOwnerProof={created:0,terminated:0,posted:[],received:[],late:[],retiredAt:null,violations:[],captures:0};
    document.addEventListener("securitypolicyviolation",e=>proof.violations.push(e.violatedDirective));
    navigator.mediaDevices.getUserMedia=()=>{proof.captures++;throw new Error("Host capture forbidden");};
    const Native=Worker;globalThis.Worker=class extends Native{
      constructor(url,options){super(url,options);proof.created++;proof.worker=this;this.addEventListener("message",({data})=>{if(data.kind==="result"||data.kind==="error"){proof.received.push({id:data.id,kind:data.kind});if(proof.retiredAt!==null)proof.late.push({id:data.id,kind:data.kind});}});}
      postMessage(data,...rest){if(data.tex!==undefined)proof.posted.push({id:data.id,tex:data.tex});return super.postMessage(data,...rest);}
      terminate(){proof.terminated++;proof.retiredAt=performance.now();if(mode==="throw")throw new DOMException("Fixture terminate failure","InvalidStateError");return super.terminate();}
    };
  },{mode});
  await page.goto(origin+(mode==="cold"?"/?cold=yes":"/"));await page.waitForFunction(()=>typeof prepareMath==="function");
  if(mode==="cold"){
    const begin=performance.now();await page.evaluate(()=>beginColdMath());await page.waitForTimeout(2100);const state=await page.evaluate(()=>mathState());assert.equal(state.retired,false);
    const result=await page.evaluate(()=>retryColdMath());assert.equal(result.created,1);assert.equal(result.terminated,0);assert.equal(result.state.retired,false);
    reports.push({mode,elapsed_ms:performance.now()-begin,result});
  }else{
    const healthy=await page.evaluate(()=>prepareMath());assert.equal(healthy.created,1);
    let resolveBegin;const began=()=>new Promise(done=>{resolveBegin=done;});page.on("console",message=>{if(message.text()==="PRIVATE_MATH_STALL_BEGIN")resolveBegin?.();});
    let begin=began();await page.evaluate(()=>__mathOwnerProof.worker.postMessage({__private_math_stall__:120}));await begin;await page.evaluate(()=>beginHealthyCancellation());const reuse=await page.evaluate(()=>healthyDone);assert.equal(reuse.retired,false);
    begin=began();await page.evaluate(()=>__mathOwnerProof.worker.postMessage({__private_math_stall__:3500}));await begin;
    const start=performance.now(),reservation=await page.evaluate(()=>beginMathStall());await page.waitForFunction(()=>mathState().retired,null,{timeout:4000});
    const retired=await page.evaluate(()=>finishMathStall()),elapsed=performance.now()-start;assert.ok(elapsed>=1900&&elapsed<4000);assert.equal(retired.state.terminationFailed,mode==="throw");
    if(mode==="throw")await page.waitForFunction(id=>__mathOwnerProof.late.some(reply=>reply.id===id),reservation.activeId,{timeout:4000});
    const after=await page.evaluate(()=>({created:__mathOwnerProof.created,terminated:__mathOwnerProof.terminated,posted:__mathOwnerProof.posted.length,late:__mathOwnerProof.late,state:mathState()}));
    assert.equal(after.created,1);assert.equal(after.terminated,1);assert.equal(after.state.bytes,11);assert.equal(after.state.retired,true);
    await page.screenshot({path:join(output,mode+"-retired.png")});reports.push({mode,healthy,reuse,reservation,retired,after,elapsed_ms:elapsed,physical_exit_acknowledged:false});
  }
  assert.deepEqual(errors,[]);const safety=await page.evaluate(()=>({violations:__mathOwnerProof.violations,captures:__mathOwnerProof.captures}));assert.deepEqual(safety.violations,[]);assert.equal(safety.captures,0);
  await page.evaluate(()=>closeMathProof());await context.close();
 }
 assert.deepEqual(canaries,[]);assert.ok(requests.every(url=>url.startsWith(origin+"/")));
 const report={passed:true,source,site,worker:workers[0],csp,reports,canaries,requests,checks:["actual pinned built Temml worker","2s queued expiry then same cold owner ready after2.5s","healthy active cancellation+actual ACK reuse","actual3.5s busy worker beyond original2s deadline","throwing terminate cleanup+actual old Temml result ignored","20facades cannot replace retired owner","validated cache/formula survives retirement","typed source fallback, inert resource canaries","production CSP/no host devices"]};
 await writeFile(join(output,"proof.json"),JSON.stringify(report,null,2));console.log(JSON.stringify({passed:true,reports:reports.map(row=>({mode:row.mode,elapsed_ms:row.elapsed_ms}))}));
}catch(error){await writeFile(join(output,"failure.json"),JSON.stringify({error:String(error),reports,canaries},null,2));throw error;}
finally{for(const context of contexts)await context.close();await browser.close();await new Promise(done=>server.close(done));}
