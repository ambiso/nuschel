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
const bindings = await import(pathToFileURL(join(sdk, "nuschel_client_wasm.js")).href);
await bindings.default({ module_or_path: await readFile(join(sdk, "nuschel_client_wasm_bg.wasm")) });

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
client.tick(performance.now());
clearInterval(keepAlive);
client.disconnect(index);
process.exit(0);
