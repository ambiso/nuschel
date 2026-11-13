<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { buildRoute, formatDuration } from "../logic.js";
  import { startItemDrag } from "../dnd.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";

  let { item, namespace, kind = "album" } = $props();
  const current = $derived(store.browseItemCurrent({ library: item.library, namespace }));
  const addable = $derived(kind !== "artist" && current);
  const href = $derived(buildRoute({ view: kind, library: item.library, id: item.id, namespace }));
  const subtitle = $derived(kind === "album" ? item.artist : kind === "playlist" ? item.owner : item.album_count == null ? null : `${item.album_count} albums`);
</script>

<article class="media-card" draggable={addable} ondragstart={(event) => {
  if (addable) startItemDrag(event, kind, item.library, item.id, item.name, namespace);
  else event.preventDefault();
}}>
  <a class="art-link" {href} draggable="false" aria-label="Open {item.name}">
    <Cover library={item.library} art={item.cover_art} round={kind === "artist"} />
  </a>
  <a class="name" {href} draggable="false" title={item.name}>{item.name}</a>
  {#if subtitle}<div class="muted subtitle" title={subtitle}>{subtitle}</div>{/if}
  {#if item.year || item.song_count != null}
    <div class="muted metadata">
      {#if item.year}<span>{item.year}</span>{/if}
      {#if item.song_count != null}<span>{item.song_count} tracks</span>{/if}
      {#if item.duration != null}<span>{formatDuration(item.duration)}</span>{/if}
    </div>
  {/if}
  {#if addable}
    <div class="actions">
      <button class="icon-button" disabled={!store.player || !current} aria-label="Add {item.name} to queue" title="Add to queue" onclick={() => store.add(store.selected, { kind, library: item.library, id: item.id, namespace })}><Icon name="add" /></button>
      <button class="icon-button" disabled={!store.player || !current} aria-label="Play {item.name} next" title="Play next" onclick={() => store.add(store.selected, { kind, library: item.library, id: item.id, namespace }, 0)}><Icon name="next" /></button>
    </div>
  {/if}
</article>

<style>
  .media-card { position: relative; min-width: 0; display: flex; flex-direction: column; gap: 0.25rem; padding: 0.65rem; border-radius: 8px; background: var(--surface); }
  .media-card[draggable="true"] { cursor: grab; }
  .media-card:hover, .media-card:focus-within { background: var(--surface-hover); }
  .art-link { display: block; margin-bottom: 0.35rem; }
  .name { font-weight: 600; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .subtitle { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 0.85rem; }
  .metadata { display: flex; flex-wrap: wrap; gap: 0.4rem; font-size: 0.75rem; }
  .actions { display: flex; justify-content: flex-end; margin-top: auto; padding-top: 0.25rem; }
</style>
