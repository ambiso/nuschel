<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { resource } from "../resource.svelte.js";
  import { query } from "../api.js";
  import { buildRoute, formatDuration, sonicPathStart } from "../logic.js";
  import { startItemDrag } from "../dnd.js";
  import Cover from "./Cover.svelte";
  import MediaCard from "./MediaCard.svelte";
  import SongList from "./SongList.svelte";
  import SongRating from "./SongRating.svelte";
  import Icon from "./Icon.svelte";

  /** `data` is the `api/song` response; `current` says whether `namespace` still holds. */
  let { data, namespace, current } = $props();
  const SIMILAR_SHOWN = 10;
  const song = $derived(data.song);
  const details = $derived(data.details ?? {});
  const item = $derived({ kind: "song", library: song.library, id: song.id, namespace });
  const playing = $derived(store.player?.current?.entry.track ?? null);
  const pathFrom = $derived(sonicPathStart(playing, song));
  const sonic = $derived(current && store.supportsSonic(song.library) && store.browseItemCurrent({ library: song.library, namespace }));
  const similar = resource(() => (sonic ? query("api/similar", { library: song.library, id: song.id }) : null), () => store.browseSignal);
  const matches = $derived(sonic ? (similar.data?.matches ?? []).slice(0, SIMILAR_SHOWN) : []);
  // One card per album; tracks without an album ID have no album page to open.
  const albums = $derived((data.appearances ?? []).filter((track) => track.album_id).map((track) => ({
    library: track.library, id: track.album_id, name: track.album || "Unknown album", artist: track.artist, cover_art: track.cover_art,
  })));
  const format = $derived([song.suffix?.toUpperCase(), details.bit_rate ? `${details.bit_rate} kbit/s` : null].filter(Boolean).join(", "));
  const position = $derived([song.track != null ? `Track ${song.track}` : null, details.disc_number != null ? `Disc ${details.disc_number}` : null].filter(Boolean).join(", "));
</script>

<div class="detail-header" role="group" aria-label={song.title} draggable={current} ondragstart={(event) => {
  if (current) startItemDrag(event, "song", song.library, song.id, song.title, namespace);
  else event.preventDefault();
}}>
  <div class="detail-art"><Cover library={song.library} art={song.cover_art} size={400} /></div>
  <div class="detail-info">
    <p>{#if song.artist_id}<a href={buildRoute({ view: "artist", library: song.library, id: song.artist_id, namespace })}>{song.artist || "Unknown artist"}</a>{:else}{song.artist || "Unknown artist"}{/if}</p>
    {#if song.album}
      <p class="muted">From {#if song.album_id}<a href={buildRoute({ view: "album", library: song.library, id: song.album_id, namespace })}>{song.album}</a>{:else}{song.album}{/if}</p>
    {/if}
    <dl class="facts">
      {#if position}<div><dt>Position</dt><dd>{position}</dd></div>{/if}
      {#if song.duration != null}<div><dt>Length</dt><dd>{formatDuration(song.duration)}</dd></div>{/if}
      {#if details.year}<div><dt>Year</dt><dd>{details.year}</dd></div>{/if}
      {#if details.genre}<div><dt>Genre</dt><dd>{details.genre}</dd></div>{/if}
      {#if format}<div><dt>Format</dt><dd>{format}</dd></div>{/if}
      {#if details.play_count != null}<div><dt>Plays</dt><dd>{details.play_count.toLocaleString()}</dd></div>{/if}
    </dl>
    <SongRating library={song.library} songId={song.id} title={song.title} {namespace} />
    <div class="detail-actions">
      <button class="primary" disabled={!current || !store.player} onclick={() => store.add(store.selected, item)}><Icon name="add" /> Add to queue</button>
      <button disabled={!current || !store.player} onclick={() => store.add(store.selected, item, 0)}><Icon name="next" /> Play next</button>
      {#if sonic && pathFrom !== null}
        <a class="action-link" draggable="false" href={buildRoute({ view: "path", library: song.library, from: pathFrom, to: song.id, namespace })} title="A sonic path from the song playing now to this one"><Icon name="path" size={18} /> Sonic path from now playing</a>
      {/if}
    </div>
  </div>
</div>

{#if sonic}
  <section class="group" aria-labelledby="song-similar">
    <div class="group-heading">
      <h2 id="song-similar">Similar songs</h2>
      <a class="action-link" href={buildRoute({ view: "similar", library: song.library, id: song.id, namespace })}><Icon name="similar" size={18} /> Show all</a>
    </div>
    {#if similar.loading}
      <p class="muted" role="status">Looking for similar songs…</p>
    {:else if similar.error}
      <div class="notice error" role="alert"><p>{similar.error}</p><button onclick={() => similar.reload()}>Try again</button></div>
    {:else if matches.length}
      <SongList songs={matches.map((match) => match.track)} {namespace} covers similarities={matches.map((match) => match.similarity)} />
    {:else}
      <p class="muted">The library found no similar songs.</p>
    {/if}
  </section>
{/if}

<section class="group" aria-labelledby="song-albums">
  <h2 id="song-albums">On albums</h2>
  {#if albums.length}
    <div class="media-grid">{#each albums as album, i (`${album.id}:${i}`)}<MediaCard item={album} {namespace} />{/each}</div>
  {:else if !data.appearances_error}
    <p class="muted">This song is not on any album in the library.</p>
  {/if}
  {#if data.appearances_error}<p class="muted small">Other albums could not be looked up: {data.appearances_error}</p>{/if}
</section>

<style>
  .detail-header { display: flex; gap: 1.5rem; align-items: center; margin: 1rem 0 2rem; }
  .detail-header[draggable="true"] { cursor: grab; }
  .detail-art { width: 11rem; flex: none; }
  .detail-info { min-width: 0; display: grid; gap: 0.5rem; }
  .detail-info p { margin: 0; overflow-wrap: anywhere; }
  .facts { display: flex; flex-wrap: wrap; gap: 0.35rem 1.25rem; margin: 0.25rem 0 0; font-size: 0.85rem; }
  .facts dt { color: var(--muted); font-size: 0.75rem; }
  .facts dd { margin: 0; }
  .detail-actions { display: flex; flex-wrap: wrap; align-items: center; gap: 0.5rem; }
  .action-link { display: inline-flex; align-items: center; gap: 0.35rem; font-size: 0.9rem; }
  .group { margin-bottom: 2rem; }
  .group-heading { display: flex; align-items: baseline; justify-content: space-between; gap: 1rem; flex-wrap: wrap; }
  @media (max-width: 600px) { .detail-header { gap: 1rem; align-items: flex-start; } .detail-art { width: 6.5rem; } .detail-actions { flex-direction: column; align-items: flex-start; } }
</style>
