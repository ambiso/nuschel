// SPDX-License-Identifier: EUPL-1.2
// Run the actual landing page in a private DOM stub. Link normalization must
// preserve certificate pins and every understood security requirement.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import { runInNewContext } from "node:vm";

const source = readFileSync(new URL("../web/site/join.js", import.meta.url), "utf8");
function page(fragment) {
  const nodes = new Map();
  const element = (id) => {
    if (!nodes.has(id)) nodes.set(id, { hidden: true, addEventListener() {} });
    return nodes.get(id);
  };
  runInNewContext(source, {
    location: new URL(`https://web.example.org/join/#${fragment}`),
    URLSearchParams,
    document: { readyState: "complete", getElementById: element },
    localStorage: { getItem() { return null; } },
  });
  return element;
}

test("landing page preserves pins and supported policies, including encoded keys", () => {
  for (const policy of ["policy=nuschl", "pol%69cy=nuschl", "policy=nuschl+nuschl", "policy=nuschl%2Bnuschl"]) {
    const element = page(`s=voice.example.org&fp=sha256:${"AB".repeat(32)}&${policy}`);
    assert.equal(element("actions").hidden, false);
    assert.ok(element("open-web").href.startsWith("/app/#t="));
    for (const id of ["open-app", "open-web"]) {
      assert.match(element(id).href, /policy=nuschl/);
      assert.match(element(id).href, new RegExp(`fp=sha256:${"ab".repeat(32)}`));
    }
  }
});

test("landing page refuses ambiguous or unsupported security requirements", () => {
  for (const fragment of [
    "s=voice.example.org&policy=nuschl&policy=",
    "s=voice.example.org&policy=&pol%69cy=nuschl",
    "s=voice.example.org&policy=nuschl+future",
    "s=voice.example.org&policy=future",
    "s=voice.example.org&%73=other.example",
    "s=voice.example.org&fp=sha256:bad",
    `s=voice.example.org&n=${"a".repeat(8192)}`,
  ]) {
    const element = page(fragment);
    assert.equal(element("actions").hidden, true, fragment);
    assert.equal(element("open-app").href, undefined, fragment);
  }
});
