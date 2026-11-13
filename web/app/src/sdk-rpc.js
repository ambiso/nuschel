// SPDX-License-Identifier: EUPL-1.2
// An RPC timeout is not a worker acknowledgment. Retire the entire owned lane
// before settling timed-out requests. A closed owner never posts new work
// while the browser finishes its asynchronous native shutdown.
// The SDK posts at most this many content jobs before their ACKs (Rust
// MAX_POSTED_JOBS; tests/content_window.unit.mjs keeps the two equal).
export const CONTENT_WINDOW=4;
export const SDK_RPC_LIMIT=128, SDK_RPC_ACK_RESERVE=CONTENT_WINDOW, SDK_RPC_TIMEOUT=15000, SDK_STARTUP_TIMEOUT=30000;
// A content completion ACK releases credit the SDK holds for one posted job.
// Four of the 128 slots (one per posted job) are reserved for these ACKs, so
// ordinary command pressure can never refuse one locally. The lane's single
// best-effort retirement notice shares that reserve and is refused, not
// queued, when all four are in flight.
const ACK_METHODS=new Set(["content_complete","content_retire"]);
export class SdkUnavailableError extends Error {
  constructor(message="The client is unavailable. Reload to reconnect.") {super(message);this.name="SdkUnavailableError";this.code="SDK_UNAVAILABLE";}
}
export class SdkRpc {
  constructor(worker,{onClose=()=>{},clock=globalThis}={}) {
    this.worker=worker;this.onClose=onClose;this.clock=clock;
    this.pending=new Map();this.acks=0;this.next=0;this.closed=false;this.phase="starting";this.initialized=false;this.lastState=0;this.statePending=null;
    this.startupTimer=this.clock.setTimeout(()=>this.close("The browser client did not finish starting. Reload to try again."),SDK_STARTUP_TIMEOUT);
  }
  get size(){return this.pending.size;}
  initialize(relay) {
    if(this.closed||this.initialized)return false;
    this.initialized=true;
    try {this.worker.postMessage({method:"init",relay});return true;}
    catch {this.close("The browser client could not start. Reload to try again.");return false;}
  }
  acceptReady() {
    if(this.closed||this.phase!=="starting"||!this.initialized)return false;
    this.clock.clearTimeout(this.startupTimer);this.startupTimer=null;this.phase="ready";
    return true;
  }
  acceptState(id) {
    if(this.closed||this.phase!=="ready")return false;
    if(!Number.isSafeInteger(id)||id!==this.lastState+1||this.statePending!==null){this.close("The browser worker sent an invalid state sequence. Reload to reconnect.");return false;}
    this.lastState=id;this.statePending=id;return true;
  }
  ackState(id) {
    if(this.closed||this.statePending===null||!Number.isSafeInteger(id)||id!==this.statePending)return false;
    try { this.worker.postMessage({method:"state_applied",args:[id]});this.statePending=null;return true; }
    catch { this.close("The browser worker could not accept its state acknowledgment. Reload to reconnect.");return false; }
  }
  request(method,args=[],proof) {
    if(this.closed)return Promise.reject(new SdkUnavailableError());
    if(this.phase!=="ready")return Promise.reject(new Error("The client is still starting."));
    let context;
    if(proof!==undefined){
      try{context=JSON.stringify(proof);if(typeof context!=="string"||context.length>4096||new TextEncoder().encode(context).length>4096)throw new Error("Action context exceeds the browser limit.");}
      catch(error){return Promise.reject(error);}
    }
    const ack=ACK_METHODS.has(method);
    if(ack?this.acks>=SDK_RPC_ACK_RESERVE:this.pending.size-this.acks>=SDK_RPC_LIMIT-SDK_RPC_ACK_RESERVE)return Promise.reject(new Error("The client is busy. Try again shortly."));
    if(!Number.isSafeInteger(this.next+1)) {
      this.close("The client request counter is exhausted. Reload to reconnect.");
      return Promise.reject(new SdkUnavailableError());
    }
    const id=++this.next;
    return new Promise((resolve,reject)=>{
      const timer=this.clock.setTimeout(()=>this.close("The browser worker did not respond. Reload to reconnect."),SDK_RPC_TIMEOUT);
      this.pending.set(id,{resolve,reject,timer,ack});if(ack)this.acks++;
      try {this.worker.postMessage(context===undefined?{id,method,args}:{id,method,args,context});}
      catch {this.close("The browser worker could not accept a command. Reload to reconnect.");}
    });
  }
  accept(data) {
    if(this.closed||data?.kind!=="result"||!Number.isSafeInteger(data.id)||data.id<=0)return false;
    const entry=this.pending.get(data.id);if(!entry)return false;
    this.pending.delete(data.id);if(entry.ack)this.acks--;this.clock.clearTimeout(entry.timer);
    if(data.error!==undefined)entry.reject(new Error(String(data.error).slice(0,4096)));
    else entry.resolve(data.value);
    return true;
  }
  close(reason="The browser worker stopped. Reload to reconnect.",{notify=true}={}) {
    if(this.closed)return;
    this.closed=true;this.phase="closed";
    this.clock.clearTimeout(this.startupTimer);this.startupTimer=null;
    for(const entry of this.pending.values())this.clock.clearTimeout(entry.timer);
    try {this.worker.terminate();}
    catch {
      // A broken termination boundary is not an ACK either. Stop all new work
      // and revoke audio, but retain bounded unresolved slots until page death.
      if(notify)this.onClose("The browser worker could not be closed. Reload this page.");
      return;
    }
    // terminate() is a shutdown request, not a synchronous physical-exit ACK.
    // This owner stays permanently closed (no auto-restart), so its inbox can
    // never acquire more work while native shutdown finishes. Only now revoke
    // the route/consent and synthetically settle its bounded pending promises.
    try {if(notify)this.onClose(reason);}
    finally {
      const error=new SdkUnavailableError(reason);
      for(const entry of this.pending.values())entry.reject(error);
      this.pending.clear();this.acks=0;
    }
  }
}
