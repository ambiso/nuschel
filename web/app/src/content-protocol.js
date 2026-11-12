// SPDX-License-Identifier: EUPL-1.2
// Validate replies again outside the codec realm. No DOM parser or decoder.
export const MAX_SOURCE = 256 * 1024;
export const MAX_IMAGE_INPUT = 8 * 1024 * 1024;
export const MAX_REPLY = 2 * 1024 * 1024;
export const MAX_PIXELS = 640 * 1024;
const encoder = new TextEncoder();
const fail = () => { throw new Error("Invalid or oversized content worker reply."); };
const integer = (value, min, max) => Number.isSafeInteger(value) && value >= min && value <= max;
function fields(value, names) {
  if (!value || Object.getPrototypeOf(value) !== Object.prototype || Object.keys(value).some(key => !names.includes(key))) fail();
}
export function textBytes(value, limit) {
  if (typeof value !== "string" || value.length > limit) fail();
  const bytes = encoder.encode(value);
  if (bytes.length > limit) fail();
  return bytes;
}
function color(value, count) {
  if (!Array.isArray(value) || value.length !== count || value.some(byte => !integer(byte, 0, 255))) fail();
}
function base64(value, bytes) {
  if (typeof value !== "string" || value.length !== Math.ceil(bytes / 3) * 4) fail();
  const alphabet = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
  const pad = (3 - bytes % 3) % 3;
  for (let i = 0; i < value.length; i++) {
    if (i >= value.length - pad ? value[i] !== "=" : alphabet.indexOf(value[i]) < 0) fail();
  }
  if (pad && (alphabet.indexOf(value[value.length - pad - 1]) & (pad === 2 ? 15 : 3))) fail();
}
function image(block, budget) {
  fields(block, ["kind", "width", "height", "rgba"]);
  if (block.kind !== "image" || !integer(block.width, 1, 360) || !integer(block.height, 1, 270)) fail();
  const bytes = block.width * block.height * 4;
  budget.pixels += bytes;
  if (++budget.images > 8 || budget.pixels > MAX_PIXELS) fail();
  base64(block.rgba, bytes);
}
function style(value) {
  fields(value, ["bold", "italic", "underline", "strike", "code", "color"]);
  for (const flag of ["bold", "italic", "underline", "strike", "code"]) {
    if (value[flag] !== undefined && typeof value[flag] !== "boolean") fail();
  }
  if (value.color != null) color(value.color, 3);
}
function role(value) {
  if (value === undefined || ["paragraph", "quote", "rule"].includes(value)) return;
  fields(value, ["heading", "item"]);
  if (Object.keys(value).length !== 1) fail();
  if (value.heading !== undefined) {
    if (!integer(value.heading, 1, 3)) fail();
  } else {
    fields(value.item, ["depth", "marker"]);
    if (!integer(value.item.depth, 1, 4)) fail();
    textBytes(value.item.marker, 32);
  }
}
// Blocks per document, and the line breaks between them (`break`, at most one
// between two other blocks and one at each end) on a budget of their own:
// together no more line breaks than the largest message holds.
export const MAX_BLOCKS = 512;
export const MAX_BREAKS = MAX_BLOCKS + 1;
export const MAX_BREAK_LINES = MAX_SOURCE;
export function validateDocument(document) {
  fields(document, ["blocks"]);
  if (!Array.isArray(document.blocks) || document.blocks.length > MAX_BLOCKS + MAX_BREAKS) fail();
  const budget = {pixels:0, images:0, text:0, spans:0, runs:0, blocks:0, breaks:0, lines:0};
  const text = value => { const bytes = textBytes(value, MAX_SOURCE); budget.text += bytes.length; if (budget.text > MAX_SOURCE) fail(); return bytes; };
  let previousBreak = false;
  for (const block of document.blocks) {
    if (block?.kind === "break") {
      fields(block, ["kind", "count"]);
      if (!integer(block.count, 0, MAX_SOURCE) || previousBreak || ++budget.breaks > MAX_BREAKS ||
          (budget.lines += block.count) > MAX_BREAK_LINES) fail();
      previousBreak = true; continue;
    }
    previousBreak = false;
    if (++budget.blocks > MAX_BLOCKS) fail();
    if (block?.kind === "image") { image(block, budget); continue; }
    if (block?.kind === "text") {
      fields(block, ["kind", "text", "spans", "role"]);
      const bytes = text(block.text); role(block.role);
      const spans = block.spans ?? [];
      if (!Array.isArray(spans) || (budget.spans += spans.length) > 2048) fail();
      let end = 0;
      for (const span of spans) {
        fields(span, ["start", "end", "style", "link", "math"]);
        if (!integer(span.start, end, bytes.length) || !integer(span.end, span.start, bytes.length) ||
            (bytes[span.start] & 0xc0) === 0x80 || (bytes[span.end] & 0xc0) === 0x80) fail();
        end = span.end;
        if (span.style !== undefined) style(span.style);
        if (span.link != null) {
          textBytes(span.link, 2048);
          if (!/^(?:https?|mumble):\/\/[^\s\x00-\x1f\x7f]+$/iu.test(span.link)) fail();
        }
        if (span.math != null) textBytes(span.math, 4096);
      }
    } else if (block?.kind === "code") {
      fields(block, ["kind", "language", "runs"]);
      if (block.language != null) textBytes(block.language, 256);
      if (!Array.isArray(block.runs) || (budget.runs += block.runs.length) > 32768) fail();
      for (const run of block.runs) {
        fields(run, ["text", "foreground", "bold", "italic"]); text(run.text); color(run.foreground, 4);
        if (typeof run.bold !== "boolean" || typeof run.italic !== "boolean") fail();
      }
    } else if (block?.kind === "math") {
      fields(block, ["kind", "source", "tex"]); text(block.source); textBytes(block.tex, 4096);
    } else fail();
  }
  return document;
}
export function validateReply(operation, json, limit) {
  textBytes(json, MAX_REPLY);
  const reply = JSON.parse(json);
  if (operation === "render") {
    fields(reply, ["marked_text", "plain_text", "document"]);
    textBytes(reply.marked_text, MAX_SOURCE); textBytes(reply.plain_text, MAX_SOURCE);
    validateDocument(reply.document);
  } else if (operation === "encode") {
    fields(reply, ["html", "thumbnail"]); textBytes(reply.html, limit);
    const prefix = /^<img src="data:image\/(?:png|jpeg);base64,/.exec(reply.html)?.[0];
    if (!prefix || !reply.html.endsWith('" />')) fail();
    const encoded = reply.html.slice(prefix.length, -4);
    if (!encoded.length) fail();
    for (let i = 0; i < encoded.length; i++) {
      const code = encoded.charCodeAt(i);
      if (code === 10 || code >= 48 && code <= 57 || code >= 65 && code <= 90 || code >= 97 && code <= 122) continue;
      if (encoded[i] !== "%" || !["2B", "2F", "3D"].includes(encoded.slice(i + 1, i + 3))) fail();
      i += 2;
    }
    image(reply.thumbnail, {pixels:0, images:0});
  } else if (operation === "decode") image(reply, {pixels:0, images:0});
  else fail();
  return reply;
}
