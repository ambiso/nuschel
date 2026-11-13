// SPDX-License-Identifier: EUPL-1.2
// ACK/credit accounting is synchronous in the SDK. Only content state
// publication is coalesced; an urgent wake cancels its sole pending timer.
export function createContentPublication(refresh,clock=globalThis) {
  let queued=false,timer=null;
  function wake() {
    if(timer!==null){clock.clearTimeout(timer);timer=null;}
    if(queued)return;
    queued=true;
    clock.queueMicrotask(()=>{queued=false;refresh();});
  }
  function content() {
    if(queued||timer!==null)return;
    timer=clock.setTimeout(()=>{timer=null;wake();},16);
  }
  return {wake,content};
}
