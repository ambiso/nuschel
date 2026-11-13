// SPDX-License-Identifier: EUPL-1.2
// Private shared scheduling primitive. Facades keep separate page-lifetime owners,
// budgets, caches and validators. There is deliberately no reset/replacement API.
export class WorkerUnavailableError extends Error {
  constructor(message) { super(message);this.name="WorkerUnavailableError";this.code="worker-unavailable"; }
}
export const bounded=(value,maximum)=>Number.isFinite(value)?Math.min(maximum,Math.max(1,Math.floor(value))):maximum;
export const workerSnapshot=job=>job?Object.freeze({id:job.id,size:job.size,deadline:job.deadline}):null;

export class WorkerOwner {
  constructor(factory,startupMs,config) {
    this.config=config;
    this.factory=factory;this.startupMs=startupMs;this.phase="empty";
    this.queue=[];this.active=null;this.bytes=0;this.next=0;this.worker=null;
    this.retiredOwner=null;this.retiredBytes=0;
  }
  error(message,unavailable=false) {
    message=message.replaceAll("Content",this.config.label).replaceAll("content",this.config.label.toLowerCase());
    return unavailable?new WorkerUnavailableError(message):new Error(message);
  }
  enqueue(client,packet,size,signal,timeoutMs) {
    if(this.phase==="retired")return Promise.reject(this.error("Content processing is unavailable. Reload to try again.",true));
    if(signal?.aborted)return Promise.reject(this.error("Content processing was cancelled."));
    if(this.queue.length+Number(Boolean(this.active))>=this.config.maxJobs||size>this.config.maxBytes-this.bytes)
      return Promise.reject(this.error("Content processing is busy; retry this message."));
    if(!Number.isSafeInteger(this.next+1))return Promise.reject(this.error("Content job identifiers are exhausted."));
    return new Promise((resolve,reject)=>{
      // Until dispatch, `deadline` is a reject-only queue deadline. The worker
      // execution deadline that can retire this owner starts at dispatch.
      const job={id:++this.next,client,packet:{...packet},size,resolve,reject,signal,timeoutMs,deadline:performance.now()+timeoutMs,settled:false,released:false};
      if(packet.bytes)job.packet.bytes=packet.bytes.slice();
      job.abort=()=>this.cancel(job);
      job.timer=setTimeout(()=>this.expire(job),timeoutMs);
      signal?.addEventListener("abort",job.abort,{once:true});
      this.bytes+=size;this.queue.push(job);this.dispatch();
    });
  }
  start() {
    this.phase="starting";this.startDeadline=performance.now()+this.startupMs;
    try {
      const factory=this.factory;this.factory=null;
      const worker=factory();this.worker=worker;
      worker.onmessage=({data})=>{if(this.worker===worker&&this.phase!=="retired")this.message(data);};
      worker.onerror=event=>{if(this.worker===worker){event?.preventDefault?.();this.retire("The content worker failed. Reload to try again.");}};
      worker.onmessageerror=worker.onerror;
      this.startTimer=setTimeout(()=>{if(this.worker===worker&&this.phase==="starting")this.retire("The content worker could not start. Reload to try again.");},this.startupMs);
    }catch{this.retire("The content worker could not start. Reload to try again.");}
  }
  dispatch() {
    if(this.phase==="retired"||this.active||!this.queue.length)return;
    if(this.phase==="empty")this.start();
    if(this.phase==="starting"&&performance.now()>=this.startDeadline){this.retire("The content worker could not start. Reload to try again.");return;}
    if(this.phase!=="ready")return;
    const job=this.queue.shift();
    if(performance.now()>=job.deadline){this.complete(job,this.error("Content processing exceeded its deadline."));this.release(job);this.dispatch();return;}
    // Queue wait never consumes the execution budget of this serial worker.
    clearTimeout(job.timer);job.deadline=performance.now()+job.timeoutMs;job.timer=setTimeout(()=>this.expire(job),job.timeoutMs);
    this.active=job;
    try{this.worker.postMessage({id:job.id,...job.packet},job.packet.bytes?[job.packet.bytes.buffer]:[]);}
    catch{this.retire("The content worker could not accept this job. Reload to try again.");}
  }
  message(data) {
    if(data?.kind==="fatal"){this.retire("The content codec became unavailable. Reload to try again.");return;}
    if(data?.kind==="ready") {
      if(this.phase==="starting") {
        // A delayed timer callback must not make late readiness acceptable.
        if(performance.now()>=this.startDeadline){this.retire("The content worker could not start. Reload to try again.");return;}
        clearTimeout(this.startTimer);this.phase="ready";this.dispatch();
      }
      return; // duplicate readiness never resets a clock or owner
    }
    const job=this.active;
    if(!job||data?.id!==job.id||!["result","error"].includes(data.kind)) {
      this.retire("The content worker returned an invalid response. Reload to try again.");return;
    }
    if(performance.now()>=job.deadline){this.expire(job);return;}
    let value,error;
    if(data.kind==="error")error=this.error(typeof data.message==="string"?data.message.slice(0,200):"Content could not be processed.");
    else {
      try{value=this.config.validate(job.packet,data);}
      catch{this.retire("The content worker returned invalid or oversized content. Reload to try again.");return;}
    }
    // A cancelled consumer is already settled, but its exact real ACK still
    // releases this reservation. The derivative is never delivered to it.
    this.complete(job,error,value);this.release(job);this.dispatch();
  }
  complete(job,error,value) {
    if(job.settled)return;
    job.settled=true;job.signal?.removeEventListener("abort",job.abort);
    if(error)job.reject(error);else job.resolve(value);
    job.resolve=null;job.reject=null;job.signal=null;
  }
  release(job) {
    if(job.released)return;
    job.released=true;clearTimeout(job.timer);this.bytes-=job.size;
    if(this.active===job)this.active=null;
    const index=this.queue.indexOf(job);if(index>=0)this.queue.splice(index,1);
    job.client=null;job.packet=null;
    job.onRelease?.();job.onRelease=null;
  }
  cancel(job) {
    if(job.released||job.settled)return;
    const error=this.error("Content processing was cancelled.");
    // Active cancellation is a tombstone, not a codec acknowledgment. Preserve
    // its absolute execution deadline and bytes/slot until real reply or
    // retirement. The rejection carries that actual release for serial callers.
    if(this.active===job)error.workerReleased=new Promise(resolve=>{job.onRelease=resolve;});
    this.complete(job,error);
    if(this.active!==job){this.release(job);this.dispatch();}
  }
  expire(job) {
    if(job.released)return;
    // An overdue active job wins any timer race: its stalled worker retires
    // the owner before a queued job's reject-only deadline is reported.
    if(this.active===job||(this.active&&performance.now()>=this.active.deadline)){this.retire("Content processing exceeded its deadline. Reload to try again.");return;}
    this.complete(job,this.error("Content processing exceeded its deadline."));this.release(job);this.dispatch();
    // Loading without a consumer is still one owned loader, with the original
    // startup deadline. Routine navigation cannot cancel/recreate WASM realms.
  }
  cancelFor(client) {
    for(const job of [this.active,...this.queue])if(job?.client===client)this.cancel(job);
  }
  retire(message) {
    if(this.phase==="retired")return;
    this.phase="retired";clearTimeout(this.startTimer);
    const worker=this.worker,active=this.active;
    this.worker=null;
    // Keep the consumed owner lease even if terminate throws or physical exit
    // is delayed. A new facade can never manufacture replacement capacity.
    this.retiredOwner={worker,terminationFailed:false};
    try{worker?.terminate();}
    catch{this.retiredOwner.terminationFailed=true;}
    finally {
      for(const job of [this.active,...this.queue].filter(Boolean)) {
        this.complete(job,this.error(message,true));this.release(job);
      }
      this.retiredBytes=active?.size||0;this.bytes=this.retiredBytes;
      this.config.onRetire?.();
    }
  }
}

