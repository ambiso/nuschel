// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {createServer} from "vite";
import {formulaInput, projectMathTree, validateMathTree} from "./math-render-protocol.js";
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
