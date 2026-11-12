// SPDX-License-Identifier: EUPL-1.2
// An RPC timeout is not a worker acknowledgment. Retire the entire owned lane
// before settling timed-out requests. A closed owner never posts new work
// while the browser finishes its asynchronous native shutdown.
export const SDK_RPC_LIMIT=128, SDK_RPC_TIMEOUT=15000, SDK_STARTUP_TIMEOUT=30000;
export class SdkUnavailableError extends Error {
  constructor(message="The client is unavailable. Reload to reconnect.") {super(message);this.name="SdkUnavailableError";this.code="SDK_UNAVAILABLE";}
}
export class SdkRpc {
  constructor(worker,{onClose=()=>{},clock=globalThis}={}) {
    this.worker=worker;this.onClose=onClose;this.clock=clock;
    this.pending=new Map();this.next=0;this.closed=false;this.phase="starting";this.initialized=false;this.lastState=0;this.statePending=null;
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
    if(this.closed||id!==this.statePending)return false;
    try { this.worker.postMessage({method:"state_applied",args:[id]});this.statePending=null;return true; }
    catch { this.close("The browser worker could not accept its state acknowledgment. Reload to reconnect.");return false; }
  }
  request(method,args=[]) {
    if(this.closed)return Promise.reject(new SdkUnavailableError());
    if(this.phase!=="ready")return Promise.reject(new Error("The client is still starting."));
    if(this.pending.size>=SDK_RPC_LIMIT)return Promise.reject(new Error("The client is busy. Try again shortly."));
    if(!Number.isSafeInteger(this.next+1)) {
      this.close("The client request counter is exhausted. Reload to reconnect.");
      return Promise.reject(new SdkUnavailableError());
    }
    const id=++this.next;
    return new Promise((resolve,reject)=>{
      const timer=this.clock.setTimeout(()=>this.close("The browser worker did not respond. Reload to reconnect."),SDK_RPC_TIMEOUT);
      this.pending.set(id,{resolve,reject,timer});
      try {this.worker.postMessage({id,method,args});}
      catch {this.close("The browser worker could not accept a command. Reload to reconnect.");}
    });
  }
  accept(data) {
    if(this.closed||data?.kind!=="result"||!Number.isSafeInteger(data.id)||data.id<=0)return false;
    const entry=this.pending.get(data.id);if(!entry)return false;
    this.pending.delete(data.id);this.clock.clearTimeout(entry.timer);
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
      this.pending.clear();
    }
  }
}
