// SPDX-License-Identifier: EUPL-1.2
// Presentation order never changes the SDK's live connection indices.
export function serverIndices(servers, requireExplicit = false) {
  if (!Array.isArray(servers)) throw new Error("Invalid server snapshot.");
  const seen = new Set();
  return servers.map((server, position) => {
    const index = server.runtime_index ?? (requireExplicit ? -1 : position);
    if (!Number.isInteger(index) || index < 0 || index >= servers.length || seen.has(index)) throw new Error("Invalid live server index in SDK snapshot.");
    seen.add(index);
    return {...server, runtime_index:index};
  });
}
export function moveServer(ids, source, target) {
  const from = ids.indexOf(source);
  if (from < 0 || !Number.isInteger(target)) return ids;
  target = Math.max(0, Math.min(ids.length - 1, target));
  if (from === target) return ids;
  const next = ids.slice(); next.splice(target, 0, ...next.splice(from, 1)); return next;
}
export function sameOrder(a, b) { return a.length === b.length && a.every((id, index) => id === b[index]); }
export function restoreOrder(prior, current) {
  const available = new Set(current), known = new Set(prior);
  return [...prior.filter(id => available.has(id)), ...current.filter(id => !known.has(id))];
}
export function edgeSpeed(y, height, xInside = true) {
  if (!xInside || y < 0 || y > height) return 0;
  const edge = Math.min(28, height / 3);
  if (y < edge) return -280 * (edge - y) / edge;
  if (y > height - edge) return 280 * (y - height + edge) / edge;
  return 0;
}
