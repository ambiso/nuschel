<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { onMount } from "svelte";
  import { store } from "../store.svelte.js";
  import { resource } from "../resource.svelte.js";
  import { getRating } from "../rating.js";
  import { postJSON, query } from "../api.js";

  let { library, songId, title, namespace, enabled = true, large = false } = $props();
  let root = $state();
  let visible = $state(false);
  let pending = $state(false);
  let writeError = $state("");
  let confirmed = $state(null);
  let preview = $state(null);
  let mutation = null;
  const current = $derived(enabled && typeof songId === "string" && songId.length > 0
    && store.browseItemCurrent({ library, namespace }));
  const key = $derived(current ? JSON.stringify([namespace, library, songId]) : null);
  const url = $derived(key === null ? null : query("api/rating", { library, id: songId }));
  const revision = $derived(store.snapshot?.rating_revision ?? 0);
  const namespaceSignal = $derived(store.browseSignal);
  const result = resource(() => {
    if (!visible || pending) return null;
    revision;
    return url;
  }, () => namespaceSignal, getRating);
  const rating = $derived(result.data?.rating ?? null);
  const validRating = $derived(Number.isInteger(rating) && rating >= 0 && rating <= 5);
  const readError = $derived(result.error || (result.data !== null && !validRating ? "The library returned an invalid rating." : null));
  const error = $derived(writeError || readError);
  const editable = $derived(current && !pending && !result.loading && !error && confirmed !== null);
  const shown = $derived(preview ?? confirmed ?? 0);

  onMount(() => {
    const observer = new IntersectionObserver(([entry]) => { visible = entry.isIntersecting; });
    observer.observe(root);
    return () => observer.disconnect();
  });

  $effect(() => {
    if (validRating) confirmed = rating;
  });

  $effect(() => {
    key;
    const signal = namespaceSignal;
    const invalidate = () => {
      mutation?.abort();
      mutation = null;
      pending = false;
      writeError = "";
      confirmed = null;
      preview = null;
    };
    signal?.addEventListener("abort", invalidate, { once: true });
    return () => {
      signal?.removeEventListener("abort", invalidate);
      invalidate();
    };
  });

  function reload() {
    if (!current || pending) return;
    writeError = "";
    result.reload();
  }

  async function save(star) {
    if (!editable) return;
    const value = confirmed === star ? 0 : star;
    const target = key;
    const controller = new AbortController();
    const signal = namespaceSignal;
    const abort = () => controller.abort();
    signal?.addEventListener("abort", abort, { once: true });
    mutation = controller;
    pending = true;
    preview = null;
    writeError = "";
    const active = () => mutation === controller && !controller.signal.aborted && current && key === target;
    try {
      await postJSON("api/rating", { library, id: songId, rating: value }, controller.signal);
      if (active()) result.reload();
    } catch (failure) {
      if (active()) writeError = failure instanceof Error ? failure.message : String(failure);
    } finally {
      signal?.removeEventListener("abort", abort);
      if (active()) {
        mutation = null;
        pending = false;
      }
    }
  }
</script>

<div class="rating" class:large bind:this={root} role="group" aria-label="Shared library rating for {title}" aria-busy={pending || result.loading} draggable="false">
  <div class="stars" title={library == null ? "This track's library is unavailable" : "Shared library rating. Click the selected star again to clear."}>
    {#each [1, 2, 3, 4, 5] as star (star)}
      <button
        class="star"
        class:filled={star <= shown}
        disabled={!editable}
        aria-label="{star} {star === 1 ? 'star' : 'stars'}"
        aria-pressed={confirmed === star}
        title={confirmed === star ? "Clear rating" : `Rate ${star} ${star === 1 ? 'star' : 'stars'}`}
        onpointerenter={(event) => { if (editable && event.pointerType === "mouse") preview = star; }}
        onpointerleave={() => (preview = null)}
        onclick={() => save(star)}
      ><svg viewBox="0 0 24 24" width="20" height="20" aria-hidden="true" focusable="false"><path d="m12 3 2.8 5.7 6.3.9-4.55 4.45 1.07 6.3L12 17.4l-5.62 2.95 1.07-6.3L2.9 9.6l6.3-.9Z" /></svg></button>
    {/each}
  </div>
  <span class="visually-hidden" role="status">{pending ? "Saving rating…" : result.loading ? "Loading rating…" : confirmed === null ? "Rating unavailable" : confirmed === 0 ? "Unrated" : `${confirmed} of 5 stars`}</span>
  {#if error && !pending}
    <span class="feedback error" role="alert" title={error}>Rating unavailable. <button class="retry" onclick={reload}>Retry</button></span>
  {/if}
</div>

<style>
  .rating { flex: none; max-width: 100%; }
  .stars { display: flex; width: max-content; }
  .star { display: grid; place-items: center; width: 24px; height: 28px; padding: 2px; border: none; border-radius: 4px; background: transparent; color: var(--muted); }
  .star.filled { color: var(--warning); }
  .star svg { display: block; fill: none; stroke: currentColor; stroke-width: 1.4; stroke-linejoin: round; }
  .star.filled svg { fill: currentColor; }
  .large .star { width: 44px; height: 48px; padding: 4px; }
  .large .star svg { width: 34px; height: 34px; }
  .large .feedback { font-size: 0.9rem; }
  .star:hover:not(:disabled) { background: var(--surface-hover); }
  .feedback { display: block; max-width: 15rem; font-size: 0.75rem; overflow-wrap: anywhere; }
  .retry { padding: 0; border: none; background: transparent; color: inherit; text-decoration: underline; font-size: inherit; }
</style>
