// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {SdkRpc,SdkUnavailableError,SDK_STARTUP_TIMEOUT,SDK_RPC_TIMEOUT} from "../web/app/src/sdk-rpc.js";
class Clock {
 constructor(){this.now=0;this.id=0;this.jobs=new Map();}
 setTimeout(callback,delay){const id=++this.id;this.jobs.set(id,{at:this.now+delay,callback});return id;}
 clearTimeout(id){this.jobs.delete(id);}
 advance(ms){this.now+=ms;for(const [id,job] of [...this.jobs])if(job.at<=this.now&&this.jobs.delete(id))job.callback();}
}
function fixture({postFails=false,terminationFails=false}={}) {
 const clock=new Clock(),trace=[],worker={posted:[],terminated:0,
  postMessage(data){if(postFails)throw new Error("clone failed");this.posted.push(data);},
  terminate(){this.terminated++;trace.push("terminate");if(terminationFails)throw new Error("host boundary failed");}};
 const rpc=new SdkRpc(worker,{clock,onClose:message=>trace.push(message)});
 return {worker,clock,rpc,trace};
}
test("startup without any RPC reaches one permanent retirement at its independent30s deadline",async()=>{
 const f=fixture();assert.equal(f.rpc.phase,"starting");assert.equal(f.rpc.initialize("ws://127.0.0.1/relay"),true);
 assert.equal(f.worker.posted.length,1);assert.equal(f.rpc.size,0);
 f.clock.advance(SDK_STARTUP_TIMEOUT-1);assert.equal(f.rpc.closed,false);
 f.clock.advance(1);assert.equal(f.rpc.closed,true);assert.equal(f.rpc.phase,"closed");assert.equal(f.worker.terminated,1);
 assert.equal(f.trace[0],"terminate");assert.match(f.trace[1],/finish starting/);assert.equal(f.clock.jobs.size,0);
 await assert.rejects(f.rpc.request("connect",[]),SdkUnavailableError);assert.equal(f.worker.posted.length,1);
});
test("first owned ready before deadline clears only startup and ordinary RPC keeps its own deadline",async()=>{
 const f=fixture();f.rpc.initialize("local");f.clock.advance(SDK_STARTUP_TIMEOUT-1);
 assert.equal(f.rpc.acceptReady(),true);assert.equal(f.rpc.phase,"ready");assert.equal(f.clock.jobs.size,0);
 f.clock.advance(SDK_STARTUP_TIMEOUT);assert.equal(f.worker.terminated,0);
 const result=f.rpc.request("older").catch(error=>error);f.clock.advance(SDK_RPC_TIMEOUT);
 assert.equal(f.worker.terminated,1);assert.ok(await result instanceof SdkUnavailableError);assert.match(f.trace[1],/did not respond/);
});
test("duplicate init/ready and premature ready cannot reset deadlines or reinitialize",()=>{
 const f=fixture();assert.equal(f.rpc.acceptReady(),false);assert.equal(f.clock.jobs.size,1);
 f.rpc.initialize("first");f.clock.advance(10000);assert.equal(f.rpc.initialize("second"),false);
 assert.equal(f.worker.posted.length,1);assert.equal(f.rpc.acceptReady(),true);assert.equal(f.rpc.acceptReady(),false);
 f.clock.advance(SDK_STARTUP_TIMEOUT);assert.equal(f.rpc.acceptReady(),false);assert.equal(f.worker.terminated,0);
 f.rpc.close("done",{notify:false});
});
test("request attempts during startup post nothing and cannot postpone watchdog",async()=>{
 const f=fixture();f.rpc.initialize("local");f.clock.advance(20000);
 await assert.rejects(f.rpc.request("join",[]),/still starting/);assert.equal(f.rpc.size,0);assert.equal(f.worker.posted.length,1);
 f.clock.advance(10000);assert.equal(f.worker.terminated,1);assert.equal(f.rpc.acceptReady(),false);
});
test("late ready cannot reopen a retired owner or trigger another termination",()=>{
 const f=fixture();f.rpc.initialize("local");f.clock.advance(SDK_STARTUP_TIMEOUT);
 for(let i=0;i<20;i++){assert.equal(f.rpc.acceptReady(),false);assert.equal(f.rpc.initialize("retry"),false);}
 assert.equal(f.rpc.closed,true);assert.equal(f.worker.terminated,1);assert.equal(f.worker.posted.length,1);
});
test("init post failure is startup-specific and retires even with zero request entries",()=>{
 const f=fixture({postFails:true});assert.equal(f.rpc.initialize("local"),false);
 assert.equal(f.rpc.size,0);assert.equal(f.rpc.closed,true);assert.equal(f.worker.terminated,1);assert.equal(f.trace[0],"terminate");
 assert.match(f.trace[1],/could not start/);f.clock.advance(SDK_STARTUP_TIMEOUT);assert.equal(f.worker.terminated,1);
});
test("failed termination still closes startup with no new work or false ready",()=>{
 const f=fixture({terminationFails:true});f.rpc.initialize("local");f.clock.advance(SDK_STARTUP_TIMEOUT);
 assert.equal(f.rpc.closed,true);assert.equal(f.rpc.phase,"closed");assert.equal(f.rpc.acceptReady(),false);
 assert.equal(f.worker.terminated,1);assert.equal(f.clock.jobs.size,0);assert.match(f.trace[1],/could not be closed/);
});
test("startup disposal clears watchdog without notifying an unmounted UI",()=>{
 const f=fixture();f.rpc.initialize("local");f.rpc.close("closed",{notify:false});f.clock.advance(SDK_STARTUP_TIMEOUT);
 assert.deepEqual(f.trace,["terminate"]);assert.equal(f.clock.jobs.size,0);
});
