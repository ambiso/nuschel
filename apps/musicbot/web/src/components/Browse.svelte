<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { resource } from "../resource.svelte.js";
  import { query } from "../api.js";
  import { ALBUM_LISTS, buildRoute, formatDuration, totalDuration } from "../logic.js";
  import { startItemDrag } from "../dnd.js";
  import Cover from "./Cover.svelte";
  import MediaCard from "./MediaCard.svelte";
  import SongList from "./SongList.svelte";
  import Icon from "./Icon.svelte";

  let { route } = $props();
  let page = $state(0);
  const namespace = store.browseKey;
  const current = $derived(namespace !== null && namespace === store.browseKey && store.connection === "open" && !store.invalid);
  const result = resource(() => {
    if (!current || (route.library != null && !store.browseItemCurrent({ library: route.library, namespace }))
      || (route.id != null && route.namespace !== namespace)) return null;
    if (route.view === "search") return route.q.trim() ? query("api/search", { q: route.q }) : null;
    if (route.view === "albums") return query("api/albums", { library: route.library, list: route.list, page });
    return query(`api/${route.view}`, { library: route.library, ...(route.id == null ? {} : { id: route.id }) });
  }, () => store.browseSignal);
  const data = $derived(current ? result.data : null);
  const detail = $derived(data?.[route.view] ?? null);
  const songs = $derived(data?.songs ?? []);
  const titles = { albums: "Albums", artists: "Artists", playlists: "Playlists", search: "Search" };
  const heading = $derived(titles[route.view] ?? detail?.name ?? "Loading…");

  function changePage(next) {
    if (!current) return;
    page = next;
    document.getElementById("main-content")?.scrollTo({ top: 0 });
  }
</script>

