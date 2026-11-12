// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {DocumentCache,ContentBridge,contentDocument,retainDocument} from "./content-bridge.js";
const document={blocks:[{kind:"text",text:"safe",spans:[]}]};

test("global derivative credit follows pinned documents until actual release",()=>{
  const cache=new DocumentCache({maxBytes:3000,maxCount:2});
  assert.equal(cache.insert("1",document,1500),true);
  const release=cache.pin("1");
  assert.equal(cache.insert("2",document,1500),true);
  assert.equal(cache.insert("3",document,2000),false);
  assert.equal(cache.get("1"),document);
  assert.equal(cache.bytes,1500);
  release();release(); // release is idempotent
  assert.equal(cache.insert("3",document,2000),true);
  assert.equal(cache.get("1"),null);
  assert.equal(cache.bytes,2000);
});

test("untrusted charge, image source and derivative counts cannot bypass global bounds",()=>{
  const cache=new DocumentCache({maxBytes:3000,maxCount:1});
  assert.equal(cache.insert("1",document,1),false);
  assert.throws(()=>cache.insert("1",{blocks:[{kind:"image",width:1,height:1,rgba:"AAAAAA==",source:"https://outside.invalid"}]},1000));
  assert.equal(cache.insert("1",document,1000),true);
  const release=cache.pin("1");
  assert.equal(cache.insert("2",document,1000),false);
  assert.equal(cache.entries.size,1);
  release();
});

test("only the SDK accepted reply becomes visible, with no credential metadata in codec jobs",async()=>{
  const calls=[];let finish;
  const service={render(packet){calls.push(packet);return new Promise(resolve=>finish=resolve);},dispose(){}};
  const bridge=new ContentBridge(async(method,id,raw)=>{
    assert.equal(method,"content_complete");assert.equal(id,"7");
    assert.equal(JSON.parse(raw).plain_text,"safe");
    return JSON.stringify({token:"7",charge:1000,document,failed:false});
  },{service});
  try {
    assert.equal(bridge.handle({kind:"content_work",operations:[{kind:"render",request:"7",mode:"legacy",source:"<b>safe</b>"}]}),true);
    assert.deepEqual(calls,[{mode:"legacy",source:"<b>safe</b>"}]);
    assert.equal(contentDocument("7"),null);
    finish({marked_text:"safe",plain_text:"safe",document});
    await new Promise(resolve=>setImmediate(resolve));
    assert.deepEqual(contentDocument("7"),document);
    const release=retainDocument("7");release();
  } finally {bridge.dispose();}
  assert.deepEqual(contentDocument("7"),document);
  assert.throws(()=>new ContentBridge(()=>{}, {service}),/page-lifetime/);
});
