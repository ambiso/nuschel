<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { buildRoute } from "../logic.js";
  import { carriesItem, droppedItem } from "../dnd.js";
  import Cover from "./Cover.svelte";
  import Icon from "./Icon.svelte";
  import Progress from "./Progress.svelte";
  import DuckingPanel from "./DuckingPanel.svelte";
  import SongRating from "./SongRating.svelte";

  const index = $derived(store.selected);
  const player = $derived(store.player);
  const players = $derived(store.snapshot?.players ?? []);
  const current = $derived(player?.current ?? null);
  const track = $derived(current?.entry.track ?? null);
  const volume = $derived(store.volume(index));
  const status = $derived.by(() => {
    if (!player) return null;
    if (player.error) return `Playback error: ${player.error}`;
    if (!current) return null;
    if (player.paused) return "Paused";
    if (player.listeners === 0) return "Waiting for listeners";
    if (current.buffering) return "Buffering…";
    if (player.ducked) return "Quieter while someone talks";
    return null;
  });
  let dropping = $state(false);

  function dragover(event) {
    if (!carriesItem(event)) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "copy";
    dropping = true;
  }

  function drop(event) {
    dropping = false;
    const item = droppedItem(event);
    if (!item) return;
    event.preventDefault();
    store.add(index, item, null);
  }
</script>

<footer
  class="player"
  class:dropping
  aria-label="Player"
  ondragover={dragover}
  ondragleave={(event) => {
    if (!event.currentTarget.contains(event.relatedTarget)) dropping = false;
  }}
  ondrop={drop}
>
  <div class="song">
    {#if track}
      <Cover library={track.library} art={track.cover_art} size={128} px={64} />
      <div class="text">
        <div class="title" title={track.title}>{track.title}</div>
        {#if track.artist}
          <div class="sub">
            {#if track.artist_id}
              <a href={buildRoute({ view: "artist", library: track.library, id: track.artist_id, namespace: store.browseKey })}>{track.artist}</a>
            {:else}{track.artist}{/if}
          </div>
        {/if}
        {#if track.album}
          <div class="sub">
            {#if track.album_id}
              <a href={buildRoute({ view: "album", library: track.library, id: track.album_id, namespace: store.browseKey })}>{track.album}</a>
            {:else}{track.album}{/if}
          </div>
        {/if}
      </div>
    {:else}
      <Cover library={0} art={null} px={64} />
      <div class="text"><div class="sub">{dropping ? "Drop to add to the queue" : "Nothing is playing"}</div></div>
    {/if}
  </div>

  <div class="center">
    <div class="controls">
      <button
        class="play"
        disabled={!player}
        aria-label={player?.paused ? "Play" : "Pause"}
        title={player?.paused ? "Play (Space)" : "Pause (Space)"}
        onclick={() => store.togglePause(index)}
      >
        <Icon name={player?.paused ? "play" : "pause"} size={24} />
      </button>
      <button class="icon-button" disabled={!current} aria-label="Skip" title="Skip" onclick={() => store.skip(index)}>
        <Icon name="skip" />
      </button>
      {#if status}<span class="status" role="status">{status}</span>{/if}
    </div>
    <Progress player={index} {current} />
  </div>

  <div class="side">
    {#if players.length > 1}
      <label class="queue-select">
        <span class="visually-hidden">Queue</span>
        <select value={index} onchange={(event) => store.selectQueue(Number(event.currentTarget.value))}>
          {#each players as p, i (i)}
            <option value={i}>{p.name || `Queue ${i + 1}`}</option>
          {/each}
        </select>
      </label>
    {/if}
    {#if player}
      <label class="volume" title="Volume {volume}%">
        <Icon name="volume" />
        <span class="visually-hidden">Volume</span>
        <input type="range" min="0" max="100" value={volume} oninput={(event) => store.setVolume(index, Number(event.currentTarget.value))} />
      </label>
      <DuckingPanel player={index} />
    {/if}
    {#if track}<SongRating library={track.library} songId={track.id} title={track.title} namespace={store.browseKey} />{/if}
  </div>
</footer>

<style>
  .player {
    display: grid;
    grid-template-columns: minmax(0, 1fr) minmax(0, 2fr) minmax(0, 1fr);
    align-items: center;
    gap: 1rem;
    padding: 0.6rem 1rem;
    border-top: 1px solid var(--border);
    background: var(--surface);
  }
  .player.dropping { outline: 2px dashed var(--accent); outline-offset: -4px; }
  .song { display: flex; align-items: center; gap: 0.75rem; min-width: 0; }
  .text { min-width: 0; line-height: 1.3; }
  .title { font-weight: 600; }
  .title, .sub { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
  .sub { font-size: 0.85rem; color: var(--muted); }
  .sub a { color: inherit; }
  .center { display: grid; justify-items: center; gap: 0.2rem; min-width: 0; }
  .controls { position: relative; display: flex; align-items: center; gap: 0.5rem; max-width: 100%; }
  .play {
    width: 40px;
    height: 40px;
    border-radius: 50%;
    border: none;
    display: grid;
    place-items: center;
    background: var(--text);
    color: var(--background);
    cursor: pointer;
    flex: none;
  }
  .play:disabled { opacity: 0.4; cursor: default; }
  .status { min-width: 0; overflow-wrap: anywhere; font-size: 0.8rem; color: var(--muted); }
  .side { display: flex; align-items: center; justify-content: flex-end; flex-wrap: wrap; gap: 0.5rem; min-width: 0; }
  .queue-select select { max-width: 10rem; }
  .volume { display: flex; align-items: center; gap: 0.35rem; color: var(--muted); }
  .volume input { width: 7rem; }

  @media (max-width: 760px) {
    .player {
      grid-template-columns: minmax(0, 1fr) minmax(0, 1fr);
      grid-template-areas: "song side" "center center";
      gap: 0.4rem 0.75rem;
      padding: 0.5rem 0.75rem;
    }
    .song { grid-area: song; }
    .center { grid-area: center; }
    .side { grid-area: side; }
    .status { white-space: normal; }
    .volume input { width: 5rem; }
    .queue-select select { max-width: 8rem; }
  }
</style>
