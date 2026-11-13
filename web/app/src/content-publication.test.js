// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {createContentPublication} from "./content-publication.js";
test("content publication owns one timer; urgent navigation cancels it and publishes current state",()=>{
 const timers=new Map(),micro=[];let next=0,state=0;const observed=[];
 const clock={setTimeout(fn,delay){assert.equal(delay,16);timers.set(++next,fn);return next;},clearTimeout(id){timers.delete(id);},queueMicrotask(fn){micro.push(fn);}};
 const publication=createContentPublication(()=>observed.push(state),clock);
 for(let i=0;i<1000;i++){state=i;publication.content();}
 assert.equal(timers.size,1);assert.deepEqual(observed,[]);
 state=1000;publication.wake();publication.wake();publication.content();
 assert.equal(timers.size,0);assert.equal(micro.length,1);
 micro.shift()();assert.deepEqual(observed,[1000]);
 publication.content();const fire=[...timers.values()][0];timers.clear();state=1001;fire();
 assert.equal(micro.length,1);micro.shift()();assert.deepEqual(observed,[1000,1001]);
 assert.equal(timers.size,0);
});
