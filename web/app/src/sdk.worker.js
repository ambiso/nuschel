// SPDX-License-Identifier: EUPL-1.2
// The SDK/session/storage owner. Complex parsing stays in the content Worker.
import init, { WebClient } from "./sdk/nuschl_client_wasm.js";
import { openHistory } from "./history.js";
import {UI_METHODS,projectedAction} from "./action-context.js";
import { StatePublication, stateFrame } from "./state-publication.js";
import { createContentPublication } from "./content-publication.js";
import { restoreOrder } from "./server-order.js";
let client;
let history;
let saved = null;
let storageQueue = Promise.resolve();
let started = false;
let orderQueue = Promise.resolve();
let orderCommitActive = false;
function fail(reason) { postMessage({ kind: "error", message: String(reason?.message || reason) }); }
const states=new StatePublication({
  collect:(id,updates)=>stateFrame(id,{servers:client.servers(),timeline:client.timeline(),voice:client.voice()},updates),
  post:packet=>postMessage(packet),
  fatal:message=>{try{postMessage({kind:"fatal",message:String(message).slice(0,4096)});}finally{close();}},
});
const publication=createContentPublication(refresh);
const wake=publication.wake;
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
function drainContentWork() {
  if(!client)return;
  const operations=JSON.parse(client.content_requests());
  if(operations.length)postMessage({kind:"content_work",operations});
}
function refresh() {
  if (!client || states.closed) return;
  const updates = client.poll();
  // Codec credit must not wait behind a large UI snapshot/layout task.
  drainContentWork();
  states.changed(updates);
  if(states.closed)return;
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
const methods = new Set(UI_METHODS);
onmessage = async ({ data }) => {
  try {
    if(states.closed)return;
    if (!data || typeof data !== "object") throw new Error("Invalid browser command.");
    if(data.method === "state_applied") {
      if(data.id!==undefined||!Array.isArray(data.args)||data.args.length!==1){states.fail("Invalid browser state acknowledgment.");return;}
      states.ack(data.args[0]);return;
    }
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
      const call=projectedAction(data.method,data.args);
      if(call){
        const projection=JSON.stringify(call);
        if(projection.length>512||new TextEncoder().encode(projection).length>512||typeof data.context!=="string"||data.context.length>4096||new TextEncoder().encode(data.context).length>4096)throw new Error("This view changed. Your draft was kept; use the updated view and try again.");
        client.validate_action(projection,data.context);
      }else if(data.context!==undefined)throw new Error("Unexpected browser action context.");
      let value;
      if (data.method === "content_complete") {
        if(typeof data.args[0]!=="string"||data.args[0].length>20||typeof data.args[1]!=="string"||data.args[1].length>2*1024*1024||new TextEncoder().encode(data.args[1]).length>2*1024*1024)throw new Error("Invalid content reply.");
        value=client.content_complete(...data.args);
      } else if (data.method === "image_begin" || data.method === "image_send") {
        const raw=JSON.stringify(data.args[0]);
        if(raw.length>2*1024*1024)throw new Error("Invalid image command.");
        value=JSON.parse(client[data.method](raw));
      } else if(data.method === "add_server" || data.method === "edit_server") {
        const index=client[data.method](...data.args);
        value={index,reference:JSON.parse(client.server_reference(index))};
      } else if (data.method === "set_server_order") {
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
      if(data.method === "content_complete" || data.method === "content_request") {
        // The real ACK above releases its exact credit immediately. Refill the
        // bounded codec lane before coalescing only the model publication.
        drainContentWork();publication.content();return;
      }
    }
    wake();
  } catch (reason) {
    // Muting/reconnecting routinely races already queued codec work. Drop these
    // revoked internal packets without turning them into user-facing toasts.
    if (data?.id === undefined && (data?.method === "voice_packet" || data?.method === "audio_ready")) return;
    postMessage({ kind: "result", id: data?.id, error: String(reason?.message || reason) });
    wake(); // errors/navigation never wait behind derivative publication
  }
};
