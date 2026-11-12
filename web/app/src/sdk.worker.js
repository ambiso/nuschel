// SPDX-License-Identifier: EUPL-1.2
// The only WASM/session/parser/storage owner. No DOM or HTML is used here.
import init, { WebClient } from "./sdk/nuschl_client_wasm.js";
import { openHistory } from "./history.js";
import { restoreOrder } from "./server-order.js";
let client;
let history;
let saved = null;
let storageQueue = Promise.resolve();
let queued = false;
let started = false;
let orderQueue = Promise.resolve();
let orderCommitActive = false;
function fail(reason) { postMessage({ kind: "error", message: String(reason?.message || reason) }); }
function wake() {
  if (queued) return;
  queued = true;
  queueMicrotask(() => { queued = false; refresh(); });
}
let audioQueued = false;
function wakeAudio() {
  if (audioQueued) return;
  audioQueued = true;
  queueMicrotask(() => { audioQueued = false; drainVoice(); });
}
function drainVoice() {
  if (!client) return;
  for (const packet of JSON.parse(client.voice_packets())) {
    // A new owned allocation, never a view into the TLS WASM heap.
    packet.opus = Uint8Array.from(packet.opus);
    postMessage({kind:"voice_packet",packet},[packet.opus.buffer]);
  }
}
function refresh() {
  if (!client) return;
  const updates = JSON.parse(client.poll());
  postMessage({ kind: "state", servers: JSON.parse(client.servers()), timeline: JSON.parse(client.timeline()), voice: JSON.parse(client.voice()), updates });
  drainVoice();
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
  if (history && !orderCommitActive && bookmarks !== saved) {
    saved = bookmarks;
    storageQueue = storageQueue.then(() => history.bookmarks(bookmarks)).catch((error) => { saved = null; fail(error); });
  }
}
async function persistServerOrder(ids) {
  const action = orderQueue.then(async () => {
    if (!history) throw new Error("Server order cannot be saved while browser storage is unavailable.");
    if (!Array.isArray(ids) || ids.length > 256 || ids.some(id => typeof id !== "string" || id.length > 2048)) throw new Error("Invalid server order.");
    const prior = JSON.parse(client.bookmarks()).map(server => server.id);
    const changed = client.set_server_order(JSON.stringify(ids));
    if (!changed) return false;
    orderCommitActive = true;
    const bookmarks = client.bookmarks();
    saved = bookmarks; // suppress duplicate refresh writes while this commit waits
    const write = storageQueue.then(() => history.bookmarks(bookmarks));
    storageQueue = write.catch(() => {}); // recover the queue; report this failure below
    try { await write; return true; }
    catch {
      const current = JSON.parse(client.bookmarks()).map(server => server.id);
      client.set_server_order(JSON.stringify(restoreOrder(prior, current)));
      saved = null;
      throw new Error("Server order could not be saved. Your previous order was restored.");
    } finally {
      // Network/cache and explicit bookmark changes during the transaction
      // are saved from the current catalog afterwards, never a stale snapshot.
      orderCommitActive = false; wake();
    }
  });
  orderQueue = action.catch(() => {});
  return action;
}
const methods = new Set(["add_server", "connect", "disconnect", "accept_certificate", "open", "browse_server", "open_direct", "send_text", "older", "join", "toggle_mute", "toggle_deaf", "audio_ready", "voice_packet", "remove_server", "dismiss_notice", "dismiss_welcome", "connect_with_password", "stop_audio", "edit_server", "set_server_order"]);
onmessage = async ({ data }) => {
  try {
    if (!data || typeof data !== "object") throw new Error("Invalid browser command.");
    if (data.method === "init") {
      if (started) throw new Error("The browser client is already initialized.");
      started = true;
      await init();
      client = new WebClient(data.relay);
      client.on_change(wake);
      client.on_audio(wakeAudio);
      try { history = await openHistory(); client.restore_bookmarks(await history.bookmarks()); saved = client.bookmarks(); }
      catch { fail("Local history is unavailable. Messages will be shown but cannot be saved."); }
      setInterval(() => { if (client.tick(performance.now())) wake(); }, 250);
      postMessage({ kind: "ready", capabilities:["remove_server","dismiss_notice", "dismiss_welcome","connect_with_password","voice","stop_audio", "edit_server", "server_order"] });
    } else {
      if (!client || !methods.has(data.method) || !Array.isArray(data.args) || data.args.length > 6) throw new Error("Unsupported browser command.");
      let value;
      if (data.method === "set_server_order") {
        value = await persistServerOrder(data.args[0]);
      } else if (data.method === "audio_ready") {
        value = client.audio_ready(JSON.stringify(data.args[0]));
      } else if (data.method === "voice_packet") {
        const packet = data.args[0];
        if (!packet || !(packet.opus instanceof Uint8Array) || packet.opus.length > 960) throw new Error("Invalid voice packet.");
        const {opus,...context} = packet;
        value = client.voice_packet(JSON.stringify(context),opus);
      } else value = client[data.method](...data.args);
      if (data.id !== undefined) postMessage({ kind: "result", id: data.id, value });
      // Encoding is frequent; its route has already been admitted in Rust.
      if (data.method === "voice_packet") return;
    }
    wake();
  } catch (reason) {
    // Muting/reconnecting routinely races already queued codec work. Drop these
    // revoked internal packets without turning them into user-facing toasts.
    if (data?.id === undefined && (data?.method === "voice_packet" || data?.method === "audio_ready")) return;
    postMessage({ kind: "result", id: data?.id, error: String(reason?.message || reason) });
  }
};
