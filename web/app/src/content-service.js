// SPDX-License-Identifier: EUPL-1.2
import {MAX_SOURCE, MAX_IMAGE_INPUT, textBytes, validateReply} from "./content-protocol.js";
const MAX_JOBS=8, MAX_QUEUED_BYTES=16*1024*1024;
const reason=message=>new Error(message);
const bounded=(value,maximum)=>Number.isFinite(value)?Math.min(maximum,Math.max(1,Math.floor(value))):maximum;
const defaultFactory=()=>new Worker(new URL("./content-worker.js",import.meta.url),{type:"module",name:"nuschl-content-codecs"});
// One owner for this module/page lifetime, not one lease per conversation or
// facade. No public reset/replacement option can discard a retiring owner's cost.
let sharedOwner;
const snapshot=job=>Object.freeze({id:job.id,size:job.size,deadline:job.deadline});

class ContentOwner {
  constructor(factory,startupMs) {
    this.factory=factory;this.startupMs=startupMs;this.phase="empty";
    this.queue=[];this.active=null;this.bytes=0;this.next=0;this.worker=null;
    this.retiredOwner=null;this.retiredBytes=0;
  }
  enqueue(client,packet,size,signal,timeoutMs) {
    if(this.phase==="retired")return Promise.reject(reason("Content processing is unavailable. Reload to try again."));
    if(signal?.aborted)return Promise.reject(reason("Content processing was cancelled."));
    if(this.queue.length+Number(Boolean(this.active))>=MAX_JOBS||size>MAX_QUEUED_BYTES-this.bytes)
      return Promise.reject(reason("Content processing is busy; retry this message."));
    if(!Number.isSafeInteger(this.next+1))return Promise.reject(reason("Content job identifiers are exhausted."));
    return new Promise((resolve,reject)=>{
      const job={id:++this.next,client,packet:{...packet},size,resolve,reject,signal,deadline:performance.now()+timeoutMs,settled:false,released:false};
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
      const factory=this.factory,worker=factory();this.worker=worker;
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
    if(performance.now()>=job.deadline){this.complete(job,reason("Content processing exceeded its deadline."));this.release(job);this.dispatch();return;}
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
    if(data.kind==="error")error=reason(typeof data.message==="string"?data.message.slice(0,200):"Content could not be processed.");
    else {
      try{value=validateReply(job.packet.operation,data.result,job.packet.limit);}
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
  }
  cancel(job) {
    if(job.released||job.settled)return;
    this.complete(job,reason("Content processing was cancelled."));
    // Active cancellation is a tombstone, not a codec acknowledgment. Preserve
    // its original absolute deadline and bytes/slot until real reply or retirement.
    if(this.active!==job){this.release(job);this.dispatch();}
  }
  expire(job) {
    if(job.released)return;
    if(this.active===job){this.retire("Content processing exceeded its deadline. Reload to try again.");return;}
    this.complete(job,reason("Content processing exceeded its deadline."));this.release(job);this.dispatch();
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
        this.complete(job,reason(message));this.release(job);
      }
      this.retiredBytes=active?.size||0;this.bytes=this.retiredBytes;
    }
  }
}

// Facades carry no SDK/session/conversation authority. Active/queued jobs are
// the only owner→facade references; disposed/idle facades are not kept in a Set.
export class ContentService {
  constructor({workerFactory=defaultFactory,timeoutMs=8000,startupMs=15000}={}) {
    this.workerFactory=workerFactory;this.timeoutMs=bounded(timeoutMs,8000);this.startupMs=bounded(startupMs,15000);this.disposed=false;
  }
  get active(){return sharedOwner?.active?snapshot(sharedOwner.active):null;}
  get queue(){return Object.freeze((sharedOwner?.queue??[]).map(snapshot));}
  get bytes(){return sharedOwner?.bytes??0;}
  get retired(){return sharedOwner?.phase==="retired";}
  get terminationFailed(){return sharedOwner?.retiredOwner?.terminationFailed??false;}
  render({source,mode="plain"},{signal}={}) {
    try {
      const size=textBytes(source,MAX_SOURCE).length;
      if(!["plain","legacy","marked"].includes(mode))throw reason("Invalid content mode.");
      return this.enqueue({operation:"render",source,mode},size,signal);
    }catch(error){return Promise.reject(error);}
  }
  encodeImage(bytes,limit,{signal}={}){return this.image("encode",bytes,limit,signal);}
  decodeImage(bytes,{signal}={}){return this.image("decode",bytes,undefined,signal);}
  image(operation,bytes,limit,signal) {
    if(!(bytes instanceof Uint8Array)||bytes.length<1||bytes.length>MAX_IMAGE_INPUT)return Promise.reject(reason("Image input exceeds the content limit."));
    if(operation==="encode"&&(!Number.isSafeInteger(limit)||limit<64||limit>MAX_SOURCE))return Promise.reject(reason("Invalid image message limit."));
    return this.enqueue({operation,bytes,limit},bytes.length,signal);
  }
  enqueue(packet,size,signal) {
    if(this.disposed)return Promise.reject(reason("Content processing is closed."));
    if(signal?.aborted)return Promise.reject(reason("Content processing was cancelled."));
    sharedOwner??=new ContentOwner(this.workerFactory,this.startupMs);
    return sharedOwner.enqueue(this,packet,size,signal,this.timeoutMs);
  }
  cancelAll(){sharedOwner?.cancelFor(this);}
  dispose(){if(!this.disposed){this.disposed=true;this.cancelAll();}}
}
