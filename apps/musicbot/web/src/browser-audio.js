// SPDX-License-Identifier: EUPL-1.2
import { HttpError, query } from "./api.js";

const FRAME_SAMPLES = 960;
const CHANNELS = 2;
const FRAME_BYTES = FRAME_SAMPLES * CHANNELS * 4;
const MAX_PENDING_FRAMES = 10;
const LITTLE_ENDIAN = new Uint8Array(new Uint32Array([1]).buffer)[0] === 1;

/** Reject incompatible streams before interpreting any bytes as PCM. */
export function validateAudioHeaders(headers) {
  const contentType = headers.get("Content-Type")?.split(";", 1)[0].trim().toLowerCase();
  if (contentType !== "application/octet-stream"
    || headers.get("X-Audio-Format") !== "f32le"
    || headers.get("X-Audio-Rate") !== "48000"
    || headers.get("X-Audio-Channels") !== "2"
    || headers.get("X-Audio-Frame-Samples") !== "960") {
    throw new Error("The browser audio stream has an unsupported format.");
  }
}

/** A single fixed-size partial frame, never a growing transport remainder. */
export class AudioRecordParser {
  #reset;
  #frame;
  #started = false;
  #payload = null;
  #filled = 0;

  constructor(reset, frame) {
    this.#reset = reset;
    this.#frame = frame;
  }

