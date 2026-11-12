<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { untrack } from "svelte";
  import { store } from "../store.svelte.js";
  import { getJSON, query } from "../api.js";
  import { buildRoute, formatDuration, mergeHistory } from "../logic.js";
  import Cover from "./Cover.svelte";

  const selected = $derived(store.selected);
  const players = $derived(store.snapshot?.players ?? []);
  const revision = $derived(store.player?.history_revision ?? 0);
  const hasPlayer = $derived(store.player !== null);
  const name = $derived(store.player?.name ?? null);
  const connected = $derived(store.connection === "open");
  const libraryKey = $derived(JSON.stringify(store.snapshot?.libraries ?? []));
  let history = $state.raw({ items: [], next_before: null, oldest_id: null, revision: 0 });
  let loading = $state(false);
  let loaded = $state(false);
  let error = $state("");
  let queue = null;
  let queueName = null;
  let libraries = null;
  let wasConnected = false;
  let request = null;

  $effect(() => {
    const index = selected;
    revision;
    const exists = hasPlayer;
    const playerName = name;
    const live = connected;
    const key = libraryKey;
    untrack(() => {
      if (queue !== index || queueName !== playerName || libraries !== key || (live && !wasConnected)) {
        queue = index;
        queueName = playerName;
        history = { items: [], next_before: null, oldest_id: null, revision: 0 };
        loaded = false;
      }
      libraries = key;
      wasConnected = live;
      if (!live) loaded = false;
      if (exists && live) load(false, index);
    });
    return () => request?.abort();
  });

  async function load(older = false, index = selected) {
    request?.abort();
    const controller = new AbortController();
    request = controller;
    loading = true;
    error = "";
    const params = { player: index, limit: 50 };
    if (older) params.before = history.next_before;
    try {
      const page = await getJSON(query("api/history", params), controller.signal);
      if (controller.signal.aborted || request !== controller || index !== queue) return;
      // A delayed HTTP response must not replace a newer live revision.
      if (page.revision < history.revision) return;
      history = mergeHistory(history, page, older);
      loaded = true;
    } catch (failure) {
      if (!controller.signal.aborted && request === controller) error = failure.message;
    } finally {
      if (request === controller) loading = false;
    }
  }

  function localTime(ms) { return new Date(ms).toLocaleString(); }
</script>

<section aria-labelledby="history-title">
  <h1 id="history-title">History</h1>
  {#if players.length}
    <label class="queue-choice"><span class="visually-hidden">Selected queue</span>
      <select value={selected} onchange={(event) => store.selectQueue(Number(event.currentTarget.value))}>
        {#each players as player, i (i)}<option value={i}>{player.name || `Queue ${i + 1}`}</option>{/each}
      </select>
    </label>
    <p class="muted">Played tracks, newest first. Transmitted time excludes pauses, seeks and waiting for listeners.</p>
    {#if error}<p class="notice error" role="alert">Could not load history: {error}</p><button onclick={() => load()}>Retry history</button>{/if}
    {#if loading}<p class="muted" role="status">Loading history…</p>{/if}
    {#if loaded && !history.items.length}<p class="notice muted">No playback history for this queue yet.</p>{/if}
    <ol class="history-list" aria-label="Played tracks">
      {#each history.items as item (item.id)}
        {@const track = item.entry.track}
        {@const available = connected && loaded && track.library !== null}
        <li>
          <Cover library={track.library} art={available ? track.cover_art : null} size={128} px={56} />
          <div class="description">
            <div class="title">{track.title}</div>
            <div class="muted">{#if available && track.artist_id}<a href={buildRoute({ view: "artist", library: track.library, id: track.artist_id, namespace: store.browseKey })}>{track.artist || "Unknown artist"}</a>{:else}{track.artist || "Unknown artist"}{/if}</div>
            {#if track.album}<div class="muted">{#if available && track.album_id}<a href={buildRoute({ view: "album", library: track.library, id: track.album_id, namespace: store.browseKey })}>{track.album}</a>{:else}{track.album}{/if}</div>{/if}
            <p class="metadata muted">{item.library_name || "Unknown library"}{!available ? " (unavailable)" : ""} · Queued by {item.entry.added_by}</p>
            <dl>
              <div><dt>Started</dt><dd>{#if item.started_at_ms === null}<strong>Never started</strong>{:else}<time datetime={new Date(item.started_at_ms).toISOString()}>{localTime(item.started_at_ms)}</time>{/if}</dd></div>
              <div><dt>Ended</dt><dd><time datetime={new Date(item.ended_at_ms).toISOString()}>{localTime(item.ended_at_ms)}</time></dd></div>
              <div><dt>Transmitted</dt><dd>{formatDuration(item.played_ms / 1000)} ({item.played_ms.toLocaleString()} ms)</dd></div>
              <div><dt>Outcome</dt><dd class:failed={item.outcome === "failed"}>{item.outcome === "completed" ? "Completed" : item.outcome === "skipped" ? "Skipped" : "Failed"}</dd></div>
            </dl>
            {#if item.error}<p class="failure">Failure reason: {item.error}</p>{/if}
          </div>
        </li>
      {/each}
    </ol>
    {#if history.next_before !== null}<button disabled={loading || !connected} onclick={() => load(true)}>Load older</button>{/if}
  {:else}<p class="notice muted">No playback queues are configured.</p>{/if}
</section>

<style>
  .queue-choice select { width: 100%; max-width: 24rem; }
  .history-list { list-style: none; padding: 0; margin: 1rem 0; }
  li { display: flex; gap: 0.85rem; padding: 1rem 0; border-bottom: 1px solid var(--border); }
  .description { min-width: 0; flex: 1; overflow-wrap: anywhere; }
  .title { font-weight: 650; }
  .metadata { font-size: 0.85rem; margin: 0.4rem 0; }
  dl { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 0.35rem 1rem; margin: 0.6rem 0 0; font-size: 0.85rem; }
  dt { color: var(--muted); }
  dd { margin: 0; }
  .failed, .failure { color: var(--danger); }
  .failure { margin-bottom: 0; font-size: 0.9rem; }
  @media (max-width: 600px) { dl { grid-template-columns: 1fr; } }
</style>