<section class="browse" aria-labelledby="browse-title" aria-busy={result.loading}>
  <div class="section-heading">
    <h1 id="browse-title">{heading}</h1>
    {#if route.view === "albums"}
      <label class="sort"><span class="visually-hidden">Sort albums</span>
        <select value={route.list} disabled={!current} onchange={(event) => { if (current) store.navigate({ ...route, list: event.currentTarget.value }); }}>
          {#each ALBUM_LISTS as [value, label]}<option {value}>{label}</option>{/each}
        </select>
      </label>
    {/if}
  </div>
  {#if route.view === "albums"}
    <div class="pagination">
      <button disabled={!current || page === 0 || result.loading} onclick={() => changePage(page - 1)}>Previous</button>
      <span class="muted">Page {page + 1}</span>
      <button disabled={!current || !data?.more || result.loading} onclick={() => changePage(page + 1)}>Next</button>
    </div>
  {/if}

  {#if !current}
    <p class="notice muted" role="status">Library browsing will resume after the connection is restored.</p>
  {:else if result.loading}
    <p class="notice" role="status">Loading library…</p>
  {:else if result.error}
    <div class="notice error" role="alert"><p>{result.error}</p><button onclick={() => result.reload()}>Try again</button></div>
  {:else if route.view === "search" && !route.q.trim()}
    <p class="notice muted">Search for songs, albums or artists using the search box.</p>
  {:else if data}
    {#if route.view === "albums"}
      <div class="media-grid">{#each data.albums as album (album.id)}<MediaCard item={album} {namespace} />{/each}</div>
      {#if !data.albums.length}<p class="notice muted">No albums in this list.</p>{/if}
      {#if data.albums.length}
        <div class="pagination bottom">
          <button disabled={page === 0} onclick={() => changePage(page - 1)}>Previous</button>
          <span class="muted">Page {page + 1}</span>
          <button disabled={!data.more} onclick={() => changePage(page + 1)}>Next</button>
        </div>
      {/if}
    {:else if route.view === "artists"}
      {#each data.groups as group, i (i)}
        <section class="group" aria-label={group.name}>
          <h2>{group.name}</h2>
          <div class="media-grid">{#each group.artists as artist (artist.id)}<MediaCard item={artist} kind="artist" {namespace} />{/each}</div>
        </section>
      {/each}
      {#if !data.groups.some((group) => group.artists.length)}<p class="notice muted">No artists in this library.</p>{/if}
    {:else if route.view === "playlists"}
      <div class="media-grid">{#each data.playlists as playlist (playlist.id)}<MediaCard item={playlist} kind="playlist" {namespace} />{/each}</div>
      {#if !data.playlists.length}<p class="notice muted">No playlists in this library.</p>{/if}
    {:else if route.view === "search"}
      <p class="muted">Results for “{route.q}”</p>
      {#each data.results as library (library.library)}
        <section class="group" aria-label={library.name}>
          <h2>{library.name}</h2>
          {#if library.error}<p class="notice error" role="alert">{library.error}</p>{/if}
          {#if library.songs.length}<h3>Songs</h3><SongList songs={library.songs} {namespace} covers />{/if}
          {#if library.albums.length}<h3>Albums</h3><div class="media-grid">{#each library.albums as album (album.id)}<MediaCard item={album} {namespace} />{/each}</div>{/if}
          {#if library.artists.length}<h3>Artists</h3><div class="media-grid">{#each library.artists as artist (artist.id)}<MediaCard item={artist} kind="artist" {namespace} />{/each}</div>{/if}
          {#if !library.error && !library.songs.length && !library.albums.length && !library.artists.length}<p class="muted">No matches in this library.</p>{/if}
        </section>
      {/each}
      {#if !data.results.length}<p class="notice muted">No results.</p>{/if}
    {:else if detail}
      <div class="detail-header" role="group" aria-label={detail.name} draggable={current && route.view !== "artist"} ondragstart={(event) => {
        if (route.view !== "artist") startItemDrag(event, route.view, detail.library, detail.id, detail.name, namespace);
        else event.preventDefault();
      }}>
        <div class="detail-art"><Cover library={detail.library} art={detail.cover_art} size={400} round={route.view === "artist"} /></div>
        <div class="detail-info">
          {#if detail.artist}
            <p>{#if detail.artist_id}<a href={buildRoute({ view: "artist", library: detail.library, id: detail.artist_id, namespace })}>{detail.artist}</a>{:else}{detail.artist}{/if}</p>
          {/if}
          {#if detail.owner}<p class="muted">By {detail.owner}</p>{/if}
          <p class="muted metadata">
            {#if detail.year}<span>{detail.year}</span>{/if}
            {#if route.view === "artist"}<span>{data.albums.length} albums</span>
            {:else}<span>{songs.length} tracks</span><span>{formatDuration(detail.duration ?? totalDuration(songs))}</span>{/if}
          </p>
          {#if route.view !== "artist"}
            <div class="detail-actions">
              <button class="primary" disabled={!current || !store.player || !songs.length} onclick={() => store.add(store.selected, { kind: route.view, library: detail.library, id: detail.id, namespace })}><Icon name="add" /> Add to queue</button>
              <button disabled={!current || !store.player || !songs.length} onclick={() => store.add(store.selected, { kind: route.view, library: detail.library, id: detail.id, namespace }, 0)}><Icon name="next" /> Play next</button>
            </div>
          {/if}
        </div>
      </div>
      {#if route.view === "artist"}
        <h2>Albums</h2>
        <div class="media-grid">{#each data.albums as album (album.id)}<MediaCard item={album} {namespace} />{/each}</div>
        {#if !data.albums.length}<p class="notice muted">No albums by this artist.</p>{/if}
      {:else}
        <SongList {songs} {namespace} covers={route.view === "playlist"} />
        {#if !songs.length}<p class="notice muted">No tracks here.</p>{/if}
      {/if}
    {/if}
  {/if}
</section>

<style>
  .section-heading { display: flex; align-items: center; justify-content: space-between; gap: 1rem; flex-wrap: wrap; margin-bottom: 1rem; }
  h1 { margin: 0; overflow-wrap: anywhere; }
  .sort select { max-width: 100%; }
  .pagination { display: flex; align-items: center; justify-content: center; gap: 1rem; margin: 0 0 1.25rem; }
  .pagination.bottom { margin: 1.5rem 0 0; }
  .group { margin-bottom: 2rem; }
  .detail-header { display: flex; gap: 1.5rem; align-items: center; margin: 1rem 0 2rem; }
  .detail-art { width: 11rem; flex: none; }
  .detail-info { min-width: 0; }
  .metadata { display: flex; flex-wrap: wrap; gap: 0.7rem; }
  .detail-actions { display: flex; flex-wrap: wrap; gap: 0.5rem; }
  @media (max-width: 600px) { .detail-header { gap: 1rem; align-items: flex-start; } .detail-art { width: 6.5rem; } .detail-actions { flex-direction: column; align-items: flex-start; } }
</style>
