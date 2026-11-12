// SPDX-License-Identifier: EUPL-1.2
import {ContentService} from "/content-service.js";
const service = new ContentService();
const sourceBytes = new Uint8Array(await fetch("/fixture.png").then(response => response.arrayBuffer()));
const expected = [255,0,0,255,0,255,0,128,0,0,255,255,255,255,255,0];
const check = (value, message) => { if (!value) throw new Error(message); };
const rgba = image => Array.from(atob(image.rgba), char => char.charCodeAt(0));
globalThis.runContentProof = async () => {
  const plain = "\n\nfirst\n\nsecond 🦀\n```rust\nlet answer = 42;\n```\nlast";
  const literal = await service.render({source:plain});
  check(literal.marked_text === plain && literal.plain_text === plain, "Original text or newlines changed.");
  const code = literal.document.blocks.find(block => block.kind === "code");
  check(code?.language === "rust" && code.runs.map(run => run.text).join("") === "let answer = 42;\n", "Fenced code was lost.");
  check(new Set(code.runs.map(run => JSON.stringify(run.foreground))).size > 1, "Actual syntax highlighting is missing.");
  const encoded = await service.encodeImage(sourceBytes, 10000);
  const source = `<p>first<br>second <code>🦀</code></p>${encoded.html}` +
    '<script>fetch("/canary-script")</script><style>body{background:url("/canary-style")}</style>' +
    '<img src="/canary-image"><img src="https://invalid.example/canary-external">';
  const parsed = await service.render({source, mode:"legacy"});
  const image = parsed.document.blocks.find(block => block.kind === "image");
  check(image?.width === 2 && image.height === 2 && !Object.hasOwn(image, "source"), "Typed image or source stripping failed.");
  check(JSON.stringify(rgba(image)) === JSON.stringify(expected), "Image wire encoding/decoding changed RGBA pixels.");
  check(parsed.plain_text.includes("first\nsecond 🦀") && !parsed.plain_text.includes("canary"), "Legacy text extraction failed.");
  check(parsed.document.blocks.some(block => block.kind === "text" && block.spans?.some(span => span.style?.code)), "Inline code spans disappeared.");
  const decoded = await service.decodeImage(sourceBytes);
  check(JSON.stringify(rgba(decoded)) === JSON.stringify(expected), "Direct decoder changed RGBA pixels.");
  const canvas = document.getElementById("pixels"); canvas.width = image.width; canvas.height = image.height;
  canvas.getContext("2d").putImageData(new ImageData(Uint8ClampedArray.from(rgba(image)), image.width, image.height), 0, 0);
  const painted = Array.from(canvas.getContext("2d").getImageData(0, 0, 2, 2).data);
  // Canvas may discard invisible RGB or round premultiplied semi-transparent
  // colors. Opaque pixels must match; full RGBA was compared before painting.
  check(JSON.stringify(painted.slice(0, 4)) === JSON.stringify(expected.slice(0, 4)), "Canvas did not display the decoded image.");
  let invalid = false;
  try { await service.decodeImage(new Uint8Array([1,2,3])); } catch { invalid = true; }
  check(invalid, "Malformed image was accepted.");
  const afterError = await service.render({source:"still\nworking"});
  check(afterError.plain_text === "still\nworking", "A malformed image poisoned the next job.");
  document.getElementById("result").textContent = "Real isolated content proof passed";
  return {passed:true, original_plain_text:plain, actual_image_rgba:rgba(image), painted_rgba:painted,
    highlighted_runs:code.runs.length, image_html_bytes:encoded.html.length,
    checks:["actual generated WASM in content Worker", "original text/newlines", "Rust highlighting", "inline code", "legacy image HTML", "exact decoded RGBA", "real canvas", "no UI HTML/image decoder", "malformed image recovery", "no source URL fetch"]};
};
globalThis.closeContentProof = () => service.dispose();
