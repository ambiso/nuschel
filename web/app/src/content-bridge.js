// SPDX-License-Identifier: EUPL-1.2
// The SDK owns origins/source tickets. This bridge holds inert derivatives;
// codec jobs receive no SDK, TLS, storage, conversation or identity capability.
import {ContentService} from "./content-service.js";
import {MAX_REPLY, textBytes, validateDocument} from "./content-protocol.js";

export const DOCUMENT_BYTES = 16 * 1024 * 1024;
export const DOCUMENT_COUNT = 256;
const listeners = new Set();
let version = 0, active;
export const documents = {subscribe(run) { listeners.add(run); run(version); return () => listeners.delete(run); }};
function changed() { version++; for (const run of listeners) run(version); }
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
export function contentFailed(token) { return cache.entries.get(token)?.failed ?? false; }
export function retryContent(token) { active?.command("content_request",token).catch(()=>{}); }
export function retainDocument(token) {
  if(typeof token!=="string")return()=>{};
  const release=cache.pin(token);
  if(!release)active?.rehydrate(token);
  return release??(()=>{});
}

export class ContentBridge {
  constructor(command,{service=new ContentService()}={}) {
    if(active)throw new Error("The content bridge already has an owner.");
    this.command=command;this.service=service;this.pending=new Map();this.missing=new Set();this.welcomes=new Map();this.disposed=false;active=this;
  }
  rehydrate(token) {
    if(this.disposed||this.missing.has(token)||this.missing.size>=128)return;
    this.missing.add(token);
    this.command("content_request",token).catch(()=>{});
  }
  handle(data) {
    if(data?.kind!=="content_work")return false;
    if(!Array.isArray(data.operations)||data.operations.length>8)throw new Error("Invalid content work batch.");
    for(const operation of data.operations) {
      const id=operation?.request;
      if(typeof id!=="string"||!/^[1-9][0-9]{0,19}$/.test(id))throw new Error("Invalid content work ticket.");
      if(operation.kind==="cancel") { this.pending.get(id)?.abort();continue; }
      if(operation.kind!=="render"||this.pending.has(id)||this.pending.size>=4)throw new Error("Invalid content work admission.");
      const controller=new AbortController();this.pending.set(id,controller);
      this.render(id,operation,controller);
    }
    return true;
  }
  async render(id,operation,controller) {
    let reply="";
    try { const result=await this.service.render({source:operation.source,mode:operation.mode},{signal:controller.signal});reply=JSON.stringify(result);textBytes(reply,MAX_REPLY); }
    catch { /* A bounded failure ACK never grants parser fallback. */ }
    if(this.disposed)return;
    try {
      const raw=await this.command("content_complete",id,reply);
      if(this.disposed)return;
      textBytes(raw,MAX_REPLY);
      const accepted=JSON.parse(raw);
      if(accepted&&Object.keys(accepted).length===4&&typeof accepted.failed==="boolean"&&cache.insert(accepted.token,accepted.document,accepted.charge,accepted.failed))changed();
    } catch { /* Original source remains in the SDK for explicit retry. */ }
    finally { this.pending.delete(id); }
  }
  state(servers,updates) {
    // Welcome references use the same cache/charge as timeline documents.
    const valid=new Set(servers.filter(server=>server.status==="connected").map(server=>`${server.id}:${server.welcome_token}`));
    for(const [key,entry] of this.welcomes)if(!valid.has(key)){entry.release();this.welcomes.delete(key);}
    for(const update of updates??[])if(update.kind==="welcome"&&typeof update.content_token==="string") {
      const key=`${update.server}:${update.token}`;
      if(valid.has(key)&&this.welcomes.get(key)?.token!==update.content_token) {
        this.welcomes.get(key)?.release();
        this.welcomes.set(key,{token:update.content_token,release:retainDocument(update.content_token)});
      }
    }
  }
  dispose() {
    if(this.disposed)return;this.disposed=true;
    for(const controller of this.pending.values())controller.abort();
    for(const entry of this.welcomes.values())entry.release();
    this.service.dispose();this.pending.clear();this.missing.clear();this.welcomes.clear();cache.clear();if(active===this)active=null;changed();
  }
}
