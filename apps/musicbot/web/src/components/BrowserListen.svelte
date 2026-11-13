<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { onMount } from "svelte";
  import { store } from "../store.svelte.js";
  import { BrowserAudio } from "../browser-audio.js";

  let { player } = $props();
  let mode = $state("off");
  let volume = $state(50);
  let error = $state("");
  const active = $derived(mode !== "off");
  const available = $derived(store.connection === "open" && !store.invalid
    && store.browseKey !== null && !!store.snapshot?.players[player]);
  const status = $derived(error || (mode === "starting" ? "Starting browser audio…"
    : mode === "playing" ? "Playing in this browser"
    : mode === "waiting" ? "Waiting for browser audio…"
    : "Browser audio · ~3.1 Mbit/s"));
  const audio = new BrowserAudio(
    (state) => { mode = state; },
    (failure) => {
      error = failure instanceof Error ? failure.message : String(failure);
      store.toast(error);
    },
  );

  onMount(() => () => audio.stop());
  $effect(() => { audio.select(player); });
  $effect(() => { if (!available) audio.stop(); });

  function toggle() {
    error = "";
    if (active) audio.stop();
    else if (available) audio.start(player, store.browseSignal);
  }

  function changeVolume(event) {
    volume = Number(event.currentTarget.value);
    audio.setVolume(volume);
  }
</script>

<svelte:window onpagehide={() => audio.stop()} />

<div class="browser-listen" role="group" aria-label="Browser listening">
  <button
    class:primary={active}
    disabled={!active && !available}
    aria-label={active ? "Stop listening in this browser" : "Listen in this browser"}
    aria-pressed={active}
    title={active ? "Stop browser audio" : "Listen to the selected queue here. Uses about 3.1 Mbit/s; independent of Mumble volume and ducking."}
    onclick={toggle}
  >{active ? "Stop" : "Listen"}</button>
  <label class="local-volume" title="Browser volume {volume}%; does not change Mumble volume">
    <span>Browser <span class="visually-hidden">volume</span></span>
    <input type="range" min="0" max="100" value={volume} oninput={changeVolume} />
  </label>
  <span class="listen-status" class:error={!!error} role="status" title={status}>{status}</span>
</div>

<style>
  .browser-listen { display: grid; grid-template-columns: 4.75rem minmax(0, 1fr); align-items: center; column-gap: 0.5rem; row-gap: 0.15rem; width: 14.5rem; max-width: 100%; flex: none; }
  button { width: 4.75rem; }
  .local-volume { display: flex; flex-direction: column; min-width: 0; color: var(--muted); font-size: 0.75rem; }
  .local-volume input { width: 100%; margin: 0; height: 1.25rem; }
  .listen-status { grid-column: 1 / -1; height: 1rem; line-height: 1rem; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--muted); font-size: 0.7rem; }
  .listen-status.error { color: var(--danger); }
</style>
