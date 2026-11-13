<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import Icon from "./Icon.svelte";

  let { player } = $props();
  let open = $state(false);
  let root = $state();
  let button = $state();
  const ducking = $derived(store.ducking(player));
  const id = $props.id();

  function set(field, value) {
    if (ducking) store.setDucking(player, { ...ducking, [field]: value });
  }

  $effect(() => {
    if (!open) return;
    const pointerdown = (event) => {
      if (!root.contains(event.target)) open = false;
    };
    const keydown = (event) => {
      if (event.key !== "Escape") return;
      open = false;
      button.focus();
    };
    document.addEventListener("pointerdown", pointerdown);
    document.addEventListener("keydown", keydown);
    return () => {
      document.removeEventListener("pointerdown", pointerdown);
      document.removeEventListener("keydown", keydown);
    };
  });
</script>

<div class="ducking" bind:this={root}>
  <button
    class="icon-button"
    class:on={ducking?.enabled}
    bind:this={button}
    aria-expanded={open}
    aria-controls="{id}-panel"
    title="Ducking: quieter while someone talks"
    onclick={() => (open = !open)}
  >
    <Icon name="duck" />
    <span class="visually-hidden">Ducking settings</span>
  </button>
  {#if open && ducking}
    <div class="panel" id="{id}-panel" role="group" aria-label="Ducking">
      <label class="check">
        <input type="checkbox" checked={ducking.enabled} onchange={(e) => set("enabled", e.currentTarget.checked)} />
        Quieter while someone talks
      </label>
      <label for="{id}-level">Level while talking: {ducking.level}%</label>
      <input id="{id}-level" type="range" min="0" max="100" value={ducking.level} disabled={!ducking.enabled} oninput={(e) => set("level", Number(e.currentTarget.value))} />
      <label for="{id}-hold">Back to full volume after (ms)</label>
      <input
        id="{id}-hold"
        type="number"
        min="0"
        max="10000"
        step="100"
        value={ducking.hold_ms}
        disabled={!ducking.enabled}
        onchange={(e) => {
          const value = Number(e.currentTarget.value);
          if (Number.isFinite(value)) set("hold_ms", value);
        }}
      />
      <label for="{id}-sensitivity">Speech detection sensitivity: {ducking.sensitivity}%</label>
      <input id="{id}-sensitivity" type="range" min="0" max="100" value={ducking.sensitivity} disabled={!ducking.enabled} oninput={(e) => set("sensitivity", Number(e.currentTarget.value))} />
    </div>
  {/if}
</div>

<style>
  .ducking { position: relative; }
  .on { color: var(--accent); }
  .panel {
    position: absolute;
    right: 0;
    bottom: calc(100% + 0.6rem);
    z-index: 30;
    width: min(18rem, calc(100vw - 1.5rem));
    max-height: calc(100dvh - 9rem);
    overflow-y: auto;
    display: grid;
    gap: 0.35rem;
    padding: 0.9rem;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--surface);
    box-shadow: var(--shadow);
    font-size: 0.875rem;
  }
  .check { display: flex; gap: 0.5rem; align-items: center; margin-bottom: 0.4rem; }
  input[type="range"], input[type="number"] { width: 100%; margin: 0 0 0.4rem; }
</style>
