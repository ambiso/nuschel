// SPDX-License-Identifier: EUPL-1.2
import {formulaInput, parseMathReply, validateMathTree} from "./math-render-protocol.js";
const MAX_JOBS = 8, CACHE_BYTES = 2 * 1024 * 1024, CACHE_ENTRIES = 64;
const error = message => new Error(message);
function freeze(node) {
  if (node.children) { node.children.forEach(freeze); Object.freeze(node.children); Object.freeze(node.attrs); }
  return Object.freeze(node);
}
export class MathRenderService {
  constructor({workerFactory, timeoutMs=2000}={}) {
    this.factory = workerFactory || (() => new Worker(new URL("./math-render-worker.js", import.meta.url), {type:"module", name:"nuschl-math-renderer"}));
    this.timeoutMs = Math.min(2000, Math.max(1, timeoutMs));
    this.worker = null; this.ready = false; this.active = null; this.queue = [];
    this.next = 0; this.cache = new Map(); this.cacheBytes = 0; this.disposed = false;
  }
  render(tex, display=false, {signal}={}) {
    try { formulaInput(tex, display); } catch (failure) { return Promise.reject(failure); }
    if (this.disposed || signal?.aborted) return Promise.reject(error("Formula rendering was cancelled."));
    const key = JSON.stringify([display,tex]), cached = this.cache.get(key);
    if (cached) { this.cache.delete(key); this.cache.set(key,cached); return Promise.resolve(cached.tree); }
    if (this.queue.length + Number(Boolean(this.active)) >= MAX_JOBS) return Promise.reject(error("Formula rendering is busy. Source is available; retry rendering."));
    if (!Number.isSafeInteger(this.next + 1)) return Promise.reject(error("Formula job identifiers are exhausted."));
    return new Promise((resolve,reject) => {
      const job = {id:++this.next,tex,display,key,resolve,reject,signal,deadline:performance.now()+this.timeoutMs};
      job.abort = () => this.cancel(job,"Formula rendering was cancelled.");
      signal?.addEventListener("abort",job.abort,{once:true});
      job.timer = setTimeout(() => this.cancel(job,"Formula rendering exceeded its deadline."),this.timeoutMs);
      this.queue.push(job); this.dispatch();
    });
  }
  start() {
    try {
      const worker=this.factory(); this.worker=worker; this.ready=false;
      worker.onmessage=({data}) => { if (this.worker===worker) this.message(data); };
      worker.onerror=() => { if (this.worker===worker) this.fail("The formula worker failed. Source is available."); };
      worker.onmessageerror=worker.onerror;
    } catch { this.fail("The formula worker could not start."); }
  }
  dispatch() {
    if (this.disposed || this.active || !this.queue.length) return;
    if (!this.worker) this.start();
    if (!this.ready) return;
    const job=this.queue.shift();this.active=job;
    if (performance.now()>=job.deadline) { this.cancel(job,"Formula rendering exceeded its deadline."); return; }
    try { this.worker.postMessage({id:job.id,tex:job.tex,display:job.display}); }
    catch { this.fail("The formula worker could not accept this formula."); }
  }
  message(data) {
    if (data?.kind==="ready" && !this.ready && !this.active) { this.ready=true;this.dispatch();return; }
    const job=this.active;
    if (!job || data?.id!==job.id || !["result","error"].includes(data.kind)) { this.fail("The formula worker returned an invalid response.");return; }
    if (performance.now()>=job.deadline) { this.cancel(job,"Formula rendering exceeded its deadline.");return; }
    if (data.kind==="error") this.settle(job,error("This formula could not be rendered. Its source is available."));
    else {
      let tree;
      try { tree=freeze(parseMathReply(data.tree)); }
      catch { this.fail("The formula worker returned unsupported MathML. Its source is available.");return; }
      const bytes = new TextEncoder().encode(data.tree).length + new TextEncoder().encode(job.key).length;
      const previous=this.cache.get(job.key);
      if(previous){this.cacheBytes-=previous.bytes;this.cache.delete(job.key);}
      while (this.cache.size>=CACHE_ENTRIES || this.cacheBytes+bytes>CACHE_BYTES) {
        const oldest=this.cache.keys().next().value;
        if (oldest===undefined) break;
        this.cacheBytes-=this.cache.get(oldest).bytes;this.cache.delete(oldest);
      }
      this.cache.set(job.key,{tree,bytes});this.cacheBytes+=bytes;
      this.settle(job,null,tree);
    }
    this.dispatch();
  }
  settle(job,failure,value) {
    if (job.done) return;
    job.done=true;clearTimeout(job.timer);job.signal?.removeEventListener("abort",job.abort);
    if (this.active===job) this.active=null;
    const index=this.queue.indexOf(job);if(index>=0)this.queue.splice(index,1);
    if (failure) job.reject(failure);else job.resolve(value);
  }
  stop() { const worker=this.worker;this.worker=null;this.ready=false;worker?.terminate(); }
  cancel(job,message) {
    if(job.done)return;
    if(this.active===job)this.stop();
    this.settle(job,error(message));
    if(!this.active&&!this.queue.length)this.stop();
    this.dispatch();
  }
  fail(message) { this.stop();for(const job of [this.active,...this.queue].filter(Boolean))this.settle(job,error(message)); }
  dispose() { this.disposed=true;this.fail("Formula rendering was cancelled.");this.cache.clear();this.cacheBytes=0; }
}
let shared;
export function mathRenderer() { return shared ||= new MathRenderService(); }
// Construct only a newly revalidated tree with explicit MathML namespacing.
// No innerHTML, DOMParser, URL loading or library toNode()/toMarkup() call.
export function mathElement(tree, document) {
  const checked=validateMathTree(tree);
  const build=node => {
    if (Object.hasOwn(node,"text"))return document.createTextNode(node.text);
    const element=document.createElementNS("http://www.w3.org/1998/Math/MathML",node.tag);
    for(const [key,value] of Object.entries(node.attrs))element.setAttribute(key,value);
    for(const child of node.children)element.appendChild(build(child));
    return element;
  };
  return build(checked);
}
