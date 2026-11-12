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
  ["/content-protocol.js",join(source,"web/app/src/content-protocol.js")],
  ["/content-worker.js",join(source,"web/app/src/content-worker.js")],
  ["/fixture.mjs",join(source,"tests/web_content.fixture.mjs")],
  ["/fixture.png",join(source,"crates/content-wasm/tests/fixtures/rgba-2x2.png")],
  ["/content-codec/nuschl_content_wasm.js",join(codec,"nuschl_content_wasm.js")],
  ["/content-codec/nuschl_content_wasm_bg.wasm",join(codec,"nuschl_content_wasm_bg.wasm")],
]);
const server=createServer(async (request,response) => {
  const path=new URL(request.url,"http://127.0.0.1").pathname;
  const headers={"Content-Security-Policy":csp,"Permissions-Policy":"camera=(), microphone=(), geolocation=()","X-Content-Type-Options":"nosniff"};
  try {
    let body, type;
    if(path === "/") { body='<!doctype html><meta charset="utf-8"><title>Private isolated content proof</title><canvas id="pixels" aria-label="Decoded attachment"></canvas><output id="result"></output><script type="module" src="/fixture.mjs"></script>'; type="text/html"; }
    else if(paths.has(path)) { body=await readFile(paths.get(path)); type=path.endsWith(".wasm")?"application/wasm":path.endsWith(".png")?"image/png":"text/javascript"; }
    else { response.writeHead(404,headers); response.end(); return; }
    response.writeHead(200,{...headers,"Content-Type":type}); response.end(body);
  } catch { response.writeHead(500,headers); response.end(); }
});
await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));
const origin=`http://127.0.0.1:${server.address().port}`;
const browser=await chromium.launch({headless:true,executablePath:wrapper});
const context=await browser.newContext(); const errors=[], requests=[];
context.on("request",request=>requests.push(request.url()));
const page=await context.newPage();page.on("pageerror",error=>errors.push(error.message));
try {
  await page.addInitScript(()=>{ globalThis.policyViolations=[]; document.addEventListener("securitypolicyviolation",event=>policyViolations.push(event.violatedDirective)); });
  await page.goto(origin); await page.waitForFunction(()=>typeof runContentProof === "function");
  const proof=await page.evaluate(()=>runContentProof());
  assert.deepEqual(errors,[]); assert.deepEqual(await page.evaluate(()=>policyViolations),[]);
  assert.ok(requests.every(url=>url === `${origin}/` || paths.has(url.slice(origin.length)) && url.startsWith(origin)));
  await page.screenshot({path:join(output,"content.png")});
  await writeFile(join(output,"proof.json"),JSON.stringify({...proof,codec,wasm_bytes:binary.length,memory,imports,csp,requests,browser_errors:errors},null,2));
  console.log(JSON.stringify({passed:true,wasm_bytes:binary.length,memory,checks:proof.checks}));
} finally {
  await page.evaluate(()=>globalThis.closeContentProof?.()).catch(()=>{});
  await context.close(); await browser.close(); await new Promise(resolve=>server.close(resolve));
}
