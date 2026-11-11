// SPDX-License-Identifier: EUPL-1.2
import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

// The build lands in dist/, which the web host serves at / (nix/web-module.nix).
// `npm run dev` proxies /relay to a local relay (nuschel-web-relay on :8090).
export default defineConfig({
  plugins: [svelte()],
  build: { target: "es2022", assetsInlineLimit: 0 },
  server: {
    proxy: { "/relay": { target: "ws://127.0.0.1:8090", ws: true } },
  },
});
