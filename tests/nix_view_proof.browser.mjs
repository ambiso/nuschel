// SPDX-License-Identifier: EUPL-1.2
// Client-view proof for Nix code in the packaged browser App (issue #185),
// driven by tests/nix_view_proof.py:
//   node tests/nix_view_proof.browser.mjs APP RELAY_URL PORT WRAPPER PLAYWRIGHT OUTPUT ORACLE_JSON
// The real App, SDK, owned relay and server text and the App's own content
// Worker; nothing of the App is replaced. The page's clipboard API is
// intercepted (the host clipboard is never read or written) and capture is
// refused. Prints {"event":"ready"} once connected, waits for "sent" on stdin,
// reads both received messages and writes OUTPUT/browser-proof.json.
import assert from "node:assert/strict";
import {createServer} from "node:http";
import {createConnection} from "node:net";
import {readFile, writeFile} from "node:fs/promises";
import {createInterface} from "node:readline";
import {resolve, extname, sep} from "node:path";
import {pathToFileURL} from "node:url";

const [app, relayUrl, port, wrapper, playwright, output, oraclePath] = process.argv.slice(2);
assert.ok(app && relayUrl && Number(port) > 1023 && wrapper && playwright && output && oraclePath, "arguments");
const relay = new URL(relayUrl);
assert.equal(relay.hostname, "127.0.0.1");
assert.equal(relay.protocol, "ws:");
const oracle = JSON.parse(await readFile(oraclePath, "utf8"));
const backstop = setTimeout(() => { console.error("driver deadline reached"); process.exit(3); }, oracle.deadline_ms);
backstop.unref();

const input = createInterface({input: process.stdin});
const received = [];
let wake = null;
input.on("line", line => { received.push(line.trim()); wake?.(); });
async function waitLine(text, ms) {
  const until = Date.now() + ms;
  while (!received.includes(text)) {
    if (Date.now() > until) throw new Error(`no ${JSON.stringify(text)} from the runner within ${ms} ms`);
    await new Promise(done => { wake = done; setTimeout(done, 250); });
  }
}

// The packaged App under /app/ with production-like CSP (tests/web_math.browser.mjs).
const directory = resolve(app);
const types = {".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".wasm": "application/wasm",
  ".svg": "image/svg+xml", ".json": "application/json", ".txt": "text/plain"};
let csp = "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; img-src 'self' data: blob:; font-src 'self'; connect-src 'self'; worker-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";
const server = createServer(async (req, res) => {
  try {
    const pathname = decodeURIComponent(new URL(req.url, "http://127.0.0.1").pathname);
    const mounted = pathname.startsWith("/app/") ? pathname.slice(4) : pathname;
    const path = resolve(directory, "." + mounted);
    if (path !== directory && !path.startsWith(directory + sep)) throw new Error("scope");
    const target = extname(mounted) ? path : resolve(path, "index.html");
    const body = await readFile(target);
    res.writeHead(200, {"Content-Type": types[extname(target)] || "application/octet-stream",
      "Content-Security-Policy": csp, "Permissions-Policy": "camera=(), geolocation=(), microphone=()",
      "Cross-Origin-Opener-Policy": "same-origin", "Referrer-Policy": "no-referrer", "X-Content-Type-Options": "nosniff"});
    res.end(body);
  } catch { res.writeHead(404); res.end(); }
});
const tunnels = new Set();
server.on("upgrade", (req, socket, head) => {
  if (req.url !== "/relay") { socket.destroy(); return; }
  const upstream = createConnection({host: "127.0.0.1", port: Number(relay.port)}, () => {
    let headers = `GET ${relay.pathname} HTTP/1.1\r\n`;
    for (let i = 0; i < req.rawHeaders.length; i += 2) headers += `${req.rawHeaders[i]}: ${req.rawHeaders[i + 1]}\r\n`;
    upstream.write(headers + "\r\n"); if (head.length) upstream.write(head);
    socket.pipe(upstream).pipe(socket);
  });
  tunnels.add(socket); tunnels.add(upstream);
  socket.on("close", () => { tunnels.delete(socket); upstream.destroy(); });
  upstream.on("close", () => { tunnels.delete(upstream); socket.destroy(); });
  socket.on("error", () => upstream.destroy()); upstream.on("error", () => socket.destroy());
});
await new Promise(done => server.listen(0, "127.0.0.1", done));
const origin = `http://127.0.0.1:${server.address().port}`;
csp = csp.replace("connect-src 'self'", `connect-src 'self' ${origin.replace("http:", "ws:")}`);

