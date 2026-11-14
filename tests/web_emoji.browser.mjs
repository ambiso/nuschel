// SPDX-License-Identifier: EUPL-1.2
// The packaged web app's emoji (docs/emoji.md): the real App, SDK and Murmur
// text through the relay, under the production CSP. Run by tests/web_emoji.py.
import assert from "node:assert/strict";
import {createServer} from "node:http";
import {createConnection} from "node:net";
import {readFile,writeFile,mkdir} from "node:fs/promises";
import {resolve,extname,sep} from "node:path";
import {pathToFileURL} from "node:url";
const [site,relayUrl,port,wrapper,playwright,output]=process.argv.slice(2);
assert.ok(site&&relayUrl&&port&&wrapper&&playwright&&output);
const relay=new URL(relayUrl);assert.equal(relay.hostname,"127.0.0.1");assert.equal(relay.protocol,"ws:");
const directory=resolve(site);await mkdir(output,{recursive:true});
let csp="default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const types={".html":"text/html",".js":"text/javascript",".css":"text/css",".wasm":"application/wasm",".svg":"image/svg+xml",".json":"application/json",".txt":"text/plain"};
const requests=[];
const server=createServer(async(req,res)=>{
  try {
    const pathname=decodeURIComponent(new URL(req.url,"http://127.0.0.1").pathname);
    const mounted=pathname.startsWith("/app/")?pathname.slice(4):pathname;
    const path=resolve(directory,"."+mounted);assert.ok(path===directory||path.startsWith(directory+sep));
    const target=extname(mounted)?path:resolve(path,"index.html");
    const body=await readFile(target);
    res.writeHead(200,{"Content-Type":types[extname(target)]||"application/octet-stream","Content-Security-Policy":csp,"Cross-Origin-Opener-Policy":"same-origin","Referrer-Policy":"no-referrer","X-Content-Type-Options":"nosniff"});res.end(body);
  }catch{res.writeHead(404);res.end();}
});
const tunnels=new Set();
server.on("upgrade",(req,socket,head)=>{
  if(req.url!=="/relay"){socket.destroy();return;}
  const upstream=createConnection({host:"127.0.0.1",port:Number(relay.port)},()=>{
    let headers=`GET ${relay.pathname} HTTP/1.1\r\n`;
    for(let i=0;i<req.rawHeaders.length;i+=2)headers+=`${req.rawHeaders[i]}: ${req.rawHeaders[i+1]}\r\n`;
    upstream.write(headers+"\r\n");if(head.length)upstream.write(head);socket.pipe(upstream).pipe(socket);
  });
  tunnels.add(socket);tunnels.add(upstream);socket.on("close",()=>{tunnels.delete(socket);upstream.destroy();});
  upstream.on("close",()=>{tunnels.delete(upstream);socket.destroy();});socket.on("error",()=>upstream.destroy());upstream.on("error",()=>socket.destroy());
});
await new Promise(done=>server.listen(0,"127.0.0.1",done));const origin=`http://127.0.0.1:${server.address().port}`;
csp=csp.replace("connect-src 'self'",`connect-src 'self' ${origin.replace("http:","ws:")}`);
const {chromium}=await import(pathToFileURL(playwright).href);
const browser=await chromium.launch({headless:true,executablePath:wrapper});const contexts=[],pages=[],errors=[];
async function open(username,width,height){
  const context=await browser.newContext({viewport:{width,height},colorScheme:"dark"});contexts.push(context);
  context.on("request",request=>requests.push(request.url()));
  const page=await context.newPage();pages.push(page);page.on("pageerror",e=>errors.push(e.message));
  await page.addInitScript(()=>{window.__violations=[];document.addEventListener("securitypolicyviolation",e=>__violations.push(e.violatedDirective));});
  await page.goto(`${origin}/app/#s=127.0.0.1:${port}&u=${username}`);
  if(width<=760)await page.getByRole("button",{name:"Open navigation",exact:true}).click();
  await page.getByRole("button",{name:"Trust this certificate",exact:true}).click({timeout:20000});
  await page.waitForFunction(()=>document.querySelector(".server-connection .status")?.textContent==="Connected",null,{timeout:20000});
  if(width<=760)await page.getByRole("button",{name:"Close navigation",exact:true}).click();
  return page;
}
// Emoji drawn in a message: each `.emoji` span's text and whether its image loaded.
async function drawn(page,needle){
  const article=page.locator("article").filter({hasText:needle}).last();await article.waitFor({timeout:20000});
  return article.evaluate(async node=>Promise.all([...node.querySelectorAll(".emoji")].map(async span=>{
    const url=getComputedStyle(span).backgroundImage.match(/url\("?(.*?)"?\)/)?.[1];
    const loaded=url?await new Promise(done=>{const image=new Image();image.onload=()=>done(image.naturalWidth>0);image.onerror=()=>done(false);image.src=url;}):false;
    return {text:span.textContent,url:url&&new URL(url).pathname,loaded,color:getComputedStyle(span).color,width:span.getBoundingClientRect().width};
  })));
}
async function send(page,text){
  const editor=page.getByLabel("Message",{exact:true});await editor.fill(text);
  await page.getByRole("button",{name:"Send",exact:true}).click();
  await new Promise(done=>setTimeout(done,1200)); // Murmur's text bucket
}
const report={};
try{
  const sender=await open("emoji-sender",1280,800),phone=await open("emoji-phone",390,844);
  // ™ is text by default: only with U+FE0F is it drawn as Twemoji.
  await send(sender,"Plain Nuschl™ stays text");
  await send(sender,"Emoji Nuschl™️ is an image");
  await send(sender,"Faces \u{1F600} \u{1F44D}\u{1F3FD} ❤ ❤️ \u{1F1E9}\u{1F1EA} `code \u{1F600}`");
  for(const page of [sender,phone]){
    assert.deepEqual(await drawn(page,"Plain Nuschl"),[]);
    const mark=await drawn(page,"Emoji Nuschl");
    assert.equal(mark.length,1);assert.equal(mark[0].text,"™️");assert.equal(mark[0].url,"/app/emoji/svg/2122.svg");
    assert.ok(mark[0].loaded,"the ™ image loads under the CSP");assert.equal(mark[0].color,"rgba(0, 0, 0, 0)");
    const faces=await drawn(page,"Faces");
    assert.deepEqual(faces.map(face=>face.text),["\u{1F600}","\u{1F44D}\u{1F3FD}","❤️","\u{1F1E9}\u{1F1EA}"]);
    assert.ok(faces.every(face=>face.loaded&&face.width>10));
    // Copying a message copies its characters, emoji included.
    const copied=await page.locator("article .text").filter({hasText:"Faces"}).last().evaluate(node=>{const range=document.createRange();range.selectNodeContents(node);const selection=getSelection();selection.removeAllRanges();selection.addRange(range);return selection.toString();});
    assert.ok(copied.includes("Faces \u{1F600} \u{1F44D}\u{1F3FD} ❤ ❤️ \u{1F1E9}\u{1F1EA}"),copied);
    // Inline code keeps its characters.
    assert.equal(await page.locator("article .inline-code").filter({hasText:"code"}).last().locator(".emoji").count(),0);
  }
  await sender.screenshot({path:resolve(output,"emoji-messages-1280.png")});
  await phone.screenshot({path:resolve(output,"emoji-messages-390.png")});

  // The picker: tabs, search, Shift+click keeps it open, skin tone, click closes.
  const editor=sender.getByLabel("Message",{exact:true});await editor.fill("Hi !");
  await editor.evaluate(node=>node.setSelectionRange(3,3));
  const toggle=sender.getByRole("button",{name:"Emoji, stickers and GIFs",exact:true});await toggle.click();
  const picker=sender.getByRole("dialog",{name:"Emoji, stickers and GIFs"});await picker.waitFor();
  await picker.getByRole("tab",{name:"Stickers"}).click();await picker.getByText("Stickers are coming soon.").waitFor();
  await picker.getByRole("tab",{name:"GIFs"}).click();await picker.getByText("GIFs are coming soon.").waitFor();
  await picker.getByRole("tab",{name:"Emoji"}).click();
  await picker.getByRole("searchbox",{name:"Search emoji"}).fill("wave");
  await sender.screenshot({path:resolve(output,"emoji-picker-search.png")});
  await picker.getByRole("button",{name:"waving hand",exact:true}).click({modifiers:["Shift"]});
  assert.ok(await picker.isVisible(),"Shift+click keeps the picker open");
  assert.equal(await editor.inputValue(),"Hi \u{1F44B}!");
  await picker.getByRole("button",{name:/^Skin tone:/}).click();
  await picker.getByRole("radio",{name:"Medium skin tone"}).click();
  assert.equal(await sender.evaluate(()=>localStorage.getItem("nuschl.emoji-tone.v1")),"3");
  await sender.screenshot({path:resolve(output,"emoji-picker-tone.png")});
  await picker.getByRole("button",{name:"waving hand",exact:true}).click();
  await picker.waitFor({state:"detached"});
  assert.equal(await editor.inputValue(),"Hi \u{1F44B}\u{1F44B}\u{1F3FD}!");
  assert.ok(await editor.evaluate(node=>document.activeElement===node&&node.selectionStart===node.value.length-1),"the cursor follows the insertion");
  // Ctrl+. from the composer; the trade mark from the picker is the emoji form; Escape closes.
  await editor.press("Control+.");await picker.waitFor();
  await picker.getByRole("searchbox",{name:"Search emoji"}).fill("trade mark");
  await picker.getByRole("searchbox",{name:"Search emoji"}).press("Shift+Enter");
  assert.ok(await picker.isVisible());
  await sender.keyboard.press("Escape");await picker.waitFor({state:"detached"});
  assert.equal(await editor.inputValue(),"Hi \u{1F44B}\u{1F44B}\u{1F3FD}™️!");
  assert.ok(await editor.evaluate(node=>document.activeElement===node));
  // The composer stays a plain textarea.
  assert.equal(await editor.evaluate(node=>node.localName),"textarea");
  await sender.getByRole("button",{name:"Send",exact:true}).click();
  const sent=await drawn(phone,"Hi ");
  assert.deepEqual(sent.map(face=>face.text),["\u{1F44B}","\u{1F44B}\u{1F3FD}","™️"]);
  await phone.screenshot({path:resolve(output,"emoji-sent-390.png")});

  // On a phone, the picker fits the screen.
  await phone.getByRole("button",{name:"Emoji, stickers and GIFs",exact:true}).click();
  const phonePicker=phone.getByRole("dialog",{name:"Emoji, stickers and GIFs"});await phonePicker.waitFor();
  const box=await phonePicker.boundingBox();assert.ok(box.x>=0&&box.x+box.width<=390&&box.y>=0,JSON.stringify(box));
  await phone.screenshot({path:resolve(output,"emoji-picker-390.png")});
  await phone.mouse.click(20,120);await phonePicker.waitFor({state:"detached"});

  assert.deepEqual(errors,[]);
  assert.ok(requests.every(url=>url.startsWith(origin+"/")||url.startsWith(origin.replace("http:","ws:")+"/")),"same origin only");
  assert.equal(requests.filter(url=>url.endsWith("/emoji/data.json")).length,2,"data.json once per page");
  for(const page of pages)assert.deepEqual(await page.evaluate(()=>__violations),[]);
  Object.assign(report,{passed:true,emoji_requests:requests.filter(url=>url.includes("/emoji/")).length});
  await writeFile(resolve(output,"proof.json"),JSON.stringify(report,null,2));console.log(JSON.stringify(report));
}catch(error){
  await writeFile(resolve(output,"failure.json"),JSON.stringify({error:String(error),stack:error.stack,errors},null,2));
  for(const [index,page] of pages.entries())await page.screenshot({path:resolve(output,`failure-${index}.png`)}).catch(()=>{});
  throw error;
}finally{for(const context of contexts)await context.close();await browser.close();for(const socket of tunnels)socket.destroy();await new Promise(done=>server.close(done));}
