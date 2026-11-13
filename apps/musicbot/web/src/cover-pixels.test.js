// SPDX-License-Identifier: EUPL-1.2
import test from 'node:test';
import assert from 'node:assert/strict';
let realm = 0;
async function fresh() { return import(`./cover-pixels.js?fixture=${++realm}`); }
const tick = () => new Promise(resolve => setImmediate(resolve));
const good = { width: 1, height: 1, rgba: 'yFAo/w==' };
const response = value => new Response(JSON.stringify(value), { headers: { 'content-type': 'application/json' } });

function fetching(t, handler) {
  const saved = globalThis.fetch;
  globalThis.fetch = handler;
  t.after(() => { globalThis.fetch = saved; });
}

test('only exact bounded canonical RGBA shapes are admitted', async () => {
  const { validatePixels } = await fresh();
  assert.deepEqual([...validatePixels(good).pixels], [200, 80, 40, 255]);
  for (const value of [null, [], { ...good, source: 'https://never.invalid' },
    { ...good, width: 0 }, { ...good, width: 401 }, { ...good, height: 1.5 },
    { ...good, rgba: 'AQID' }, { ...good, rgba: 'AQIDBB==' }, { ...good, rgba: 'data:image/png,raw' }]) {
    assert.throws(() => validatePixels(value));
  }
  assert.throws(() => validatePixels({ ...good, width: 2 }, 1));
});

test('aborted active fetch retains its real slot until actual settlement', async t => {
  const { loadCover } = await fresh();
  const calls = [];
  fetching(t, (url, options) => new Promise(resolve => calls.push({ url, options, resolve })));
  const a = new AbortController(), b = new AbortController(), c = new AbortController();
  const first = loadCover('art/0/a', 400, a.signal, 'page:0').catch(error => error.name);
  const second = loadCover('art/0/b', 400, b.signal, 'page:0').catch(error => error.name);
  const third = loadCover('art/0/c', 400, c.signal, 'page:0');
  assert.equal(calls.length, 2);
  a.abort(); assert.equal(await first, 'AbortError');
  await tick(); assert.equal(calls.length, 2);
  calls[0].resolve(response(good)); await tick(); await tick();
  assert.equal(calls.length, 3);
  calls[2].resolve(response(good)); assert.equal((await third).width, 1);
  b.abort(); assert.equal(await second, 'AbortError'); calls[1].resolve(response(good));
  await tick(); await tick();
  // A discarded late response did not populate the cache.
  const again = loadCover('art/0/a', 400, undefined, 'page:0');
  assert.equal(calls.length, 4); calls[3].resolve(response(good)); await again;
});

test('global queue and absolute deadlines cannot multiply stopped fetches', async t => {
  const { loadCover } = await fresh();
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const calls = [];
  fetching(t, () => new Promise(resolve => calls.push(resolve)));
  const jobs = Array.from({ length: 66 }, (_, i) => loadCover(`art/0/${i}`, 400, undefined, 'page:0').catch(error => error.message));
  assert.equal(calls.length, 2);
  await assert.rejects(loadCover('art/0/overflow', 400, undefined, 'page:0'), /queue full/);
  t.mock.timers.tick(10001);
  assert.ok((await Promise.all(jobs)).every(value => value.includes('timed out')));
  assert.equal(calls.length, 2);
  const later = loadCover('art/0/later', 400, undefined, 'page:0').catch(error => error.message);
  assert.equal(calls.length, 2);
  t.mock.timers.tick(10001); await later;
  calls.forEach(resolve => resolve(response(good))); await tick(); await tick();
});