  push(bytes) {
    let offset = 0;
    while (offset < bytes.length) {
      if (this.#payload === null) {
        const type = bytes[offset++];
        if (!this.#started && type !== 0) throw new Error("The browser audio stream did not start with a reset.");
        this.#started = true;
        if (type === 0) {
          this.#reset();
          continue;
        }
        // Keep-alive while the queue has nothing to play; it carries no audio.
        if (type === 2) continue;
        if (type !== 1) throw new Error(`The browser audio stream contains an invalid record (${type}).`);
        this.#payload = new Uint8Array(FRAME_BYTES);
        this.#filled = 0;
      }
      const count = Math.min(FRAME_BYTES - this.#filled, bytes.length - offset);
      this.#payload.set(bytes.subarray(offset, offset + count), this.#filled);
      this.#filled += count;
      offset += count;
      if (this.#filled !== FRAME_BYTES) continue;
      const view = new DataView(this.#payload.buffer);
      const samples = new Float32Array(this.#payload.buffer);
      for (let i = 0; i < samples.length; i++) {
        const sample = view.getFloat32(i * 4, true);
        if (!Number.isFinite(sample)) throw new Error("The browser audio stream contains a non-finite sample.");
        // The server clamps to full scale; anything louder is not this protocol.
        if (sample < -1 || sample > 1) throw new Error("The browser audio stream contains an out-of-range sample.");
        if (!LITTLE_ENDIAN) samples[i] = sample;
      }
      this.#payload = null;
      this.#filled = 0;
      this.#frame(samples);
    }
  }

  finish() {
    if (!this.#started || this.#payload !== null) throw new Error("The browser audio stream ended with a truncated record.");
  }
}

/** Owns one user-started audio graph and at most one selected-queue fetch. */
export class BrowserAudio {
  #onState;
  #onError;
  #wanted = false;
  #ready = false;
  #resuming = false;
  #session = 0;
  #streamEpoch = 0;
  #audioEpoch = 0;
  #player = null;
  #volume = 50;
  #context = null;
  #node = null;
  #gain = null;
  #controller = null;
  #reader = null;
  #namespace = null;
  #frames = [];
  #resetPending = false;
  #messageId = 0;
  #inFlight = null;
  #namespaceAborted = () => this.stop(new Error("Browser listening stopped because the connection or library changed. Press Listen to start again."));
  #contextChanged = () => {
    const state = this.#context?.state;
    if (!this.#wanted || state === "running" || (this.#resuming && state === "suspended")) return;
    this.stop(new Error(`Browser audio ${state === "closed" ? "closed" : "was suspended or interrupted"}. Press Listen to start again.`));
  };

  constructor(onState, onError) {
    this.#onState = onState;
    this.#onError = onError;
  }

  setVolume(volume) {
    this.#volume = Math.max(0, Math.min(100, volume));
    if (this.#gain) this.#gain.gain.value = (this.#volume / 100) ** 2;
  }

  select(player) {
    if (player === this.#player) return;
    this.#player = player;
    if (this.#wanted && this.#ready) this.#replaceStream();
  }

  // Call directly from the Listen click: context creation and resume happen
  // before the first await, while the browser still has the user gesture.
  async start(player, namespace) {
    if (this.#wanted) return;
    if (!globalThis.isSecureContext) {
      this.#onError(new Error("Browser listening requires HTTPS or localhost."));
      return;
    }
    if (typeof globalThis.AudioContext !== "function" || typeof globalThis.AudioWorkletNode !== "function" || typeof globalThis.fetch !== "function") {
      this.#onError(new Error("This browser does not support AudioContext, AudioWorklet, and streaming fetch required for listening."));
      return;
    }
    if (!namespace || namespace.aborted) return;
    this.#wanted = true;
    this.#player = player;
    this.#namespace = namespace;
    namespace.addEventListener("abort", this.#namespaceAborted, { once: true });
    const session = ++this.#session;
    this.#onState("starting");
    try {
      const context = new AudioContext({ sampleRate: 48000, latencyHint: "interactive" });
      this.#context = context;
      this.#resuming = true;
      context.addEventListener("statechange", this.#contextChanged);
      if (!context.audioWorklet || context.sampleRate !== 48000) {
        throw new Error("This browser cannot create the required 48 kHz AudioWorklet output.");
      }
      await Promise.all([
        context.resume().then(() => {
          if (!this.#wanted || session !== this.#session) return;
          this.#resuming = false;
          this.#contextChanged();
        }),
        context.audioWorklet.addModule(new URL("./browser-audio-worklet.js?no-inline", import.meta.url).href),
      ]);
      if (!this.#wanted || session !== this.#session) return;
      if (context.state !== "running") throw new Error("Browser audio could not start. Allow audio playback and press Listen again.");
      const node = new AudioWorkletNode(context, "nuschl-browser-audio", {
        numberOfInputs: 0,
        numberOfOutputs: 1,
        outputChannelCount: [2],
        channelCount: 2,
        channelCountMode: "explicit",
      });
      this.#node = node;
      node.onprocessorerror = () => this.stop(new Error("The browser audio processor failed. Press Listen to start again."));
      node.port.onmessage = (event) => this.#receive(event.data);
      this.#gain = context.createGain();
      this.setVolume(this.#volume);
      node.connect(this.#gain);
      this.#gain.connect(context.destination);
      this.#ready = true;
      this.#replaceStream();
    } catch (error) {
      if (this.#wanted && session === this.#session) this.stop(error);
    }
  }

  stop(error = null) {
    const wasWanted = this.#wanted;
    this.#wanted = false;
    this.#ready = false;
    this.#resuming = false;
    this.#session++;
    this.#cancelStream();
    this.#namespace?.removeEventListener("abort", this.#namespaceAborted);
    this.#namespace = null;
    this.#frames.length = 0;
    this.#resetPending = false;
    this.#inFlight = null;
    if (this.#node) {
      this.#node.onprocessorerror = null;
      this.#node.port.onmessage = null;
      this.#node.port.close();
      this.#node.disconnect();
      this.#node = null;
    }
    this.#gain?.disconnect();
    this.#gain = null;
    const context = this.#context;
    this.#context = null;
    if (context) {
      context.removeEventListener("statechange", this.#contextChanged);
      if (context.state !== "closed") context.close().catch(() => {});
    }
    this.#onState("off");
    if (wasWanted && error) this.#onError(error);
  }

  #cancelStream() {
    this.#streamEpoch++;
    this.#controller?.abort();
    this.#controller = null;
    this.#reader?.cancel().catch(() => {});
    this.#reader = null;
  }

  #replaceStream() {
    this.#cancelStream();
    this.#reset();
    const controller = new AbortController();
    this.#controller = controller;
    this.#readStream(controller, this.#streamEpoch, this.#player);
  }

  async #readStream(controller, epoch, player) {
    const active = () => this.#wanted && this.#controller === controller && this.#streamEpoch === epoch && !controller.signal.aborted;
    let response = null;
    let reader = null;
    try {
      response = await fetch(query("api/listen", { player }), {
        signal: controller.signal,
        cache: "no-store",
        headers: { Accept: "application/octet-stream" },
      });
      if (!active()) return;
      if (!response.ok) {
        const detail = response.status === 404 ? "This listening link is no longer valid."
          : response.status === 503 ? "All browser listening slots are in use."
          : response.status === 400 ? "The selected listening queue is not available."
          : `${response.status} ${response.statusText}`.trim();
        throw new HttpError(response.status, `Browser listening failed: ${detail}`);
      }
      validateAudioHeaders(response.headers);
      if (!response.body || typeof response.body.getReader !== "function") throw new Error("This browser did not provide a streaming audio response.");
      reader = response.body.getReader();
      this.#reader = reader;
      const parser = new AudioRecordParser(
        () => { if (active()) this.#reset(); },
        (frame) => { if (active()) this.#queue(frame); },
      );
      while (active()) {
        const { done, value } = await reader.read();
        if (!active()) break;
        if (done) {
          parser.finish();
          throw new Error("The browser audio stream ended. Press Listen to start again.");
        }
        parser.push(value);
      }
    } catch (error) {
      if (active()) this.stop(error);
    } finally {
      if (reader) {
        await reader.cancel().catch(() => {});
        reader.releaseLock();
        if (this.#reader === reader) this.#reader = null;
      } else if (response?.body && !response.body.locked) {
        await response.body.cancel().catch(() => {});
      }
    }
  }

  #reset() {
    this.#audioEpoch++;
    this.#frames.length = 0;
    this.#resetPending = true;
    this.#onState("waiting");
    this.#flush();
  }

  #queue(frame) {
    if (this.#frames.length === MAX_PENDING_FRAMES) this.#frames.shift();
    this.#frames.push(frame);
    this.#flush();
  }

  // One acknowledged batch at a time bounds the MessagePort mailbox as well
  // as the main-thread queue. Reset supersedes any unsent pre-reset frames.
  #flush() {
    if (!this.#node || this.#inFlight !== null || (!this.#resetPending && this.#frames.length === 0)) return;
    const id = ++this.#messageId;
    const frames = this.#frames;
    this.#frames = [];
    const reset = this.#resetPending;
    this.#resetPending = false;
    this.#inFlight = id;
    this.#node.port.postMessage({ id, epoch: this.#audioEpoch, reset, frames }, frames.map((frame) => frame.buffer));
  }

  #receive(message) {
    if (!this.#wanted) return;
    if (message.type === "ack" && message.id === this.#inFlight) {
      this.#inFlight = null;
      this.#flush();
    } else if (message.type === "state" && message.epoch === this.#audioEpoch) {
      this.#onState(message.playing ? "playing" : "waiting");
    }
  }
}
