// SPDX-License-Identifier: EUPL-1.2
// Actual local Vite compilation, never browser execution/network/devices. Fake
// package terms are test data; this does not certify any real release inventory.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import path from 'node:path';
import os from 'node:os';
import { pathToFileURL } from 'node:url';
import { bundledRuntimeNotices } from '../web/app/build-runtime-notices.js';

const source = path.resolve(import.meta.dirname, '../web/app');
const viteRoot = process.env.NUSCHL_VITE_PACKAGE;
if (!viteRoot) throw new Error('set NUSCHL_VITE_PACKAGE to the existing pinned Vite package (no npm install)');
const { build } = await import(pathToFileURL(path.join(viteRoot, 'dist/node/index.js')));

function setup() {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), 'nuschl-notice-build-'));
  const lock = JSON.parse(fs.readFileSync(path.join(source, 'package-lock.json')));
  fs.mkdirSync(path.join(root, 'node_modules'), { recursive: true });
  const modules = path.dirname(viteRoot);
  for (const name of ['vite', 'rolldown']) fs.symlinkSync(path.join(modules, name), path.join(root, 'node_modules', name));
  for (const name of ['fake-main', 'fake-worker', 'fake-unused']) {
    const directory = path.join(root, 'node_modules', name);
    fs.mkdirSync(directory);
    fs.writeFileSync(path.join(directory, 'package.json'), JSON.stringify({ name, version: '1.0.0', type: 'module', main: 'index.js', sideEffects: false, license: 'MIT' }));
    fs.writeFileSync(path.join(directory, 'index.js'), `export function value() { return ${JSON.stringify(name)}; }`);
    fs.writeFileSync(path.join(directory, 'LICENSE'), `Synthetic ${name} fixture notice, preserved exactly.\n`);
    lock.packages[`node_modules/${name}`] = { version: '1.0.0', resolved: `https://example.invalid/${name}.tgz`, integrity: 'sha512-fixture' };
  }
  fs.writeFileSync(path.join(root, 'package.json'), '{"name":"private-fixture","version":"0.0.0","type":"module"}');
  fs.writeFileSync(path.join(root, 'package-lock.json'), JSON.stringify(lock));
  fs.writeFileSync(path.join(root, 'index.html'), '<script type="module" src="/main.js"></script>');
  fs.writeFileSync(path.join(root, 'main.js'), `import {value} from 'fake-main'; import {value as unused} from 'fake-unused'; console.log(value()); new Worker(new URL('./worker.js', import.meta.url),{type:'module'});`);
  fs.writeFileSync(path.join(root, 'worker.js'), `import {value} from 'fake-worker'; postMessage(value());`);
  return root;
}
async function compile(root) {
  const notices = bundledRuntimeNotices();
  await build({ root, configFile: false, logLevel: 'silent', plugins: [notices.main],
    worker: { format: 'es', plugins: () => [notices.worker()] },
    build: { target: 'es2022', assetsInlineLimit: 0, minify: false } });
}

test('real emitted main/worker owners and actual license bytes, excluding dead npm code', async () => {
  const root = setup();
  try {
    await compile(root);
    const report = JSON.parse(fs.readFileSync(path.join(root, 'dist/licenses/npm/BUNDLED-NPM.json')));
    const main = report.packages.find((entry) => entry.name === 'fake-main');
    const worker = report.packages.find((entry) => entry.name === 'fake-worker');
    assert.ok(main.phases.includes('main'));
    assert.ok(worker.phases.includes('worker'));
    assert.ok(report.packages.some((entry) => entry.name === 'vite'));
    assert.ok(!report.packages.some((entry) => entry.name === 'fake-unused'));
    assert.equal(fs.readFileSync(path.join(root, 'dist', main.files[0].path), 'utf8'), 'Synthetic fake-main fixture notice, preserved exactly.\n');
    assert.ok(report.chunks.some((entry) => entry.phase === 'worker'));
    assert.ok(!JSON.stringify(report).includes(root));
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});

test('real compilation refuses absent required license text and lock identity drift', async () => {
  const root = setup();
  try {
    fs.rmSync(path.join(root, 'node_modules/fake-worker/LICENSE'));
    await assert.rejects(compile(root), /no actual license\/notice text/);
    fs.writeFileSync(path.join(root, 'node_modules/fake-worker/LICENSE'), 'Synthetic fixture notice');
    const lock = JSON.parse(fs.readFileSync(path.join(root, 'package-lock.json')));
    lock.packages['node_modules/fake-worker'].version = '2.0.0';
    fs.writeFileSync(path.join(root, 'package-lock.json'), JSON.stringify(lock));
    await assert.rejects(compile(root), /absent or ambiguous/);
  } finally { fs.rmSync(root, { recursive: true, force: true }); }
});
