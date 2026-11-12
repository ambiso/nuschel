// SPDX-License-Identifier: EUPL-1.2
import {formulaInput, parseMathReply, validateMathTree, MAX_TEX_BYTES} from "./math-render-protocol.js";
import {WorkerOwner,WorkerUnavailableError,bounded,workerSnapshot} from "./worker-owner.js";
export {WorkerUnavailableError} from "./worker-owner.js";
const CACHE_BYTES=2*1024*1024,CACHE_ENTRIES=64;
const defaultFactory=()=>new Worker(new URL("./math-render-worker.js",import.meta.url),{type:"module",name:"nuschl-math-renderer"});
// Distinct from content's owner. Only admitted jobs retain facade references;
// cached results and this one page-lifetime lease survive facade disposal.
let sharedOwner,shared,cacheBytes=0;
const cache=new Map(),retirementListeners=new Set();
function freeze(node) {
  if(node.children){node.children.forEach(freeze);Object.freeze(node.children);Object.freeze(node.attrs);}
  return Object.freeze(node);
}
function unavailable(){return new WorkerUnavailableError("Formula rendering is unavailable. Reload the client to try again. Its source is available.");}
function notifyRetired() {
  for(const listener of retirementListeners){try{listener();}catch{/* One view cannot interrupt owner cleanup. */}}
  retirementListeners.clear();
}
function retain(key,tree) {
  const bytes=new TextEncoder().encode(JSON.stringify(tree)).length+new TextEncoder().encode(key).length;
  const previous=cache.get(key);if(previous){cacheBytes-=previous.bytes;cache.delete(key);}
  while(cache.size>=CACHE_ENTRIES||cacheBytes+bytes>CACHE_BYTES){
    const oldest=cache.keys().next().value;if(oldest===undefined)break;
    cacheBytes-=cache.get(oldest).bytes;cache.delete(oldest);
  }
  cache.set(key,{tree,bytes});cacheBytes+=bytes;
}
export class MathRenderService {
  constructor({workerFactory=defaultFactory,timeoutMs=2000,startupMs=15000}={}) {
    this.workerFactory=workerFactory;this.timeoutMs=bounded(timeoutMs,2000);this.startupMs=bounded(startupMs,15000);this.disposed=false;
  }
  get active(){return workerSnapshot(sharedOwner?.active);}
  get queue(){return Object.freeze((sharedOwner?.queue??[]).map(workerSnapshot));}
  get bytes(){return sharedOwner?.bytes??0;}
  get retired(){return sharedOwner?.phase==="retired";}
  get terminationFailed(){return sharedOwner?.retiredOwner?.terminationFailed??false;}
  get cacheEntries(){return cache.size;}
  get cacheBytes(){return cacheBytes;}
  // Mounted views explicitly unsubscribe on destruction. No set retains idle
  // or disposed facades, and fatal notification empties the subscriber set.
  onUnavailable(listener) {
    if(typeof listener!=="function")throw new TypeError("Invalid formula observer.");
    if(this.retired){listener();return ()=>{};}
    retirementListeners.add(listener);return ()=>retirementListeners.delete(listener);
  }
  render(tex,display=false,{signal}={}) {
    try{formulaInput(tex,display);}catch(error){return Promise.reject(error);}
    if(this.disposed||signal?.aborted)return Promise.reject(new Error("Formula rendering was cancelled."));
    const key=JSON.stringify([display,tex]),cached=cache.get(key);
    if(cached){cache.delete(key);cache.set(key,cached);return Promise.resolve(cached.tree);}
    if(this.retired)return Promise.reject(unavailable());
    sharedOwner??=new WorkerOwner(this.workerFactory,this.startupMs,{
      label:"Formula",maxJobs:8,maxBytes:8*MAX_TEX_BYTES,onRetire:notifyRetired,
      validate:(_packet,data)=>freeze(parseMathReply(data.tree)),
    });
    return sharedOwner.enqueue(this,{tex,display},new TextEncoder().encode(tex).length,signal,this.timeoutMs).then(tree=>{
      // Canceled consumers reject, so their ACK never reattaches or caches a
      // derivative. Only successfully delivered, revalidated immutable trees.
      retain(key,tree);return tree;
    });
  }
  cancelAll(){sharedOwner?.cancelFor(this);}
  dispose(){if(!this.disposed){this.disposed=true;this.cancelAll();}}
}
export function mathRenderer(){return shared ||=new MathRenderService();}
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
