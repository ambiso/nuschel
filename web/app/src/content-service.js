// SPDX-License-Identifier: EUPL-1.2
import {MAX_SOURCE, MAX_IMAGE_INPUT, textBytes, validateReply} from "./content-protocol.js";
const MAX_JOBS = 8, MAX_QUEUED_BYTES = 16 * 1024 * 1024;
const reason = message => new Error(message);

// Keep this adapter outside the TLS worker. It owns a finite queue and only
// local job IDs cross into the codec worker. SDK source/conversation tickets
// remain with their owner and must be checked again when each Promise resolves.
export class ContentService {
  constructor({workerFactory, timeoutMs=8000, startupMs=15000}={}) {
    this.workerFactory = workerFactory || (() => new Worker(new URL("./content-worker.js", import.meta.url), {type:"module", name:"nuschl-content-codecs"}));
    this.timeoutMs = Math.min(8000, Math.max(1, timeoutMs));
    this.startupMs = Math.min(15000, Math.max(1, startupMs));
    this.queue = []; this.active = null; this.bytes = 0; this.next = 0;
    this.worker = null; this.ready = false; this.disposed = false;
  }
  render({source, mode="plain"}, {signal}={}) {
    try {
      const size = textBytes(source, MAX_SOURCE).length;
      if (!["plain", "legacy", "marked"].includes(mode)) throw reason("Invalid content mode.");
      return this.enqueue({operation:"render", source, mode}, size, signal);
    } catch (error) { return Promise.reject(error); }
  }
  encodeImage(bytes, limit, {signal}={}) { return this.image("encode", bytes, limit, signal); }
  decodeImage(bytes, {signal}={}) { return this.image("decode", bytes, undefined, signal); }
  image(operation, bytes, limit, signal) {
    if (!(bytes instanceof Uint8Array) || bytes.length < 1 || bytes.length > MAX_IMAGE_INPUT) return Promise.reject(reason("Image input exceeds the content limit."));
    if (operation === "encode" && (!Number.isSafeInteger(limit) || limit < 64 || limit > MAX_SOURCE)) return Promise.reject(reason("Invalid image message limit."));
    // Copy only after queue admission; never detach the caller's attachment.
    return this.enqueue({operation, bytes, limit}, bytes.length, signal);
  }
  enqueue(packet, size, signal) {
    if (this.disposed) return Promise.reject(reason("Content processing is closed."));
    if (signal?.aborted) return Promise.reject(reason("Content processing was cancelled."));
    if (this.queue.length + Number(Boolean(this.active)) >= MAX_JOBS || size > MAX_QUEUED_BYTES - this.bytes) return Promise.reject(reason("Content processing is busy; retry this message."));
    if (!Number.isSafeInteger(this.next + 1)) return Promise.reject(reason("Content job identifiers are exhausted."));
    return new Promise((resolve, reject) => {
      const job = {id:++this.next, packet:{...packet}, size, resolve, reject, signal, deadline:performance.now() + this.timeoutMs};
      if (packet.bytes) job.packet.bytes = packet.bytes.slice();
      job.abort = () => this.expire(job, "Content processing was cancelled.");
      job.timer = setTimeout(() => this.expire(job, "Content processing exceeded its deadline."), this.timeoutMs);
      signal?.addEventListener("abort", job.abort, {once:true});
      this.bytes += size; this.queue.push(job); this.dispatch();
    });
  }
  start() {
    try {
      const worker = this.workerFactory(); this.worker = worker; this.ready = false;
      worker.onmessage = ({data}) => { if (this.worker === worker) this.message(data); };
      worker.onerror = () => { if (this.worker === worker) this.rejectAll("The content worker failed; retry the message."); };
      worker.onmessageerror = worker.onerror;
      this.startTimer = setTimeout(() => { if (this.worker === worker) this.rejectAll("The content worker could not start."); }, this.startupMs);
    } catch { this.rejectAll("The content worker could not start."); }
  }
  dispatch() {
    if (this.disposed || this.active || !this.queue.length) return;
    if (!this.worker) this.start();
    if (!this.ready) return;
    const job = this.queue.shift(); this.active = job;
    if (performance.now() >= job.deadline) { this.expire(job, "Content processing exceeded its deadline."); return; }
    try {
      const transfer = job.packet.bytes ? [job.packet.bytes.buffer] : [];
      this.worker.postMessage({id:job.id, ...job.packet}, transfer);
    } catch { this.rejectAll("The content worker could not accept this job."); }
  }
  message(data) {
    if (data?.kind === "ready" && !this.ready && !this.active) {
      clearTimeout(this.startTimer); this.ready = true; this.dispatch(); return;
    }
    const job = this.active;
    if (!job || data?.id !== job.id || !["result", "error"].includes(data.kind)) {
      this.rejectAll("The content worker returned an invalid response."); return;
    }
    if (performance.now() >= job.deadline) { this.expire(job, "Content processing exceeded its deadline."); return; }
    if (data.kind === "error") {
      this.settle(job, reason(typeof data.message === "string" ? data.message.slice(0, 200) : "Content could not be processed."));
    } else {
      let reply;
      try { reply = validateReply(job.packet.operation, data.result, job.packet.limit); }
      catch { this.rejectAll("The content worker returned invalid or oversized content."); return; }
      this.settle(job, null, reply);
    }
    this.dispatch();
  }
  settle(job, error, value) {
    if (job.done) return;
    job.done = true; clearTimeout(job.timer); job.signal?.removeEventListener("abort", job.abort);
    this.bytes -= job.size;
    if (this.active === job) this.active = null;
    const index = this.queue.indexOf(job); if (index >= 0) this.queue.splice(index, 1);
    if (error) job.reject(error); else job.resolve(value);
  }
  stopWorker() {
    clearTimeout(this.startTimer); this.ready = false;
    const worker = this.worker; this.worker = null; worker?.terminate();
  }
  expire(job, message) {
    if (job.done) return;
    if (this.active === job) this.stopWorker();
    this.settle(job, reason(message));
    // Cancellation during startup must not leave a now-unneeded decoder
    // compiling/loading after its last consumer has gone away.
    if (!this.active && !this.queue.length) this.stopWorker();
    this.dispatch();
  }
  rejectAll(message) {
    this.stopWorker();
    for (const job of [this.active, ...this.queue].filter(Boolean)) this.settle(job, reason(message));
  }
  cancelAll() { this.rejectAll("Content processing was cancelled."); }
  dispose() { this.disposed = true; this.cancelAll(); }
}