const proof = {app, origin, csp, checks: {}, views: {}};
const requests = [], errors = [];
const check = (name, passed, detail) => { proof.checks[name] = {passed: Boolean(passed), detail}; };
// The line break before a closing fence is no line of its own in every view.
const strip = text => typeof text === "string" ? text.replace(/\n$/, "") : text;
const space = character => /\s/.test(character);
function expectedChars(runs) {
  const chars = [];
  for (const run of runs) { const rgb = run.foreground.slice(0, 3).join(","); for (const c of run.text) chars.push([c, rgb]); }
  return chars;
}
function shownChars(spans) {
  const chars = [];
  for (const span of spans) {
    const match = /^rgba?\((\d+),\s*(\d+),\s*(\d+)/.exec(span.color);
    const rgb = match ? `${match[1]},${match[2]},${match[3]}` : span.color;
    for (const c of span.text) chars.push([c, rgb]);
  }
  return chars;
}

async function inspect(page, kind) {
  const language = oracle[kind].language;
  const article = page.locator("article").filter({hasText: oracle.tags[kind]});
  await article.waitFor({timeout: 45000});
  // The SDK shows a plain derivative until the content Worker's document
  // arrives; a long message is a preview until "Show more" (either document).
  const pre = article.locator(`pre[aria-label="${language} code"]`);
  const more = article.getByRole("button", {name: "Show more", exact: true});
  const until = Date.now() + 30000;
  while (Date.now() < until) {
    if (await more.count()) await more.click();
    else if (await pre.count()) break;
    await new Promise(done => setTimeout(done, 250));
  }
  await pre.waitFor({timeout: 1000});
  await new Promise(done => setTimeout(done, 500));
  if (await more.count()) await more.click();
  const code = pre.locator("code");
  await pre.scrollIntoViewIfNeeded();
  const rendered = await code.evaluate(element => {
    const block = getComputedStyle(element.closest("pre"));
    return {text: element.textContent, whiteSpace: block.whiteSpace, font: block.fontFamily,
      toolbar: element.closest(".code-block")?.querySelector(".code-toolbar span")?.textContent ?? null,
      spans: [...element.querySelectorAll("span")].map(span => {
        const style = getComputedStyle(span);
        return {text: span.textContent, color: style.color, weight: style.fontWeight, style: style.fontStyle};
      })};
  });
  await article.locator(".code-block").screenshot({path: resolve(output, `browser-${kind}-code.png`)});
  check(`text: ${kind} code element's textContent is the exact body (lines, indentation, blank line, trailing spaces)`,
    strip(rendered.text) === oracle.code, {exact_with_final_newline: rendered.text === oracle.body, shown: rendered.text});
  check(`text: ${kind} code toolbar names ${language}`, rendered.toolbar === language, rendered.toolbar);
  check(`text: ${kind} code block keeps white-space: pre`, rendered.whiteSpace === "pre", rendered.whiteSpace);

  const range = await code.evaluate(element => {
    const selection = getSelection(); selection.removeAllRanges();
    const range = document.createRange(); range.selectNodeContents(element); selection.addRange(range);
    const text = selection.toString(); selection.removeAllRanges(); return text;
  });
  check(`copy: ${kind} selecting the code element yields the exact body (Selection.toString)`, strip(range) === oracle.code, range);
  const ends = await code.evaluate(element => {
    const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT), nodes = [];
    while (walker.nextNode()) if (walker.currentNode.data.length) nodes.push(walker.currentNode);
    const box = (node, at) => { const r = document.createRange(); r.setStart(node, at); r.setEnd(node, at + 1); return r.getBoundingClientRect(); };
    let last = null;
    for (let n = nodes.length - 1; n >= 0 && !last; n--)
      for (let i = nodes[n].data.length - 1; i >= 0; i--) if (!/\s/.test(nodes[n].data[i])) { last = box(nodes[n], i); break; }
    const first = box(nodes[0], 0);
    return {first: {x: first.left, y: first.top + first.height / 2}, last: {x: last.right, y: last.top + last.height / 2}};
  });
  await page.mouse.move(ends.first.x + 0.5, ends.first.y);
  await page.mouse.down();
  await page.mouse.move(ends.last.x + 30, ends.last.y, {steps: 20});
  await page.mouse.up();
  const dragged = await page.evaluate(() => getSelection().toString());
  await article.locator(".code-block").screenshot({path: resolve(output, `browser-${kind}-selection.png`)});
  await page.evaluate(() => getSelection().removeAllRanges());
  check(`copy: ${kind} a pointer drag over the whole block selects the exact body`, strip(dragged) === oracle.code, {dragged, ends});
  const before = await page.evaluate(() => window.__nixProof.copied.length);
  await article.getByRole("button", {name: "Copy code", exact: true}).click();
  await page.waitForFunction(count => window.__nixProof.copied.length > count, before, {timeout: 5000});
  const copied = await page.evaluate(() => window.__nixProof.copied.at(-1));
  check(`copy: ${kind} the Copy code button hands the exact body to the intercepted clipboard API`, strip(copied) === oracle.code, copied);

  const want = expectedChars(oracle[kind].runs), shown = shownChars(rendered.spans);
  const textEqual = want.map(c => c[0]).join("") === shown.map(c => c[0]).join("");
  const mismatches = [];
  let compared = 0;
  if (textEqual) for (let i = 0; i < want.length; i++) {
    if (space(want[i][0])) continue;
    compared++;
    if (want[i][1] !== shown[i][1]) mismatches.push({index: i, char: want[i][0], shown: shown[i][1], expected: want[i][1]});
  }
  const plain = oracle.plain.join(",");
  const distinct = [...new Set(shown.filter(c => !space(c[0])).map(c => c[1]))].filter(rgb => rgb !== plain);
  if (kind === "nix") {
    check("highlight: nix every drawn character's computed colour equals the content worker's",
      textEqual && compared > 0 && mismatches.length === 0, {compared, text_equal: textEqual, mismatches: mismatches.slice(0, 50)});
    check("highlight: nix at least four distinct non-plain colours are rendered", distinct.length >= 4, distinct);
  } else {
    check("highlight: control unknown language renders the plain fallback colour only",
      textEqual && compared > 0 && mismatches.length === 0 && distinct.length === 0,
      {compared, text_equal: textEqual, mismatches: mismatches.slice(0, 50), distinct});
  }
  return {rendered, range, dragged, copied};
}

