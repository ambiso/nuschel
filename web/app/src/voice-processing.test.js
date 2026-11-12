// SPDX-License-Identifier: EUPL-1.2
import assert from "node:assert/strict";
import {voiceProcessing} from "./voice-processing.js";

const UNKNOWN = "unknown (the browser did not report it)";
const texts = result => Object.fromEntries(result.rows.map(row => [row.field, row.text]));

// Nothing captured yet: BrowserAudio starts with an empty applied record.
for (const empty of [undefined, null, {}, {sampleRate:48000}, "on"]) {
  assert.deepEqual(voiceProcessing(empty), {reported:false, rows:[], hint:null});
}

// Chromium-style full report.
let result = voiceProcessing({echoCancellation:true, noiseSuppression:true, autoGainControl:false, sampleRate:48000});
assert.equal(result.reported, true);
assert.deepEqual(texts(result), {echoCancellation:"on", noiseSuppression:"on", autoGainControl:"off"});
assert.deepEqual(result.rows.map(row => row.label), ["Echo cancellation", "Noise suppression", "Automatic gain"]);
assert.equal(result.hint, null);

// WebKit reports only echoCancellation; BrowserAudio records the rest as "unknown".
result = voiceProcessing({echoCancellation:true, noiseSuppression:"unknown", autoGainControl:"unknown", sampleRate:48000});
assert.deepEqual(texts(result), {echoCancellation:"on", noiseSuppression:UNKNOWN, autoGainControl:UNKNOWN});
assert.equal(result.hint, null);

// Echo cancellation off or unknown always produces the headphones hint.
result = voiceProcessing({echoCancellation:false, noiseSuppression:false, autoGainControl:false});
assert.equal(texts(result).echoCancellation, "off");
assert.match(result.hint, /off.*headphones/i);
result = voiceProcessing({echoCancellation:"unknown", noiseSuppression:true, autoGainControl:true});
assert.equal(texts(result).echoCancellation, UNKNOWN);
assert.match(result.hint, /not confirmed.*Headphones/);

// Unrecognised values are unknown, never treated as on.
for (const value of [1, "true", "yes", {}, null, undefined, "remote"]) {
  result = voiceProcessing({echoCancellation:value, noiseSuppression:value, autoGainControl:value});
  assert.deepEqual(texts(result), {echoCancellation:UNKNOWN, noiseSuppression:UNKNOWN, autoGainControl:UNKNOWN});
  assert.ok(result.hint);
}

// Spec echoCancellation modes: "all" is on; "remote-only" does not cover
// Nuschl's playback, which is not a peer connection.
result = voiceProcessing({echoCancellation:"all", noiseSuppression:true, autoGainControl:true});
assert.equal(texts(result).echoCancellation, "on (all system audio)");
assert.equal(result.hint, null);
result = voiceProcessing({echoCancellation:"remote-only", noiseSuppression:true, autoGainControl:true});
assert.equal(result.rows[0].state, "partial");
assert.ok(result.hint);
// Mode strings apply only to echo cancellation.
assert.equal(texts(voiceProcessing({echoCancellation:true, noiseSuppression:"all", autoGainControl:"remote-only"})).noiseSuppression, UNKNOWN);
