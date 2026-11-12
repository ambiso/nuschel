// SPDX-License-Identifier: EUPL-1.2
import {ContentService} from "/content-service.js";
import {MAX_IMAGE_INPUT} from "/content-protocol.js";
const outcome=promise=>promise.then(value=>({value}),error=>({error:String(error.message||error)}));
const check=(value,message)=>{if(!value)throw new Error(message);};
let activeFacade,queuedFacade,activeOutcome,queuedOutcome;
globalThis.prepareContentRetirement=async()=>{
  const positive=await globalThis.runContentProof();
  const first=new ContentService(),second=new ContentService();
  const abort=new AbortController();
  const cancelled=outcome(first.render({source:"cancelled consumer derivative"},{signal:abort.signal}));
  const current=outcome(second.render({source:"current consumer"}));abort.abort();
  check(first.bytes>0,"Cancellation released the active reservation before ACK.");
  check((await cancelled).error?.includes("cancelled"),"Active consumer was not cancelled promptly.");
  check((await current).value?.plain_text==="current consumer","Old derivative crossed into a new consumer.");
  // Fill the global job count synchronously before any worker callback can ACK.
  const clients=Array.from({length:8},()=>new ContentService()),jobs=[];
  for(let i=0;i<8;i++)jobs.push(outcome(clients[i].render({source:`count canary ${i}`})));
  const overflow=await outcome(second.render({source:"ninth job"}));check(overflow.error?.includes("busy"),"Global job cap was bypassed across facades.");
  clients.forEach(client=>client.dispose());await Promise.all(jobs);
  const after=await second.render({source:"same owner after disposal"});check(after.plain_text==="same owner after disposal","Disposal prevented healthy reuse.");
  first.dispose();second.dispose();
  check(__contentOwnerProof.created===1,"Routine cancellation/disposal multiplied workers.");
  return {positive,cancelled_derivative_discarded:true,global_job_cap:8,healthy_owner_reused:true,workers:__contentOwnerProof.created};
};
globalThis.beginContentStall=()=>{
  activeFacade=new ContentService();queuedFacade=new ContentService({timeoutMs:8000,startupMs:15000,workerFactory:()=>{throw new Error("Replacement factory must never run");}});
  const bytes=new Uint8Array(MAX_IMAGE_INPUT),abort=new AbortController();
  activeOutcome=outcome(activeFacade.decodeImage(bytes,{signal:abort.signal}));
  queuedOutcome=outcome(queuedFacade.decodeImage(bytes));
  const activeId=activeFacade.active.id;abort.abort();
  const reserved=activeFacade.bytes;
  check(reserved===16*1024*1024,"Canceled input did not retain its global byte charge.");
  check(bytes.length===MAX_IMAGE_INPUT,"Caller bytes were detached.");
  globalThis.contentRetirementState=()=>({retired:activeFacade.retired,bytes:activeFacade.bytes,terminationFailed:activeFacade.terminationFailed,active:activeFacade.active,queued:activeFacade.queue.length});
  return {activeId,reserved,caller_bytes:bytes.length};
};
globalThis.finishContentRetirement=async()=>{
  const cancelled=await activeOutcome,queued=await queuedOutcome;
  check(cancelled.error?.includes("cancelled"),"Active cancellation changed after late processing.");
  check(queued.error?.includes("deadline"),"Global deadline did not settle waiting work.");
  const state=contentRetirementState();check(state.retired&&state.active===null&&state.queued===0,"Failed owner retained live consumer jobs.");
  check(state.bytes===MAX_IMAGE_INPUT,"Retired input was presented as fresh capacity.");
  const retries=await Promise.all(Array.from({length:20},()=>outcome(new ContentService({workerFactory:()=>{throw new Error("Fresh owner bypass");}}).render({source:"retry after failure"}))));
  check(retries.every(result=>result.error?.includes("unavailable")),"A new facade bypassed permanent retirement.");
  return {state,cancelled_consumer_settled:true,queued_consumer_settled:true,rejected_new_facades:retries.length};
};
globalThis.closeContentRetirement=()=>{activeFacade?.dispose();queuedFacade?.dispose();globalThis.closeContentProof?.();};
