// SPDX-License-Identifier: EUPL-1.2
// Emoji for the web client: the picker's table and which text is drawn as a
// Twemoji image. The data is nix/emoji.py's data.json, served from this origin
// with the images under emoji/svg/ (nix/web-client.nix); the desktop applies
// the same rules (apps/desktop/src/emoji.rs).
//
// Text is drawn as an image only from the text itself: characters Unicode
// shows as text by default (™, ©, ❤) only with U+FE0F after them, and nothing
// followed by U+FE0E. The composer stays a plain textarea.

export const TONES = 5;
export const TONE_NAMES = ["Default skin tone", "Light skin tone", "Medium-light skin tone", "Medium skin tone", "Medium-dark skin tone", "Dark skin tone"];
export const TONE_SAMPLES = ["✋", "✋🏻", "✋🏼", "✋🏽", "✋🏾", "✋🏿"];
const TEXT_STYLE = 0xfe0e, EMOJI_STYLE = 0xfe0f;
// Bounds for the served data; the pinned data has 9 groups, 1914 emoji and
// 4009 images, keys of at most nine code points.
const LIMITS = { bytes: 2 * 1024 * 1024, groups: 32, emoji: 8192, images: 16384, parts: 10, field: 512 };
const KEY = /^[0-9a-f]{1,6}(?:-[0-9a-f]{1,6}){0,9}$/;

function text(value, limit = LIMITS.field) {
  if (typeof value !== "string" || value.length > limit || /[\t\n\r]/.test(value)) throw new Error("invalid emoji data");
  return value;
}

/** Validates data.json (already parsed) into the picker's groups and the image index. */
export function emojiIndex(data) {
  if (!data || !Array.isArray(data.groups) || !Array.isArray(data.images) || !Array.isArray(data.text)) throw new Error("invalid emoji data");
  if (data.groups.length > LIMITS.groups || data.images.length > LIMITS.images || data.text.length > LIMITS.images) throw new Error("too much emoji data");
  const images = new Set(), prefixes = new Set();
  for (const key of data.images) {
    if (typeof key !== "string" || !KEY.test(key)) throw new Error("invalid emoji image key");
    images.add(key);
    const parts = key.split("-");
    for (let length = 1; length < parts.length; length++) prefixes.add(parts.slice(0, length).join("-"));
  }
  const textDefault = new Set();
  for (const key of data.text) {
    if (!images.has(key) || key.includes("-")) throw new Error("invalid text-presentation key");
    textDefault.add(key);
  }
  let count = 0;
  const groups = data.groups.map((group) => {
    if (!group || !Array.isArray(group.emoji)) throw new Error("invalid emoji group");
    count += group.emoji.length;
    if (count > LIMITS.emoji) throw new Error("too many emoji");
    return {
      name: text(group.name, 64),
      emoji: group.emoji.map((row) => {
        if (!Array.isArray(row) || (row.length !== 3 && row.length !== 3 + TONES)) throw new Error("invalid emoji");
        const [emoji, name, search, ...tones] = row.map((field) => text(field));
        return { emoji, name, search: search.split(" "), tones: tones.length ? tones : null };
      }),
    };
  });
  return { groups, images, prefixes, textDefault };
}

/** `emoji` in skin tone `tone` (0 default, 1 light to 5 dark) where it has that tone. */
export function inTone(emoji, tone) {
  return (tone >= 1 && tone <= TONES && emoji.tones?.[tone - 1]) || emoji.emoji;
}

/** Emoji with a word of their name, keywords or short codes starting with each word of `query`. */
export function searchEmoji(index, query) {
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  const found = [];
  for (const group of index.groups) for (const emoji of group.emoji) {
    if (words.every((word) => emoji.search.some((part) => part.startsWith(word)))) found.push(emoji);
  }
  return found;
}

/**
 * The parts of `value` drawn as images: [{start, end, key}] in UTF-16
 * offsets, the longest sequence with an image at each position (U+FE0F
 * ignored for the match but kept in the range).
 */
export function findImages(index, value) {
  const found = [];
  if (!index || !value) return found;
  const points = [];
  for (let at = 0; at < value.length;) {
    const point = value.codePointAt(at);
    points.push({ at, point });
    at += point > 0xffff ? 2 : 1;
  }
  const offset = (i) => i < points.length ? points[i].at : value.length;
  let i = 0;
  while (i < points.length) {
    const first = points[i].point;
    // Keycaps start with # * or a digit; every other key is outside ASCII.
    if (first < 0x80 && !(first === 0x23 || first === 0x2a || (first >= 0x30 && first <= 0x39))) { i++; continue; }
    let key = "", best = null, j = i, parts = 0;
    while (j < points.length && parts < LIMITS.parts) {
      const point = points[j].point;
      if (point === EMOJI_STYLE) {
        j++;
        if (best && best.end === j - 1) best.end = j;
        continue;
      }
      key += (parts ? "-" : "") + point.toString(16);
      parts++; j++;
      if (index.images.has(key)) best = { end: j, key };
      else if (!index.prefixes.has(key)) break;
    }
    if (!best) { i++; continue; }
    const single = best.end === i + 1 || (best.end === i + 2 && points[i + 1].point === EMOJI_STYLE);
    const styled = points[best.end - 1].point === EMOJI_STYLE;
    const asText = points[best.end]?.point === TEXT_STYLE || (index.textDefault.has(best.key) && single && !styled);
    if (!asText) found.push({ start: offset(i), end: offset(best.end), key: best.key });
    i = best.end;
  }
  return found;
}

/** `value` as text and image parts: [{text}] and [{text, key}]. */
export function emojiParts(index, value) {
  const parts = [];
  let at = 0;
  for (const { start, end, key } of findImages(index, value)) {
    if (start > at) parts.push({ text: value.slice(at, start) });
    parts.push({ text: value.slice(start, end), key });
    at = end;
  }
  if (at < value.length || !parts.length) parts.push({ text: value.slice(at) });
  return parts;
}

/** The image key of `emoji` if all of it is one image. */
export function imageKey(index, emoji) {
  const [found, more] = findImages(index, emoji);
  return found && !more && found.start === 0 && found.end === emoji.length ? found.key : null;
}

/** Where an image is served; keys come only from the validated index. */
export function imageUrl(base, key) {
  return `${base}emoji/svg/${key}.svg`;
}

/** Fetches and validates data.json from this origin, at most once. */
let pending = null;
export function loadEmojiIndex(base, fetcher = fetch) {
  pending ??= (async () => {
    const response = await fetcher(`${base}emoji/data.json`, { credentials: "same-origin" });
    if (!response.ok) throw new Error(`emoji data: HTTP ${response.status}`);
    const body = await response.text();
    if (body.length > LIMITS.bytes) throw new Error("emoji data too large");
    return emojiIndex(JSON.parse(body));
  })();
  return pending;
}

/**
 * `value` with `insert` replacing its selection [start, end); the new value
 * and where the cursor goes (after the insertion).
 */
export function insertAt(value, start, end, insert) {
  const from = Math.max(0, Math.min(start ?? value.length, value.length));
  const to = Math.max(from, Math.min(end ?? from, value.length));
  return { value: value.slice(0, from) + insert + value.slice(to), cursor: from + insert.length };
}
