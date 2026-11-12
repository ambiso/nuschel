<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { resource } from "../resource.svelte.js";
  import { postJSON, query } from "../api.js";

  let { library, songId, title, namespace, enabled = true, placement = "below" } = $props();
  let open = $state(false);
  let pending = $state(false);
  let writeError = $state("");
  let draft = $state(0);
  let root = $state();
  let button = $state();
  let mutation = null;
  const id = $props.id();
  const current = $derived(enabled && typeof songId === "string" && songId.length > 0
    && store.browseItemCurrent({ library, namespace }));
  // Keep reloads tied to primitive identity and rating revision.
  const key = $derived(current ? JSON.stringify([namespace, library, songId]) : null);
  const url = $derived(key === null ? null : query("api/rating", { library, id: songId }));
  const revision = $derived(store.snapshot?.rating_revision ?? 0);
  const namespaceSignal = $derived(store.browseSignal);
  const result = resource(() => {
    if (!open || pending) return null;
    revision;
    return url;
  }, () => namespaceSignal);
  const rating = $derived(result.data?.rating ?? null);
  const validRating = $derived(Number.isInteger(rating) && rating >= 0 && rating <= 5);
  const readError = $derived(result.error || (result.data !== null && !validRating ? "The library returned an invalid rating." : null));

  $effect(() => {
    const value = rating;
    if (value !== null) draft = value;
  });

  $effect(() => {
    key;
    const signal = namespaceSignal;
    const invalidate = () => {
      mutation?.abort();
      mutation = null;
      pending = false;
      writeError = "";
      draft = 0;
      open = false;
    };
    signal?.addEventListener("abort", invalidate, { once: true });
    return () => {
      signal?.removeEventListener("abort", invalidate);
      invalidate();
    };
  });

  $effect(() => {
    if (!open) return;
    const pointerdown = (event) => {
      if (!root.contains(event.target)) open = false;
    };
    const keydown = (event) => {
      if (event.key !== "Escape") return;
      open = false;
      button.focus();
    };
    document.addEventListener("pointerdown", pointerdown);
    document.addEventListener("keydown", keydown);
    return () => {
      document.removeEventListener("pointerdown", pointerdown);
      document.removeEventListener("keydown", keydown);
    };
  });

  function reload() {
    if (!current || pending) return;
    writeError = "";
    result.reload();
  }

  async function save(value) {
    if (!current || !open || pending || result.loading || readError || writeError || !validRating
      || !Number.isInteger(value) || value < 0 || value > 5) return;
    const target = key;
    const controller = new AbortController();
    const signal = namespaceSignal;
    const abort = () => controller.abort();
    signal?.addEventListener("abort", abort, { once: true });
    mutation = controller;
    pending = true;
    writeError = "";
    const active = () => mutation === controller && !controller.signal.aborted && current && key === target;
    try {
      await postJSON("api/rating", { library, id: songId, rating: value }, controller.signal);
      // Read the backend after a successful write.
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

<div class="rating" bind:this={root} draggable="false">
  <button
    class="toggle"
    bind:this={button}
    disabled={!current}
    aria-label="Rate {title} in the shared library account"
    aria-expanded={open && current}
    aria-controls="{id}-panel"
    title={library == null ? "This track's library is unavailable" : "Shared library rating"}
    onclick={() => (open = !open)}
  >Rate</button>
  {#if open && current}
    <div class="panel" class:above={placement === "above"} id="{id}-panel" role="group" aria-label="Shared library rating for {title}" aria-busy={pending || result.loading}>
      <strong>Shared library rating</strong>
      <p class="explanation muted">Shared by listeners using this library account. The latest rating wins.</p>
      {#if pending}
        <p role="status">Saving to the shared library…</p>
      {:else if result.loading}
        <p class="muted" role="status">Loading current library rating…</p>
      {:else if readError}
        <p class="error" role="alert">Could not load rating: {readError}</p>
      {:else if validRating}
        <p role="status">Current rating: {rating === 0 ? "Unrated (0)" : `${rating} of 5 stars`}</p>
      {/if}
      {#if writeError}
        <p class="error" role="alert">Rating write was not confirmed: {writeError}. Reload the current rating before trying again.</p>
      {/if}
      {#if !pending && (readError || writeError)}
        <button onclick={reload}>Reload rating</button>
      {/if}
      <label for="{id}-value">New rating</label>
      <select id="{id}-value" bind:value={draft} disabled={pending || result.loading || !!readError || !!writeError || !validRating}>
        <option value={0}>0 — Unrated</option>
        <option value={1}>1 star — ★</option>
        <option value={2}>2 stars — ★★</option>
        <option value={3}>3 stars — ★★★</option>
        <option value={4}>4 stars — ★★★★</option>
        <option value={5}>5 stars — ★★★★★</option>
      </select>
      <div class="actions">
        <button class="primary" disabled={pending || result.loading || !!readError || !!writeError || !validRating} onclick={() => save(draft)}>Save rating</button>
        <button disabled={pending || result.loading || !!readError || !!writeError || !validRating || rating === 0} onclick={() => save(0)}>Clear rating</button>
      </div>
    </div>
  {/if}
</div>

<style>
  .rating { position: relative; flex: none; }
  .toggle { padding: 0.3rem 0.5rem; font-size: 0.8rem; }
  .panel {
    position: absolute;
    right: 0;
    top: calc(100% + 0.4rem);
    z-index: 30;
    width: min(19rem, calc(100vw - 1.5rem));
    max-height: calc(100dvh - 9rem);
    overflow-y: auto;
    display: grid;
    gap: 0.4rem;
    padding: 0.9rem;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--surface);
    box-shadow: var(--shadow);
    font-size: 0.875rem;
    overflow-wrap: anywhere;
  }
  .panel.above { top: auto; bottom: calc(100% + 0.6rem); }
  p { margin: 0; }
  .explanation { font-size: 0.8rem; }
  select { width: 100%; min-width: 0; }
  .actions { display: flex; flex-wrap: wrap; gap: 0.4rem; }
</style>
