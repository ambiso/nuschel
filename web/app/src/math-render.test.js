// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {createServer} from "vite";
import {formulaInput, projectMathTree, validateMathTree} from "./math-render-protocol.js";
import {MathRenderService} from "./math-render-service.js";
import {visibleBlocks} from "./presentation.js";
let server, renderFormula;
test.before(async () => {
  server=await createServer({root:new URL("..",import.meta.url).pathname,logLevel:"error",server:{middlewareMode:true,hmr:false,watch:null},ssr:{noExternal:["temml"]}});
  ({renderFormula}=await server.ssrLoadModule("/src/math-render-core.js"));
});
test.after(async () => { await server?.close(); });
function elements(node) { return [node,...(node.children||[]).flatMap(elements)]; }
test("pinned core API produces fractions, roots, scripts, operators and matrices without HTML or CSS", () => {
  const fraction=renderFormula("\\frac{1}{\\sqrt{x^2+1}}",true);
  assert.ok(elements(fraction).some(node=>node.tag==="mfrac"));
  assert.ok(elements(fraction).some(node=>node.tag==="msqrt"));
  assert.ok(elements(fraction).some(node=>node.tag==="msup"));
  for(const tex of ["x_i^2+\\sum_{n=1}^{\\infty}n","\\begin{pmatrix}a&b\\\\c&d\\end{pmatrix}"]) {
    const result=renderFormula(tex,true); assert.equal(result.tag,"math");
    assert.ok(!/"(?:style|class|href|src|id)"/.test(JSON.stringify(result)));
  }
});
test("recursive and cross-message macros fail closed and do not poison subsequent parsing", () => {
  assert.throws(()=>renderFormula("\\def\\loop{\\loop}\\loop",false));
  renderFormula("\\gdef\\privateMacro{q}\\privateMacro",false);
  assert.throws(()=>renderFormula("\\privateMacro",false));
  assert.throws(()=>renderFormula("\\frac{",true));
  assert.equal(renderFormula("a+b",false).tag,"math");
  assert.throws(()=>formulaInput("x".repeat(4097),false));
});
test("links, graphics and arbitrary attributes cannot become executable or resource-bearing MathML", () => {
  for(const tex of ["\\href{https://invalid.example/canary}{x}","\\includegraphics{https://invalid.example/canary}","\\style{background:url(/canary)}{x}"]) {
    let tree;
    try { tree=renderFormula(tex,false); }
    catch { /* Unsupported trusted-only commands preserve source instead. */ }
    if(tree) for(const node of elements(tree)) {
      assert.ok(!Object.keys(node.attrs||{}).some(name=>["href","src","style","class"].includes(name)));
    }
  }
  for(const name of ["href","src","style","class","onclick","xmlns","id"]) {
    assert.throws(()=>validateMathTree({tag:"math",attrs:{[name]:"bad"},children:[]}));
    assert.throws(()=>projectMathTree({type:"math",attributes:{[name]:"bad"},children:[]}));
  }
  assert.throws(()=>validateMathTree({tag:"svg",attrs:{},children:[]}));
  assert.throws(()=>projectMathTree({type:"math",attributes:{},children:[],unexpected:"new upstream shape"}));
  assert.throws(()=>validateMathTree({tag:"math",attrs:{},children:[{tag:"mspace",attrs:{width:"9999em"},children:[]}]}));
});
test("collapsed display formulas retain complete copy source without parsing hidden or partial TeX", () => {
  const source="$$\n"+"x+\n".repeat(20)+"y\n$$", block={kind:"math",source,tex:"x+y"};
  const preview=visibleBlocks({blocks:[block]});
  assert.ok(preview.hasMore);assert.equal(preview.blocks[0].source,source);
  assert.equal(preview.blocks[0].tex,undefined);assert.ok(preview.blocks[0].preview.length<source.length);
  assert.equal(visibleBlocks({blocks:[block]},true).blocks[0].tex,"x+y");
});
const plainTree=text=>({tag:"math",attrs:{},children:[{tag:"mi",attrs:{},children:[{text}]}]});
class Worker {
  sent=[];terminated=false;
  postMessage(message){this.sent.push(message);}
  terminate(){this.terminated=true;}
  emit(data){this.onmessage?.({data});}
  ready(){this.emit({kind:"ready"});}
  finish(text="x"){this.emit({kind:"result",id:this.sent.at(-1).id,tree:JSON.stringify(plainTree(text))});}
}
const outcome=promise=>promise.then(value=>({value}),error=>({error}));
function fixture(options={}) {
  const workers=[];
  const service=new MathRenderService({...options,workerFactory:()=>{const worker=new Worker();workers.push(worker);return worker;}});
  return {workers,service};
}
test("finite queue, stale worker results and cache accounting survive source replacement", async () => {
  const {service,workers}=fixture();const abort=new AbortController();
  const old=outcome(service.render("old",false,{signal:abort.signal}));
  const jobs=Array.from({length:7},(_,i)=>outcome(service.render(`x${i}`)));
  await assert.rejects(service.render("overflow"),/busy/);workers[0].ready();abort.abort();
  assert.ok((await old).error);assert.ok(workers[0].terminated);workers[1].ready();
  workers[0].finish("stale");
  for(let i=0;i<7;i++){workers[1].finish(`x${i}`);assert.ok((await jobs[i]).value);}
  const before=workers[1].sent.length;const cached=await service.render("x0");
  assert.equal(workers[1].sent.length,before);assert.ok(Object.isFrozen(cached));
  service.dispose();
});
test("duplicate in-flight results do not inflate the bounded cache", async () => {
  const {service,workers}=fixture();const a=outcome(service.render("same")),b=outcome(service.render("same"));
  workers[0].ready();workers[0].finish();await a;const bytes=service.cacheBytes;
  workers[0].finish();await b;assert.equal(service.cache.size,1);assert.equal(service.cacheBytes,bytes);
  for(let i=0;i<70;i++){const p=outcome(service.render(`cache${i}`));workers[0].finish();await p;}
  assert.ok(service.cache.size<=64);assert.ok(service.cacheBytes<=2*1024*1024);service.dispose();
});
test("deadline terminates a stalled parser and malformed replies fail closed before DOM construction", async () => {
  const {service,workers}=fixture({timeoutMs:25});const first=outcome(service.render("stalled"));workers[0].ready();
  assert.match((await first).error.message,/deadline/);assert.ok(workers[0].terminated);
  const next=outcome(service.render("new"));workers[1].ready();
  workers[1].emit({kind:"result",id:workers[1].sent[0].id,tree:JSON.stringify({tag:"math",attrs:{href:"/canary"},children:[]})});
  assert.ok((await next).error);assert.ok(workers[1].terminated);service.dispose();
});
