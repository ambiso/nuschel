// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {textRuns,safeLink} from "./presentation.js";
// Independent encoder-backed reference: UTF-8 byte offsets must retain the
// original presentation behavior, including ignored non-boundary spans.
function reference(block){
 const text=block.text||"",boundaries=new Map([[0,0]]);let bytes=0,units=0;
 const encoder=new TextEncoder();
 for(const char of text){bytes+=encoder.encode(char).length;units+=char.length;boundaries.set(bytes,units);}
 const runs=[];let previous=0;
 for(const span of (block.spans||[]).slice(0,4096)){
  if(!Number.isInteger(span.start)||!Number.isInteger(span.end)||span.start<previous||span.end<=span.start||!boundaries.has(span.start)||!boundaries.has(span.end))continue;
  if(span.start>previous)runs.push({text:text.slice(boundaries.get(previous),boundaries.get(span.start)),style:{}});
  runs.push({text:text.slice(boundaries.get(span.start),boundaries.get(span.end)),style:span.style||{},link:safeLink(span.link),math:typeof span.math==="string"?span.math:undefined});previous=span.end;
 }
 if(previous<bytes)runs.push({text:text.slice(boundaries.get(previous)),style:{}});
 return runs;
}
test("UTF-8/UTF-16 boundaries match encoder for ASCII, astral, combining and lone surrogates",()=>{
 for(const text of ["", "ascii", "aé🙂e\u0301\ud800中\nz", "\udfffA👨‍👩‍👧‍👧\u0000"]){
  const bytes=new TextEncoder().encode(text).length;
  assert.deepEqual(textRuns({text}),reference({text}));
  for(let start=-1;start<=bytes+1;start++)for(let end=start;end<=bytes+1;end++){
   const block={text,spans:[{start,end,style:{bold:true},link:"https://example.invalid/x",math:"x"}]};
   assert.deepEqual(textRuns(block),reference(block),`${JSON.stringify(text)} ${start}:${end}`);
  }
 }
});
test("span order, invalid offsets, unsafe links and the existing span cap are unchanged",()=>{
 const block={text:"éabc🙂",spans:[{start:1,end:2},{start:0,end:2,link:"javascript:alert(1)"},{start:0,end:3},{start:2,end:5,style:{code:true}},{start:5,end:9,link:"https://example.invalid",math:"x"},...Array.from({length:4100},()=>({start:0,end:1}))]};
 assert.deepEqual(textRuns(block),reference(block));
});
test("plain and styled runs allocate no TextEncoder or per-character encoded arrays",()=>{
 const original=globalThis.TextEncoder;let calls=0;
 globalThis.TextEncoder=class {constructor(){calls++;}encode(){calls++;throw new Error("Unexpected encoder allocation");}};
 try{
  assert.deepEqual(textRuns({text:"x".repeat(100000)}),[{text:"x".repeat(100000),style:{}}]);
  assert.equal(textRuns({text:"🙂é",spans:[{start:0,end:4,style:{bold:true}}]}).length,2);
  assert.equal(calls,0);
 }finally{globalThis.TextEncoder=original;}
});
