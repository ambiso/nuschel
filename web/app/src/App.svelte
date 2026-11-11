<!-- SPDX-License-Identifier: EUPL-1.2 -->
<!-- The web client's skeleton (docs/web.md): the SDK in WebAssembly connects
     through the web relay, TLS runs here, and the page shows whether it is
     connected. The full client comes later. -->
<script>
  import { onMount } from "svelte";
  import init, { WebClient } from "./sdk/nuschel_client_wasm.js";
  import { inviteFragment } from "./invite.js";

  const scheme = location.protocol === "https:" ? "wss" : "ws";

  // "Join in the browser" from a share link: #t=<token>&s=<host>[:<port>]&fp=sha256:<hex>…
  function fromLink() {
    return inviteFragment(location.hash);
  }

  const link = fromLink();
  let relay = $state(`${scheme}://${location.host}/relay`);
  let host = $state(link.host);
  let port = $state(link.port);
  let username = $state(link.username);
  let pin = $state(link.pin);
  let loaded = $state(false);
  let error = $state(link.error);
  let servers = $state([]);
  let client = null;

  function refresh() {
    if (!client) return;
    client.poll();
    servers = JSON.parse(client.servers());
  }

  onMount(() => {
    init()
      .then(() => {
        loaded = true;
        if (link.host && link.username && !link.error) connect();
      })
      .catch((reason) => (error = `The client could not load: ${reason}`));
    // Mumble servers drop clients that are silent for 30 seconds.
    const timer = setInterval(() => {
      if (client) {
        client.tick(performance.now());
        refresh();
      }
    }, 5000);
    return () => clearInterval(timer);
  });

  function connect(event) {
    event?.preventDefault();
    if (link.error) {
      error = link.error;
      return;
    }
    error = "";
    try {
      if (!client) {
        client = new WebClient(relay);
        client.on_change(refresh);
      }
      const index = client.add_server("", host.trim(), Number(port), username.trim(), pin.trim() || undefined);
      client.connect(index);
      refresh();
    } catch (reason) {
      error = String(reason);
    }
  }

  function trust(index, sha256) {
    client.accept_certificate(index, sha256);
    refresh();
  }

  function disconnect(index) {
    client.disconnect(index);
    refresh();
  }

  const label = { connected: "Connected", connecting: "Connecting…", disconnected: "Not connected" };
</script>

<main>
  <h1><img src="/nuschel.svg" alt="" width="32" height="32"> Nuschel</h1>
  <p class="muted">The web client is in its first steps: it connects and shows the channels.</p>

  <form onsubmit={connect}>
    <label>Server <input bind:value={host} required autocomplete="off" placeholder="voice.example.org"></label>
    <label>Port <input bind:value={port} type="number" min="1" max="65535" required></label>
    <label>Name <input bind:value={username} required autocomplete="nickname"></label>
    <label>Certificate (SHA-256, optional) <input bind:value={pin} autocomplete="off" spellcheck="false"></label>
    <details>
      <summary>Relay</summary>
      <label>Relay address <input bind:value={relay} required></label>
    </details>
    <button type="submit" disabled={!loaded || !!link.error}>Connect</button>
  </form>

  {#if error}
    <p class="bad" role="alert">{error}</p>
  {/if}

  {#each servers as server, index (server.id)}
    <section aria-labelledby={`server-${index}`}>
      <h2 id={`server-${index}`}>{server.name}</h2>
      <p role="status" aria-live="polite" class={server.status === "connected" ? "good" : ""}>
        {label[server.status]}{server.notice ? ` — ${server.notice}` : ""}
      </p>
      {#if server.certificate}
        <p>No certificate authority vouches for this server. Its certificate is
          <code>{server.certificate}</code>. Trust it only if it is what the server's owner told you.</p>
        <button onclick={() => trust(index, server.certificate)}>Trust this certificate</button>
      {/if}
      {#if server.status !== "disconnected"}
        <button onclick={() => disconnect(index)}>Disconnect</button>
      {/if}
      {#if server.status === "connected"}
        <ul aria-label="Channels">
          {#each server.channels as channel (channel.id)}
            <li>{channel.name || "Root"}
              {#each server.users.filter((user) => user.channel === channel.id) as user (user.session)}
                <span class="user">{user.name}</span>
              {/each}
            </li>
          {/each}
        </ul>
      {/if}
    </section>
  {/each}
</main>

<style>
  main { max-width: 40rem; margin: 0 auto; padding: 24px 16px; }
  h1 { display: flex; gap: 10px; align-items: center; margin: 0; }
  .muted { color: var(--muted); }
  form { display: grid; gap: 10px; margin: 20px 0; }
  label { display: grid; gap: 4px; font-weight: 600; }
  input { font: inherit; padding: 8px 10px; border-radius: 8px; border: 1px solid var(--border);
    background: var(--surface); color: var(--text); }
  button { font: inherit; font-weight: 600; padding: 10px 14px; border-radius: 10px; border: 1px solid var(--accent);
    background: var(--accent); color: var(--accent-text); cursor: pointer; justify-self: start; }
  button:disabled { opacity: 0.6; cursor: not-allowed; }
  section { background: var(--surface); border: 1px solid var(--border); border-radius: 12px; padding: 16px; margin-top: 16px; }
  h2 { margin: 0 0 4px; font-size: 1.1rem; }
  .good { color: var(--good); font-weight: 600; }
  .bad { color: var(--bad); }
  code { overflow-wrap: anywhere; }
  .user { margin-left: 8px; color: var(--muted); }
</style>
