<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { buildRoute, formatDuration, gapIndex, moveTarget, stepTarget } from "../logic.js";
  import { carriesEntry, carriesItem, droppedEntry, droppedItem, leftElement, startEntryDrag } from "../dnd.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";

  let { compact = false } = $props();
  const player = $derived(store.player);
  const selected = $derived(store.selected);
  const queue = $derived(store.queue(selected));
  const current = $derived(player?.current ?? null);
  const players = $derived(store.snapshot?.players ?? []);
  const attached = $derived(store.snapshot?.channels.filter((channel) => channel.player === selected).length ?? 0);
  let root = $state();
  let gap = $state(null);
  let dragged = $state(null);
  const id = $props.id();

  $effect(() => {
    selected;
    gap = null;
    dragged = null;
  });

  function dropGap(event) {
    const row = event.target.closest?.("[data-queue-index]");
    if (row && root.contains(row)) {
      const rect = row.getBoundingClientRect();
      return gapIndex(Number(row.dataset.queueIndex), event.clientY - rect.top, rect.height);
    }
    if (event.target.closest?.("[data-play-next]")) return 0;
    return queue.length;
  }

  function dragover(event) {
    if (!player || (!carriesItem(event) && !carriesEntry(event))) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = carriesItem(event) ? "copy" : "move";
    gap = dropGap(event);
  }

  function drop(event) {
    const target = dropGap(event);
    gap = null;
    dragged = null;
    if (!player) return;
    if (carriesItem(event)) {
      const item = droppedItem(event);
      if (!item) return;
      event.preventDefault();
      store.add(selected, item, target);
    } else if (carriesEntry(event)) {
      const entry = droppedEntry(event);
      const from = queue.findIndex((item) => item.id === entry);
      const to = moveTarget(from, target, queue.length);
      event.preventDefault();
      if (to !== null) store.move(selected, entry, to);
    }
  }

  function reorder(event, entry) {
    if (!event.altKey || (event.key !== "ArrowUp" && event.key !== "ArrowDown")) return;
    event.preventDefault();
    event.stopPropagation();
    const from = queue.findIndex((item) => item.id === entry);
    const to = stepTarget(from, event.key === "ArrowUp" ? -1 : 1, queue.length);
    if (to !== null) store.move(selected, entry, to);
  }
</script>

