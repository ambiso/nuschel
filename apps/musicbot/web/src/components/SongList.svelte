<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { buildRoute, formatDuration, similarityPercent, sonicPathStart } from "../logic.js";
  import { startItemDrag } from "../dnd.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";
  import SongRating from "./SongRating.svelte";

  /** `similarities` is optional and parallel to `songs`: OpenSubsonic scores, null when unknown. */
  let { songs, namespace, covers = false, similarities = null } = $props();
  const current = $derived(namespace !== null && namespace === store.browseKey && store.connection === "open" && !store.invalid);
  const playing = $derived(store.player?.current?.entry.track ?? null);
  const sonic = (song) => current && store.supportsSonic(song.library) && store.browseItemCurrent({ library: song.library, namespace });
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
      {#if similarities}
        {@const percent = similarityPercent(similarities[i])}
        {#if percent !== null}<span class="similarity muted" title="Sonic similarity">{percent}%</span>{/if}
      {/if}
      <span class="duration muted">{formatDuration(song.duration)}</span>
      <div class="actions" class:sonic={sonic(song)}>
        {#if sonic(song)}
          {@const from = sonicPathStart(playing, song)}
          <a class="icon-button" draggable="false" href={buildRoute({ view: "similar", library: song.library, id: song.id, namespace })} aria-label="Songs similar to {song.title}" title="Similar songs"><Icon name="similar" /></a>
          {#if from !== null}
            <a class="icon-button" draggable="false" href={buildRoute({ view: "path", library: song.library, from, to: song.id, namespace })} aria-label="Sonic path from now playing to {song.title}" title="Sonic path from now playing"><Icon name="path" /></a>
          {/if}
        {/if}
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
  .similarity { flex: none; font-variant-numeric: tabular-nums; font-size: 0.8rem; }
  .actions { display: flex; }
  .actions a.icon-button { color: inherit; }
  .actions a.icon-button:hover { color: var(--accent); }
  @media (max-width: 480px) { .number { display: none; } li { gap: 0.4rem; } .duration { display: none; } .actions.sonic { flex-wrap: wrap; justify-content: flex-end; max-width: 8.5rem; } }
</style>
