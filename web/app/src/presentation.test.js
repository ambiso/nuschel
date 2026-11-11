// SPDX-License-Identifier: EUPL-1.2
import { test } from "node:test";
import assert from "node:assert/strict";
import { channelRows, visibleBlocks, codeColor } from "./presentation.js";
test("channel navigation preserves tree and only opens root initially", () => {
 const channels = [{id:0,name:"Root",parent:null},{id:1,name:"Long child",parent:0},{id:2,name:"Grandchild",parent:1}];
 assert.deepEqual(channelRows(channels,new Map(),"server").map(({id,depth})=>[id,depth]),[[0,0],[1,1]]);
 assert.deepEqual(channelRows(channels,new Map([["server:1",true]]),"server").map(({id,depth})=>[id,depth]),[[0,0],[1,1],[2,2]]);
 const cycle = [{id:1,parent:2},{id:2,parent:1}];
 assert.ok(channelRows(cycle,new Map([["s:1",true],["s:2",true]]),"s").length<=2);
});
test("preview preserves blank lines/Unicode and full expansion never edits source", () => {
 const text="👋\n\n"+"line\n".repeat(20);
 const document={blocks:[{kind:"text",text}]};
 const original=structuredClone(document);
 const preview=visibleBlocks(document);
 assert.ok(preview.hasMore);
 assert.ok(preview.blocks[0].text.startsWith("👋\n\n"));
 assert.deepEqual(visibleBlocks(document,true).blocks,document.blocks);
 assert.deepEqual(document,original);
});
test("highlight runs remain inert text; untrusted colors cannot inject CSS", () => {
 const document={blocks:[{kind:"code",runs:[{text:'<img src="https://bad.invalid">',foreground:[255,0,10,255]}]}]};
 assert.equal(visibleBlocks(document).blocks[0].runs[0].text,document.blocks[0].runs[0].text);
 assert.equal(codeColor([255,0,10,255]),"rgb(255,0,10)");
 assert.equal(codeColor(['red);url(https://bad.invalid)',0,0]),"inherit");
});

import { safeLink, textRuns, conversationKey, filterMessages, appearance, rgbaPixels } from "./presentation.js";
test("UTF-8 span offsets preserve exact Unicode boundaries and reject executable links", () => {
 const block={text:"👋 hello world",spans:[{start:5,end:10,link:"https://example.invalid/x",style:{bold:true}},{start:11,end:16,link:"javascript:alert(1)"}]};
 const runs=textRuns(block);
 assert.equal(runs.map(x=>x.text).join(""),block.text);
 assert.equal(runs[1].text,"hello");
 assert.equal(runs[1].link,"https://example.invalid/x");
 assert.equal(safeLink("javascript:alert(1)"),null);
 assert.equal(safeLink("https://user:secret@example.invalid"),null);
 assert.equal(safeLink("https://example.invalid/\nthing"),null);
 assert.equal(textRuns({text:"👋 test",spans:[{start:1,end:3,link:"https://bad.invalid"}]}).length,1);
});
test("draft context includes certificate identity and search is inert", () => {
 const timeline={channel:1};
 assert.notEqual(conversationKey(timeline,{id:"s",pin:"a"}),conversationKey(timeline,{id:"s",pin:"b"}));
 assert.deepEqual(filterMessages([{author:"Ada",document:{blocks:[{kind:"code",runs:[{text:"fn main"}]}]}}],"MAIN").length,1);
 assert.deepEqual(appearance({theme:"url(bad)",fontSize:200,density:"bad"}),{theme:"system",fontSize:16,density:"comfortable",members:true});
});
test("pixels require bounded exact RGBA; source URLs are never used", () => {
 assert.deepEqual([...rgbaPixels({width:1,height:1,rgba:"/wAA/w==",source:"https://bad.invalid"})],[255,0,0,255]);
 assert.equal(rgbaPixels({width:100000,height:100000,rgba:""}),null);
 assert.equal(rgbaPixels({width:1,height:2,rgba:"/wAA/w=="}),null);
 const image={kind:"image",width:1,height:1,rgba:"/wAA/w=="};
 assert.deepEqual(visibleBlocks({blocks:[image]}).blocks,[image]);
});
