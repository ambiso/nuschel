// SPDX-License-Identifier: EUPL-1.2
// Real IndexedDB boundary/paging/reopen tests, in a disposable Chromium profile.
// NUSCHL_PLAYWRIGHT=/path/to/playwright-core/index.mjs node tests/web_history.mjs
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import assert from "node:assert/strict";
import { pathToFileURL } from "node:url";
const module = process.env.NUSCHL_PLAYWRIGHT;
if (!module) throw new Error("Set NUSCHL_PLAYWRIGHT to the cached playwright-core index.mjs.");
const { chromium } = await import(pathToFileURL(module));
const code = await readFile(new URL("../web/app/src/history.js", import.meta.url));
const server = createServer((req,res) => {
  if (req.url === "/history.js") { res.writeHead(200,{"Content-Type":"text/javascript"}); res.end(code); }
  else { res.writeHead(200,{"Content-Type":"text/html"}); res.end("<!doctype html><title>Private IndexedDB fixture</title>"); }
});
await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));
const browser = await chromium.launch({ headless:true, ...(process.env.NUSCHL_CHROMIUM ? { executablePath:process.env.NUSCHL_CHROMIUM } : {}) });
try {
 const page = await browser.newPage();
 await page.goto(`http://127.0.0.1:${server.address().port}`);
 const result = await page.evaluate(async () => {
  const { openHistory } = await import("/history.js");
  let db = await openHistory(indexedDB,"private-fixture");
  let ticket=0;
  const identity="ab".repeat(32), other="cd".repeat(32);
  const append=async(message,outgoing=false)=>db.execute({kind:"append",ticket:String(++ticket),message,outgoing});
  const message=(body,at,server=identity,channel="0")=>({server,channel,author:"Ada",observed_at_ms:at,body});
  const records=[];
  for(let i=0;i<237;i++) {
   const entry=message(`message ${i}\n\nsecond line`,Math.floor(i/3));
   const {id}=await append(entry,i%2===0);records.push({id,...entry});
  }
  const backdated=message("late row with old observation",2);
  records.push({id:(await append(backdated)).id,...backdated});
  await append(message("another certificate",1,other));
  await append(message("another channel",1,identity,"1"));
  await db.bookmarks('[{"host":"private.fixture"}]');
  db.close();
  db=await openHistory(indexedDB,"private-fixture");
  const bookmarks=await db.bookmarks();
  const pageRequest=(before=null,server=identity,channel="0")=>db.execute({kind:"page",ticket:String(++ticket),identity:server,channel,before,limit:100});
  const pages=[];
  let before=null;
  do { const result=await pageRequest(before); pages.unshift(...result.rows);before=result.next_before; } while(before!==null);
  const expected=records.sort((a,b)=>a.observed_at_ms-b.observed_at_ms||a.id-b.id).map(r=>r.id);
  let wrongScope=false;
  try { await pageRequest(pages[0].id,other); } catch { wrongScope=true; }
  let oversized=false;
  try { await append(message("x".repeat(262145),1)); } catch { oversized=true; }
  for(let i=0;i<4;i++) await append(message("\u0001".repeat(256000),1000+i,identity,"large"));
  let largeCount=0,largePages=0;before=null;
  do { const result=await pageRequest(before,identity,"large");largeCount+=result.rows.length;largePages++;before=result.next_before; } while(before!==null);
  const isolated=await pageRequest(null,other);
  db.close();
  return {count:pages.length,ids:pages.map(r=>r.id),expected,bookmarks,wrongScope,oversized,isolated:isolated.rows.map(r=>r.message.body),newlines:pages.every(r=>r.message.body.includes("\n\n")||r.message.body.startsWith("late")),largeCount,largePages};
 });
 assert.equal(result.count,238);
 assert.deepEqual(result.ids,result.expected);
 assert.equal(new Set(result.ids).size,238);
 assert.equal(result.bookmarks,'[{"host":"private.fixture"}]');
 assert.equal(result.wrongScope,true);
 assert.equal(result.oversized,true);
 assert.equal(result.newlines,true);
 assert.deepEqual(result.isolated,["another certificate"]);
 assert.equal(result.largeCount,4);
 assert.ok(result.largePages>=2);
 console.log("IndexedDB: 238 records survive reopen and complete paging; timestamp ties/backdated rows stable; certificate/channel/cursor scopes enforced; multiline preserved; bounded messages/pages retain all large history.");
} finally { await browser.close(); await new Promise(resolve=>server.close(resolve)); }
