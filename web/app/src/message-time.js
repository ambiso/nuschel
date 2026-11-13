// SPDX-License-Identifier: EUPL-1.2
// Date's default toLocaleString fields, and the exact existing [] / HH:mm
// toLocaleTimeString options, with two shared formatters instead of two per row.
const fullOptions={year:"numeric",month:"numeric",day:"numeric",hour:"numeric",minute:"numeric",second:"numeric"};
const clockOptions={hour:"2-digit",minute:"2-digit"};
export function createTimestampFormatter(locales) {
  const full=new Intl.DateTimeFormat(locales,fullOptions),clock=new Intl.DateTimeFormat(locales??[],clockOptions);
  const key=JSON.stringify(full.resolvedOptions());
  return {key,format(value){
    let date;try{date=new Date(Number(value));}catch{date=new Date(NaN);}
    if(!Number.isFinite(date.getTime()))return {iso:undefined,title:"Invalid Date",label:"Invalid Date"};
    return {iso:date.toISOString(),title:full.format(date),label:clock.format(date)};
  }};
}
let formatter,version=0;
const listeners=new Set();
export const timestampFormats={subscribe(run){listeners.add(run);run(version);return()=>listeners.delete(run);}};
export function messageTime(value){formatter??=createTimestampFormatter();return formatter.format(value);}
function refresh(force){
  const next=createTimestampFormatter();
  if(!formatter||force||formatter.key!==next.key){formatter=next;version++;for(const run of listeners)run(version);}
}
// A single app lifecycle subscription notices language/system-timezone changes
// on return to the page. No per-row timers, global timestamp cache or polling.
export function observeMessageTimeLocale(window){
  const focus=()=>refresh(false),language=()=>refresh(true);
  refresh(false);window.addEventListener("focus",focus);window.addEventListener("languagechange",language);
  return()=>{window.removeEventListener("focus",focus);window.removeEventListener("languagechange",language);};
}
