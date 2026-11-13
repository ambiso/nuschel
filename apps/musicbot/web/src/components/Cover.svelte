<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { artUrl } from '../api.js';
  import { loadCover, paintCover, MAX_EDGE } from '../cover-pixels.js';
  import Icon from './Icon.svelte';
  import { store } from '../store.svelte.js';

  let { library, art, size = 300, px = null, round = false } = $props();
  const bound = $derived(Math.max(32, Math.min(MAX_EDGE, Math.trunc(size))));
  const src = $derived(art ? artUrl(library, art, bound, store.snapshot?.art_keys?.[library]) : null);
  let container = $state(null), canvas = $state(null), visible = $state(false), ready = $state(false);

  $effect(() => {
    const node = container;
    if (!node || typeof IntersectionObserver === 'undefined') return;
    const observer = new IntersectionObserver(entries => {
      for (const entry of entries) if (entry.target === node) visible = entry.isIntersecting;
    });
    observer.observe(node);
    return () => observer.disconnect();
  });
  $effect(() => {
    const target = canvas, url = src, maximum = bound, shown = visible;
    const namespace = store.browseKey, namespaceSignal = store.browseSignal;
    const allowed = store.connection === 'open' && !store.invalid;
    if (!target) return;
    ready = false;
    target.width = 0; target.height = 0;
    if (!url || !shown || !namespace || !allowed || namespaceSignal?.aborted) return;
    const request = new AbortController();
    let current = true, release = null;
    const invalidate = () => { current = false; request.abort(); release?.(); ready = false; target.width = 0; target.height = 0; };
    namespaceSignal?.addEventListener('abort', invalidate, { once: true });
    loadCover(url, maximum, request.signal, namespace).then(value => {
      if (!current || request.signal.aborted || src !== url || canvas !== target || bound !== maximum
          || !visible || store.browseKey !== namespace || store.connection !== 'open' || store.invalid) return;
      release = paintCover(target, value);
      ready = release !== null;
    }).catch(() => { /* Keep the decorative placeholder; never decode an encoded fallback. */ });
    return () => {
      namespaceSignal?.removeEventListener('abort', invalidate);
      invalidate();
    };
  });
</script>

<div bind:this={container} class="cover" class:fill={px === null} class:round style:width={px === null ? null : `${px}px`} style:height={px === null ? null : `${px}px`}>
  <canvas width="0" height="0" bind:this={canvas} class:ready aria-hidden="true"></canvas>
  {#if !ready}<div class="placeholder"><Icon name="note" size={px ? Math.max(16, Math.round(px * 0.4)) : 48} /></div>{/if}
</div>

<style>
  .cover { position: relative; flex: none; overflow: hidden; border-radius: 6px; background: var(--placeholder); }
  .cover.fill { width: 100%; aspect-ratio: 1; }
  .cover.round { border-radius: 50%; }
  canvas { display: block; width: 100%; height: 100%; object-fit: cover; opacity: 0; }
  canvas.ready { opacity: 1; }
  .placeholder { position: absolute; inset: 0; display: grid; place-items: center; color: var(--muted); opacity: 0.6; }
</style>
