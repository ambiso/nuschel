// SPDX-License-Identifier: EUPL-1.2
// Inert presentation only: shared SDK owns messages, selection and history.
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
  // A malformed all-cyclic tree must remain navigable and cannot recurse forever.
  if (!rows.length && channels.length) walk(channels[0], 0);
  return rows;
}
export function visibleBlocks(document, expanded = false) {
  let chars = 0;
  let lines = 1;
  let clipped = false;
  const trim = (text) => {
    if (expanded) return text;
    let result = "";
    for (const char of text) {
      if (chars >= 3000 || (char === "\n" && lines >= 8)) { clipped = true; break; }
      result += char;
      chars++;
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
    else if (block.kind === "math") blocks.push({ kind: "text", text: trim(block.source) });
    else blocks.push({ kind: "text", text: "[Image display is not supported in this browser client]" });
    // Separate blocks consume a visual line, preserving text inside them exactly.
    lines++;
  }
  return { blocks, hasMore: clipped };
}
export function codeColor(value) {
  if (!Array.isArray(value) || value.length < 3 || value.slice(0, 3).some((part) => !Number.isInteger(part) || part < 0 || part > 255)) return "inherit";
  return `rgb(${value.slice(0, 3).join(",")})`;
}
