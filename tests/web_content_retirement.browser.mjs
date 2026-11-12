// SPDX-License-Identifier: EUPL-1.2
import assert from "node:assert/strict";
import {createServer} from "node:http";
import {readFile, writeFile, mkdir} from "node:fs/promises";
import {join} from "node:path";
import {pathToFileURL} from "node:url";
const [source, codec, wrapper, playwright, output] = process.argv.slice(2);
assert.ok(source && codec && wrapper && playwright && output);
await mkdir(output, {recursive:true});
const binary = await readFile(join(codec, "nuschl_content_wasm_bg.wasm"));
const module = new WebAssembly.Module(binary);
const imports = WebAssembly.Module.imports(module);
assert.ok(imports.every(item => item.kind === "function" && !/fetch|socket|storage|random|clock|date|performance|document|window|navigator/i.test(item.name)));
function number(bytes, position) {
  let value=0, shift=0;
  for (let count=0; count<5; count++) {
    const byte=bytes[position.value++]; assert.notEqual(byte, undefined);
    value += (byte & 127) * 2 ** shift;
    if (!(byte & 128)) { assert.ok(value <= 0xffffffff); return value; } shift += 7;
  }
  throw new Error("Invalid WASM u32.");
}
const cursor={value:8}; let memory;
while (cursor.value < binary.length) {
  const section=binary[cursor.value++], size=number(binary,cursor), end=cursor.value+size;
  assert.ok(end <= binary.length);
  if (section === 5) {
    assert.equal(number(binary,cursor),1); assert.equal(number(binary,cursor),1, "Memory must have an explicit unshared 32-bit maximum.");
    memory={initial_pages:number(binary,cursor), max_pages:number(binary,cursor)};
    assert.equal(cursor.value,end);
  }
  cursor.value=end;
}
assert.equal(memory?.max_pages,4096, "Content WASM must be capped at 256 MiB.");
const {chromium}=await import(pathToFileURL(playwright).href);
const csp="default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; worker-src 'self'; connect-src 'self'; img-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const paths = new Map([
  ["/content-service.js",join(source,"web/app/src/content-service.js")],
  ["/worker-owner.js",join(source,"web/app/src/worker-owner.js")],
  ["/content-protocol.js",join(source,"web/app/src/content-protocol.js")],
  ["/content-worker.js",join(source,"web/app/src/content-worker.js")],
  ["/fixture.mjs",join(source,"tests/web_content.fixture.mjs")],
  ["/retirement.mjs",join(source,"tests/web_content_retirement.fixture.mjs")],
  ["/fixture.png",join(source,"crates/content-wasm/tests/fixtures/rgba-2x2.png")],
  ["/content-codec/nuschl_content_wasm.js",join(codec,"nuschl_content_wasm.js")],
  ["/content-codec/nuschl_content_wasm_bg.wasm",join(codec,"nuschl_content_wasm_bg.wasm")],
]);
const server=createServer(async (request,response) => {
  const path=new URL(request.url,"http://127.0.0.1").pathname;
  const headers={"Content-Security-Policy":csp,"Permissions-Policy":"camera=(), microphone=(), geolocation=()","X-Content-Type-Options":"nosniff"};
  try {
    let body, type;
    if(path === "/") { body='<!doctype html><meta charset="utf-8"><title>Private isolated content proof</title><canvas id="pixels" aria-label="Decoded attachment"></canvas><output id="result"></output><script type="module" src="/fixture.mjs"></script><script type="module" src="/retirement.mjs"></script>'; type="text/html"; }
    else if(paths.has(path)) { body=await readFile(paths.get(path));
      // Fixture-only ordinary task on the real initialized WASM worker. The
      // production prefix remains unchanged; no inspector-owned evaluation.
      if(path==="/content-worker.js")body=Buffer.concat([body,Buffer.from(`
self.addEventListener("message",event=>{if(event.data?.__private_content_stall__!==true)return;
 console.log("PRIVATE_CONTENT_STALL_BEGIN");const until=performance.now()+12000;
 while(performance.now()<until){};console.log("PRIVATE_CONTENT_STALL_END");
});
`)]);
      type=path.endsWith(".wasm")?"application/wasm":path.endsWith(".png")?"image/png":"text/javascript"; }
    else { response.writeHead(404,headers); response.end(); return; }
    response.writeHead(200,{...headers,"Content-Type":type}); response.end(body);
  } catch { response.writeHead(500,headers); response.end(); }
});
await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));
const origin=`http://127.0.0.1:${server.address().port}`;
const browser=await chromium.launch({headless:true,executablePath:wrapper});

