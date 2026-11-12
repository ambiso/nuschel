// SPDX-License-Identifier: EUPL-1.2
// Inert presentation only: the SDK owns identity, selection, messages and history.
export function channelRows(channels, expanded, server) {
  const nodes = new Map(channels.map((channel) => [channel.id, channel]));
  const children = new Map();
  for (const channel of channels) {
    const parent = channel.parent === channel.id || !nodes.has(channel.parent) ? null : channel.parent;
    const group = children.get(parent) || [];
    group.push(channel);
    children.set(parent, group);
  }
  const rows = [];
  const visited = new Set();
  const walk = (channel, depth) => {
    if (visited.has(channel.id) || depth > 32) return;
    visited.add(channel.id);
    const hasChildren = (children.get(channel.id)?.length || 0) > 0;
    const key = `${server}:${channel.id}`;
    const open = expanded.has(key) ? expanded.get(key) : channel.id === 0;
    rows.push({ ...channel, depth, hasChildren, open });
    if (open) for (const child of children.get(channel.id) || []) walk(child, depth + 1);
  };
  for (const root of children.get(null) || []) walk(root, 0);
  if (!rows.length && channels.length) walk(channels[0], 0);
  return rows;
}
export function visibleBlocks(document, expanded = false) {
  let chars = 0, lines = 1, clipped = false;
  const trim = (text) => {
    if (expanded) return text;
    let result = "";
    for (const char of text) {
      if (chars >= 3000 || (char === "\n" && lines >= 8)) { clipped = true; break; }
      result += char; chars++;
      if (char === "\n") lines++;
    }
    return result;
  };
  const blocks = [];
  for (const block of document?.blocks || []) {
    if (clipped) break;
    if (block.kind === "code") {
      const runs = [];
      for (const run of block.runs) {
        if (clipped) break;
        runs.push({ ...run, text: trim(run.text) });
      }
      blocks.push({ ...block, runs });
    } else if (block.kind === "text") blocks.push({ ...block, text: trim(block.text) });
    else if (block.kind === "math") {
      const preview = trim(block.source);
      // A collapsed preview must not parse hidden source or turn a partial
      // formula into another expression. Copy/details retain the original.
      blocks.push({ ...block, preview, tex: preview === block.source ? block.tex : undefined });
    }
    else if (block.kind === "image") blocks.push(block);
    lines++;
  }
  return { blocks, hasMore: clipped };
}
export function codeColor(value) {
  if (!Array.isArray(value) || value.length < 3 || value.slice(0, 3).some((part) => !Number.isInteger(part) || part < 0 || part > 255)) return "inherit";
  return `rgb(${value.slice(0, 3).join(",")})`;
}
export function safeLink(value) {
  if (typeof value !== "string" || value.length > 8192 || /[\u0000-\u0020\u007f]/u.test(value)) return null;
  try {
    const url = new URL(value);
    return ["https:", "http:", "mumble:", "nuschl:"].includes(url.protocol) && url.hostname && !url.username && !url.password ? url.href : null;
  } catch { return null; }
}
// Rust spans address UTF-8 bytes. Reject non-boundary, overlapping or malformed
// spans instead of turning a byte offset into the wrong link on Unicode text.
export function textRuns(block) {
  const text = block.text || "";
  const boundaries = new Map([[0, 0]]);
  let bytes = 0, units = 0;
  const encoder = new TextEncoder();
  for (const char of text) { bytes += encoder.encode(char).length; units += char.length; boundaries.set(bytes, units); }
  const runs = [];
  let previous = 0;
  for (const span of (block.spans || []).slice(0, 4096)) {
    if (!Number.isInteger(span.start) || !Number.isInteger(span.end) || span.start < previous || span.end <= span.start || !boundaries.has(span.start) || !boundaries.has(span.end)) continue;
    if (span.start > previous) runs.push({ text: text.slice(boundaries.get(previous), boundaries.get(span.start)), style: {} });
    runs.push({ text: text.slice(boundaries.get(span.start), boundaries.get(span.end)), style: span.style || {}, link: safeLink(span.link), math: typeof span.math === "string" ? span.math : undefined });
    previous = span.end;
  }
  if (previous < bytes) runs.push({ text: text.slice(boundaries.get(previous)), style: {} });
  return runs;
}
export function initials(name) {
  return String(name || "?").trim().split(/\s+/u).slice(0, 2).map((part) => [...part][0] || "").join("").toLocaleUpperCase();
}
export function avatarColor(name) {
  let hash = 0;
  for (const char of String(name)) hash = ((hash * 31) + char.codePointAt(0)) >>> 0;
  return ["violet", "teal", "blue", "rose", "amber"][hash % 5];
}
export function conversationKey(timeline, server) {
  return timeline && server ? JSON.stringify([server.id, server.pin || server.certificate || "unverified", timeline.channel]) : "";
}
export function filterMessages(messages, query) {
  const needle = query.trim().toLocaleLowerCase();
  if (!needle) return messages;
  return messages.filter((message) => [message.author, ...(message.document?.blocks || []).map((block) => block.text || block.source || (block.runs || []).map((run) => run.text).join(""))].join("\n").toLocaleLowerCase().includes(needle));
}
export function appearance(raw) {
  return {
    theme: ["system", "dark", "light"].includes(raw?.theme) ? raw.theme : "system",
    density: raw?.density === "compact" ? "compact" : "comfortable",
    fontSize: [14, 16, 18, 20].includes(Number(raw?.fontSize)) ? Number(raw.fontSize) : 16,
    members: raw?.members !== false,
  };
}
// Only already-decoded worker RGBA is drawn. Never use source/data URLs to ask
// the browser to decode an attachment, and never load a remote image URL.
export function rgbaPixels(block) {
  const { width, height, rgba } = block;
  if (!Number.isInteger(width) || !Number.isInteger(height) || width < 1 || height < 1 || width > 4096 || height > 4096 || width * height > 2_000_000 || typeof rgba !== "string" || rgba.length > 10_666_668) return null;
  try {
    const bytes = atob(rgba);
    if (bytes.length !== width * height * 4) return null;
    return Uint8ClampedArray.from(bytes, (byte) => byte.charCodeAt(0));
  } catch { return null; }
}
