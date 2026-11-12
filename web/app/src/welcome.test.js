// SPDX-License-Identifier: EUPL-1.2
import {test} from "node:test";
import assert from "node:assert/strict";
import {welcomeDocuments} from "./welcome.js";
const server = {id:"one",status:"connected",welcome_token:"1"};
const update = {kind:"welcome",server:"one",token:"1",document:{blocks:[{kind:"text",text:"something.de"}]}};
test("documents survive ordinary snapshots without retransmission and cannot survive revocation",()=>{
  const first=welcomeDocuments(new Map(),[server],[update]);
  assert.equal(first.get("one").document,update.document);
  assert.equal(welcomeDocuments(first,[server]).get("one"),first.get("one"));
  for(const servers of [[],[{...server,status:"disconnected"}],[{...server,status:"unavailable"}],[{...server,welcome_token:null}],[{...server,welcome_token:"2"}]]) {
    assert.equal(welcomeDocuments(first,servers,[update]).size,0);
  }
});
test("late/other-server documents never replace the current welcome",()=>{
  assert.equal(welcomeDocuments(new Map(),[server],[{...update,token:"old"},{...update,server:"other"}]).size,0);
  const next={...update,token:"2"};
  const result=welcomeDocuments(welcomeDocuments(new Map(),[server],[update]),[{...server,welcome_token:"2"}],[update,next]);
  assert.equal(result.get("one").token,"2");
});