const reports=[],contexts=[];
try {
 for(const mode of ["normal","throw"]){
  const context=await browser.newContext();contexts.push(context);const requests=[],errors=[];
  context.on("request",request=>requests.push(request.url()));const page=await context.newPage();page.on("pageerror",error=>errors.push(error.message));
  await page.addInitScript(({mode})=>{
   globalThis.policyViolations=[];document.addEventListener("securitypolicyviolation",event=>policyViolations.push(event.violatedDirective));
   const proof=globalThis.__contentOwnerProof={created:0,terminated:0,posted:[],received:[],late:[],retiredAt:null};
   const NativeWorker=globalThis.Worker;
   globalThis.Worker=class extends NativeWorker {
    constructor(url,options){super(url,options);this.owned=String(url).includes("content-worker.js");if(this.owned){proof.created++;proof.worker=this;
     this.addEventListener("message",({data})=>{if(data.kind==="result"||data.kind==="error"){proof.received.push({id:data.id,kind:data.kind});if(proof.retiredAt!==null)proof.late.push({id:data.id,kind:data.kind});}});}}
    postMessage(data,...rest){if(this.owned&&data.operation)proof.posted.push({id:data.id,operation:data.operation});return super.postMessage(data,...rest);}
    terminate(){if(this.owned){proof.terminated++;proof.retiredAt=performance.now();if(mode==="throw")throw new DOMException("Fixture failed termination","InvalidStateError");}return super.terminate();}
   };
  },{mode});
  await page.goto(origin);await page.waitForFunction(()=>typeof prepareContentRetirement==="function"&&typeof runContentProof==="function");
  const healthy=await page.evaluate(()=>prepareContentRetirement());assert.equal(healthy.workers,1);
  let tail=false;const began=new Promise(resolve=>page.on("console",message=>{if(message.text()==="PRIVATE_CONTENT_STALL_BEGIN")resolve();if(message.text()==="PRIVATE_CONTENT_STALL_END")tail=true;}));
  await page.evaluate(()=>__contentOwnerProof.worker.postMessage({__private_content_stall__:true}));await began;
  const start=performance.now(),reservation=await page.evaluate(()=>beginContentStall());
  await page.waitForFunction(()=>contentRetirementState().retired,null,{timeout:11000});
  const retired=await page.evaluate(()=>finishContentRetirement());const observed=performance.now()-start;
  assert.ok(observed>=7500&&observed<11000);assert.equal(retired.state.terminationFailed,mode==="throw");
  const beforeLate=await page.evaluate(()=>({created:__contentOwnerProof.created,terminated:__contentOwnerProof.terminated,posted:__contentOwnerProof.posted.length}));
  assert.equal(beforeLate.created,1);assert.equal(beforeLate.terminated,1);
  if(mode==="throw")await page.waitForFunction(id=>__contentOwnerProof.late.some(reply=>reply.id===id&&reply.kind==="error"),reservation.activeId,{timeout:7000});
  const after=await page.evaluate(()=>({created:__contentOwnerProof.created,terminated:__contentOwnerProof.terminated,posted:__contentOwnerProof.posted.length,late:__contentOwnerProof.late,state:contentRetirementState()}));
  assert.equal(after.created,1);assert.equal(after.terminated,1);assert.equal(after.posted,beforeLate.posted);assert.equal(after.state.bytes,8*1024*1024);
  assert.ok(after.state.retired);assert.deepEqual(errors,[]);assert.deepEqual(await page.evaluate(()=>policyViolations),[]);
  assert.ok(requests.every(url=>url===origin+"/"||url.startsWith(origin)&&paths.has(url.slice(origin.length))));
  await page.screenshot({path:join(output,mode+"-retired.png")});
  reports.push({mode,healthy,reservation,retired,observed_deadline_ms:observed,beforeLate,after,ordinary_task_tail_observed:tail,
   physical_exit_acknowledged:false,requests,browser_errors:errors});
  await page.evaluate(()=>closeContentRetirement());await context.close();
 }
 const report={passed:true,source,codec,memory,imports,csp,job_deadline_ms:8000,worker_busy_task_ms:12000,reports,
  checks:["actual initialized WASM worker","ordinary invalid-image recovery","global8jobs/16MiB across facades","active cancel retains slot/deadline/bytes until ACK","healthy same-owner reuse","stalled original8s deadline","terminate throwing still settles cleanup","retired owner cannot be replaced by20newfacades","actual late old-WASM error ignored","no physical exit assumption","no source URL or host-device access"]};
 await writeFile(join(output,"proof.json"),JSON.stringify(report,null,2));console.log(JSON.stringify({passed:true,reports:reports.map(row=>({mode:row.mode,observed_deadline_ms:row.observed_deadline_ms,created:row.after.created,late:row.after.late.length})),memory}));
}catch(error){
 await writeFile(join(output,"failure.json"),JSON.stringify({error:String(error),reports,states:await Promise.all(contexts.map(async context=>context.pages()[0]?.evaluate(()=>({created:__contentOwnerProof.created,terminated:__contentOwnerProof.terminated,posted:__contentOwnerProof.posted,late:__contentOwnerProof.late,state:typeof contentRetirementState==="function"?contentRetirementState():null})).catch(()=>null)))},null,2));throw error;
}finally{
 for(const context of contexts)await context.close();await browser.close();await new Promise(resolve=>server.close(resolve));
}
