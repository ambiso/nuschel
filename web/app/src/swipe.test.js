// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {SWIPE,startSwipe,moveSwipe,swipeTravel,releaseVelocity,endSwipe,swipeExempt,SWIPE_EXEMPT} from "./swipe.js";

// A straight drag from (x,y) by (dx,dy), sampled every 16 ms over `ms`.
function drag(direction,{x=100,y=300,dx,dy=0,ms=200,steps=Math.max(2,Math.round(ms/16))}) {
  let swipe=startSwipe({x,y,t:0},direction);
  for(let i=1;i<=steps;i++)swipe=moveSwipe(swipe,{x:x+dx*i/steps,y:y+dy*i/steps,t:ms*i/steps});
  return swipe;
}

test("the axis is decided only after the slop, and a sideways drag must dominate",()=>{
  const start=startSwipe({x:0,y:0,t:0},1);
  assert.equal(moveSwipe(start,{x:6,y:2,t:16}).axis,"pending");
  assert.equal(moveSwipe(start,{x:12,y:3,t:16}).axis,"horizontal");
  // Diagonal (under 1.5:1) and vertical drags stay with scrolling.
  assert.equal(moveSwipe(start,{x:12,y:10,t:16}).axis,"rejected");
  assert.equal(moveSwipe(start,{x:1,y:14,t:16}).axis,"rejected");
  // The wrong direction is never this swipe.
  assert.equal(moveSwipe(start,{x:-14,y:0,t:16}).axis,"rejected");
  assert.equal(moveSwipe(startSwipe({x:0,y:0,t:0},-1),{x:-14,y:0,t:16}).axis,"horizontal");
});

test("a decision is final: returning to the axis later does not revive it",()=>{
  let swipe=moveSwipe(startSwipe({x:0,y:0,t:0},1),{x:0,y:20,t:16});
  swipe=moveSwipe(swipe,{x:200,y:20,t:120});
  assert.equal(swipe.axis,"rejected");
  assert.equal(swipeTravel(swipe),0);
  assert.equal(endSwipe(swipe,{x:220,y:20,t:140}),false);
  let horizontal=moveSwipe(startSwipe({x:0,y:0,t:0},1),{x:20,y:0,t:16});
  horizontal=moveSwipe(horizontal,{x:22,y:60,t:200});
  assert.equal(horizontal.axis,"horizontal");
});

test("a long press before moving is left to text selection",()=>{
  let swipe=startSwipe({x:0,y:0,t:0},1);
  swipe=moveSwipe(swipe,{x:2,y:0,t:SWIPE.hold+1});
  assert.equal(swipe.axis,"rejected");
  assert.equal(endSwipe(swipe,{x:200,y:0,t:SWIPE.hold+200}),false);
});

test("a slow drag commits by distance, a short one does not",()=>{
  assert.equal(endSwipe(drag(1,{dx:SWIPE.distance+6,ms:900})),true);
  assert.equal(endSwipe(drag(1,{dx:SWIPE.distance-10,ms:900})),false);
  assert.equal(endSwipe(drag(-1,{dx:-(SWIPE.distance+6),ms:900})),true);
});

test("a quick flick commits with less travel, but not below the fling minimum",()=>{
  const flick=drag(1,{dx:40,ms:60});
  assert.ok(releaseVelocity(flick)>=SWIPE.velocity);
  assert.equal(endSwipe(flick),true);
  assert.equal(endSwipe(drag(1,{dx:SWIPE.fling-8,ms:20,steps:2})),false);
});

test("flinging back against the swipe cancels it even after enough travel",()=>{
  let swipe=drag(1,{dx:200,ms:300});
  swipe=moveSwipe(swipe,{x:250,y:300,t:316});
  swipe=moveSwipe(swipe,{x:190,y:300,t:332});
  assert.ok(swipeTravel(swipe)>=SWIPE.distance);
  assert.ok(releaseVelocity(swipe)<-SWIPE.velocity);
  assert.equal(endSwipe(swipe),false);
});

test("a pause before release leaves only the distance rule",()=>{
  let swipe=drag(1,{dx:30,ms:40});
  swipe=moveSwipe(swipe,{x:130,y:300,t:400});
  assert.equal(releaseVelocity(swipe),0);
  assert.equal(endSwipe(swipe),false);
});

test("a swipe that drifts more vertically than sideways does not commit",()=>{
  let swipe=moveSwipe(startSwipe({x:0,y:0,t:0},1),{x:20,y:0,t:16});
  swipe=moveSwipe(swipe,{x:80,y:120,t:200});
  assert.equal(endSwipe(swipe),false);
});

test("travel follows the finger and never goes negative",()=>{
  let swipe=drag(-1,{dx:-90,ms:200});
  assert.equal(swipeTravel(swipe),90);
  swipe=moveSwipe(swipe,{x:150,y:300,t:260});
  assert.equal(swipeTravel(swipe),0);
});

test("the sample history stays bounded",()=>{
  const swipe=drag(1,{dx:400,ms:4000,steps:500});
  assert.equal(swipe.samples.length,SWIPE.samples);
});

test("an incomplete gesture never commits",()=>{
  assert.equal(endSwipe(null),false);
  assert.equal(endSwipe(startSwipe({x:0,y:0,t:0},1)),false);
});

// Minimal element stand-ins: closest() over a parent chain, scroll sizes and computed style.
function element(name,{parent=null,matches=false,scrollWidth=100,clientWidth=100,overflowX="visible",touchAction="auto"}={}) {
  const node={name,parentElement:parent,scrollWidth,clientWidth,style:{overflowX,touchAction},
    closest(selector){assert.equal(selector,SWIPE_EXEMPT);for(let n=node;n;n=n.parentElement)if(n.matches)return n;return null;},matches};
  return node;
}
const style=node=>node.style;

test("text fields, code and handles keep their own drags",()=>{
  const root=element("chat"),messages=element("messages",{parent:root,overflowX:"auto",touchAction:"pan-y pinch-zoom"});
  const pre=element("pre",{parent:messages,matches:true}),code=element("span",{parent:pre});
  assert.equal(swipeExempt(code,root,style),true);
  assert.equal(swipeExempt(element("textarea",{parent:root,matches:true}),root,style),true);
  assert.equal(swipeExempt(element("text",{parent:messages}),root,style),false);
  assert.equal(swipeExempt(null,root,style),true);
  assert.equal(swipeExempt({},root,style),true);
});

test("a region the browser can pan sideways keeps the gesture",()=>{
  const root=element("chat");
  const wide=element("wide",{parent:root,overflowX:"auto",scrollWidth:400,clientWidth:200});
  assert.equal(swipeExempt(element("inside",{parent:wide}),root,style),true);
  // The same overflow with sideways panning disabled, or nothing to scroll, is ours.
  const restricted=element("restricted",{parent:root,overflowX:"auto",scrollWidth:400,clientWidth:200,touchAction:"pan-y"});
  assert.equal(swipeExempt(element("inside",{parent:restricted}),root,style),false);
  const fits=element("fits",{parent:root,overflowX:"auto",scrollWidth:200,clientWidth:200});
  assert.equal(swipeExempt(element("inside",{parent:fits}),root,style),false);
  const panX=element("panx",{parent:root,overflowX:"scroll",scrollWidth:400,clientWidth:200,touchAction:"pan-x"});
  assert.equal(swipeExempt(panX,root,style),true);
  // Only elements inside the gesture root count.
  const outer=element("outer",{overflowX:"auto",scrollWidth:900,clientWidth:200});
  const inner=element("root",{parent:outer});
  assert.equal(swipeExempt(element("leaf",{parent:inner}),inner,style),false);
});
