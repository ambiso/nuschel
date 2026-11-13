// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {UI_METHODS,projectedAction,actionProof,serverReference} from "./action-context.js";
const server={runtime_index:3,id:"original",catalog_instance:"9",generation:"15",own_instance:"21",muted:true,deafened:true,channels:[{id:7,instance:"24",peer:null}],users:[{session:4,instance:"32"}]};
const timeline={server:"original",channel:7,instance:"24",peer:null};
test("every allowed UI method has an explicit fixed argument/guard classification",()=>{
 const args={add_server:["","127.0.0.1",64738,"fixture",null],connect:[3],disconnect:[3],accept_certificate:[3,"a".repeat(64)],open:[3,7],browse_server:[3],open_direct:[3,4],send_text:["draft"],older:[],join:[3,7],toggle_mute:[3],toggle_deaf:[3],audio_ready:[{}],voice_packet:[{}],remove_server:[3],dismiss_notice:[3],dismiss_welcome:[3,"ticket"],connect_with_password:[3,"not-a-real-secret"],stop_audio:[],edit_server:[3,"","127.0.0.1",64738,"fixture",null],set_server_order:[["original"]],content_complete:["1",""],content_request:["1"],image_begin:[{}],image_send:[{}],image_cancel:["1"]};
 assert.deepEqual([...UI_METHODS].sort(),Object.keys(args).sort());
 const exempt=new Set(["add_server","set_server_order","stop_audio","audio_ready","voice_packet","content_complete","content_request","image_begin","image_send","image_cancel"]);
 for(const [method,values] of Object.entries(args)){
  const projection=projectedAction(method,values);assert.equal(projection===null,exempt.has(method),method);
  assert.throws(()=>projectedAction(method,[...values,0]));
 }
 assert.throws(()=>projectedAction("new_unreviewed_action",[]));
});
test("proof captures visible exact scalar references and never follows later selection/ID reuse",()=>{
 const proof=actionProof("send_text",["body"],[server],timeline,null);
 const target=actionProof("join",[3,7],[server],timeline,null);
 assert.equal(proof.server.id,"original");assert.equal(proof.selected.instance,"24");assert.deepEqual(target.target,{kind:"channel",id:7,instance:"24",peer:null});
 const changed={...server,generation:"16",channels:[{id:7,instance:"99",peer:null}]};
 assert.equal(proof.server.generation,"15");assert.equal(target.target.instance,"24");
 assert.equal(actionProof("join",[3,7],[changed],timeline,null).target.instance,"99");
 assert.throws(()=>actionProof("join",[0,7],[server],timeline,null));
});
test("newly configured form chain uses only its explicit returned local reference",()=>{
 const reference=serverReference(server);
 assert.deepEqual(actionProof("connect",[0],[],null,null,reference),{server:reference});
 assert.deepEqual(actionProof("edit_server",[0,"","a",1,"b",null],[],null,null,reference),{server:reference});
 assert.throws(()=>actionProof("send_text",["draft"],[],timeline,null,reference));
 assert.throws(()=>serverReference({...server,catalog_instance:"0"}));
 assert.throws(()=>serverReference({...server,generation:15}));
});

test("RPC serializes bounded original proof before posting, without re-reading newer view state",async()=>{
 const {SdkRpc}=await import("./sdk-rpc.js");const messages=[];let next=0;
 const worker={postMessage:value=>messages.push(value),terminate(){}};
 const rpc=new SdkRpc(worker,{clock:{setTimeout:()=>++next,clearTimeout(){}}});rpc.initialize("ws://127.0.0.1/relay");rpc.acceptReady();messages.length=0;
 const proof=actionProof("send_text",["draft"],[server],timeline,null);
 const pending=rpc.request("send_text",["draft"],proof);proof.server.generation="999";
 assert.equal(JSON.parse(messages[0].context).server.generation,"15");
 rpc.accept({kind:"result",id:messages[0].id,error:"stale target"});await assert.rejects(pending,/stale target/);
 const before=messages.length;await assert.rejects(rpc.request("send_text",["draft"],{value:"🙂".repeat(1500)}),/limit/);assert.equal(messages.length,before);assert.equal(rpc.size,0);
 const lease=rpc.request("content_request",["1"]);assert.equal(Object.hasOwn(messages.at(-1),"context"),false);rpc.accept({kind:"result",id:messages.at(-1).id,value:null});await lease;rpc.close();
});
