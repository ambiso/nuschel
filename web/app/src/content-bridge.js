// SPDX-License-Identifier: EUPL-1.2
// The SDK owns origins/source tickets. This bridge holds inert derivatives.
// Codec jobs receive only source text and mode; the codec Worker holds no SDK,
// TLS, conversation or identity state. As a same-origin Worker it can still use
// IndexedDB and CSP-permitted fetch: a browser realm, not an OS sandbox.
import {ContentService} from "./content-service.js";
import {MAX_REPLY, textBytes, validateDocument} from "./content-protocol.js";

export const DOCUMENT_BYTES = 16 * 1024 * 1024;
export const DOCUMENT_COUNT = 256;
// Matches the SDK's posted-job window and the RPC layer's ACK reserve.
const MAX_ADMITTED = 4;
// Mounting a long conversation must not fill the shared SDK RPC table.
const REHYDRATION_RPCS = 8, MAX_WAITING = 128;
const listeners = new Set();
const tokenListeners = new Map();
let version = 0, active, consumed = false;
export const documents = {subscribe(run) { listeners.add(run); run(version); return () => listeners.delete(run); }};
function changed(token) {
  version++; for (const run of listeners) run(version);
  if(token===undefined)for(const group of tokenListeners.values())for(const run of group)run();
  else for(const run of tokenListeners.get(token)??[])run();
}
// Observers follow mounted rows, not every cached handle. Release is mandatory
// on unmount/token replacement; an accepted derivative wakes only its owner.
export function observeDocument(token,run) {
  const key=typeof token==="string"?token:null;
  let group=tokenListeners.get(key);if(!group){group=new Set();tokenListeners.set(key,group);}
  group.add(run);run();let held=true;
  return()=>{if(!held)return;held=false;group.delete(run);if(!group.size)tokenListeners.delete(key);};
}
function minimumCharge(document) {
  const encoded=textBytes(JSON.stringify(document),MAX_REPLY).length;
  let pixels=0,spans=0,runs=0;
  for(const block of document.blocks) {
    if(block.kind==="image")pixels+=block.width*block.height*4;
    spans+=block.spans?.length??0;runs+=block.runs?.length??0;
  }
  return encoded*2+pixels*3+document.blocks.length*128+spans*64+runs*96;
}

// A mounted document (including a welcome/viewer) pins its complete logical
// charge. Eviction cannot turn retained canvas/viewer pixels into free credit.
export class DocumentCache {
  constructor({maxBytes=DOCUMENT_BYTES,maxCount=DOCUMENT_COUNT}={}) { this.maxBytes=maxBytes;this.maxCount=maxCount;this.bytes=0;this.entries=new Map(); }
  get(token) { const entry=this.entries.get(token); if(!entry)return null;this.entries.delete(token);this.entries.set(token,entry);return entry.document; }
  insert(token,document,charge,failed=false) {
    if(typeof token!=="string"||!/^[1-9][0-9]{0,19}$/.test(token)||!Number.isSafeInteger(charge)||charge<1||charge>this.maxBytes)return false;
    validateDocument(document);
    if(charge<minimumCharge(document))return false;
    if(this.entries.has(token))return true;
    for(const [key,entry] of this.entries) {
      if(this.entries.size<this.maxCount&&this.bytes+charge<=this.maxBytes)break;
      if(entry.pins)continue;
      this.entries.delete(key);this.bytes-=entry.charge;
    }
    if(this.entries.size>=this.maxCount||this.bytes+charge>this.maxBytes)return false;
    this.entries.set(token,{document,charge,pins:0,failed});this.bytes+=charge;return true;
  }
  pin(token) { const entry=this.entries.get(token);if(!entry)return null;entry.pins++;let held=true;return()=>{if(held){held=false;entry.pins--;}}; }
  clear() { this.entries.clear();this.bytes=0; }
}

const cache = new DocumentCache();
export function contentDocument(token) { return cache.get(token); }
export function contentFailed(token) { return Boolean(cache.entries.get(token)?.failed || active?.denied.has(token)); }
export function contentUnavailable(token) { return !active || active.unavailable() || active.denied.get(token) === true; }
export function retryContent(token) { active?.retry(token); }
export function retainDocument(token,failed=false) {
  if(typeof token!=="string")return()=>{};
  const owner=active;
  const release=cache.pin(token);
  if(!release&&!failed)owner?.rehydrate(token);
  // UI dedupe metadata follows the mounted lease. Removing it neither cancels
  // background SDK work nor releases any codec/transport concurrency credit.
  return()=>{release?.();owner?.forget(token);};
}

