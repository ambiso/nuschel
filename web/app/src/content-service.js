// SPDX-License-Identifier: EUPL-1.2
import {MAX_SOURCE, MAX_IMAGE_INPUT, textBytes, validateReply} from "./content-protocol.js";
const reason=message=>new Error(message);
import {WorkerOwner,bounded,workerSnapshot} from "./worker-owner.js";
const defaultFactory=()=>new Worker(new URL("./content-worker.js",import.meta.url),{type:"module",name:"nuschl-content-codecs"});
// One owner for this module/page lifetime, not one lease per conversation or
// facade. No public reset/replacement option can discard a retiring owner's cost.
let sharedOwner;


// Facades carry no SDK/session/conversation authority. Active/queued jobs are
// the only owner→facade references; disposed/idle facades are not kept in a Set.
export class ContentService {
  constructor({workerFactory=defaultFactory,timeoutMs=8000,startupMs=15000}={}) {
    this.workerFactory=workerFactory;this.timeoutMs=bounded(timeoutMs,8000);this.startupMs=bounded(startupMs,15000);this.disposed=false;
  }
  get active(){return sharedOwner?.active?workerSnapshot(sharedOwner.active):null;}
  get queue(){return Object.freeze((sharedOwner?.queue??[]).map(workerSnapshot));}
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
    sharedOwner??=new WorkerOwner(this.workerFactory,this.startupMs,{label:"Content",maxJobs:8,maxBytes:16*1024*1024,validate:(packet,data)=>validateReply(packet.operation,data.result,packet.limit)});
    return sharedOwner.enqueue(this,packet,size,signal,this.timeoutMs);
  }
  cancelAll(){sharedOwner?.cancelFor(this);}
  dispose(){if(!this.disposed){this.disposed=true;this.cancelAll();}}
}
