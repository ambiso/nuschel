// SPDX-License-Identifier: EUPL-1.2
// One page-wide bounded fetch lane. Only validated raw pixels reach Canvas;
// no Image/ImageBitmap/encoded resource parser is used here.
export const MAX_EDGE = 400;
const MAX_ACTIVE = 2, MAX_QUEUED = 64, MAX_CACHE = 64, CACHE_BYTES = 8 * 1024 * 1024;
const DEADLINE = 10000;
let active = 0, cacheBytes = 0, painted = 0, paintedBytes = 0;
const queue = [], cache = new Map();

function aborted() { return new DOMException('Cover request cancelled', 'AbortError'); }
function copy(value) { return { width: value.width, height: value.height, pixels: new Uint8ClampedArray(value.pixels) }; }
function remembered(url, value) {
  const bytes = value.pixels.byteLength + url.length * 2 + 64;
  const old = cache.get(url);
  if (old) { cacheBytes -= old.bytes; cache.delete(url); }
  cache.set(url, { value, bytes }); cacheBytes += bytes;
  while (cache.size > MAX_CACHE || cacheBytes > CACHE_BYTES) {
    const key = cache.keys().next().value;
    cacheBytes -= cache.get(key).bytes; cache.delete(key);
  }
}

export function validatePixels(value, maximum = MAX_EDGE) {
  if (!Number.isInteger(maximum) || maximum < 1 || maximum > MAX_EDGE
      || !value || typeof value !== 'object' || Array.isArray(value)
      || Object.keys(value).sort().join(',') !== 'height,rgba,width') throw Error('Invalid cover shape');
  const { width, height, rgba } = value;
  if (!Number.isInteger(width) || !Number.isInteger(height) || width < 1 || height < 1
      || width > maximum || height > maximum || typeof rgba !== 'string') throw Error('Invalid cover dimensions');
  const bytes = width * height * 4;
  if (rgba.length !== Math.ceil(bytes / 3) * 4 || !/^(?:[A-Za-z0-9+/]{4})*(?:[A-Za-z0-9+/]{2}==|[A-Za-z0-9+/]{3}=)?$/.test(rgba)) throw Error('Invalid cover pixels');
  const binary = atob(rgba);
  if (binary.length !== bytes || btoa(binary) !== rgba) throw Error('Invalid cover encoding');
  const pixels = new Uint8ClampedArray(bytes);
  for (let index = 0; index < bytes; index++) pixels[index] = binary.charCodeAt(index);
  return { width, height, pixels };
}

async function fetchPixels(url, maximum, signal) {
  const response = await fetch(url, { signal, cache: 'no-store', headers: { Accept: 'application/json' } });
  if (!response.ok || !/^application\/json(?:;|$)/i.test(response.headers.get('content-type') || '')) { await response.body?.cancel().catch(() => {}); throw Error('Cover unavailable'); }
  const limit = Math.ceil(maximum * maximum * 4 / 3) * 4 + 4096;
  const declared = response.headers.get('content-length');
  if (declared && (!/^\d+$/.test(declared) || Number(declared) > limit)) { await response.body?.cancel().catch(() => {}); throw Error('Cover response too large'); }
  if (!response.body) throw Error('Cover response missing');
  const reader = response.body.getReader(), bytes = new Uint8Array(limit);
  let size = 0, chunks = 0;
  try {
    for (;;) {
      const part = await reader.read();
      if (part.done) break;
      if (!(part.value instanceof Uint8Array) || part.value.byteLength === 0 || ++chunks > 4096
          || size + part.value.byteLength > limit) throw Error('Cover response too large');
      bytes.set(part.value, size); size += part.value.byteLength;
    }
  } catch (error) {
    await reader.cancel().catch(() => {});
    throw error;
  } finally { reader.releaseLock(); }
  return validatePixels(JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes.subarray(0, size))), maximum);
}

function pump() {
  while (active < MAX_ACTIVE && queue.length) {
    const job = queue.shift();
    if (job.settled) continue;
    if (performance.now() >= job.deadline) { job.finish(Error('Cover request timed out')); job.cleanup(); continue; }
    const stored = cache.get(job.key);
    if (stored && stored.value.width <= job.maximum && stored.value.height <= job.maximum) {
      job.finish(null, copy(stored.value)); job.cleanup(); continue;
    }
    job.active = true; active++;
    fetchPixels(job.url, job.maximum, job.controller.signal).then(value => {
      if (!job.settled && performance.now() >= job.deadline) job.finish(Error('Cover request timed out'));
      if (!job.settled) { remembered(job.key, value); job.finish(null, copy(value)); }
    }, error => job.finish(error)).finally(() => {
      // Only the real fetch/body completion releases network capacity. An
      // abort/deadline settles its consumer but cannot manufacture new slots.
      job.cleanup(); active--; pump();
    });
  }
}

export function loadCover(url, maximum = MAX_EDGE, signal, namespace) {
  if (typeof url !== 'string' || url.length > 4096 || !url.startsWith('art/')
      || !Number.isInteger(maximum) || maximum < 1 || maximum > MAX_EDGE
      || typeof namespace !== 'string' || !namespace || namespace.length > 128) return Promise.reject(Error('Invalid cover request'));
  if (signal?.aborted) return Promise.reject(aborted());
  return new Promise((resolve, reject) => {
    const job = { url, key: namespace + '\0' + url, maximum, controller: new AbortController(), deadline: performance.now() + DEADLINE, active: false, settled: false };
    job.finish = (error, value) => {
      if (job.settled) return;
      job.settled = true;
      if (error) reject(error); else resolve(value);
    };
    job.cancel = () => {
      job.finish(aborted()); job.controller.abort();
      if (!job.active) { const index = queue.indexOf(job); if (index >= 0) queue.splice(index, 1); job.cleanup(); }
    };
    job.cleanup = () => { clearTimeout(job.timer); signal?.removeEventListener('abort', job.cancel); };
    job.timer = setTimeout(() => {
      job.finish(Error('Cover request timed out')); job.controller.abort();
      if (!job.active) { const index = queue.indexOf(job); if (index >= 0) queue.splice(index, 1); job.cleanup(); }
    }, DEADLINE);
    signal?.addEventListener('abort', job.cancel, { once: true });
    if (queue.length >= MAX_QUEUED) { job.finish(Error('Cover queue full')); job.cleanup(); return; }
    queue.push(job); pump();
  });
}

/** Retain a bounded viewport bitmap lease; its caller releases on hide/unmount. */
export function paintCover(canvas, value) {
  const { width, height, pixels } = value;
  if (!Number.isInteger(width) || !Number.isInteger(height) || width < 1 || height < 1
      || width > MAX_EDGE || height > MAX_EDGE || !(pixels instanceof Uint8ClampedArray)
      || pixels.byteLength !== width * height * 4) throw Error('Invalid Canvas pixels');
  const bytes = pixels.byteLength;
  if (painted >= 64 || paintedBytes + bytes > 16 * 1024 * 1024) return null;
  painted++; paintedBytes += bytes;
  let live = true;
  const release = () => {
    if (!live) return;
    live = false; painted--; paintedBytes -= bytes;
    canvas.width = 0; canvas.height = 0;
  };
  try {
    const context = canvas.getContext('2d');
    if (!context) { release(); return null; }
    canvas.width = width; canvas.height = height;
    context.putImageData(new ImageData(pixels, width, height), 0, 0);
    return release;
  } catch { release(); return null; }
}
