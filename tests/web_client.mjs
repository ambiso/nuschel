// SPDX-License-Identifier: EUPL-1.2
// The web client's SDK (crates/client-wasm, built by `just wasm-client`) in
// Node.js, which has the browser's WebSocket, WebAssembly and Web Crypto: it
// connects through the web relay to a Mumble server, trusts the certificate it
// is shown, and must reach "connected". Run by tests/web_client.py.
//   node tests/web_client.mjs <sdk directory> <relay URL> <host> <port>
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const [sdk, relay, host, port] = process.argv.slice(2);
// A busy or distant network can leave CONNECTING across several timer ticks.
// Delay the real local socket's open event/state to exercise that exact race;
// the tunnel's Open must still be there when the transport becomes writable.
const NativeWebSocket = globalThis.WebSocket;
class DelayedWebSocket extends NativeWebSocket {
  #exposed = false;

  constructor(...arguments_) {
    super(...arguments_);
    this.addEventListener("open", (event) => {
      if (this.#exposed) return;
      event.stopImmediatePropagation();
      setTimeout(() => {
        if (super.readyState !== NativeWebSocket.OPEN) return;
        this.#exposed = true;
        this.dispatchEvent(new Event("open"));
      }, 350);
    });
  }

  get readyState() {
    const actual = super.readyState;
    return actual === NativeWebSocket.OPEN && !this.#exposed
      ? NativeWebSocket.CONNECTING
      : actual;
  }
}
globalThis.WebSocket = DelayedWebSocket;
const bindings = await import(pathToFileURL(join(sdk, "nuschl_client_wasm.js")).href);
await bindings.default({ module_or_path: await readFile(join(sdk, "nuschl_client_wasm_bg.wasm")) });

const client = new bindings.WebClient(relay);
let wake = () => {};
client.on_change(() => wake());
const index = client.add_server("", host, Number(port), "web-sdk", undefined);
client.connect(index);
const keepAlive = setInterval(() => client.tick(performance.now()), 10);

const deadline = Date.now() + 20000;
let trusted = null;
for (;;) {
  client.poll();
  const [server] = JSON.parse(client.servers());
  if (server.certificate && !trusted) {
    trusted = server.certificate;
    console.log(`certificate ${trusted}`);
    client.accept_certificate(index, trusted);
  } else if (server.status === "connected") {
    const users = server.users.map((user) => user.name);
    console.log(`connected channels=${server.channels.length} users=${users.join(",")}`);
    break;
  } else if (Date.now() > deadline) {
    console.log(`failed: ${JSON.stringify(server)}`);
    process.exit(1);
  }
  await new Promise((resolve) => {
    wake = resolve;
    setTimeout(resolve, 200);
  });
}
// A second WASM client exchanges actual legacy text through the same local
// relay. This in-memory adapter tests the opaque Rust storage bridge; the real
// IndexedDB reopen/scoping tests live in web_history.mjs.
const stores = new Map();
function drain(client) {
  const rows = stores.get(client) || [];
  stores.set(client, rows);
  for (let pass = 0; pass < 20; pass++) {
    client.poll();
    const operations = JSON.parse(client.storage_requests());
    if (!operations.length) break;
    for (const operation of operations) {
      let response;
      if (operation.kind === "append") {
        const id = rows.length + 1;
        rows.push({ id, message: operation.message, outgoing: operation.outgoing });
        response = { id };
      } else {
        let found = rows.filter(row => row.message.server === operation.identity && row.message.channel === operation.channel)
          .sort((a,b)=>a.message.observed_at_ms-b.message.observed_at_ms||a.id-b.id);
        if (operation.before !== null) found = found.slice(0, found.findIndex(row=>row.id===operation.before));
        const more = found.length > operation.limit;
        found = found.slice(-operation.limit);
        response = { rows: found, next_before: more ? found[0].id : null };
      }
      client.storage_complete(operation.ticket, JSON.stringify(response));
    }
  }
}
const peer = new bindings.WebClient(relay);
peer.on_change(() => wake());
const peerIndex = peer.add_server("",host,Number(port),"web-peer",trusted || JSON.parse(client.servers())[0].pin);
peer.connect(peerIndex);
const peerTimer=setInterval(()=>peer.tick(performance.now()),50);
async function until(check, description) {
  const deadline=Date.now()+15000;
  for (;;) {
    drain(client);drain(peer);
    if (check()) return;
    if (Date.now()>deadline) throw new Error(description);
    await new Promise(resolve=>setTimeout(resolve,25));
  }
}
await until(()=>JSON.parse(peer.servers())[0].status==="connected","peer did not connect");
client.open(index,0);peer.open(peerIndex,0);drain(client);drain(peer);
const text='literal <img src="https://tracker.invalid/x">\n\n```rust\nfn main() {}\n```';
client.send_text(text);
await until(()=>JSON.parse(peer.timeline()).messages.some(message=>message.author==="web-sdk"),"peer did not receive text");
const incoming=JSON.parse(peer.timeline()).messages.find(message=>message.author==="web-sdk");
const blocks=incoming.document.blocks;
if (!blocks.some(block=>block.kind==="text"&&block.text.includes('literal <img src="https://tracker.invalid/x">\n\n')) || !blocks.some(block=>block.kind==="code"&&block.runs.some(run=>run.text.includes("fn")))) {
  throw new Error(`multiline/code rendering failed: ${JSON.stringify(blocks)}`);
}
// Navigate to a different selected channel. Root traffic stays in its own SDK
// timeline/cache and does not replace the selected view when writes complete.
const alternate=JSON.parse(peer.servers())[0].channels.find(channel=>channel.id!==0);
if (alternate) {
  peer.open(peerIndex,alternate.id);drain(peer);
  client.send_text("root message while another channel is selected");
  await until(()=>stores.get(peer).some(row=>row.message.body.includes("root message while")),"background root message was not stored");
  if (JSON.parse(peer.timeline()).channel!==alternate.id) throw new Error("live message changed selected channel");
  peer.open(peerIndex,0);drain(peer);
  if (!JSON.parse(peer.timeline()).messages.some(message=>message.document.blocks.some(block=>block.kind==="text"&&block.text.includes("root message while")))) throw new Error("cached root history missing after navigation");
}
// Private conversation selection/send remains in the shared SDK. A legacy
// peer without a certificate keeps an ephemeral key instead of adopting a
// reused session number's history.
const recipient=JSON.parse(client.servers())[0].users.find(user=>user.name==="web-peer");
client.open_direct(index,recipient.session);drain(client);
client.send_text("private text from the browser SDK");
await until(()=>stores.get(peer).some(row=>row.message.body==="private text from the browser SDK"),"private text did not arrive");
const direct=JSON.parse(peer.servers())[0].channels.find(channel=>channel.direct);
if(!direct)throw new Error("SDK private conversation missing");
peer.open(peerIndex,direct.id);drain(peer);
if(!JSON.parse(peer.timeline()).messages.some(message=>message.document.blocks.some(block=>block.kind==="text"&&block.text.includes("private text from the browser SDK"))))throw new Error("private timeline missing");
console.log("text interchange: multiline literals, inert HTML, highlighted Rust, certificate-scoped write acknowledgements and SDK-selected timeline navigation passed");
clearInterval(keepAlive);clearInterval(peerTimer);
client.disconnect(index);peer.disconnect(peerIndex);
process.exit(0);