let browser, context, page;
try {
  const {chromium} = await import(pathToFileURL(playwright).href);
  browser = await chromium.launch({headless: true, executablePath: wrapper});
  context = await browser.newContext({viewport: {width: 1280, height: 900}, colorScheme: "dark"});
  context.on("request", request => requests.push(request.url()));
  page = await context.newPage();
  page.on("pageerror", error => errors.push(error.message));
  await page.addInitScript(tags => {
    const state = window.__nixProof = {copied: [], clipboardReads: 0, captures: 0, contentResults: 0, renderOps: [], workers: []};
    Object.defineProperty(navigator, "clipboard", {configurable: true, value: {
      writeText: async text => { state.copied.push(String(text)); },
      readText: async () => { state.clipboardReads++; throw new Error("No clipboard reads in this fixture."); }}});
    if (navigator.mediaDevices) navigator.mediaDevices.getUserMedia = () => {
      state.captures++; return Promise.reject(new Error("No capture in this fixture."));
    };
    const Native = window.Worker;
    window.Worker = class extends Native {
      constructor(url, options) {
        super(url, options);
        const name = String(url);
        state.workers.push(name.replace(/^.*\//, ""));
        if (name.includes("content-worker")) this.addEventListener("message", ({data}) => { if (data?.kind === "result") state.contentResults++; });
        if (name.includes("sdk.worker")) this.addEventListener("message", ({data}) => {
          if (data?.kind !== "content_work" || !Array.isArray(data.operations)) return;
          for (const operation of data.operations)
            if (operation?.kind === "render" && typeof operation.source === "string" && state.renderOps.length < 64)
              state.renderOps.push({mode: operation.mode ?? null, nix: operation.source.includes(tags.nix),
                control: operation.source.includes(tags.control), bytes: operation.source.length, resultsBefore: state.contentResults});
        });
      }
    };
  }, oracle.tags);
  await page.goto(`${origin}/app/#s=127.0.0.1:${port}&u=nix-reader`);
  await page.getByRole("button", {name: "Trust this certificate", exact: true}).click({timeout: 30000});
  await page.waitForFunction(() => document.querySelector(".server-connection .status")?.textContent === "Connected", null, {timeout: 30000});
  console.log(JSON.stringify({event: "ready"}));
  await waitLine("sent", 120000);
  for (const kind of ["nix", "control"]) proof.views[kind] = await inspect(page, kind);
  await page.screenshot({path: resolve(output, "browser-page.png")});
  const state = await page.evaluate(() => window.__nixProof);
  proof.state = state;
  const operation = state.renderOps.find(entry => entry.nix);
  check("provenance: the SDK handed the Nix message to the content Worker broker and a Worker result followed",
    Boolean(operation) && state.contentResults > operation.resultsBefore,
    {renderOps: state.renderOps, contentResults: state.contentResults, workers: state.workers});
  const foreign = requests.filter(url => !url.startsWith(origin + "/") && !url.startsWith("data:") && !url.startsWith("blob:"));
  check("safety: no capture request, clipboard read, page error or request outside the fixture origin",
    state.captures === 0 && state.clipboardReads === 0 && errors.length === 0 && foreign.length === 0,
    {captures: state.captures, clipboardReads: state.clipboardReads, errors, foreign});
} catch (error) {
  proof.error = String(error?.stack ?? error);
  check("harness: the browser driver completed", false, proof.error);
  await page?.screenshot({path: resolve(output, "browser-failure.png")}).catch(() => {});
} finally {
  proof.requests = requests.length;
  proof.errors = errors;
  await writeFile(resolve(output, "browser-proof.json"), JSON.stringify(proof, null, 2));
  await context?.close().catch(() => {});
  await browser?.close().catch(() => {});
  for (const socket of tunnels) socket.destroy();
  server.closeAllConnections?.();
  await new Promise(done => server.close(done));
  input.close();
  const results = Object.values(proof.checks);
  const passed = results.length > 0 && results.every(result => result.passed);
  console.log(JSON.stringify({event: "done", passed}));
  process.exitCode = passed ? 0 : 1;
}
