// SPDX-License-Identifier: EUPL-1.2
// One posted snapshot plus bounded event bookkeeping. The next snapshot is
// collected from the current SDK model only after the UI acknowledges applying
// the preceding one; no queue of superseded timeline copies is retained.
export const MAX_STATE_BYTES=32*1024*1024, MAX_STATE_EVENTS=128, MAX_STATE_EVENT_BYTES=2*1024*1024, STATE_ACK_MS=15000;
function bytes(raw,limit) {
  if(typeof raw!=="string"||raw.length>limit)throw new Error("Browser state exceeds its publication limit.");
  const size=new TextEncoder().encode(raw).length;
  if(size>limit)throw new Error("Browser state exceeds its publication limit.");
  return size;
}
export function stateFrame(publication,raw,updates,maxBytes=MAX_STATE_BYTES) {
  let size=256+bytes(JSON.stringify(updates),maxBytes);
  for(const value of Object.values(raw)) { size+=bytes(value,maxBytes);if(size>maxBytes)throw new Error("Browser state exceeds its publication limit."); }
  const servers=JSON.parse(raw.servers),timeline=JSON.parse(raw.timeline),voice=JSON.parse(raw.voice);
  if(!Array.isArray(servers)||!(timeline===null||typeof timeline==="object")||!(voice===null||typeof voice==="object"))throw new Error("Invalid browser state publication.");
  return {packet:{kind:"state",publication,servers,timeline,voice,updates},bytes:size};
}
export class StatePublication {
  constructor({collect,post,fatal,clock={now:()=>performance.now(),setTimeout:(...args)=>setTimeout(...args),clearTimeout:id=>clearTimeout(id)}}) {
    this.collect=collect;this.post=post;this.fatal=fatal;this.clock=clock;
    this.next=0;this.inflight=null;this.events=[];this.eventBytes=0;this.dirty=false;this.closed=false;this.timer=null;
  }
  fail(message) {
    if(this.closed)return;
    this.closed=true;this.clock.clearTimeout(this.timer);this.timer=null;
    this.events=[];this.eventBytes=0;this.dirty=false;
    // Closing never grants replacement credit or claims the old UI/Worker has
    // physically exited. This owner stays permanently consumed.
    this.fatal(message);
  }
  changed(raw) {
    if(this.closed)return;
    try {
      const size=bytes(raw,MAX_STATE_EVENT_BYTES),events=JSON.parse(raw);
      if(!Array.isArray(events)||events.length+this.events.length+(this.inflight?.events??0)>MAX_STATE_EVENTS)throw new Error("Browser state event limit exceeded.");
      const charge=events.length?size:0;
      if(charge+this.eventBytes+(this.inflight?.eventBytes??0)>MAX_STATE_EVENT_BYTES)throw new Error("Browser state event limit exceeded.");
      this.events.push(...events);this.eventBytes+=charge;this.dirty=true;this.flush();
    } catch(error) { this.fail(String(error?.message||error)); }
  }
  flush() {
    if(this.closed||this.inflight||!this.dirty)return;
    if(!Number.isSafeInteger(this.next+1)){this.fail("Browser state counter exhausted.");return;}
    try {
      const id=this.next+1,built=this.collect(id,this.events);
      if(!built||!Number.isSafeInteger(built.bytes)||built.bytes<0||built.bytes>MAX_STATE_BYTES)throw new Error("Browser state exceeds its publication limit.");
      this.next=id;
      this.inflight={id,bytes:built.bytes,events:this.events.length,eventBytes:this.eventBytes,deadline:this.clock.now()+STATE_ACK_MS};
      this.events=[];this.eventBytes=0;this.dirty=false;
      this.timer=this.clock.setTimeout(()=>this.fail("The browser did not apply its state in time. Reload to reconnect."),STATE_ACK_MS);
      this.post(built.packet);
    } catch(error) { this.fail(String(error?.message||error)); }
  }
  ack(id) {
    if(this.closed)return false;
    if(!Number.isSafeInteger(id)||id<=0||id>this.next){this.fail("Invalid browser state acknowledgment.");return false;}
    if(id!==this.inflight?.id)return false; // old/duplicate ACK cannot free new credit
    if(this.clock.now()>=this.inflight.deadline){this.fail("The browser applied its state too late. Reload to reconnect.");return false;}
    this.clock.clearTimeout(this.timer);this.timer=null;this.inflight=null;this.flush();return !this.closed;
  }
}