export class ContentBridge {
  constructor(command,{service=new ContentService()}={}) {
    if(consumed)throw new Error("The content bridge already consumed its page-lifetime owner. Reload to reconnect.");
    consumed=true;
    this.command=command;this.service=service;
    // SDK-admitted renders stay here until their exact ACK reached the SDK.
    this.pending=new Map();this.queue=[];this.running=null;
    this.missing=new Set();this.waiting=new Set();this.requesting=0;
    this.denied=new Map();this.welcomes=new Map();this.disposed=false;this.retired=false;this.reportedRetirement=false;active=this;
  }
  unavailable() { return this.retired||this.service.retired===true; }
  rehydrate(token) {
    if(this.disposed||this.unavailable()||this.denied.has(token)||this.missing.has(token))return;
    if(this.missing.size>=MAX_WAITING){this.deny(token);return;}
    this.missing.add(token);this.request(token);
  }
  forget(token) { this.missing.delete(token);this.waiting.delete(token); }
  deny(token,terminal=false) {
    if(this.denied.size>=512)this.denied.delete(this.denied.keys().next().value);
    this.denied.set(token,terminal||this.denied.get(token)===true);this.notify(token);
  }
  notify(token) {
    changed(token);
    if(this.service.retired&&!this.reportedRetirement){this.reportedRetirement=true;changed();}
  }
  retry(token) {
    if(this.disposed||this.unavailable()||this.denied.get(token)===true)return;
    this.request(token);
  }
  request(token) {
    if(!this.waiting.has(token)&&this.waiting.size>=MAX_WAITING){this.deny(token);return;}
    this.waiting.add(token);this.drainRequests();
  }
  drainRequests() {
    while(!this.disposed&&this.requesting<REHYDRATION_RPCS&&this.waiting.size) {
      const token=this.waiting.values().next().value;this.waiting.delete(token);this.requesting++;
      this.command("content_request",token).then(queued=>{if(queued!==true)this.deny(token);},()=>this.deny(token,true))
        .finally(()=>{this.requesting--;this.drainRequests();});
    }
  }
  handle(data) {
    if(data?.kind!=="content_work")return false;
    if(!Array.isArray(data.operations)||data.operations.length>8)throw new Error("Invalid content work batch.");
    for(const operation of data.operations) {
      const id=operation?.request;
      if(typeof id!=="string"||!/^[1-9][0-9]{0,19}$/.test(id))throw new Error("Invalid content work ticket.");
      if(operation.kind==="cancel") { this.cancel(id);continue; }
      if(operation.kind!=="render"||this.pending.has(id)||this.pending.size>=MAX_ADMITTED)throw new Error("Invalid content work admission.");
      const entry={id,source:operation.source,mode:operation.mode,controller:new AbortController()};
      this.pending.set(id,entry);this.queue.push(entry);
    }
    this.pump();
    return true;
  }
  cancel(id) {
    const entry=this.pending.get(id);
    if(!entry)return; // its ACK has already reached the SDK
    const index=this.queue.indexOf(entry);
    if(index<0){entry.controller.abort();return;}
    this.queue.splice(index,1);this.acknowledge(entry,""); // never reached the codec
  }
  // The content service runs one job at a time and the SDK times only its oldest
  // unacknowledged job. Hand over the next job only after the previous one has
  // actually left the worker, so queue wait never runs against a job's deadline.
  pump() {
    if(this.running||this.disposed||this.retired||!this.queue.length)return;
    this.run(this.queue.shift());
  }
  async run(entry) {
    this.running=entry;
    let result,released;
    try { result=await this.service.render({source:entry.source,mode:entry.mode},{signal:entry.controller.signal}); }
    catch(error) { released=error?.workerReleased; }
    // A cancelled active job is settled early but still holds the worker until
    // its real reply or owner retirement; so does its SDK ACK.
    if(released)await released;
    if(this.running===entry)this.running=null;
    this.pump();
    let reply="";
    if(result!==undefined) {
      try { const json=JSON.stringify(result);textBytes(json,MAX_REPLY);reply=json; }
      catch { /* A bounded failure ACK never grants parser fallback. */ }
    }
    await this.acknowledge(entry,reply);
  }
  // Only this exact ACK releases the SDK's credit for the job. It is never
  // dropped locally: failed delivery keeps the credit charged and retires the
  // preview lane visibly, instead of degrading into a permanently silent busy.
  async acknowledge(entry,reply) {
    if(this.disposed||this.retired)return;
    let raw;
    try { raw=await this.command("content_complete",entry.id,reply); }
    catch { if(!this.disposed)this.retire();return; }
    this.pending.delete(entry.id);
    if(this.disposed)return;
    try {
      textBytes(raw,MAX_REPLY);
      const accepted=JSON.parse(raw);
      if(accepted&&Object.keys(accepted).length===4&&typeof accepted.failed==="boolean") {
        if(cache.insert(accepted.token,accepted.document,accepted.charge,accepted.failed))this.notify(accepted.token);
        else this.deny(accepted.token); // never turn pinned-cache pressure into a re-render loop
      }
    } catch { this.deny(entry.id); /* Original source remains in the SDK for explicit retry. */ }
  }
  retire() {
    if(this.retired)return;
    this.retired=true;changed();
  }
  state(servers,updates) {
    // Welcome references use the same cache/charge as timeline documents.
    const valid=new Set(servers.filter(server=>server.status==="connected").map(server=>`${server.id}:${server.welcome_token}`));
    for(const [key,entry] of this.welcomes)if(!valid.has(key)){entry.release();this.welcomes.delete(key);}
    for(const update of updates??[])if(update.kind==="welcome") {
      const key=`${update.server}:${update.token}`;
      if(valid.has(key)&&this.welcomes.get(key)?.token!==update.content_token) {
        this.welcomes.get(key)?.release();this.welcomes.delete(key);
        if(typeof update.content_token==="string")this.welcomes.set(key,{token:update.content_token,release:retainDocument(update.content_token)});
      }
    }
  }
  dispose() {
    if(this.disposed)return;this.disposed=true;
    for(const entry of this.pending.values())entry.controller.abort();
    for(const entry of this.welcomes.values())entry.release();
    this.service.dispose();this.pending.clear();this.queue.length=0;this.running=null;this.missing.clear();this.waiting.clear();this.denied.clear();this.welcomes.clear();
    // Retained views/canvases may outlive this callback. Keep their cache charge
    // and permanently consume this page owner; disposal is not an exit ACK or
    // permission to manufacture another 16 MiB of derivative capacity.
    if(active===this)active=null;
    changed();
  }
}
