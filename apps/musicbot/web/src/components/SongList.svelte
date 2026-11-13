<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { buildRoute, formatDuration } from "../logic.js";
  import { startItemDrag } from "../dnd.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";
  import SongRating from "./SongRating.svelte";

  let { songs, namespace, covers = false } = $props();
  const current = $derived(namespace !== null && namespace === store.browseKey && store.connection === "open" && !store.invalid);
</script>

<ol class="song-list" aria-label="Tracks">
  {#each songs as song, i (`${song.library}:${song.id}:${i}`)}
    <li draggable={current} ondragstart={(event) => startItemDrag(event, "song", song.library, song.id, song.title, namespace)}>
      <span class="number">{song.track ?? i + 1}</span>
      {#if covers}<Cover library={song.library} art={song.cover_art} size={96} px={42} />{/if}
      <div class="description">
        <div class="title" title={song.title}>{song.title}</div>
        <div class="secondary muted">
          {#if song.artist_id}<a href={buildRoute({ view: "artist", library: song.library, id: song.artist_id, namespace })}>{song.artist || "Unknown artist"}</a>
          {:else if song.artist}<span>{song.artist}</span>{/if}
          {#if covers && song.album}
            {#if song.album_id}<a href={buildRoute({ view: "album", library: song.library, id: song.album_id, namespace })}>{song.album}</a>
            {:else}<span>{song.album}</span>{/if}
          {/if}
        </div>
      </div>
      <span class="duration muted">{formatDuration(song.duration)}</span>
      <div class="actions">
        <button class="icon-button" disabled={!store.player || !current} aria-label="Add {song.title} to queue" title="Add to queue" onclick={() => store.add(store.selected, { kind: "song", library: song.library, id: song.id, namespace })}><Icon name="add" /></button>
        <button class="icon-button" disabled={!store.player || !current} aria-label="Play {song.title} next" title="Play next" onclick={() => store.add(store.selected, { kind: "song", library: song.library, id: song.id, namespace }, 0)}><Icon name="next" /></button>
        <SongRating library={song.library} songId={song.id} title={song.title} {namespace} />
      </div>
    </li>
  {/each}
</ol>

<style>
  .song-list { list-style: none; padding: 0; margin: 0; }
  li { display: flex; align-items: center; gap: 0.65rem; min-height: 3.6rem; padding: 0.4rem; border-bottom: 1px solid var(--border); cursor: grab; border-radius: 4px; }
  li:hover, li:focus-within { background: var(--surface-hover); }
  .number { flex: none; width: 2rem; text-align: right; font-variant-numeric: tabular-nums; color: var(--muted); font-size: 0.85rem; }
  .description { flex: 1; min-width: 0; }
  .title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .secondary { display: flex; gap: 0.6rem; font-size: 0.8rem; }
  .secondary > * { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .duration { font-variant-numeric: tabular-nums; font-size: 0.85rem; }
  .actions { display: flex; }
  @media (max-width: 480px) { .number { display: none; } li { gap: 0.4rem; } .duration { display: none; } }
</style>
