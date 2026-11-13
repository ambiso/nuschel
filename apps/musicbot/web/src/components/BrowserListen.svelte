<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { listening } from "../listening.svelte.js";
  import Icon from "./Icon.svelte";

  // Compact controls for the shared browser stream: a headphones toggle and,
  // while listening, the browser volume. Status text goes to the player status line.
  let { player, large = false } = $props();
  const active = $derived(listening.active);
  const available = $derived(listening.available(player));
  const iconSize = $derived(large ? 32 : 20);
</script>

<div class="browser-listen" class:large role="group" aria-label="Browser listening">
  {#if active}
    <label class="local-volume" title="Browser volume {listening.volume}%; does not change Mumble volume">
      <span class="visually-hidden">Browser volume</span>
      <input type="range" min="0" max="100" value={listening.volume} oninput={(event) => listening.setVolume(Number(event.currentTarget.value))} />
    </label>
  {/if}
  <button
    class="icon-button"
    class:on={active}
    disabled={!active && !available}
    aria-label="Listen in this browser"
    aria-pressed={active}
    title={active ? "Stop listening in this browser" : "Listen to the selected queue here. Uses about 3.1 Mbit/s; independent of Mumble volume and ducking."}
    onclick={() => listening.toggle(player)}
  >
    <Icon name="headphones" size={iconSize} />
  </button>
</div>

<style>
  .browser-listen { display: flex; align-items: center; gap: 0.25rem; min-width: 0; }
  .local-volume { display: flex; align-items: center; min-width: 0; flex: 0 1 6rem; }
  .local-volume input { width: 100%; min-width: 2.5rem; margin: 0; }
  .large .local-volume { flex-basis: 10rem; }
  .on { color: var(--accent); background: var(--surface-hover); }
</style>
