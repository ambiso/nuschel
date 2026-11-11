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
