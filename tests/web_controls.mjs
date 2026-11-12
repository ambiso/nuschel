// SPDX-License-Identifier: EUPL-1.2
// Control sizing policy of the web app (docs/web.md): connection, voice, profile
// and composer controls keep a 44x44 CSS px hit target at phone, desktop, zoomed
// and enlarged-text layouts, with long names, visible keyboard focus and no
// overflow. Chromium under the production CSP with a deterministic SDK snapshot;
// no transport, audio device or physical browser zoom is exercised (zoom is the
// equivalent CSS viewport and device pixel ratio).
// node tests/web_controls.mjs web/app/dist OUTPUT [--report-only]
import { createServer } from "node:http";
import { readFile, mkdir, writeFile } from "node:fs/promises";
import { resolve, extname, sep } from "node:path";
import { pathToFileURL } from "node:url";
import assert from "node:assert/strict";
const args = process.argv.slice(2), reportOnly = args.includes("--report-only");
const [site, output] = args.filter(arg => !arg.startsWith("--"));
assert.ok(site && output, "built site and disk output directory required");
const { chromium } = await import(pathToFileURL(process.env.NUSCHL_PLAYWRIGHT || "/nix/store/phc5gsqnm205r6hvlay77f7ia3w4bzzk-playwright-core-1.64.1/index.mjs"));
const directory = resolve(site), out = resolve(output), TARGET = 44;
await mkdir(out, { recursive:true });
const types = {".html":"text/html", ".js":"text/javascript", ".wasm":"application/wasm", ".css":"text/css", ".svg":"image/svg+xml"};
let csp = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const server = createServer(async (req, res) => {
  try {
    const pathname = decodeURIComponent(new URL(req.url, "http://127.0.0.1").pathname);
    const mounted = pathname.startsWith("/app/") ? pathname.slice(4) : pathname;
    const path = resolve(directory, "." + mounted);
    assert.ok(path === directory || path.startsWith(directory + sep));
    const target = extname(mounted) ? path : resolve(path, "index.html");
    res.writeHead(200, {"Content-Type":types[extname(target)] || "application/octet-stream", "Content-Security-Policy":csp, "Referrer-Policy":"no-referrer", "Cross-Origin-Opener-Policy":"same-origin", "Permissions-Policy":"camera=(), geolocation=(), microphone=(self)"});
    res.end(await readFile(target));
  } catch { res.writeHead(404); res.end(); }
});
await new Promise(done => server.listen(0, "127.0.0.1", done));
const origin = `http://127.0.0.1:${server.address().port}`;
csp = csp.replace("connect-src 'self'", `connect-src 'self' ${origin.replace("http:", "ws:")}`);
const longServer = "The Extraordinarily Long Community Server Name That Never Ends";
const longUser = "Maximilian-Alexander Wolfeschlegelsteinhausenbergerdorff";
const longChannel = "A voice channel whose name is far too long to fit anywhere at all";
const channels = [{id:0,parent:null,name:"Root"}, {id:1,parent:0,name:longChannel}, {id:2,parent:0,name:"lounge"}];
const snapshot = {kind:"state", servers:[longServer, "Second", "Third"].map((name, i) => ({id:`server${i}`, name, host:"127.0.0.1", port:64738, username:longUser, status:"connected", pin:"a".repeat(64), own_session:1, own_channel:0, muted:true, deafened:false, voice_supported:true, channels, users:[{session:1,name:longUser,channel:0,self_mute:true},{session:2,name:"Ada Lovelace",channel:1}], unread_count:0})), timeline:{server:"server0",channel:1,name:longChannel,loading:false,has_older:false,sending:false,messages:[{serial:"1",author:"Ada Lovelace",at_ms:1700000000000,outgoing:false,document:{blocks:[{kind:"text",text:"Controls must stay reachable."}]}}]}, updates:[]};
// name, CSS viewport, device pixel ratio, touch, text size (the app's setting).
const cases = [
  ["desktop", 1440, 960, 1, false, 16],
  ["phone-390", 390, 844, 3, true, 16],
  ["phone-320", 320, 640, 2, true, 16],
  // Browser zoom 200% of a 1280x800 window: a 640x400 CSS viewport at DPR 2.
  ["zoom-200", 640, 400, 2, false, 16],
  // The largest text size at the narrowest supported width.
  ["phone-320-text-20", 320, 640, 2, true, 20],
];
const browser = await chromium.launch({headless:true, executablePath:process.env.NUSCHL_CHROMIUM || "/nix/store/fqriy9vihl23l4md56qgiks8z0d2xdlw-chromium-156.0.8078.79/bin/chromium"});
const report = [], failures = [];
const check = (condition, message) => { if (!condition) { failures.push(message); if (!reportOnly) assert.fail(message); } };
async function measure(page, locator, label) {
  const box = await locator.boundingBox();
  check(box, `${label}: not rendered`);
  return box && {label, width:Math.round(box.width * 10) / 10, height:Math.round(box.height * 10) / 10};
}
// Focus the preceding tabbable element, then press Tab: the browser itself decides
// keyboard focus visibility; the outline must be drawn and stay inside the viewport.
async function keyboardFocus(page, locator, label) {
  const handle = await locator.elementHandle();
  await page.evaluate(target => {
    const tabbable = [...document.querySelectorAll("button,a[href],input,textarea,select,[tabindex]")].filter(node => !node.disabled && node.tabIndex >= 0 && node.getClientRects().length && getComputedStyle(node).visibility !== "hidden");
    const scope = target.closest("dialog[open]") || document;
    const inScope = tabbable.filter(node => scope === document ? !node.closest("dialog") || node.closest("dialog[open]") : scope.contains(node));
    const index = inScope.indexOf(target);
    if (index < 1) throw Error("no preceding tabbable element");
    inScope[index - 1].focus();
  }, handle);
  await page.keyboard.press("Tab");
  const focus = await page.evaluate(target => {
    const style = getComputedStyle(target), rect = target.getBoundingClientRect();
    const ring = (parseFloat(style.outlineWidth) || 0) + (parseFloat(style.outlineOffset) || 0);
    return {focused:document.activeElement === target, visible:target.matches(":focus-visible"), outline:style.outlineStyle, width:parseFloat(style.outlineWidth) || 0,
      inside:rect.left - ring >= 0 && rect.top - ring >= 0 && rect.right + ring <= innerWidth && rect.bottom + ring <= innerHeight};
  }, handle);
  check(focus.focused && focus.visible && focus.outline !== "none" && focus.width >= 2, `${label}: keyboard focus not visible ${JSON.stringify(focus)}`);
  check(focus.inside, `${label}: focus ring clipped by the viewport`);
  return focus;
}
async function noOverflow(page, name, selectors) {
  const result = await page.evaluate(selectors => {
    const issues = [];
    if (document.documentElement.scrollWidth > innerWidth) issues.push(`page scrollWidth ${document.documentElement.scrollWidth} > ${innerWidth}`);
    for (const selector of selectors) for (const node of document.querySelectorAll(selector)) {
      const rect = node.getBoundingClientRect();
      if (!rect.width) continue;
      if (rect.left < -0.5 || rect.right > innerWidth + 0.5) issues.push(`${selector} outside viewport ${rect.left}-${rect.right}`);
      if (node.scrollWidth > node.clientWidth + 1 && getComputedStyle(node).overflowX === "visible") issues.push(`${selector} content overflows ${node.scrollWidth}>${node.clientWidth}`);
      const parent = node.parentElement?.getBoundingClientRect();
      if (parent && (rect.left < parent.left - 0.5 || rect.right > parent.right + 0.5)) issues.push(`${selector} escapes its container`);
    }
    // Controls in one group must not overlap each other.
    for (const group of document.querySelectorAll(".voice-actions,.server-connection,.profile,.composer-input")) {
      const items = [...group.querySelectorAll(":scope > button, :scope > label, :scope > div > button")].map(node => node.getBoundingClientRect()).filter(rect => rect.width);
      for (let a = 0; a < items.length; a++) for (let b = a + 1; b < items.length; b++) {
        const x = items[a], y = items[b];
        if (x.left < y.right - 0.5 && y.left < x.right - 0.5 && x.top < y.bottom - 0.5 && y.top < x.bottom - 0.5) issues.push(`overlapping controls in .${group.className}`);
      }
    }
    return issues;
  }, selectors);
  check(!result.length, `${name}: ${result.join("; ")}`);
  return result;
}
try {
  for (const [name, width, height, scale, touch, textSize] of cases) {
    const context = await browser.newContext({viewport:{width,height}, deviceScaleFactor:scale, hasTouch:touch, colorScheme:"dark", reducedMotion:"reduce"});
    const page = await context.newPage(), errors = [], remote = [];
    page.on("pageerror", error => errors.push(error.message));
    page.on("request", request => { if (!request.url().startsWith(origin + "/")) remote.push(request.url()); });
    await page.addInitScript(([initial, textSize]) => {
      window.__csp = [];
      document.addEventListener("securitypolicyviolation", event => window.__csp.push(event.violatedDirective + ":" + event.blockedURI));
      try { localStorage.setItem("nuschl.appearance.v1", JSON.stringify({theme:"dark", density:"comfortable", fontSize:textSize})); } catch {}
      let state = structuredClone(initial);
      window.AudioContext = undefined; // Layout only; no audio device is opened.
      window.Worker = class {
        postMessage(data) {
          queueMicrotask(() => {
            if (data.method === "init") { this.onmessage?.({data:{kind:"ready", capabilities:["remove_server","dismiss_notice","voice","stop_audio"]}}); this.onmessage?.({data:structuredClone(state)}); return; }
            const [index, channel] = data.args || [];
            const server = state.servers[index];
            if (data.method === "join") { server.own_channel = channel; server.users[0].channel = channel; server.muted = true; server.deafened = false; state.voice = {server_id:server.id, connection_generation:"1", voice_generation:"1", channel_id:channel, joined:true, muted:true, deafened:false, can_transmit:false}; }
            if (data.method === "toggle_mute") { server.muted = !server.muted; server.deafened = false; state.voice = {...state.voice, muted:server.muted, deafened:false}; }
            if (data.method === "toggle_deaf") { server.deafened = !server.deafened; server.muted = true; state.voice = {...state.voice, muted:true, deafened:server.deafened}; }
            if (data.method === "disconnect") { server.status = "disconnected"; if (state.voice?.server_id === server.id) state.voice = null; }
            if (data.method === "connect") server.status = "connected";
            this.onmessage?.({data:{kind:"result", id:data.id}});
            this.onmessage?.({data:structuredClone(state)});
          });
        }
        terminate() {}
      };
    }, [snapshot, textSize]);
    await page.goto(origin + "/app/");
    await page.getByLabel("Message", {exact:true}).waitFor();
    const mobile = width <= 760;
    const openNavigation = async () => { if (mobile) { await page.getByRole("button", {name:"Open navigation", exact:true}).click(); await page.getByRole("dialog", {name:"Server navigation"}).waitFor(); } };
    const closeNavigation = async () => { if (mobile) { await page.keyboard.press("Escape"); await page.getByRole("dialog", {name:"Server navigation"}).waitFor({state:"detached"}); } };
    const sizes = [], focus = {};
    await page.getByRole("button", {name:"Join voice", exact:true}).click();
    const voice = page.getByRole("region", {name:"Voice controls"});
    await voice.waitFor();
    const controls = {
      unmute: voice.getByRole("button", {name:"Unmute microphone", exact:true}),
      deafen: voice.getByRole("button", {name:"Deafen", exact:true}),
      enableAudio: voice.getByRole("button", {name:"Enable audio", exact:true}),
      stopAudio: voice.getByRole("button", {name:"Stop audio", exact:true}),
      send: page.getByRole("button", {name:"Send", exact:true}),
    };
    for (const [label, locator] of Object.entries(controls)) sizes.push(await measure(page, locator, `${name} ${label}`));
    await noOverflow(page, `${name} voice`, [".voice-bar", ".voice-actions", ".composer-input", ".chat-header"]);
    focus.unmute = await keyboardFocus(page, controls.unmute, `${name} unmute`);
    await voice.screenshot({path:resolve(out, `${name}-voice-focus.png`)});
    await page.screenshot({path:resolve(out, `${name}-voice.png`)});
    // Mute and deafen keep their accessible names through a toggle round trip.
    await controls.unmute.click();
    sizes.push(await measure(page, voice.getByRole("button", {name:"Mute microphone", exact:true}), `${name} mute`));
    await controls.deafen.click();
    sizes.push(await measure(page, voice.getByRole("button", {name:"Enable sound", exact:true}), `${name} enable-sound`));
    focus.enableSound = await keyboardFocus(page, voice.getByRole("button", {name:"Enable sound", exact:true}), `${name} enable sound`);
    await voice.getByRole("button", {name:"Enable sound", exact:true}).click();
    await voice.getByRole("button", {name:"Unmute microphone", exact:true}).waitFor();
    // Connection and profile controls, in the drawer on phones.
    await openNavigation();
    const disconnect = page.getByRole("button", {name:"Disconnect", exact:true});
    const profileSettings = page.locator(".profile").getByRole("button", {name:"Settings", exact:true});
    sizes.push(await measure(page, disconnect, `${name} disconnect`));
    sizes.push(await measure(page, profileSettings, `${name} profile-settings`));
    await noOverflow(page, `${name} navigation`, [".server-connection", ".profile", ".server-header"]);
    focus.disconnect = await keyboardFocus(page, disconnect, `${name} disconnect`);
    await page.screenshot({path:resolve(out, `${name}-navigation-focus.png`)});
    focus.profileSettings = await keyboardFocus(page, profileSettings, `${name} profile settings`);
    await disconnect.click();
    const connect = page.getByRole("button", {name:"Connect", exact:true});
    await connect.waitFor();
    sizes.push(await measure(page, connect, `${name} connect`));
    await noOverflow(page, `${name} disconnected`, [".server-connection", ".profile"]);
    await page.screenshot({path:resolve(out, `${name}-disconnected.png`)});
    await connect.click();
    await page.getByRole("button", {name:"Disconnect", exact:true}).waitFor();
    await closeNavigation();
    for (const size of sizes.filter(Boolean)) check(size.width >= TARGET - 0.01 && size.height >= TARGET - 0.01, `${size.label}: ${size.width}x${size.height} below ${TARGET}x${TARGET}`);
    check(!(await page.evaluate(() => window.__csp)).length, `${name}: CSP violations`);
    check(!errors.length, `${name}: browser errors ${errors.join("; ")}`);
    check(!remote.length, `${name}: remote requests ${remote.join(", ")}`);
    report.push({name, viewport:{width,height}, deviceScaleFactor:scale, textSize, sizes:sizes.filter(Boolean), focus});
    await context.close();
  }
  await writeFile(resolve(out, "controls.json"), JSON.stringify({target:TARGET, reportOnly, productionCsp:true, syntheticSdk:true, failures, cases:report}, null, 2) + "\n");
  if (reportOnly) console.log(`Control report written (${failures.length} findings).`);
  else console.log("Control sizing passed:", report.map(item => item.name).join(", "));
} finally { await browser.close(); await new Promise(done => server.close(done)); }
