// SPDX-License-Identifier: EUPL-1.2
import test from "node:test";
import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import { emojiIndex, emojiParts, findImages, imageKey, imageUrl, inTone, insertAt, loadEmojiIndex, searchEmoji, TONES } from "./emoji.js";

// The generated data (nix/emoji.nix): copied in by the Nix build, linked by
// `just emoji-assets` for local runs.
const path = new URL("../public/emoji/data.json", import.meta.url);
if (!existsSync(path)) throw new Error("public/emoji/data.json is missing: run `just emoji-assets`");
const index = emojiIndex(JSON.parse(readFileSync(path, "utf8")));
const images = (value) => findImages(index, value).map(({ start, end }) => value.slice(start, end));
const all = index.groups.flatMap((group) => group.emoji);
const named = (name) => all.find((emoji) => emoji.name === name);

test("the table has its groups, names and an image for every emoji in every tone", () => {
  assert.equal(index.groups.length, 9);
  assert.equal(index.groups[0].name, "Smileys & emotion");
  assert.equal(named("grinning face").emoji, "😀");
  for (const emoji of all) for (let tone = 0; tone <= TONES; tone++) {
    const shown = inTone(emoji, tone);
    assert.ok(imageKey(index, shown), `${shown} (${emoji.name})`);
  }
});

test("™ stays text; ™ with U+FE0F is drawn as Twemoji", () => {
  const textTm = "™", emojiTm = "™️";
  assert.deepEqual(images(textTm), []);
  assert.deepEqual(images(emojiTm), [emojiTm]);
  assert.deepEqual(emojiParts(index, `Nuschl${textTm} and ${emojiTm}!`), [
    { text: `Nuschl${textTm} and ` }, { text: emojiTm, key: "2122" }, { text: "!" },
  ]);
  assert.deepEqual(images("™︎"), []);
  for (const [plain, emoji] of [["©", "©️"], ["®", "®️"], ["❤", "❤️"], ["☝", "☝️"]]) {
    assert.deepEqual(images(plain), [], plain);
    assert.deepEqual(images(emoji), [emoji]);
  }
  // The picker inserts the emoji form.
  assert.equal(named("trade mark").emoji, emojiTm);
});

test("emoji, sequences, flags, keycaps and skin tones are images", () => {
  assert.deepEqual(images("a 😀 b"), ["😀"]);
  assert.deepEqual(images("😀︎"), []);
  assert.deepEqual(images("👍🏽👍"), ["👍🏽", "👍"]);
  assert.deepEqual(images("🇩🇪🇦🇹"), ["🇩🇪", "🇦🇹"]);
  assert.deepEqual(images("1️⃣ #️⃣"), ["1️⃣", "#️⃣"]);
  assert.deepEqual(images("❤️‍🔥 👩‍👩‍👧‍👦 🧑🏻‍🤝‍🧑🏿"), ["❤️‍🔥", "👩‍👩‍👧‍👦", "🧑🏻‍🤝‍🧑🏿"]);
  assert.deepEqual(images("Version 2024 # * → ✓ ü ß 中文"), []);
  assert.deepEqual(emojiParts(index, "plain"), [{ text: "plain" }]);
  assert.deepEqual(emojiParts(index, ""), [{ text: "" }]);
});

test("search covers names, keywords and short codes", () => {
  const found = (query, name) => searchEmoji(index, query).some((emoji) => emoji.name === name);
  assert.ok(found("grin", "grinning face"));
  assert.ok(found("THUMBSUP", "thumbs up"));
  assert.ok(found("thumbs up", "thumbs up"));
  assert.ok(found("cheerful", "grinning face"));
  assert.ok(found("germany", "flag: Germany"));
  assert.ok(!found("rinning", "grinning face"));
  assert.deepEqual(searchEmoji(index, "zzzqqq"), []);
});

test("skin tones", () => {
  const wave = named("waving hand");
  assert.equal(inTone(wave, 0), "👋");
  assert.equal(inTone(wave, 1), "👋🏻");
  assert.equal(inTone(wave, 5), "👋🏿");
  assert.equal(inTone(named("people holding hands"), 3), "🧑🏽‍🤝‍🧑🏽");
  assert.equal(inTone(named("grinning face"), 4), "😀");
  assert.equal(inTone(wave, 9), "👋");
});

test("data that is not the expected shape is refused", () => {
  const valid = { groups: [{ name: "G", emoji: [["😀", "grinning face", "grin", "grinning grinning_face"]] }], images: ["1f600"], text: [] };
  assert.ok(emojiIndex(valid));
  for (const broken of [
    null,
    { ...valid, images: ["../secret"] },
    { ...valid, images: ["1f600", "https://example.org/x"] },
    { ...valid, text: ["2122"] },
    { ...valid, groups: [{ name: "G", emoji: [["😀", "a\tb", "x", "x"]] }] },
    { ...valid, groups: [{ name: "G", emoji: [["😀", "x", "x"]] }] },
    { ...valid, groups: [{ name: "G", emoji: [["😀", "x", "x", ":grin:"]] }] },
    { ...valid, groups: [{ name: "G", emoji: [["😀", "x", "x", ""]] }] },
    { ...valid, groups: Array.from({ length: 100 }, () => valid.groups[0]) },
  ]) assert.throws(() => emojiIndex(broken));
  assert.equal(imageUrl("/app/", "1f600"), "/app/emoji/svg/1f600.svg");
});

test("data is fetched from this origin once", async () => {
  const calls = [];
  const fetcher = async (url, options) => { calls.push([url, options.credentials]); return { ok: true, text: async () => readFileSync(path, "utf8") }; };
  const [first, second] = await Promise.all([loadEmojiIndex("/app/", fetcher), loadEmojiIndex("/app/", fetcher)]);
  assert.equal(first, second);
  assert.deepEqual(calls, [["/app/emoji/data.json", "same-origin"]]);
});

test("insertion replaces the selection and puts the cursor after it", () => {
  assert.deepEqual(insertAt("hello world", 5, 5, "😀"), { value: "hello😀 world", cursor: 7 });
  assert.deepEqual(insertAt("hello world", 0, 5, "👋"), { value: "👋 world", cursor: 2 });
  assert.deepEqual(insertAt("", undefined, undefined, "™️"), { value: "™️", cursor: 2 });
  assert.deepEqual(insertAt("ab", 9, 1, "x"), { value: "abx", cursor: 3 });
});
