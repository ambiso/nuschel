// SPDX-License-Identifier: EUPL-1.2
// Actual packaged musicbot UI under its production CSP; owned HTTP services only.
import assert from 'node:assert/strict';
import { writeFile } from 'node:fs/promises';
import { pathToFileURL } from 'node:url';
const [link, executablePath, playwright, output, proxyPort] = process.argv.slice(2);
const { chromium } = await import(pathToFileURL(playwright));
const browser = await chromium.launch({ executablePath, headless: true, args: [
  `--proxy-server=http://127.0.0.1:${proxyPort}`, '--proxy-bypass-list=127.0.0.1;localhost', '--dns-prefetch-disable',
] });
const origin = new URL(link).origin;
const proof = { viewports: [], stale: null, malformed: null };
async function setup(viewport) {
  const context = await browser.newContext({ viewport });
  const page = await context.newPage();
  const failures = [];
  await page.addInitScript(() => {
    window.fixtureViolations = []; window.fixtureMediaRequests = 0;
    document.addEventListener('securitypolicyviolation', e => window.fixtureViolations.push(e.violatedDirective));
    if (navigator.mediaDevices) navigator.mediaDevices.getUserMedia = () => { window.fixtureMediaRequests++; throw Error('fixture forbids capture'); };
  });
  page.on('request', request => { if (new URL(request.url()).origin !== origin) failures.push(request.url()); });
  page.on('pageerror', error => failures.push(String(error)));
  return { context, page, failures };
}
async function pixel(page, expected) {
  await page.waitForFunction(color => [...document.querySelectorAll('canvas.ready')].some(c => {
    const p = c.getContext('2d').getImageData(0, 0, 1, 1).data;
    return color.every((v, i) => v === p[i]);
  }), expected);
}
async function choose(page, library) {
  const menu = page.getByRole('button', { name: 'Open navigation', exact: true });
  if (await menu.isVisible()) await menu.click();
  await page.getByLabel('Library', { exact: true }).selectOption(String(library));
  const close = page.getByRole('button', { name: 'Close navigation', exact: true });
  if (await close.isVisible()) await close.click();
}
async function clean(page, failures) {
  assert.deepEqual(failures, []);
  assert.equal(await page.locator('img').count(), 0);
  assert.deepEqual(await page.evaluate(() => window.fixtureViolations), []);
  assert.equal(await page.evaluate(() => window.fixtureMediaRequests), 0);
}
try {
  for (const width of [320, 390, 1280]) {
    const { context, page, failures } = await setup({ width, height: 800 });
    await page.goto(link + '#/albums?library=0&list=newest');
    await pixel(page, [200, 80, 40, 255]);
    const before = await page.evaluate(() => ({
      total: document.querySelectorAll('.media-grid canvas').length,
      live: [...document.querySelectorAll('canvas')].filter(c => c.width > 0).length,
      bytes: [...document.querySelectorAll('canvas')].reduce((s,c) => s+c.width*c.height*4,0),
      overflow: document.documentElement.scrollWidth > innerWidth,
    }));
    assert.equal(before.total, 48); assert.equal(before.overflow, false);
    assert.ok(before.live > 0 && before.live < 48 && before.live <= 64);
    assert.ok(before.bytes <= 16*1024*1024);
    await page.screenshot({ path: `${output}/canvas-${width}.png` });
    await page.locator('#main-content').evaluate(node => { node.scrollTop = node.scrollHeight; });
    await page.waitForFunction(() => document.querySelector('.media-grid canvas').width === 0);
    await choose(page, 1);
    await pixel(page, [40, 80, 200, 255]);
    await clean(page, failures);
    proof.viewports.push({ width, ...before, offscreenReleased: true, libraryPixelsChanged: true });
    await context.close();
  }
  {
    const { context, page, failures } = await setup({ width: 1280, height: 800 });
    let release, entered;
    const enteredPromise = new Promise(r => entered=r);
    const held = new Promise(r => release=r);
    let first = true, discarded = false;
    await page.route('**/art/0/**', async route => {
      if (!first) return route.continue();
      first = false;
      const response = await route.fetch();
      entered(); await held;
      try { await route.fulfill({ response }); } catch { discarded = true; }
    });
    await page.goto(link + '#/albums?library=0&list=newest');
    await enteredPromise;
    await choose(page, 1);
    await pixel(page, [40, 80, 200, 255]);
    release();
    await page.waitForTimeout(150);
    assert.equal(await page.evaluate(() => [...document.querySelectorAll('canvas.ready')].some(c => c.getContext('2d').getImageData(0,0,1,1).data[0]===200)), false);
    await clean(page, failures);
    proof.stale = { heldRealResponse: true, noOldPixelsAfterNavigation: true, abortedFulfillRejected: discarded };
    await context.close();
  }
  {
    const { context, page, failures } = await setup({ width: 390, height: 800 });
    await page.route('**/art/**', route => route.fulfill({ status:200,contentType:'application/json',body:JSON.stringify({ width:1,height:1,rgba:'yFAo/w==',source:'https://never-fetch.invalid/image' }) }));
    await page.goto(link + '#/albums?library=0&list=newest');
    await page.waitForSelector('.media-grid canvas');
    await page.waitForTimeout(250);
    assert.equal(await page.locator('canvas.ready').count(), 0);
    assert.equal(await page.evaluate(() => [...document.querySelectorAll('canvas')].some(c => c.width || c.height)), false);
    await clean(page, failures);
    proof.malformed = { unexpectedSourceRejected: true, allCanvasesEmpty: true, remoteFetches: 0 };
    await context.close();
  }
  await writeFile(`${output}/browser-proof.json`, JSON.stringify(proof,null,2)+'\n');
  console.log(JSON.stringify(proof));
} finally { await browser.close(); }
