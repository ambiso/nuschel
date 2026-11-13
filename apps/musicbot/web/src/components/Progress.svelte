<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import { formatDuration, interpolatePosition, pointerFraction, seekKeyTarget } from "../logic.js";

  let { player, current, large = false } = $props();

  const duration = $derived(current?.entry.track.duration ?? null);
  const max = $derived(typeof duration === "number" && duration > 0 ? duration * 1000 : 0);
  let now = $state(performance.now());
  let dragging = $state(false);
  let preview = $state(0);
  let dragEntry = $state(null);
  let dragPlayer = $state(null);
  let hover = $state(null);
  let bar = $state();
  const hold = $derived(store.holds[player]);

  // Redraw every frame while the song plays (and the user is not dragging).
  const animate = $derived(!dragging && Boolean(current?.advancing));
  $effect(() => {
    if (!animate) return;
    let frame = requestAnimationFrame(function tick() {
      now = performance.now();
      frame = requestAnimationFrame(tick);
    });
    return () => cancelAnimationFrame(frame);
  });

  // A song/queue change during a drag must not seek the replacement song.
  $effect(() => {
    if (dragging && (current?.entry.id !== dragEntry || player !== dragPlayer)) {
      dragging = false;
      hover = null;
    }
  });

  // While dragging, server positions are ignored; after a seek, the target
  // shows until the server reports it (see store.seek).
  const position = $derived.by(() => {
    if (!current) return 0;
    if (dragging) return preview;
    if (hold && hold.entry === current.entry.id) return store.holdPosition(hold, current, now);
    return interpolatePosition(current.position_ms, current.advancing, store.receivedAt, now, duration);
  });
  const percent = $derived(max ? Math.min(100, (position / max) * 100) : 0);
  const tip = $derived(dragging ? preview : hover);

  function msAt(event) {
    const rect = bar.getBoundingClientRect();
    return pointerFraction(event.clientX, rect.left, rect.width) * max;
  }

  function pointerdown(event) {
    if (!max || event.button !== 0) return;
    event.preventDefault();
    bar.setPointerCapture(event.pointerId);
    bar.focus();
    dragEntry = current.entry.id;
    dragPlayer = player;
    dragging = true;
    preview = msAt(event);
  }

  function pointermove(event) {
    if (!max) return;
    const ms = msAt(event);
    if (dragging) preview = ms;
    else if (event.pointerType === "mouse") hover = ms;
  }

  function pointerup(event) {
    if (!dragging) return;
    const target = msAt(event);
    dragging = false;
    hover = null;
    now = performance.now();
    if (bar.hasPointerCapture(event.pointerId)) bar.releasePointerCapture(event.pointerId);
    if (current?.entry.id === dragEntry && player === dragPlayer) store.seek(player, target, true);
  }

  function keydown(event) {
    if (!max) return;
    const target = seekKeyTarget(event.key, position, max);
    if (target === null) return;
    event.preventDefault();
    store.seek(player, target);
  }
</script>

<div class="progress" class:large>
  <span class="time">{formatDuration(position / 1000)}</span>
  <div
    class="bar"
    class:active={dragging}
    class:disabled={!max}
    bind:this={bar}
    role="slider"
    tabindex={max ? 0 : -1}
    aria-label="Position"
    aria-valuemin="0"
    aria-valuemax={Math.round(duration ?? 0)}
    aria-valuenow={Math.round(position / 1000)}
    aria-valuetext={max ? `${formatDuration(position / 1000)} of ${formatDuration(duration)}` : current ? "Duration unknown" : "No song"}
    aria-disabled={!max}
    onpointerdown={pointerdown}
    onpointermove={pointermove}
    onpointerup={pointerup}
    onpointercancel={() => (dragging = false)}
    onlostpointercapture={() => (dragging = false)}
    onpointerleave={() => (hover = null)}
    onkeydown={keydown}
  >
    <div class="track"><div class="fill" style:width="{percent}%"></div></div>
    {#if max}<div class="thumb" style:left="{percent}%"></div>{/if}
    {#if tip !== null && max}
      <div class="tip" style:left="{(tip / max) * 100}%">{formatDuration(tip / 1000)}</div>
    {/if}
  </div>
  <span class="time">{formatDuration(duration)}</span>
</div>

<style>
  .progress {
    display: flex;
    align-items: center;
    gap: 0.6rem;
    width: 100%;
  }
  .time {
    flex: none;
    min-width: 3.2em;
    font-size: 0.75rem;
    color: var(--muted);
    font-variant-numeric: tabular-nums;
    text-align: center;
  }
  .bar {
    position: relative;
    flex: 1;
    height: 20px;
    cursor: pointer;
    touch-action: none;
    border-radius: 4px;
  }
  .bar.disabled { cursor: default; }
  .track {
    position: absolute;
    left: 0;
    right: 0;
    top: 8px;
    height: 4px;
    border-radius: 2px;
    background: var(--border);
    overflow: hidden;
  }
  .fill { height: 100%; background: var(--text); }
  .bar:hover .fill, .bar.active .fill, .bar:focus-visible .fill { background: var(--accent); }
  .thumb {
    position: absolute;
    top: 4px;
    width: 12px;
    height: 12px;
    margin-left: -6px;
    border-radius: 50%;
    background: var(--text);
    opacity: 0;
  }
  .bar:hover .thumb, .bar.active .thumb, .bar:focus-visible .thumb { opacity: 1; }
  .tip {
    position: absolute;
    bottom: 22px;
    transform: translateX(-50%);
    padding: 0.1rem 0.4rem;
    border-radius: 4px;
    background: var(--text);
    color: var(--background);
    font-size: 0.75rem;
    font-variant-numeric: tabular-nums;
    pointer-events: none;
    white-space: nowrap;
  }
  .large { gap: 1rem; }
  .large .time { font-size: 1rem; }
  .large .bar { height: 32px; }
  .large .track { top: 13px; height: 6px; border-radius: 3px; }
  .large .thumb { top: 8px; width: 16px; height: 16px; margin-left: -8px; }
  .large .tip { bottom: 34px; font-size: 0.9rem; }
</style>
