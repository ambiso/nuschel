// SPDX-License-Identifier: EUPL-1.2
import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { bundledRuntimeNotices } from "../../../web/app/build-runtime-notices.js";
import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";

const notices = bundledRuntimeNotices({ wasmDirectories: [], ancestryNotices: false });
let noticeRoot;
let noticeOut;

export default defineConfig({
  base: "./",
  plugins: [
    svelte({ emitCss: true }),
    notices.main,
    {
      name: "musicbot-notice-text",
      apply: "build",
      configResolved(config) { noticeRoot = config.root; noticeOut = path.resolve(config.root, config.build.outDir); },
      writeBundle() {
        const output = path.join(noticeOut, "licenses/runtime");
        const result = spawnSync("python3", [path.resolve(noticeRoot, "../../../release/web-notices.py"),
          "--profile", "musicbot", "--app", noticeOut,
          "--lock", path.join(noticeRoot, "package-lock.json"),
          "--own-license", path.resolve(noticeRoot, "../../../LICENSE"), "--output", output], { stdio: "inherit" });
        if (result.error || result.status !== 0) throw new Error("musicbot runtime notice completeness failed", { cause: result.error });
        fs.copyFileSync(path.join(output, "NOTICES.txt"), path.join(noticeOut, "licenses/NOTICES.txt"));
      },
    },
  ],
  server: {
    // The page runs at `/` and authenticates with the bot's cookie: open
    // `/u/<token>/` once so the bot sets it and redirects here. Vite serves
    // index.html and source modules (including /assets) itself.
    proxy: {
      "^/(?:api|art|licenses)(?:/|\\?|$)": {
        target: "http://localhost:8474",
        changeOrigin: true,
      },
      "^/u/": {
        target: "http://localhost:8474",
        changeOrigin: true,
      },
    },
  },
});
