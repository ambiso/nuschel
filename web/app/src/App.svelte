<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { onMount, tick } from "svelte";
  import { inviteFragment } from "./invite.js";
  import { channelRows } from "./presentation.js";
  import Message from "./Message.svelte";
  const link = inviteFragment(location.hash);
  let relay = $state(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/relay`);
  let host = $state(link.host);
  let port = $state(link.port);
  let username = $state(link.username);
  let pin = $state(link.pin);
  let loaded = $state(false);
  let error = $state(link.error);
  let servers = $state([]);
  let timeline = $state(null);
  let showAdd = $state(true);
  let body = $state("");
  let expanded = $state(new Map());
  let messagesElement = $state();
  let initialState = true;
  let follow = true;
  let worker;
  let requestId = 0;
  const requests = new Map();
  const selected = $derived(servers.find((server) => server.id === timeline?.server));
  const selectedIndex = $derived(servers.findIndex((server) => server.id === timeline?.server));
  const rows = $derived(selected ? channelRows(selected.channels, expanded, selected.id) : []);
  const labels = { connected:"Connected", connecting:"Connecting…", disconnected:"Disconnected" };
  function command(method, ...args) {
    return new Promise((resolve, reject) => {
      const id = ++requestId;
      requests.set(id, { resolve, reject });
      worker.postMessage({ id, method, args });
    });
  }
  async function act(method, ...args) {
    try { return await command(method, ...args); }
    catch (reason) { error = String(reason); }
  }
  onMount(() => {
    worker = new Worker(new URL("./sdk.worker.js", import.meta.url), { type:"module" });
    worker.onmessage = async ({ data }) => {
      if (data.kind === "result") {
        const pending = requests.get(data.id);
        requests.delete(data.id);
        if (pending) data.error ? pending.reject(data.error) : pending.resolve(data.value);
        else if (data.error) error = data.error;
      } else if (data.kind === "error") error = data.message;
      else if (data.kind === "ready") {
        loaded = true;
        if (link.host && link.username && !link.error) await connect();
      } else if (data.kind === "state") {
        const prior = timeline;
        servers = data.servers;
        timeline = data.timeline;
        for (const update of data.updates) if (update.kind === "error") error = update.message;
        if (initialState) {
          if (servers.length && !link.host) showAdd = false;
          initialState = false;
        }
        const changed = prior?.server !== timeline?.server || prior?.channel !== timeline?.channel;
        if (changed) {
          follow = true;
          if (messagesElement) delete messagesElement.dataset.previousHeight;
        }
        await tick();
        if (messagesElement?.dataset.previousHeight && prior?.loading && !timeline?.loading) {
          const difference = messagesElement.scrollHeight - Number(messagesElement.dataset.previousHeight);
          messagesElement.scrollTop = Number(messagesElement.dataset.previousTop || 0) + difference;
          delete messagesElement.dataset.previousHeight;
          delete messagesElement.dataset.previousTop;
        } else if (messagesElement && follow) messagesElement.scrollTop = messagesElement.scrollHeight;
      }
    };
    worker.onerror = () => { error = "The browser worker stopped. Reload to reconnect."; loaded = false; };
    worker.postMessage({ method:"init", relay });
    return () => { worker.terminate(); for (const pending of requests.values()) pending.reject("Client closed."); requests.clear(); };
  });
  async function connect(event) {
    event?.preventDefault();
    if (link.error) { error = link.error; return; }
    try {
      error = "";
      const index = await command("add_server", "", host.trim(), Number(port), username.trim(), pin.trim() || undefined);
      await command("connect", index);
      await command("open", index, 0);
      showAdd = false;
    } catch (reason) { error = String(reason); }
  }
  async function send(event) {
    event?.preventDefault();
    const sent = body;
    if (!sent.trim()) return;
    try { await command("send_text", sent); if (body === sent) body = ""; }
    catch (reason) { error = String(reason); }
  }
  function toggle(row) {
    const next = new Map(expanded);
    next.set(`${selected.id}:${row.id}`, !row.open);
    expanded = next;
  }
  async function older() {
    const height = messagesElement?.scrollHeight || 0;
    follow = false;
    await act("older");
    // History completion is asynchronous; preserve the visible anchor on the
    // next state containing older messages rather than jumping to the newest.
    if (messagesElement) {
      messagesElement.dataset.previousHeight = String(height);
      messagesElement.dataset.previousTop = String(messagesElement.scrollTop);
    }
  }
</script>

<main>
  <nav class="servers" aria-label="Servers">
    <a class="brand" href="/" aria-label="Nuschl"><img src="/nuschl.svg" alt="" width="36" height="36"></a>
    {#each servers as server, index (server.id)}
      <button class:active={selected?.id === server.id} class="server-icon" title={server.name} aria-label={`Browse ${server.name}`} aria-current={selected?.id === server.id ? "true" : undefined} onclick={() => act("browse_server", index)}>{server.name.slice(0,2).toUpperCase()}</button>
    {/each}
    <button class="add" aria-label="Add server" title="Add server" onclick={() => showAdd = !showAdd}>+</button>
  </nav>
  <aside class="channels">
    <h1>{selected?.name || "Nuschl"}</h1>
    {#if selected}
      <p class="status" role="status">{labels[selected.status]}</p>
      <p class="security"><span aria-hidden="true">🔓</span> TLS transport · server can read chat</p>
      {#if selected.notice}<p class="notice">{selected.notice}</p>{/if}
      {#if selected.status === "disconnected"}<button onclick={() => act("connect", selectedIndex)}>Connect</button>{:else}<button onclick={() => act("disconnect", selectedIndex)}>Disconnect</button>{/if}
      {#if selected.certificate}
        <div class="certificate" role="alert"><p>{selected.pin ? "The certificate changed." : "Verify this server certificate with its owner:"}</p><code>{selected.certificate}</code><button onclick={() => act("accept_certificate", selectedIndex, selected.certificate)}>Trust this certificate</button></div>
      {/if}
      <nav class="channel-tree" aria-label="Channels">
        {#each rows as row (row.id)}
          <div class="channel-row" style:padding-left={`${row.depth * .75}rem`}>
            {#if row.hasChildren}<button class="expand" aria-label={`${row.open ? "Collapse" : "Expand"} ${row.name}`} aria-expanded={row.open} onclick={() => toggle(row)}>{row.open ? "▾" : "▸"}</button>{:else}<span class="spacer"></span>{/if}
            <button class:current={timeline?.channel === row.id} class="channel" title={row.name} onclick={() => act("open", selectedIndex, row.id)}>{row.name || "Root"}</button>
          </div>
          {#each selected.users.filter((user) => user.channel === row.id) as user (user.session)}<div class="user" style:padding-left={`${row.depth * .75 + 1.8}rem`}>{user.name}</div>{/each}
        {/each}
      </nav>
    {:else}<p class="muted">Add a server to start chatting.</p>{/if}
    <p class="scope">Legacy text chat · observed history saved in this browser. Voice, server archives and MLS are not available here yet.</p>
  </aside>
  <section class="chat" aria-label="Chat">
    <header class="chat-header"><h2>{timeline?.name || "Welcome"}</h2><button onclick={() => showAdd = !showAdd}>Add server</button></header>
    {#if showAdd}
      <form class="connection" onsubmit={connect}>
        <h2>Add a server</h2>
        <label>Server <input bind:value={host} required autocomplete="off" placeholder="voice.example.org"></label>
        <label>Port <input bind:value={port} type="number" min="1" max="65535" required></label>
        <label>Your name <input bind:value={username} required autocomplete="nickname"></label>
        <label>Certificate SHA-256 (optional)<input bind:value={pin} autocomplete="off" spellcheck="false"></label>
        <details><summary>Relay</summary><p>{relay}</p><small>The relay is fixed for this page session. TLS runs in this browser.</small></details>
        <button type="submit" disabled={!loaded || !!link.error}>Connect</button>
      </form>
    {/if}
    {#if error}<div class="error" role="alert"><span>{error}</span><button aria-label="Dismiss error" onclick={() => error = ""}>×</button></div>{/if}
    <div class="messages" bind:this={messagesElement} onscroll={() => {
      if (!messagesElement) return;
      follow = messagesElement.scrollHeight - messagesElement.scrollTop - messagesElement.clientHeight < 80;
    }}>
      {#if timeline?.has_older}<button class="older" disabled={timeline.loading} onclick={older}>{timeline.loading ? "Loading…" : "Load older messages"}</button>{/if}
      {#each timeline?.messages || [] as message (message.serial)}<Message {message}/>{/each}
      {#if timeline && !timeline.messages.length && !timeline.loading}<p class="empty">No messages observed in this channel yet.</p>{/if}
    </div>
    <form class="composer" onsubmit={send}>
      <label class="sr-only" for="message">Message</label>
      <textarea id="message" bind:value={body} rows="3" placeholder={timeline ? `Message ${timeline.name} — newlines stay newlines` : "Select a channel"} disabled={!timeline || selected?.status !== "connected"} onkeydown={(event) => { if ((event.ctrlKey || event.metaKey) && event.key === "Enter") { event.preventDefault(); send(); } }}></textarea>
      <div><small>Enter adds a newline · Ctrl/⌘ Enter sends</small><button type="submit" disabled={!body.trim() || !timeline || timeline.sending || selected?.status !== "connected"}>Send</button></div>
    </form>
  </section>
</main>

<style>
main { display:grid; grid-template-columns:76px minmax(180px,230px) minmax(0,1fr); height:100dvh; overflow:hidden; }
button,input,textarea { font:inherit; } button { cursor:pointer; border:1px solid var(--border); border-radius:.4rem; background:var(--surface); color:var(--text); padding:.4rem .65rem; } button:disabled { opacity:.5; cursor:default; }
.servers { background:var(--surface); border-right:1px solid var(--border); display:flex; flex-direction:column; align-items:center; gap:.6rem; padding:.9rem .4rem; overflow-y:auto; } .brand { margin-bottom:.5rem; } .server-icon,.add { width:48px; height:48px; flex-shrink:0; border-radius:35%; font-weight:700; } .active { background:var(--accent); color:var(--accent-text); border-radius:25%; }
.channels { display:flex; flex-direction:column; overflow:hidden; padding:.85rem .7rem; background:color-mix(in srgb,var(--surface) 70%,var(--background)); border-right:1px solid var(--border); } h1,h2 { font-size:1.05rem; margin:0; overflow-wrap:anywhere; } .status,.security { font-size:.75rem; margin:.3rem 0; color:var(--muted); } .security { color:var(--bad); } .notice,.certificate { font-size:.8rem; overflow-wrap:anywhere; } .certificate code { display:block; font-size:.7rem; margin:.5rem 0; } .channel-tree { min-height:0; overflow:auto; scrollbar-gutter:stable; margin:.75rem -.25rem; padding:.25rem; flex:1; scroll-behavior:smooth; } .channel-row { display:flex; align-items:center; min-width:0; } .expand { width:1.5rem; padding:0; border:0; background:none; flex-shrink:0; } .spacer { width:1.5rem; flex-shrink:0; } .channel { text-align:left; border:0; min-width:0; width:100%; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; background:none; } .current { background:color-mix(in srgb,var(--accent) 20%,transparent); color:var(--accent); } .user { color:var(--muted); font-size:.8rem; overflow-wrap:anywhere; margin:.1rem 0 .2rem; } .scope { color:var(--muted); font-size:.7rem; margin:.5rem 0 0; } .muted { color:var(--muted); }
.chat { display:flex; flex-direction:column; min-width:0; min-height:0; } .chat-header { min-height:56px; display:flex; align-items:center; justify-content:space-between; padding:.6rem 1rem; border-bottom:1px solid var(--border); gap:1rem; } .connection { max-width:600px; padding:1rem; display:grid; grid-template-columns:1fr 1fr; gap:.6rem; max-height:60vh; overflow:auto; } .connection h2,.connection details { grid-column:1/-1; } label { display:grid; gap:.2rem; font-size:.8rem; } input,textarea { background:var(--background); color:var(--text); caret-color:var(--text); border:1px solid var(--border); border-radius:.4rem; padding:.5rem; min-width:0; width:100%; } .error { color:var(--bad); display:flex; justify-content:space-between; padding:.5rem 1rem; border-bottom:1px solid var(--border); overflow-wrap:anywhere; } .messages { flex:1; min-height:0; overflow:auto; scrollbar-gutter:stable; scroll-behavior:smooth; padding:.75rem 0; } .empty { color:var(--muted); padding:1rem; } .older { margin:.5rem 1rem; } .composer { padding:.75rem 1rem; } .composer textarea { resize:vertical; max-height:35vh; } .composer div { display:flex; justify-content:space-between; gap:.5rem; align-items:center; } small { color:var(--muted); font-size:.72rem; } .composer button { background:var(--accent); color:var(--accent-text); } .sr-only { position:absolute; width:1px; height:1px; overflow:hidden; clip-path:inset(50%); }
@media(max-width:650px) { main { grid-template-columns:56px 150px minmax(0,1fr); } .servers { padding:.5rem .2rem; } .server-icon,.add { width:40px; height:40px; } .connection { grid-template-columns:1fr; } .composer div small { display:none; } }
@media(prefers-reduced-motion:reduce) { .messages,.channel-tree { scroll-behavior:auto; } }
</style>
