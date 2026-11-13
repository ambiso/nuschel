// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {createTimestampFormatter,timestampFormats,observeMessageTimeLocale} from "./message-time.js";
test("shared formatters preserve original Date locale/options output including non-Gregorian calendars",()=>{
 for(const locale of [undefined,"en-US","de-DE","ar-EG","ja-JP-u-ca-japanese"]){
  const formatter=createTimestampFormatter(locale);
  for(const value of [0,-1,"1794484800123",Date.UTC(2024,2,31,1,30),Date.UTC(2024,9,27,1,30)]){
   const date=new Date(Number(value)),actual=formatter.format(value);
   assert.equal(actual.iso,date.toISOString());assert.equal(actual.title,date.toLocaleString(locale));
   assert.equal(actual.label,date.toLocaleTimeString(locale??[],{hour:"2-digit",minute:"2-digit"}));
  }
 }
});
test("invalid timestamps stay inert and repeated rows construct only two formatters",()=>{
 const original=Intl.DateTimeFormat;let constructors=0;
 Intl.DateTimeFormat=new Proxy(original,{construct(target,args){constructors++;return Reflect.construct(target,args);}});
 try{
  const formatter=createTimestampFormatter();
  for(let index=0;index<1000;index++)formatter.format(index);
  for(const value of [undefined,"not-a-date",Infinity,Number.MAX_VALUE,Symbol("invalid")])assert.deepEqual(formatter.format(value),{iso:undefined,title:"Invalid Date",label:"Invalid Date"});
  assert.equal(constructors,2);
 }finally{Intl.DateTimeFormat=original;}
});
test("locale refresh owns two lifecycle listeners, no timers, and cleans up",()=>{
 const events=new Map(),window={addEventListener:(kind,fn)=>events.set(kind,fn),removeEventListener:(kind,fn)=>{assert.equal(events.get(kind),fn);events.delete(kind);}};
 let calls=0;const stop=timestampFormats.subscribe(()=>calls++),cleanup=observeMessageTimeLocale(window),before=calls;
 events.get("focus")();assert.equal(calls,before,"unchanged default locale/timezone does not invalidate rows");
 events.get("languagechange")();assert.equal(calls,before+1);
 cleanup();assert.equal(events.size,0);stop();
});