test('cache is copied, bounded by entries and scoped to the connection lease', async t => {
  const { loadCover } = await fresh();
  let count = 0;
  fetching(t, async () => { count++; return response(good); });
  const first = await loadCover('art/0/shared', 400, undefined, 'page:0'); first.pixels[0] = 0;
  assert.equal((await loadCover('art/0/shared', 400, undefined, 'page:0')).pixels[0], 200);
  assert.equal(count, 1);
  await loadCover('art/0/shared', 400, undefined, 'page:1'); assert.equal(count, 2);
  for (let i = 0; i < 64; i++) await loadCover(`art/0/${i}`, 400, undefined, 'page:1');
  const before = count; await loadCover('art/0/shared', 400, undefined, 'page:0');
  assert.equal(count, before + 1);
});

test('bad types, oversized advertised/body sizes and empty chunks are cancelled', async t => {
  const { loadCover } = await fresh();
  let cancelled = 0, version = 0;
  fetching(t, async () => {
    const body = new ReadableStream({ start(controller) {
      if (version === 2) controller.enqueue(new Uint8Array(5000));
      else if (version === 3) controller.enqueue(new Uint8Array(0));
    }, cancel() { cancelled++; } });
    return new Response(body, { headers: { 'content-type': version === 0 ? 'image/png' : 'application/json',
      ...(version === 1 ? { 'content-length': '50000000' } : {}) } });
  });
  for (version = 0; version < 4; version++) await assert.rejects(loadCover(`art/0/${version}`, 1, undefined, 'page:0'));
  assert.equal(cancelled, 4);
});

test('live Canvas memory and count leases release idempotently', async t => {
  const { paintCover } = await fresh();
  const old = globalThis.ImageData;
  globalThis.ImageData = class { constructor(pixels, width, height) { this.data = pixels; this.width = width; this.height = height; } };
  t.after(() => { globalThis.ImageData = old; });
  const canvas = () => ({ width: 0, height: 0, getContext: () => ({ putImageData() {} }) });
  const value = { width: 400, height: 400, pixels: new Uint8ClampedArray(400 * 400 * 4) };
  const releases = [];
  for (let i = 0; i < 26; i++) { const release = paintCover(canvas(), value); assert.ok(release); releases.push(release); }
  assert.equal(paintCover(canvas(), value), null);
  releases[0](); releases[0](); const replacement = paintCover(canvas(), value); assert.ok(replacement);
  assert.equal(paintCover(canvas(), value), null);
  replacement(); releases.forEach(release => release());
  const tiny = { width: 1, height: 1, pixels: new Uint8ClampedArray(4) };
  const small = Array.from({ length: 64 }, () => paintCover(canvas(), tiny));
  assert.ok(small.every(Boolean)); assert.equal(paintCover(canvas(), tiny), null);
  small.forEach(release => release());
});


test('delayed timer dispatch cannot admit expired responses or cached queued jobs', async t => {
  const { loadCover } = await fresh();
  let now = 0;
  t.mock.method(performance, 'now', () => now);
  t.mock.timers.enable({ apis: ['setTimeout'] });
  const calls = [];
  fetching(t, url => new Promise(resolve => calls.push({ url, resolve })));
  const cached = loadCover('art/0/cached', 1, undefined, 'page:0');
  calls[0].resolve(response(good)); await cached; await tick();
  const a = loadCover('art/0/a', 1, undefined, 'page:0').catch(error => error.message);
  const b = loadCover('art/0/b', 1, undefined, 'page:0').catch(error => error.message);
  const queued = loadCover('art/0/cached', 1, undefined, 'page:0').catch(error => error.message);
  assert.equal(calls.length, 3);
  now = 10001; // Timers intentionally have not dispatched.
  calls[1].resolve(response(good)); calls[2].resolve(response(good));
  assert.ok((await Promise.all([a,b,queued])).every(value => value.includes('timed out')));
  await tick(); await tick();
  assert.equal(calls.length, 3);
  const retry = loadCover('art/0/a', 1, undefined, 'page:0');
  assert.equal(calls.length, 4); // Expired response did not populate the cache.
  calls[3].resolve(response(good)); assert.equal((await retry).width, 1);
});
