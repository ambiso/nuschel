// SPDX-License-Identifier: EUPL-1.2
// Actual Chromium layout/interaction under production CSP, deterministic SDK
// snapshots only. This does not certify transport, audio, Safari or OS keyboards.
// node tests/web_responsive.mjs web/app/dist OUTPUT
import { createServer } from "node:http";
import { readFile, mkdir, writeFile } from "node:fs/promises";
import { resolve, extname, sep } from "node:path";
import { pathToFileURL } from "node:url";
import assert from "node:assert/strict";
const [site, output] = process.argv.slice(2);
assert.ok(site && output, "built site and disk output directory required");
const { chromium } = await import(pathToFileURL(process.env.NUSCHL_PLAYWRIGHT || "/nix/store/phc5gsqnm205r6hvlay77f7ia3w4bzzk-playwright-core-1.64.1/index.mjs"));
const directory = resolve(site), out = resolve(output);
await mkdir(out, { recursive:true });
const types = {".html":"text/html", ".js":"text/javascript", ".wasm":"application/wasm", ".css":"text/css", ".svg":"image/svg+xml"};
let csp = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const server = createServer(async (req, res) => {
  try {
    const pathname=decodeURIComponent(new URL(req.url,"http://127.0.0.1").pathname);
    const mounted=pathname.startsWith("/app/") ? pathname.slice(4) : pathname;
    const path = resolve(directory,"." + mounted);
    assert.ok(path === directory || path.startsWith(directory + sep));
    const target = extname(mounted) ? path : resolve(path, "index.html");
    const bytes = await readFile(target);
    res.writeHead(200, {"Content-Type":types[extname(target)] || "application/octet-stream", "Content-Security-Policy":csp, "Referrer-Policy":"no-referrer","Cross-Origin-Opener-Policy":"same-origin","Permissions-Policy":"camera=(), geolocation=(), microphone=(self)"});
    res.end(bytes);
  } catch { res.writeHead(404); res.end(); }
});
await new Promise((done) => server.listen(0,"127.0.0.1",done));
const origin = `http://127.0.0.1:${server.address().port}`;
csp=csp.replace("connect-src 'self'",`connect-src 'self' ${origin.replace("http:","ws:")}`);
const channels = [{id:0,parent:null,name:"Root"}, {id:1,parent:0,name:"lounge"}, {id:2,parent:1,name:"Deeply nested discussion"}, {id:3,parent:0,name:"A very long channel name with plenty of words that must never push the chat out of view"}, ...Array.from({length:40},(_,i)=>({id:i+4,parent:0,name:`Channel ${i+4}`,unread_count:i===39 ? 2:0}))];
const messages = [
  {serial:"1",author:"Ada Lovelace",at_ms:1700000000000,outgoing:false,document:{blocks:[{kind:"text",text:"A little closer, from anywhere.\n\nNewlines stay exactly where you put them."}]}},
  {serial:"2",author:"Lin",at_ms:1700000010000,outgoing:false,document:{blocks:[{kind:"text",text:"A shared space for the team."},{kind:"code",language:"rust",runs:[{text:"fn ",foreground:[190,145,255,255]},{text:"main",foreground:[115,210,190,255]},{text:'() { println!("hello from Nuschl"); }',foreground:[220,225,240,255]}]}]}},
  {serial:"3",author:"Ada Lovelace",at_ms:1700000020000,outgoing:false,document:{blocks:[{kind:"text",text:"https://example.invalid/docs",spans:[{start:0,end:28,link:"https://example.invalid/docs",style:{bold:true}}]}]}},
  {serial:"4",author:"You",at_ms:1700000030000,outgoing:true,document:{blocks:[{kind:"text",text:Array.from({length:20},(_,i)=>`Long message line ${i}`).join("\n")}]}},
];
const snapshot = {kind:"state", servers:Array.from({length:16},(_,i)=>({id:`server${i}`,name:i ? `Community ${i}` : "The Common Room",host:"127.0.0.1",port:64738,username:"You",status:"connected",pin:"a".repeat(64),own_session:1,own_channel:0,muted:true,deafened:false,voice_supported:true,channels,users:[{session:1,name:"You",channel:0,self_mute:true},{session:2,name:"Ada Lovelace",channel:0},{session:3,name:"Lin",channel:1}],unread_count:i===15 ? 4:0})),timeline:{server:"server0",channel:0,name:"Root",loading:false,has_older:false,sending:false,messages},updates:[]};
const browser = await chromium.launch({headless:true, executablePath:process.env.NUSCHL_CHROMIUM || "/nix/store/fqriy9vihl23l4md56qgiks8z0d2xdlw-chromium-156.0.8078.79/bin/chromium"});
const proofs = [];
try {
  for (const [name,width,height,touch] of [["phone-320",320,640,true],["phone-360",360,740,true],["phone-390",390,844,true],["tablet",820,1180,true],["landscape",844,390,true],["desktop",1440,960,false]]) {
    const context = await browser.newContext({viewport:{width,height},hasTouch:touch,colorScheme:"dark",reducedMotion:"reduce"});
    const page = await context.newPage(), errors = [], remote = [];
    page.on("pageerror", (error) => errors.push(error.message));
    page.on("request", (request) => { if (!request.url().startsWith(origin + "/")) remote.push(request.url()); });
    await page.addInitScript((initial) => {
      window.__commands=[]; window.__csp=[];
      document.addEventListener("securitypolicyviolation", (event) => window.__csp.push(event.violatedDirective + ":" + event.blockedURI));
      let state=structuredClone(initial);
      // This matrix proves layout and commands, not actual audio. Keep all
      // device/context creation disabled; the isolated real-audio fixture is separate.
      window.AudioContext = undefined;
      window.Worker=class {
        postMessage(data) {
          window.__commands.push(data);
          queueMicrotask(() => {
            if (data.method === "init") { this.onmessage?.({data:{kind:"ready",capabilities:["remove_server","dismiss_notice","voice","stop_audio"]}}); this.onmessage?.({data:structuredClone(state)}); return; }
            const [index,channel]=data.args || [];
            if (data.method === "open" || data.method === "browse_server") { const server=state.servers[index]; state.timeline={...state.timeline,server:server.id,channel:channel ?? 0,name:server.channels.find(x=>x.id===(channel ?? 0)).name}; }
            if (data.method === "join") {
              state.servers[index].users[0].channel=channel; state.servers[index].own_channel=channel;
              state.servers[index].muted=true; state.servers[index].deafened=false;
              state.voice={server_id:state.servers[index].id,connection_generation:"1",voice_generation:String(Number(state.voice?.voice_generation||0)+1),channel_id:channel,joined:true,muted:true,deafened:false,can_transmit:false};
            }
            if (data.method === "toggle_mute") { const s=state.servers[index];s.muted=!s.muted;s.deafened=false;state.voice={...state.voice,muted:s.muted,deafened:false,voice_generation:String(Number(state.voice.voice_generation)+1)}; }
            if (data.method === "toggle_deaf") { const s=state.servers[index];s.deafened=!s.deafened;s.muted=true;state.voice={...state.voice,muted:true,deafened:s.deafened,voice_generation:String(Number(state.voice.voice_generation)+1)}; }
            if (data.method === "stop_audio") state.voice=null;
            if (data.method === "send_text") {
              const body=data.args[0],server=state.timeline.server,channel=state.timeline.channel;
              state.timeline.sending=true;
              window.__ackLastSend=()=>{
                state.timeline.sending=false;
                this.onmessage?.({data:{...structuredClone(state),updates:[{kind:"sent",server,channel,body}]}});
              };
            }
            this.onmessage?.({data:{kind:"result",id:data.id}});
            this.onmessage?.({data:structuredClone(state)});
          });
        }
        terminate() {}
      };
    }, snapshot);
    await page.goto(origin+"/app/");
    await page.getByLabel("Message",{exact:true}).waitFor();
    await page.waitForFunction(()=>document.querySelector(".chat-header h2")?.textContent==="Root");
    assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true,`${name}: page overflow`);
    const chat=await page.locator(".chat").boundingBox();
    assert.ok(chat.width>=width-(width<=760 ? 1:312),`${name}: chat squeezed`);
    if (width<=760) {
      assert.equal(await page.locator("main .navigation").count(),0);
      await page.getByRole("button",{name:"Open navigation",exact:true}).click();
      const dialog=page.getByRole("dialog",{name:"Server navigation"});
      await dialog.waitFor();
      for(let i=0;i<12;i++)await page.keyboard.press("Tab");
      assert.equal(await page.evaluate(()=>document.querySelector("dialog").contains(document.activeElement)),true,"drawer focus escaped");
      await page.keyboard.press("Escape");
      await dialog.waitFor({state:"detached"});
      await page.getByRole("button",{name:"Open navigation",exact:true}).evaluate(node=>{if(node!==document.activeElement)throw Error("focus not restored");});
      await page.getByRole("button",{name:"Open navigation",exact:true}).click();
      await page.getByRole("button",{name:"Expand lounge",exact:true}).click();
      await page.getByRole("button",{name:"Deeply nested discussion",exact:true}).waitFor();
      await page.getByRole("button",{name:"lounge",exact:true}).dispatchEvent("pointerdown",{pointerType:"touch"});
      await page.getByRole("button",{name:"lounge",exact:true}).dispatchEvent("dblclick");
      assert.equal(await page.evaluate(()=>window.__commands.filter(x=>x.method==="join").length),0,"touch doubletap joined voice");
      await page.getByRole("button",{name:"lounge",exact:true}).click();
      await dialog.waitFor({state:"detached"});
      await page.getByRole("button",{name:"Join voice",exact:true}).click();
      assert.equal(await page.evaluate(()=>window.__commands.filter(x=>x.method==="join").at(-1).args[1]),1);
      await page.getByRole("button",{name:"Open navigation",exact:true}).click();
      await page.setViewportSize({width:1000,height});
      await dialog.waitFor({state:"detached"});
      assert.equal(await page.locator("main .navigation").count(),1);
      await page.setViewportSize({width,height});
    } else {
      await page.getByRole("button",{name:"lounge",exact:true}).click();
      assert.equal(await page.evaluate(()=>window.__commands.filter(x=>x.method==="join").length),0,"single click joined voice");
      await page.getByRole("button",{name:"lounge",exact:true}).dblclick();
      assert.equal(await page.evaluate(()=>window.__commands.filter(x=>x.method==="join").at(-1).args[1]),1);
    }
    await page.getByRole("region",{name:"Voice controls"}).waitFor();
    await page.getByRole("button",{name:"Unmute microphone",exact:true}).click();
    await page.getByRole("button",{name:"Mute microphone",exact:true}).waitFor();
    // The deterministic SDK is unmuted, but there is no real media/device here.
    assert.equal(await page.getByText("Microphone on",{exact:true}).count(),0);
    await page.getByRole("button",{name:"Deafen",exact:true}).click();
    await page.getByRole("button",{name:"Enable sound",exact:true}).click();
    await page.getByRole("button",{name:"Unmute microphone",exact:true}).waitFor();
    if (name === "desktop") {
      const composer=page.getByLabel("Message",{exact:true});
      await composer.fill("Queued message");await page.getByRole("button",{name:"Send",exact:true}).click();
      assert.equal(await composer.inputValue(),"Queued message","queueing cleared composer before SDK Sent");
      await composer.fill("A new draft typed while sending");await page.evaluate(()=>window.__ackLastSend());
      assert.equal(await composer.inputValue(),"A new draft typed while sending","old Sent erased newer draft");
      await composer.fill("Clear only this acknowledged body");await page.getByRole("button",{name:"Send",exact:true}).click();
      assert.equal(await composer.inputValue(),"Clear only this acknowledged body");
      await page.evaluate(()=>window.__ackLastSend());
      await page.waitForFunction(()=>document.querySelector("#message").value === "");
    }
    // Changing conversations preserves independent drafts.
    await page.getByLabel("Message",{exact:true}).fill("A draft in lounge\nsecond line");
    const navigate=async(label)=>{if(width<=760)await page.getByRole("button",{name:"Open navigation",exact:true}).click();await page.getByRole("button",{name:label,exact:true}).click();};
    await navigate("Root");
    assert.equal(await page.getByLabel("Message",{exact:true}).inputValue(),"");
    await navigate("lounge");
    assert.equal(await page.getByLabel("Message",{exact:true}).inputValue(),"A draft in lounge\nsecond line");
    await navigate("Root");
    await page.getByRole("button",{name:"Show more",exact:true}).click();
    await page.getByText("Long message line 19",{exact:false}).waitFor();
    await page.getByRole("button",{name:"Search messages",exact:true}).click();
    await page.getByLabel("Search this loaded history").fill("main");
    assert.equal(await page.locator("article").count(),1);
    await page.getByRole("button",{name:"Close search",exact:true}).click();
    await page.locator(".messages").evaluate(node=>node.scrollTop=0);
    await page.screenshot({path:resolve(out,name+".png")});
    // Native dialog focus containment and persisted, real CSS theme changes.
    if(width<=760)await page.getByRole("button",{name:"Open navigation",exact:true}).click();
    await page.getByRole("button",{name:"Settings",exact:true}).last().click();
    await page.getByLabel("Theme",{exact:true}).selectOption("light");
    assert.equal(await page.evaluate(()=>getComputedStyle(document.querySelector(".chat")).backgroundColor),"rgb(255, 255, 255)");
    await page.getByLabel("Text size",{exact:true}).selectOption("20");
    await page.keyboard.press("Escape");
    await page.setViewportSize({width,height:Math.min(height,410)});
    await page.waitForFunction(()=>document.querySelector("main").getBoundingClientRect().height <= innerHeight + 1);
    const composer=await page.locator(".composer").boundingBox();
    assert.ok(composer.y+composer.height<=Math.min(height,410)+1,`${name}: keyboard-height surrogate hides composer (${JSON.stringify(composer)})`);
    assert.equal(await page.evaluate(()=>document.documentElement.scrollWidth<=innerWidth),true);
    assert.deepEqual(await page.evaluate(()=>window.__csp),[],`${name}: CSP violations`);
    assert.deepEqual(errors,[],`${name}: browser errors`);assert.deepEqual(remote,[],`${name}: remote request`);
    proofs.push({name,width,height,passed:true});
    await context.close();
  }
  await writeFile(resolve(out,"proof.json"),JSON.stringify({productionCsp:true,syntheticSdk:true,transportAndAudioVerified:false,cases:proofs},null,2)+"\n");
  console.log("Responsive UI passed:",proofs.map(x=>x.name).join(", "));
} finally { await browser.close(); await new Promise(done=>server.close(done)); }
