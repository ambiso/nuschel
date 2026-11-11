// SPDX-License-Identifier: EUPL-1.2
// The only WASM/session/parser/storage owner. No DOM or HTML is used here.
import init, { WebClient } from "./sdk/nuschl_client_wasm.js";
import { openHistory } from "./history.js";
let client;
let history;
let saved = null;
let storageQueue = Promise.resolve();
let queued = false;
let started = false;
function fail(reason) { postMessage({ kind: "error", message: String(reason?.message || reason) }); }
function wake() {
  if (queued) return;
  queued = true;
  queueMicrotask(() => { queued = false; refresh(); });
}
function refresh() {
  if (!client) return;
  const updates = JSON.parse(client.poll());
  postMessage({ kind: "state", servers: JSON.parse(client.servers()), timeline: JSON.parse(client.timeline()), updates });
  for (const operation of JSON.parse(client.storage_requests())) {
    // Pages cannot overtake preceding writes. Rust retains opaque navigation
    // tickets and drops replies for old selections/connections/certificates.
    storageQueue = storageQueue.then(async () => {
      let response;
      try {
        if (!history) throw new Error("Browser storage is unavailable.");
        response = await history.execute(operation);
      } catch { response = { error: "Browser storage is unavailable or full." }; }
      client.storage_complete(operation.ticket, JSON.stringify(response));
      wake();
    }).catch(fail);
  }
  const bookmarks = client.bookmarks();
  if (history && bookmarks !== saved) {
    saved = bookmarks;
    storageQueue = storageQueue.then(() => history.bookmarks(bookmarks)).catch((error) => { saved = null; fail(error); });
  }
}
const methods = new Set(["add_server", "connect", "disconnect", "accept_certificate", "open", "browse_server", "open_direct", "send_text", "older", "join"]);
onmessage = async ({ data }) => {
  try {
    if (!data || typeof data !== "object") throw new Error("Invalid browser command.");
    if (data.method === "init") {
      if (started) throw new Error("The browser client is already initialized.");
      started = true;
      await init();
      client = new WebClient(data.relay);
      client.on_change(wake);
      try { history = await openHistory(); client.restore_bookmarks(await history.bookmarks()); saved = client.bookmarks(); }
      catch { fail("Local history is unavailable. Messages will be shown but cannot be saved."); }
      setInterval(() => { client.tick(performance.now()); wake(); }, 5000);
      postMessage({ kind: "ready" });
    } else {
      if (!client || !methods.has(data.method) || !Array.isArray(data.args) || data.args.length > 5) throw new Error("Unsupported browser command.");
      const value = client[data.method](...data.args);
      postMessage({ kind: "result", id: data.id, value });
    }
    wake();
  } catch (reason) {
    postMessage({ kind: "result", id: data?.id, error: String(reason?.message || reason) });
  }
};
