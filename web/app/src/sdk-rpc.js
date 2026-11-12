// SPDX-License-Identifier: EUPL-1.2
// An RPC timeout is not a worker acknowledgment. Retire the entire owned lane
// before settling timed-out requests, so its inbox cannot outlive its credits.
export const SDK_RPC_LIMIT=128, SDK_RPC_TIMEOUT=15000;
export class SdkUnavailableError extends Error {
  constructor(message="The client is unavailable. Reload to reconnect.") {super(message);this.name="SdkUnavailableError";this.code="SDK_UNAVAILABLE";}
}
export class SdkRpc {
  constructor(worker,{onClose=()=>{},clock=globalThis}={}) {
    this.worker=worker;this.onClose=onClose;this.clock=clock;
    this.pending=new Map();this.next=0;this.closed=false;
  }
  get size(){return this.pending.size;}
  request(method,args=[]) {
    if(this.closed)return Promise.reject(new SdkUnavailableError());
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
    this.closed=true;
    for(const entry of this.pending.values())this.clock.clearTimeout(entry.timer);
    try {this.worker.terminate();}
    catch {
      // A broken termination boundary is not an ACK either. Stop all new work
      // and revoke audio, but retain bounded unresolved slots until page death.
      if(notify)this.onClose("The browser worker could not be closed. Reload this page.");
      return;
    }
    // These callbacks may release BrowserAudio's global two-send credits, so
    // they run only after termination and synchronous route/consent revocation.
    try {if(notify)this.onClose(reason);}
    finally {
      const error=new SdkUnavailableError(reason);
      for(const entry of this.pending.values())entry.reject(error);
      this.pending.clear();
    }
  }
}
