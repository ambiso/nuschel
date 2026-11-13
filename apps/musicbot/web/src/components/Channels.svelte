<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { store } from "../store.svelte.js";
  import Icon from "./Icon.svelte";

  const channels = $derived(store.snapshot?.channels ?? []);
  const players = $derived(store.snapshot?.players ?? []);
  let pending = $state({});

  async function attach(event, index) {
    const control = event.currentTarget;
    const player = Number(control.value);
    pending[index] = true;
    await store.attach(index, player);
    control.value = String(store.snapshot?.channels[index]?.player ?? 0);
    delete pending[index];
  }
</script>

<section aria-labelledby="channels-title">
  <h1 id="channels-title">Channels</h1>
  <p class="muted">Choose which queue each Mumble channel receives. Several channels can listen to the same queue.</p>
  <div class="channel-list">
    {#each channels as channel, i (i)}
      <article class="channel">
        <div class="channel-heading"><Icon name="channel" /><h2>{channel.name}</h2><span class="connection" class:connected={channel.connected}>{channel.connected ? "Connected" : "Disconnected"}</span></div>
        <p class="muted">{channel.server}{channel.channel ? ` · ${channel.channel}` : ""}</p>
        <p>{channel.listeners} {channel.listeners === 1 ? "listener" : "listeners"}</p>
        {#if channel.error}<p class="notice error" role="alert">{channel.error}</p>{/if}
        <label class="attachment"><span>Playback queue</span>
          <select value={channel.player} disabled={!players.length || Boolean(pending[i])} onchange={(event) => attach(event, i)}>
            {#each players as player, index (index)}<option value={index}>{player.name || `Queue ${index + 1}`}</option>{/each}
          </select>
        </label>
      </article>
    {/each}
  </div>
  {#if !channels.length}<p class="notice muted">No Mumble channels are configured.</p>{/if}
</section>

<style>
  h1 { margin-top: 0; }
  .channel-list { display: grid; grid-template-columns: repeat(auto-fit, minmax(min(100%, 20rem), 1fr)); gap: 1rem; margin-top: 1.5rem; }
  .channel { padding: 1rem; border: 1px solid var(--border); border-radius: 8px; background: var(--surface); }
  .channel-heading { display: flex; align-items: center; gap: 0.5rem; flex-wrap: wrap; }
  h2 { font-size: 1.15rem; margin: 0; overflow-wrap: anywhere; }
  .connection { margin-left: auto; color: var(--warning); font-size: 0.8rem; }
  .connection.connected { color: var(--accent); }
  p { overflow-wrap: anywhere; }
  .attachment { display: grid; gap: 0.4rem; margin-top: 1rem; }
  .attachment select { width: 100%; }
</style>
