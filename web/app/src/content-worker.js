// SPDX-License-Identifier: EUPL-1.2
// No SDK/session/identity/device/storage API crosses this realm. The fixed
// loader fetches only its own same-origin WASM. Browser Workers are not an OS
// process sandbox; see docs/browser-content.md for this boundary's limits.
import init, {ContentProcessor} from "./content-codec/nuschl_content_wasm.js";
import {MAX_SOURCE, MAX_IMAGE_INPUT, textBytes} from "./content-protocol.js";
let processor;
try { await init(); processor = new ContentProcessor(); postMessage({kind:"ready"}); }
catch { postMessage({kind:"fatal", message:"The content codec could not start."}); }
onmessage = ({data}) => {
  if (!processor) return;
  const id = data?.id;
  if (!Number.isSafeInteger(id) || id < 1) return;
  try {
    let result;
    if (data.operation === "render") {
      textBytes(data.source, MAX_SOURCE);
      if (!["plain", "legacy", "marked"].includes(data.mode)) throw new Error("Invalid content mode.");
      result = processor.render_message(data.source, data.mode);
    } else {
      if (!(data.bytes instanceof Uint8Array) || !data.bytes.length || data.bytes.length > MAX_IMAGE_INPUT) throw new Error("Invalid image input.");
      if (data.operation === "encode") {
        if (!Number.isSafeInteger(data.limit) || data.limit < 64 || data.limit > MAX_SOURCE) throw new Error("Invalid image limit.");
        result = processor.encode_image(data.bytes, data.limit);
      } else if (data.operation === "decode") result = processor.decode_image(data.bytes);
      else throw new Error("Invalid content operation.");
    }
    postMessage({kind:"result", id, result});
  } catch (error) {
    if (error instanceof WebAssembly.RuntimeError) {
      // A trap (including exhausted linear memory) may leave Rust's execution
      // state unusable. Never run another message in this instance.
      postMessage({kind:"fatal", message:"The content codec trapped; retry this message."});
      close(); return;
    }
    postMessage({kind:"error", id, message:String(error?.message || error).slice(0, 200)});
  }
};
