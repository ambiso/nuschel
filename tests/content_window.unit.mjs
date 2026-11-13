// SPDX-License-Identifier: EUPL-1.2
// One explicit content window: the SDK's posted-job limit (Rust), the RPC ACK
// reserve and the bridge's admission must agree, or a batch would be refused.
import test from "node:test";
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";
import {CONTENT_WINDOW,SDK_RPC_ACK_RESERVE} from "../web/app/src/sdk-rpc.js";
import {ContentBridge,contentUnavailable} from "../web/app/src/content-bridge.js";

test("the SDK posted-job window, RPC ACK reserve and bridge admission are one value",async()=>{
  const rust=await readFile(new URL("../crates/client-wasm/src/content_broker.rs",import.meta.url),"utf8");
  const declared=[...rust.matchAll(/pub const MAX_POSTED_JOBS: usize = (\d+);/g)];
  assert.equal(declared.length,1,"exactly one Rust declaration");
  assert.equal(Number(declared[0][1]),CONTENT_WINDOW);
  assert.equal(SDK_RPC_ACK_RESERVE,CONTENT_WINDOW);
  const bridge=new ContentBridge(async()=>"null",{service:{render:()=>new Promise(()=>{}),dispose(){}}});
  try {
    const renders=Array.from({length:CONTENT_WINDOW},(_,index)=>({kind:"render",request:String(index+1),mode:"plain",source:"x"}));
    assert.equal(bridge.handle({kind:"content_work",operations:renders}),true);
    assert.equal(bridge.pending.size,CONTENT_WINDOW);assert.equal(contentUnavailable("1"),false);
    bridge.handle({kind:"content_work",operations:[{kind:"render",request:String(CONTENT_WINDOW+1),mode:"plain",source:"x"}]});
    assert.equal(bridge.pending.size,CONTENT_WINDOW);assert.equal(contentUnavailable("1"),true,"one more than the window retires visibly");
  } finally { bridge.dispose(); }
});
