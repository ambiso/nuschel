// SPDX-License-Identifier: EUPL-1.2
// Disposable Chromium UI/worker integration against a PRIVATE local relay/server.
// node tests/web_browser.mjs SITE RELAY_URL HOST PORT [SCREENSHOT]
import { createServer } from "node:http";
import { createConnection } from "node:net";
import { readFile } from "node:fs/promises";
import { resolve, extname, sep } from "node:path";
import { pathToFileURL } from "node:url";
import assert from "node:assert/strict";
const [root, relayUrl, host, port, screenshot] = process.argv.slice(2);
assert.ok(root && relayUrl && host === "127.0.0.1" && Number(port)>1023,"private fixture arguments required");
const relay = new URL(relayUrl);
assert.ok(relay.hostname === "127.0.0.1" && relay.protocol === "ws:","private loopback relay required");
const { chromium } = await import(pathToFileURL(process.env.NUSCHL_PLAYWRIGHT));
const directory = resolve(root);
const entry = await readFile(resolve(directory,"app/index.html")).then(()=>"/app/").catch(()=>"/");
const types={".html":"text/html",".js":"text/javascript",".wasm":"application/wasm",".css":"text/css",".svg":"image/svg+xml"};
const server=createServer(async(req,res)=>{
 try {
  const path=resolve(directory,"."+decodeURIComponent(new URL(req.url,"http://127.0.0.1").pathname));
  if (path!==directory && !path.startsWith(directory+sep)) throw new Error("scope");
  const target=extname(path)?path:resolve(path,"index.html");
  const data=await readFile(target);
  res.writeHead(200,{"Content-Type":types[extname(target)]||"application/octet-stream"});res.end(data);
 } catch {res.writeHead(404);res.end();}
});
const tunnels=new Set();
server.on("upgrade",(req,socket,head)=>{
 if(req.url!=="/relay"){socket.destroy();return;}
 const upstream=createConnection({host:"127.0.0.1",port:Number(relay.port)},()=>{
  let headers=`GET ${relay.pathname} HTTP/1.1\r\n`;
  for(let i=0;i<req.rawHeaders.length;i+=2)headers+=`${req.rawHeaders[i]}: ${req.rawHeaders[i+1]}\r\n`;
  upstream.write(headers+"\r\n");if(head.length)upstream.write(head);
  socket.pipe(upstream).pipe(socket);
 });
 tunnels.add(socket);tunnels.add(upstream);
 socket.on("close",()=>{tunnels.delete(socket);upstream.destroy();});upstream.on("close",()=>{tunnels.delete(upstream);socket.destroy();});
 socket.on("error",()=>upstream.destroy());upstream.on("error",()=>socket.destroy());
});
await new Promise(resolve=>server.listen(0,"127.0.0.1",resolve));
const origin=`http://127.0.0.1:${server.address().port}`;
const browser=await chromium.launch({headless:true,executablePath:process.env.NUSCHL_CHROMIUM});
const contexts=[];
const forbidden=[];
async function open(username) {
 const context=await browser.newContext({viewport:{width:1280,height:900}});contexts.push(context);
 context.on("request",request=>{if(!request.url().startsWith(origin+"/")&&!request.url().startsWith("data:"))forbidden.push(request.url());});
 const page=await context.newPage();
 page.on("pageerror",error=>console.error("browser error:",error.message));
 await page.goto(`${origin}${entry}#s=${host}:${port}&u=${username}`);
 await page.getByRole("button",{name:"Trust this certificate",exact:true}).waitFor({timeout:20000});
 await page.getByRole("button",{name:"Trust this certificate",exact:true}).click();
 await page.waitForFunction(()=>document.querySelector('[role="status"]')?.textContent==="Connected",null,{timeout:20000});
 return page;
}
try {
 const first=await open("browser-one"),second=await open("browser-two");
 const text='browser literal <img src="https://tracker.invalid/x">\n\n```rust\nfn main() {}\n```';
 await first.getByLabel("Message",{exact:true}).fill(text);
 await first.getByRole("button",{name:"Send",exact:true}).click();
 await second.locator("article").filter({hasText:"browser literal"}).waitFor();
 const message=second.locator("article").filter({hasText:"browser literal"});
 assert.ok((await message.locator(".text").textContent()).includes('\n\n'));
 assert.ok(await message.locator('pre[aria-label="rust code"] span').count()>1);
 assert.equal(await message.locator("img").count(),0);
 const long=Array.from({length:25},(_,i)=>`Long message line ${i}`).join("\n");
 await first.getByLabel("Message",{exact:true}).fill(long);
 await first.getByRole("button",{name:"Send",exact:true}).click();
 const more=second.getByRole("button",{name:"Show more",exact:true});await more.waitFor();await more.click();
 assert.ok((await second.locator("article").filter({hasText:"Long message line 0"}).textContent()).includes("Long message line 24"));
 // Legacy channel browsing must not move the voice session. Every new guest
 // remains self-muted in Root while choosing another channel's text timeline.
 await second.getByRole("button",{name:"Lobby",exact:true}).click();
 await second.waitForFunction(()=>document.querySelector(".chat-header h2")?.textContent==="Lobby");
 await first.getByLabel("Message",{exact:true}).fill("root while lobby selected");
 await first.getByRole("button",{name:"Send",exact:true}).click();
 await first.locator("article").filter({hasText:"root while lobby selected"}).waitFor();
 await new Promise(resolve=>setTimeout(resolve,250));
 assert.equal(await second.locator("article").filter({hasText:"root while lobby selected"}).count(),0);
 await second.getByRole("button",{name:"Root",exact:true}).click();
 await second.locator("article").filter({hasText:"root while lobby selected"}).waitFor();
 await second.reload();
 await second.waitForFunction(()=>document.querySelector('[role="status"]')?.textContent==="Connected",null,{timeout:20000});
 await second.locator("article").filter({hasText:"browser literal"}).waitFor();
 await second.locator("article").filter({hasText:"root while lobby selected"}).waitFor();
 assert.equal(await second.getByRole("button",{name:"Trust this certificate",exact:true}).count(),0);
 await second.getByRole("button",{name:"Add server",exact:true}).click();
 await second.getByRole("heading",{name:"Add a server",exact:true}).waitFor();
 await new Promise(resolve=>setTimeout(resolve,5200));
 assert.equal(await second.getByRole("heading",{name:"Add a server",exact:true}).count(),1);
 if(screenshot)await second.screenshot({path:screenshot});
 assert.deepEqual(forbidden,[]);
 console.log("Browser worker UI: local two-peer text, inert HTML/no remote requests, multiline/code/Show more, independent selected channel, IndexedDB page reload and saved certificate, Add server passed.");
} finally {
 for(const context of contexts)await context.close();await browser.close();
 for(const socket of tunnels)socket.destroy();await new Promise(resolve=>server.close(resolve));
}
