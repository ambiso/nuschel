// SPDX-License-Identifier: EUPL-1.2
import { test } from "node:test";
import assert from "node:assert/strict";
import { AudioRecordParser, validateAudioHeaders } from "./browser-audio.js";

function audioRecord(sample = (i) => i % 2 === 0 ? 1.25 : -2.5) {
  const record = new Uint8Array(1 + 960 * 2 * 4);
  record[0] = 1;
  const view = new DataView(record.buffer);
  for (let i = 0; i < 960 * 2; i++) view.setFloat32(1 + i * 4, sample(i), true);
  return record;
}

function validHeaders() {
  return new Headers({
    "Content-Type": "application/octet-stream",
    "X-Audio-Format": "f32le",
    "X-Audio-Rate": "48000",
    "X-Audio-Channels": "2",
    "X-Audio-Frame-Samples": "960",
  });
}

test("browser audio validates every PCM format header before accepting a stream", () => {
  assert.doesNotThrow(() => validateAudioHeaders(validHeaders()));
  for (const [key, value] of [
    ["Content-Type", "audio/opus"], ["X-Audio-Format", "s16le"],
    ["X-Audio-Rate", "44100"], ["X-Audio-Channels", "1"], ["X-Audio-Frame-Samples", "480"],
  ]) {
    const headers = validHeaders();
    headers.set(key, value);
    assert.throws(() => validateAudioHeaders(headers), /unsupported format/);
    headers.delete(key);
    assert.throws(() => validateAudioHeaders(headers), /unsupported format/);
  }
});

test("browser records survive every chunk boundary and preserve finite stereo PCM", () => {
  const first = audioRecord();
  const second = audioRecord((i) => i === 0 ? -0 : i === 1 ? 0.125 : -0.75);
  const wire = new Uint8Array(1 + first.length + 1 + second.length);
  wire[0] = 0;
  wire.set(first, 1);
  wire[1 + first.length] = 0;
  wire.set(second, first.length + 2);
  for (const chunkSize of [1, 2, 3, 127, 960, 7680, wire.length]) {
    const received = [];
    const parser = new AudioRecordParser(() => received.push("reset"), (frame) => received.push(frame));
    parser.push(new Uint8Array());
    for (let offset = 0; offset < wire.length; offset += chunkSize) parser.push(wire.subarray(offset, offset + chunkSize));
    parser.finish();
    assert.equal(received.length, 4);
    assert.equal(received[0], "reset");
    assert.equal(received[2], "reset");
    assert.equal(received[1].length, 1920);
    for (let i = 0; i < received[1].length; i++) assert.equal(received[1][i], i % 2 === 0 ? 1.25 : -2.5);
    assert.ok(Object.is(received[3][0], -0));
    assert.equal(received[3][1], 0.125);
    assert.equal(received[3][1919], -0.75);
  }
});

test("browser audio rejects missing resets, invalid records, truncated frames and non-finite PCM", () => {
  const parser = () => new AudioRecordParser(() => {}, () => {});
  assert.throws(() => parser().push(audioRecord()), /did not start with a reset/);
  assert.throws(() => parser().push(new Uint8Array([0, 2])), /invalid record/);
  assert.throws(() => parser().finish(), /truncated record/);
  const partial = parser();
  partial.push(new Uint8Array([0]));
  partial.push(audioRecord().subarray(0, 7680));
  assert.throws(() => partial.finish(), /truncated record/);
  for (const sample of [NaN, Infinity, -Infinity]) {
    const malformed = parser();
    malformed.push(new Uint8Array([0]));
    assert.throws(() => malformed.push(audioRecord((i) => i === 1919 ? sample : 0)), /non-finite sample/);
  }
});
