<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { artUrl } from "../api.js";
  import Icon from "./Icon.svelte";

  // `size`: requested image width in pixels. `px`: displayed size; without it
  // the cover fills the width of its container as a square.
  let { library, art, size = 300, px = null, round = false } = $props();
  const src = $derived(art ? artUrl(library, art, size) : null);
  let failed = $state(null);
</script>

<div class="cover" class:fill={px === null} class:round style:width={px === null ? null : `${px}px`} style:height={px === null ? null : `${px}px`}>
  {#if src && failed !== src}
    <img {src} alt="" width={size} height={size} loading="lazy" decoding="async" draggable="false" onerror={() => (failed = src)} />
  {:else}
    <div class="placeholder"><Icon name="note" size={px ? Math.max(16, Math.round(px * 0.4)) : 48} /></div>
  {/if}
</div>

<style>
  .cover {
    flex: none;
    overflow: hidden;
    border-radius: 6px;
    background: var(--placeholder);
  }
  .cover.fill { width: 100%; aspect-ratio: 1; }
  .cover.round { border-radius: 50%; }
  img { display: block; width: 100%; height: 100%; object-fit: cover; }
  .placeholder {
    width: 100%;
    height: 100%;
    display: grid;
    place-items: center;
    color: var(--muted);
    opacity: 0.6;
  }
</style>
