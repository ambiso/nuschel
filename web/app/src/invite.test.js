// SPDX-License-Identifier: EUPL-1.2
import assert from "node:assert/strict";
import { test } from "node:test";
import { inviteFragment } from "./invite.js";

test("a legacy invite preserves its certificate pin", () => {
  const invite = inviteFragment(`#s=voice.example.org:64739&fp=sha256:${"AB".repeat(32)}&u=Ada`);
  assert.equal(invite.error, "");
  assert.equal(invite.host, "voice.example.org");
  assert.equal(invite.port, 64739);
  assert.equal(invite.pin, "ab".repeat(32));
  assert.equal(invite.username, "Ada");
});

test("a required protocol never becomes a legacy connection", () => {
  for (const policy of ["nuschl", "future-policy", "nuschl+future-policy"]) {
    assert.match(inviteFragment(`#s=voice.example.org&policy=${policy}`).error, /requires the Nuschl protocol/);
  }
});

test("malformed pins and oversized invites never become unpinned connections", () => {
  assert.match(inviteFragment("#s=voice.example.org&fp=sha256:bad").error, /invalid certificate pin/);
  assert.match(inviteFragment(`#s=${"a".repeat(8192)}`).error, /too long/);
  assert.match(inviteFragment("#s=voice.example.org&policy=&policy=nuschl").error, /repeats/);
});
