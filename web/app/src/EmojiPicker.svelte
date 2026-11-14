<!-- SPDX-License-Identifier: EUPL-1.2 -->
<!-- The picker beside the composer: emoji (stickers and GIFs to come). A
     click inserts at the composer's cursor and closes it; with Shift held it
     stays open for more. Images come only from the validated index (emoji.js). -->
<script>
  import { onMount } from "svelte";
  import { emoji as emojiData } from "./emoji-state.svelte.js";
  import { imageKey, imageUrl, inTone, searchEmoji, TONE_NAMES, TONE_SAMPLES, TONES } from "./emoji.js";
  let { onpick, onclose, recent = [] } = $props();
  const base = import.meta.env.BASE_URL;
  let tab = $state("emoji"), query = $state(""), choosingTone = $state(false), searchElement = $state(), element = $state();
  let tone = $state(storedTone());
  function storedTone() {
    try { const value = Number(localStorage.getItem("nuschl.emoji-tone.v1")); return Number.isInteger(value) && value >= 0 && value <= TONES ? value : 0; }
    catch { return 0; }
  }
  function chooseTone(value) {
    tone = value; choosingTone = false;
    try { localStorage.setItem("nuschl.emoji-tone.v1", String(value)); } catch { /* Applies until reload in private browsing. */ }
    searchElement?.focus();
  }
  const sections = $derived.by(() => {
    if (!emojiData.index) return [];
    if (query.trim()) return [{ name: "Results", emoji: searchEmoji(emojiData.index, query) }];
    return [...(recent.length ? [{ name: "Recently used", emoji: recent }] : []), ...emojiData.index.groups];
  });
  function pick(item, keepOpen) { onpick(inTone(item, tone), item, keepOpen); if (keepOpen) searchElement?.focus(); }
  function key(event) {
    if (event.key === "Escape") { event.preventDefault(); event.stopPropagation(); if (choosingTone) choosingTone = false; else onclose(); }
  }
  function searchKey(event) {
    if (event.key !== "Enter") return;
    event.preventDefault();
    const first = sections.find((section) => section.emoji.length)?.emoji[0];
    if (first) pick(first, event.shiftKey);
  }
  onMount(() => {
    searchElement?.focus();
    // A press outside closes it (the toggle button handles its own).
    const outside = (event) => { if (!element?.contains(event.target) && !event.target.closest?.(".emoji-toggle")) onclose(); };
    document.addEventListener("pointerdown", outside, true);
    return () => document.removeEventListener("pointerdown", outside, true);
  });
  function image(text) { const found = emojiData.index && imageKey(emojiData.index, text); return found ? imageUrl(base, found) : null; }
</script>
<div class="emoji-picker" role="dialog" aria-label="Emoji, stickers and GIFs" tabindex="-1" bind:this={element} onkeydown={key}>
  <div class="emoji-tabs" role="tablist" aria-label="Picker">
    {#each [["emoji","Emoji"],["stickers","Stickers"],["gifs","GIFs"]] as [id, label] (id)}
      <button type="button" role="tab" id={`emoji-tab-${id}`} aria-controls="emoji-panel" aria-selected={tab === id} class:selected={tab === id} onclick={() => { tab = id; choosingTone = false; }}>{label}</button>
    {/each}
  </div>
  <div class="emoji-panel" id="emoji-panel" role="tabpanel" aria-labelledby={`emoji-tab-${tab}`}>
    {#if tab !== "emoji"}
      <p class="emoji-soon">{tab === "stickers" ? "Stickers" : "GIFs"} are coming soon.</p>
    {:else if emojiData.failed}
      <p class="emoji-soon" role="alert">Emoji could not be loaded.</p>
    {:else if !emojiData.index}
      <p class="emoji-soon" role="status">Loading emoji…</p>
    {:else}
      {#if choosingTone}
        <div class="emoji-tones" role="radiogroup" aria-label="Skin tone">
          {#each TONE_SAMPLES as sample, value (value)}
            <button type="button" role="radio" aria-checked={tone === value} aria-label={TONE_NAMES[value]} title={TONE_NAMES[value]} class:selected={tone === value} onclick={() => chooseTone(value)}><img src={image(sample)} alt="" width="22" height="22" draggable="false"></button>
          {/each}
        </div>
      {:else}
        <div class="emoji-search">
          <label class="sr-only" for="emoji-search">Search emoji</label>
          <input id="emoji-search" type="search" placeholder="Search emoji" autocomplete="off" spellcheck="false" maxlength="64" bind:value={query} bind:this={searchElement} onkeydown={searchKey}>
          <button type="button" class="emoji-tone" aria-label={`Skin tone: ${TONE_NAMES[tone]}`} title="Skin tone" aria-expanded="false" onclick={() => choosingTone = true}><img src={image(TONE_SAMPLES[tone])} alt="" width="22" height="22" draggable="false"></button>
        </div>
      {/if}
      <div class="emoji-grid">
        {#each sections as section (section.name)}
          <h3>{section.name}</h3>
          <div class="emoji-cells">
            {#each section.emoji as item (item.emoji)}
              {@const shown = inTone(item, tone)}
              <button type="button" aria-label={item.name} title={item.name} onclick={(event) => pick(item, event.shiftKey)}><img src={image(shown)} alt="" width="28" height="28" loading="lazy" draggable="false"></button>
            {/each}
          </div>
        {:else}
          <p class="emoji-soon">No emoji found</p>
        {/each}
        {#if query.trim() && sections[0] && !sections[0].emoji.length}<p class="emoji-soon" role="status">No emoji found</p>{/if}
      </div>
      <small class="emoji-hint">Shift+click adds several · Twemoji, CC-BY 4.0</small>
    {/if}
  </div>
</div>
