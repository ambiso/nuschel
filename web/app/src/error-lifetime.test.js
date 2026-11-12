// SPDX-License-Identifier: EUPL-1.2
import assert from "node:assert/strict";
import {ErrorLifetime} from "./error-lifetime.js";

const errors = new ErrorLifetime();
let visible = "";
const show = (message, conversation = null) => {
  errors.replace(conversation); visible = message;
};
const sent = async (conversation, finish) => {
  const old = errors.capture(conversation);
  await finish;
  if (errors.accepts(old)) show("");
};
show("128-character limit", "server-a/channel-a");
await sent("server-b/channel-a");
assert.equal(visible, "128-character limit", "another conversation cannot dismiss this send error");
await sent("server-a/channel-a");
assert.equal(visible, "");
for (const newerConversation of [null, "server-a/channel-a"]) {
  show("128-character limit", "server-a/channel-a");
  let resolve;
  const completion = new Promise(done => {resolve = done;});
  const pending = sent("server-a/channel-a", completion);
  show("128-character limit", newerConversation);
  resolve(); await pending;
  assert.equal(visible, "128-character limit", "a delayed successful send dismissed a newer identical error");
}
show("128-character limit", "server-a/channel-a");
let finish;
const pending = sent("server-a/channel-a", new Promise(done => {finish = done;}));
show("Unrelated worker failed");
finish(); await pending;
assert.equal(visible, "Unrelated worker failed");
show("");
assert.equal(errors.capture("server-a/channel-a"), null);
assert.equal(errors.accepts(null), false);
console.log("App error lifetime interleavings passed");
