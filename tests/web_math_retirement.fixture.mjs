// SPDX-License-Identifier: EUPL-1.2
import {MathRenderService,mathElement} from "/math-render-service.js";
const facades=[];
function facade(options={}) {
  const service=new MathRenderService({workerFactory:()=>new Worker(document.body.dataset.worker,{type:"module",name:"private-temml-proof"}),...options});
  facades.push(service);return service;
}
const outcome=promise=>promise.then(value=>({value}),error=>({error:error.message,code:error.code}));
const assert=(condition,message)=>{if(!condition)throw new Error(message);};
let service,stalled,queued,reservation,consumer;
globalThis.mathState=()=>({retired:service.retired,terminationFailed:service.terminationFailed,bytes:service.bytes,active:service.active,queued:service.queue.length,cacheEntries:service.cacheEntries,cacheBytes:service.cacheBytes});
globalThis.prepareMath=async()=>{
  service=facade();
  const tree=await service.render(String.raw`\frac{1}{\sqrt{x^2+1}}`,true);
  document.querySelector("#formula").replaceChildren(mathElement(tree,document));
  assert(Object.isFrozen(tree)&&document.querySelector("mfrac")&&document.querySelector("msqrt"),"real Temml fraction/root absent");
  const invalid=await outcome(service.render(String.raw`\def\loop{\loop}\loop`));assert(invalid.error&&!invalid.code,"ordinary macro error was not retryable");
  await service.render("z^2");
  const before=__mathOwnerProof.posted.length;await facade().render("z^2");assert(__mathOwnerProof.posted.length===before,"cache not shared");
  for(const tex of [String.raw`\includegraphics{/canary-image}`,String.raw`\style{background:url(/canary-css)}{x}`,String.raw`\text{<img src=/canary-text>}`]){
    const result=await outcome(service.render(tex));if(result.value)document.querySelector("#canaries").append(mathElement(result.value,document));
  }
  assert(!document.querySelector("math [href], math [src], math [style], math img, math svg"),"resource-bearing MathML escaped validation");
  return {cacheEntries:service.cacheEntries,cacheBytes:service.cacheBytes,created:__mathOwnerProof.created};
};
globalThis.beginHealthyCancellation=()=>{
  const abort=new AbortController();const discarded=outcome(service.render("discarded+1",false,{signal:abort.signal}));
  const other=facade(),next=outcome(other.render("current+2"));abort.abort();
  assert(service.active&&service.queue.length===1,"active cancellation released credit early");
  globalThis.healthyDone=Promise.all([discarded,next]).then(([old,current])=>{
    assert(old.error&&!old.code&&current.value,"healthy cancellation/reuse failed");
    assert(__mathOwnerProof.created===1&&__mathOwnerProof.terminated===0,"healthy cancellation replaced owner");
    return {created:__mathOwnerProof.created,bytes:service.bytes,retired:service.retired};
  });
};
globalThis.beginMathStall=()=>{
  const abort=new AbortController();consumer=facade();
  stalled=outcome(service.render("abandoned+3",false,{signal:abort.signal}));
  queued=outcome(consumer.render("queued+4"));reservation=service.active;abort.abort();
  return {activeId:reservation.id,deadline:reservation.deadline,bytes:service.bytes};
};
globalThis.finishMathStall=async()=>{
  const results=await Promise.all([stalled,queued]);assert(results[0].error,"abandoned consumer did not cancel");assert(results[1].code==="worker-unavailable","queued consumer did not receive terminal error");
  assert(service.retired&&!service.active&&service.queue.length===0,"retirement did not settle cleanup");
  const before=__mathOwnerProof.posted.length;
  for(let i=0;i<20;i++){const failed=await outcome(facade().render(`new uncached ${i}`));assert(failed.code==="worker-unavailable","new facade bypassed retirement");}
  // A fatal owner does not erase already validated cache or visible formulas.
  await facade().render("z^2");assert(document.querySelector("mfrac"),"retirement erased valid formula");
  assert(__mathOwnerProof.posted.length===before&&__mathOwnerProof.created===1,"new work posted after retirement");
  document.querySelector("#result").textContent="Formula owner retired; original source remains available. Reload client to render new formulas.";
  return {results,state:mathState(),created:__mathOwnerProof.created,terminated:__mathOwnerProof.terminated};
};
globalThis.beginColdMath=()=>{
  service=facade();globalThis.coldFirst=outcome(service.render("first+cold"));
};
globalThis.retryColdMath=async()=>{
  const first=await coldFirst;assert(first.error&&!first.code,"queued cold timeout unexpectedly retired owner");
  const tree=await facade().render("second+cold");assert(tree.tag==="math","cold retry failed");
  return {first,created:__mathOwnerProof.created,terminated:__mathOwnerProof.terminated,state:mathState()};
};
globalThis.closeMathProof=()=>facades.forEach(service=>service.dispose());