<section class="queue-panel" class:compact bind:this={root} aria-labelledby="{id}-title" ondragover={dragover} ondrop={drop} ondragleave={(event) => { if (leftElement(event, root)) gap = null; }}>
  <div class="queue-heading">
    <h1 id="{id}-title">Queue</h1>
    {#if queue.length}<button class="small" onclick={() => store.clear(selected)}>Clear upcoming</button>{/if}
  </div>
  {#if players.length}
    <label class="queue-choice"><span class="visually-hidden">Selected queue</span>
      <select value={selected} onchange={(event) => store.selectQueue(Number(event.currentTarget.value))}>
        {#each players as p, i (i)}<option value={i}>{p.name || `Queue ${i + 1}`}</option>{/each}
      </select>
    </label>
  {/if}
  {#if player}
    <p class="muted queue-meta">{queue.length} upcoming · {attached} attached {attached === 1 ? "channel" : "channels"}</p>
    {#if player.error}<p class="notice error" role="alert">Playback error: {player.error}</p>{/if}
    <div class="now-playing" data-play-next>
      <h2>Now playing</h2>
      {#if current}
        {@const track = current.entry.track}
        <div class="current-track">
          <Cover library={track.library} art={track.cover_art} size={128} px={56} />
          <div class="description">
            <div class="title" title={track.title}>{#if track.library != null && track.id}<a href={buildRoute({ view: "song", library: track.library, id: track.id, namespace: store.browseKey })}>{track.title}</a>{:else}{track.title}{/if}</div>
            {#if track.artist}<div class="muted secondary">{#if track.artist_id}<a href={buildRoute({ view: "artist", library: track.library, id: track.artist_id, namespace: store.browseKey })}>{track.artist}</a>{:else}{track.artist}{/if}</div>{/if}
            {#if track.album}<div class="muted secondary">{#if track.album_id}<a href={buildRoute({ view: "album", library: track.library, id: track.album_id, namespace: store.browseKey })}>{track.album}</a>{:else}{track.album}{/if}</div>{/if}
            <div class="muted added">Added by {current.entry.added_by}</div>
            {#if store.supportsSonic(track.library) && store.browseItemCurrent({ library: track.library, namespace: store.browseKey })}
              <a class="similar-link" href={buildRoute({ view: "similar", library: track.library, id: track.id, namespace: store.browseKey })} aria-label="Songs similar to {track.title}" title="Similar songs"><Icon name="similar" size={16} /> Similar songs</a>
            {/if}
          </div>
        </div>
      {:else}<p class="muted">Nothing is playing. Add music from the library.</p>{/if}
    </div>
    <div class="upcoming-heading"><h2>Up next</h2><span class="muted small">Drag to reorder</span></div>
    <p class="visually-hidden" id="{id}-help">Use the move handle and Alt plus Up or Down arrow to reorder a track.</p>
    <ol class="queue-list" aria-label="Upcoming tracks">
      {#each queue as entry, i (entry.id)}
        <li data-queue-index={i} draggable="true" class:drop-before={gap === i} class:drop-after={gap === queue.length && i === queue.length - 1} class:dragged={dragged === entry.id} ondragstart={(event) => { dragged = entry.id; startEntryDrag(event, entry.id); }} ondragend={() => { dragged = null; gap = null; }}>
          <button class="icon-button grip" draggable="true" aria-label="Move {entry.track.title}" aria-describedby="{id}-help" title="Drag to reorder, or Alt + ↑ / ↓" onkeydown={(event) => reorder(event, entry.id)}><Icon name="grip" /></button>
          <Cover library={entry.track.library} art={entry.track.cover_art} size={96} px={40} />
          <div class="description">
            <div class="title" title={entry.track.title}>{#if entry.track.library != null && entry.track.id}<a href={buildRoute({ view: "song", library: entry.track.library, id: entry.track.id, namespace: store.browseKey })} draggable="false">{entry.track.title}</a>{:else}{entry.track.title}{/if}</div>
            <div class="muted secondary" title={entry.track.artist ?? ""}>
              {#if entry.track.artist_id}<a href={buildRoute({ view: "artist", library: entry.track.library, id: entry.track.artist_id, namespace: store.browseKey })}>{entry.track.artist || "Unknown artist"}</a>{:else}{entry.track.artist ?? "Unknown artist"}{/if}
            </div>
            <div class="muted added">Added by {entry.added_by}</div>
          </div>
          <span class="muted duration">{formatDuration(entry.track.duration)}</span>
          <div class="row-actions">
            <button class="icon-button" disabled={i === 0} aria-label="Move {entry.track.title} to play next" title="Play next" onclick={() => store.move(selected, entry.id, 0)}><Icon name="next" size={18} /></button>
            <button class="icon-button" aria-label="Remove {entry.track.title}" title="Remove from queue" onclick={() => store.remove(selected, entry.id)}><Icon name="remove" size={18} /></button>
          </div>
        </li>
      {/each}
    </ol>
    <div class="drop-end" class:target={gap === queue.length}>
      <p class="muted">{gap === queue.length ? "Drop here to add at the end" : queue.length ? "Drop music here to add to the queue" : "Your queue is empty. Add tracks, albums or playlists, or drop them here."}</p>
    </div>
  {:else}<p class="notice muted">No playback queues are configured.</p>{/if}
</section>

<style>
  .queue-panel { min-width: 0; }
  .queue-heading { display: flex; align-items: center; justify-content: space-between; gap: 0.75rem; }
  h1 { margin: 0 0 0.75rem; }
  h2 { font-size: 1rem; margin: 1rem 0 0.5rem; }
  .queue-choice select { width: 100%; }
  .queue-meta { font-size: 0.8rem; }
  .now-playing { padding-bottom: 0.75rem; border-bottom: 1px solid var(--border); }
  .current-track { display: flex; gap: 0.75rem; align-items: center; }
  .description { min-width: 0; flex: 1; }
  .title { font-weight: 500; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .secondary { font-size: 0.8rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .added { font-size: 0.7rem; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .similar-link { display: inline-flex; align-items: center; gap: 0.3rem; font-size: 0.8rem; margin-top: 0.2rem; }
  .upcoming-heading { display: flex; align-items: baseline; justify-content: space-between; gap: 1rem; }
  .queue-list { list-style: none; margin: 0; padding: 0; }
  li { position: relative; display: flex; align-items: center; gap: 0.5rem; padding: 0.55rem 0; border-bottom: 1px solid var(--border); cursor: grab; }
  li:hover, li:focus-within { background: var(--surface-hover); }
  li.drop-before::before, li.drop-after::after { content: ""; position: absolute; left: 0; right: 0; height: 3px; background: var(--accent); z-index: 1; }
  li.drop-before::before { top: -2px; }
  li.drop-after::after { bottom: -2px; }
  li.dragged { opacity: 0.45; }
  .grip { cursor: grab; padding: 0.15rem; }
  .duration { font-variant-numeric: tabular-nums; font-size: 0.8rem; }
  .row-actions { display: flex; }
  .drop-end { min-height: 6rem; padding: 1rem; text-align: center; font-size: 0.85rem; border: 2px dashed transparent; margin-top: 0.5rem; border-radius: 8px; }
  .drop-end.target { border-color: var(--accent); background: var(--surface-hover); }
  .compact h1 { font-size: 1.3rem; }
  .compact .duration { display: none; }
  .compact .row-actions { flex-direction: column; }
  .compact .row-actions :global(button) { padding: 0.2rem; min-height: 26px; }
  @media (max-width: 480px) { .duration { display: none; } li { gap: 0.35rem; } }
</style>
