// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import {ContentService} from "../web/app/src/content-service.js";
import {MAX_IMAGE_INPUT, validateDocument, validateReply} from "../web/app/src/content-protocol.js";

const rendered = text => JSON.stringify({marked_text:text, plain_text:text, document:{blocks:[{kind:"text", text}]}});
class Worker {
  sent = []; terminated = false;
  postMessage(message) { this.sent.push(message); }
  terminate() { this.terminated = true; }
  emit(data) { this.onmessage?.({data}); }
  ready() { this.emit({kind:"ready"}); }
  finish(text="done") { this.emit({kind:"result", id:this.sent.at(-1).id, result:rendered(text)}); }
}
function fixture(options={}) {
  const workers = [];
  const service = new ContentService({...options, workerFactory:() => { const worker = new Worker(); workers.push(worker); return worker; }});
  return {service, workers};
}
const outcome = promise => promise.then(value => ({value}), error => ({error}));

test("eight queued jobs admit only one codec operation and reject further work", async () => {
  const {service, workers} = fixture();
  const jobs = Array.from({length:8}, (_, index) => outcome(service.render({source:`source ${index}`})));
  await assert.rejects(service.render({source:"overflow"}), /busy/);
  assert.equal(workers.length, 1); assert.equal(workers[0].sent.length, 0);
  workers[0].ready(); assert.equal(workers[0].sent.length, 1);
  for (let index=0; index<8; index++) {
    assert.equal(workers[0].sent.length, index + 1);
    workers[0].finish(`reply ${index}`);
    assert.equal((await jobs[index]).value.plain_text, `reply ${index}`);
  }
  assert.equal(service.bytes, 0); service.dispose();
});

test("a cancelled active job terminates its realm and old replies cannot settle a new job", async () => {
  const {service, workers} = fixture();
  const abort = new AbortController();
  const first = outcome(service.render({source:"old"}, {signal:abort.signal}));
  const next = outcome(service.render({source:"new"}));
  workers[0].ready(); const old = workers[0]; abort.abort();
  assert.match((await first).error.message, /cancelled/); assert.ok(old.terminated);
  assert.equal(workers.length, 2); workers[1].ready();
  old.finish("forged old result");
  assert.equal(service.active.packet.source, "new");
  workers[1].finish("current"); assert.equal((await next).value.plain_text, "current");
  service.dispose();
});

test("cancelling a waiting job preserves the active codec and sends no cancelled payload", async () => {
  const {service, workers} = fixture();
  const first = outcome(service.render({source:"active"}));
  const abort = new AbortController();
  const cancelled = outcome(service.render({source:"never dispatched"}, {signal:abort.signal}));
  workers[0].ready(); abort.abort();
  assert.match((await cancelled).error.message, /cancelled/); assert.ok(!workers[0].terminated);
  workers[0].finish(); await first;
  assert.equal(workers[0].sent.length, 1); service.dispose();
});

test("a stalled codec times out, frees credits and permits a fresh realm", async () => {
  const {service, workers} = fixture({timeoutMs:25});
  const first = outcome(service.render({source:"stall"})); workers[0].ready();
  assert.match((await first).error.message, /deadline/); assert.ok(workers[0].terminated);
  const next = outcome(service.render({source:"fresh"})); workers[1].ready();
  workers[1].finish("recovered"); assert.equal((await next).value.plain_text, "recovered");
  assert.equal(service.bytes, 0); service.dispose();
});

test("encoded input byte budget is independent of queue count and caller buffers stay intact", async () => {
  const {service, workers} = fixture(); const bytes = new Uint8Array(MAX_IMAGE_INPUT);
  const first = outcome(service.decodeImage(bytes)), second = outcome(service.decodeImage(bytes));
  await assert.rejects(service.decodeImage(new Uint8Array([1])), /busy/);
  assert.equal(bytes.length, MAX_IMAGE_INPUT); workers[0].ready();
  assert.notEqual(workers[0].sent[0].bytes.buffer, bytes.buffer);
  service.dispose(); assert.ok((await first).error); assert.ok((await second).error);
  assert.equal(service.bytes, 0);
});

test("a forged worker response fails closed for the queue but retry is possible", async () => {
  const {service, workers} = fixture();
  const first = outcome(service.render({source:"first"})), next = outcome(service.render({source:"next"}));
  workers[0].ready(); workers[0].emit({kind:"result", id:99999, result:rendered("wrong source")});
  assert.match((await first).error.message, /invalid response/); assert.ok((await next).error);
  assert.ok(workers[0].terminated);
  const retry = outcome(service.render({source:"retry"})); workers[1].ready(); workers[1].finish();
  assert.ok((await retry).value); service.dispose();
});

test("the receiving boundary rejects hidden resource URLs, invalid pixels and split UTF8 spans", () => {
  const pixel = {kind:"image", width:1, height:1, rgba:"/wAA/w=="};
  assert.ok(validateDocument({blocks:[pixel]}));
  for (const image of [{...pixel, source:"https://invalid.example/canary"}, {...pixel, width:4096}, {...pixel, rgba:"/wAA/w=A"}]) {
    assert.throws(() => validateDocument({blocks:[image]}));
  }
  assert.throws(() => validateDocument({blocks:[{kind:"text", text:"🦀", spans:[{start:1,end:4}]}]}));
  assert.throws(() => validateDocument({blocks:[{kind:"text", text:"link", spans:[{start:0,end:4,link:"javascript://alert(1)"}]}]}));
  assert.throws(() => validateReply("encode", JSON.stringify({html:'<img src="https://invalid.example/a" />', thumbnail:pixel}), 10000));
  assert.throws(() => validateReply("encode", JSON.stringify({html:'<img src="data:image/png;base64,AA\" onerror=\"bad" />', thumbnail:pixel}), 10000));
  const valid = validateReply("render", rendered("first\n\nsecond 🦀"));
  assert.equal(valid.plain_text, "first\n\nsecond 🦀");
});
