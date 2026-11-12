// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {serverIndices,moveServer,sameOrder,restoreOrder,edgeSpeed} from "./server-order.js";
test("display permutations preserve explicit command indices",()=>{
  const servers=serverIndices([{id:"c",runtime_index:2},{id:"a",runtime_index:0},{id:"b",runtime_index:1}],true);
  assert.equal(servers.find(s=>s.id==="a").runtime_index,0);
  assert.deepEqual(servers.map(s=>s.runtime_index),[2,0,1]);
  assert.throws(()=>serverIndices([{id:"a"}],true));
  assert.throws(()=>serverIndices([{runtime_index:0},{runtime_index:0}],true));
  assert.throws(()=>serverIndices([{runtime_index:3}],true));
});
test("legacy snapshots only infer indices before ordering is negotiated",()=>assert.deepEqual(serverIndices([{id:"a"},{id:"b"}]).map(s=>s.runtime_index),[0,1]));
test("preview is immutable and cancelling restores its baseline",()=>{
  const ids=["a","b","c"];
  assert.deepEqual(moveServer(ids,"a",2),["b","c","a"]);
  assert.deepEqual(ids,["a","b","c"]);
  assert.equal(moveServer(ids,"unknown",1),ids);
  assert.ok(sameOrder(ids,ids.slice())); assert.ok(!sameOrder(ids,["c","b","a"]));
});
test("failed persistence restores prior positions without losing concurrent add/remove IDs",()=>assert.deepEqual(restoreOrder(["a","b","c"],["c","d","a"]),["a","c","d"]));
test("edge scrolling stops outside its lane and away from edges",()=>{
  assert.equal(edgeSpeed(50,100),0);assert.equal(edgeSpeed(0,100,false),0);assert.equal(edgeSpeed(-1,100),0);
  assert.equal(edgeSpeed(101,100),0);assert.ok(edgeSpeed(4,100)<0);assert.ok(edgeSpeed(96,100)>0);
});
