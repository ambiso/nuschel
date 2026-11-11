<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { onMount, tick } from "svelte";
  import { inviteFragment } from "./invite.js";
  import { channelRows, initials, avatarColor, conversationKey, filterMessages, appearance } from "./presentation.js";
  import Message from "./Message.svelte";
  const link = inviteFragment(location.hash);
  let relay = $state(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/relay`);
  let host = $state(link.host), port = $state(link.port), username = $state(link.username), pin = $state(link.pin), serverName = $state("");
  let loaded = $state(false), error = $state(link.error), servers = $state([]), timeline = $state(null);
  let showAdd = $state(false), showSettings = $state(false), drawer = $state(false), mobile = $state(false);
  let body = $state(""), expanded = $state(new Map()), search = $state(""), showSearch = $state(false);
  let messagesElement = $state(), composerElement = $state(), navigationButton = $state(), searchElement = $state();
  let settings = $state(appearance(null)), voice = $state(null), audioState = $state(null), capabilities = $state([]);
  let serverScroll = $state(), channelScroll = $state(), serverUnread = $state({above:false,below:false}), channelUnread = $state({above:false,below:false});
  let membersOpen = $state(false);
  let lastChannelPointer = "";
  let initialState = true, follow = true, worker, requestId = 0, activateAudio = () => {}, disposeAudio = () => {};
  const requests = new Map(), drafts = new Map();
  const selected = $derived(servers.find((server) => server.id === timeline?.server) || servers[0]);
  const selectedIndex = $derived(servers.findIndex((server) => server.id === selected?.id));
  const rows = $derived(selected ? channelRows(selected.channels, expanded, selected.id) : []);
  const currentChannel = $derived(selected?.channels.find((channel) => channel.id === timeline?.channel));
  const ownUser = $derived(selected?.users.find((user) => user.session === selected.own_session));
  const selectedVoiceChannel = $derived(selected?.channels.find((channel) => channel.id === ownUser?.channel));
  const displayedMessages = $derived(filterMessages(timeline?.messages || [], search));
  const selectedUsers = $derived((selected?.users || []).filter((user) => user.channel === timeline?.channel));
  const canSend = $derived(!!timeline && selected?.status === "connected" && !timeline.sending);
  const usersByChannel = $derived.by(() => {
    const groups = new Map();
    for (const user of selected?.users || []) { const group = groups.get(user.channel) || []; group.push(user); groups.set(user.channel, group); }
    return groups;
  });
  const labels = { connected:"Connected", connecting:"Connecting…", disconnected:"Disconnected" };
  const icons = {
    menu:"M4 6h16M4 12h16M4 18h16", close:"m6 6 12 12M6 18 18 6", plus:"M12 5v14M5 12h14", hash:"M10 3 8 21M17 3l-2 18M4 9h17M3 15h17",
    voice:"M11 5 6 9H3v6h3l5 4V5Zm4 3a6 6 0 0 1 0 8m3-11a10 10 0 0 1 0 14", mic:"M12 15a3 3 0 0 0 3-3V5a3 3 0 0 0-6 0v7a3 3 0 0 0 3 3Zm-6-4v1a6 6 0 0 0 12 0v-1M12 18v4M8 22h8", muted:"m3 3 18 18M9 9v3a3 3 0 0 0 5 2M9 5a3 3 0 0 1 6 0v5M6 11v1a6 6 0 0 0 10 4M18 11v1M12 18v4M8 22h8",
    headphones:"M4 14v-3a8 8 0 0 1 16 0v3M4 13h4v8H4v-8Zm12 0h4v8h-4v-8", search:"m21 21-5-5M10 18a8 8 0 1 0 0-16 8 8 0 0 0 0 16", people:"M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2M16 3a4 4 0 0 1 0 8m6 10v-2a4 4 0 0 0-3-3.87M13 7a4 4 0 1 1-8 0 4 4 0 0 1 8 0",
    settings:"m9 3-.7 3-2.8 1-2.5-1.5-2 3.5 2.3 2V14L1 16l2 3.5L5.5 18l2.8 1L9 22h4l.7-3 2.8-1 2.5 1.5 2-3.5-2.3-2v-3L21 9l-2-3.5L16.5 7l-2.8-1L13 3H9ZM15 12a4 4 0 1 1-8 0 4 4 0 0 1 8 0", send:"m22 2-7 20-4-9-9-4 20-7ZM11 13 22 2", leave:"M10 17l5-5-5-5M15 12H3M15 3h5v18h-5", lock:"M5 10h14v11H5V10Zm3 0V6a4 4 0 0 1 8 0", unlock:"M5 10h14v11H5V10Zm3 0V6a4 4 0 0 1 7-3", chevron:"m9 5 7 7-7 7",
  };
  function command(method, ...args) {
    return new Promise((resolve, reject) => {
      if (!worker) { reject("The client is still starting."); return; }
      const id = ++requestId; requests.set(id, { resolve, reject }); worker.postMessage({ id, method, args });
    });
  }
  async function act(method, ...args) { try { return await command(method, ...args); } catch (reason) { error = String(reason); } }
  function rememberDraft() {
    const key = conversationKey(timeline, selected);
    if (!key) return;
    if (body) { drafts.delete(key); drafts.set(key, body.slice(0, 65536)); } else drafts.delete(key);
    let total = 0;
    for (const value of drafts.values()) total += value.length;
    while (drafts.size > 30 || total > 131072) { const first = drafts.keys().next().value; total -= drafts.get(first).length; drafts.delete(first); }
  }
  function persistSettings() {
    settings = appearance(settings);
    try { localStorage.setItem("nuschl.appearance.v1", JSON.stringify(settings)); } catch { error = "Appearance preferences could not be saved in this browser."; }
  }
  $effect(() => {
    document.documentElement.dataset.theme = settings.theme;
    document.documentElement.dataset.density = settings.density;
    document.documentElement.style.setProperty("--font-size", `${settings.fontSize}px`);
  });
  onMount(() => {
    try { settings = appearance(JSON.parse(localStorage.getItem("nuschl.appearance.v1") || "null")); } catch { /* Defaults remain usable in private browsing. */ }
    const media = matchMedia("(max-width: 760px)");
    const updateMobile = () => { mobile = media.matches; if (!mobile) drawer = false; };
    updateMobile(); media.addEventListener("change", updateMobile);
    const viewport = window.visualViewport;
    const resizeViewport = () => { document.documentElement.style.setProperty("--app-height", `${viewport?.height || window.innerHeight}px`); };
    resizeViewport(); viewport?.addEventListener("resize", resizeViewport);
    worker = new Worker(new URL("./sdk.worker.js", import.meta.url), { type:"module" });
    worker.onmessage = async ({ data }) => {
      if (data.kind === "result") {
        const pending = requests.get(data.id); requests.delete(data.id);
        if (pending) data.error ? pending.reject(data.error) : pending.resolve(data.value);
        else if (data.error) error = data.error;
      } else if (data.kind === "error") error = data.message;
      else if (data.kind === "ready") {
        loaded = true; capabilities = data.capabilities || [];
        if (link.host && link.username && !link.error) await connect();
      } else if (data.kind === "state") {
        const prior = timeline, priorKey = conversationKey(timeline, selected);
        rememberDraft();
        servers = data.servers; timeline = data.timeline;
        voice = data.voice || null;
        for (const update of data.updates || []) if (update.kind === "error") error = update.message;
        if (initialState) { initialState = false; if (!servers.length && !link.host) showAdd = true; }
        const changed = priorKey !== conversationKey(timeline, selected);
        if (changed) {
          body = drafts.get(conversationKey(timeline, selected)) || ""; search = ""; follow = true;
          if (messagesElement) delete messagesElement.dataset.previousHeight;
        }
        await tick();
        if (messagesElement?.dataset.previousHeight && prior?.loading && !timeline?.loading) {
          const difference = messagesElement.scrollHeight - Number(messagesElement.dataset.previousHeight);
          messagesElement.scrollTop = Number(messagesElement.dataset.previousTop || 0) + difference;
          delete messagesElement.dataset.previousHeight; delete messagesElement.dataset.previousTop;
        } else if (messagesElement && follow && !search) messagesElement.scrollTop = messagesElement.scrollHeight;
        updateUnread();
      }
    };
    worker.onerror = () => { error = "The browser worker stopped. Reload to reconnect."; loaded = false; };
    worker.postMessage({ method:"init", relay });
    return () => { media.removeEventListener("change", updateMobile); viewport?.removeEventListener("resize", resizeViewport); disposeAudio(); worker.terminate(); for (const pending of requests.values()) pending.reject("Client closed."); requests.clear(); };
  });
  async function connect(event) {
    event?.preventDefault();
    if (link.error) { error = link.error; return; }
    try {
      error = "";
      const index = await command("add_server", serverName.trim(), host.trim(), Number(port), username.trim(), pin.trim() || undefined);
      await command("connect", index); await command("open", index, 0); showAdd = false;
    } catch (reason) { error = String(reason); }
  }
  function addServer() { serverName = ""; host = ""; port = 64738; username = selected?.username || ""; pin = ""; showAdd = true; drawer = false; }
  function editServer() { if (!selected) return; serverName = selected.name; host = selected.host; port = selected.port; username = selected.username; pin = selected.pin || ""; showAdd = true; drawer = false; }
  async function send(event) {
    event?.preventDefault();
    const sent = body, key = conversationKey(timeline, selected);
    if (!sent.trim() || !canSend) return;
    try { await command("send_text", sent); if (body === sent && key === conversationKey(timeline, selected)) { body = ""; drafts.delete(key); } }
    catch (reason) { error = String(reason); }
  }
  async function openChannel(id) { rememberDraft(); await act("open", selectedIndex, id); if (mobile) drawer = false; }
  function joinVoice(channel = timeline?.channel) {
    if (!selected || selected.status !== "connected" || channel == null) return;
    activateAudio(); void act("join", selectedIndex, channel);
  }
  function toggle(row) { const next = new Map(expanded); next.set(`${selected.id}:${row.id}`, !row.open); expanded = next; }
  async function older() {
    follow = false;
    if (messagesElement) { messagesElement.dataset.previousHeight = String(messagesElement.scrollHeight); messagesElement.dataset.previousTop = String(messagesElement.scrollTop); }
    await act("older");
  }
  function modal(node) { node.showModal(); return { destroy() { node.close(); } }; }
  function closeDrawer() { drawer = false; tick().then(() => navigationButton?.focus()); }
  function unreadCount(item) { return Number(item?.unread_count || item?.unread || 0); }
  function unreadPosition(node) {
    if (!node) return { above:false, below:false };
    const bounds = node.getBoundingClientRect();
    const markers = [...node.querySelectorAll('[data-unread="true"]')].map((element) => element.getBoundingClientRect());
    return { above:markers.some((rect) => rect.top < bounds.top), below:markers.some((rect) => rect.bottom > bounds.bottom) };
  }
  function updateUnread() { serverUnread = unreadPosition(serverScroll); channelUnread = unreadPosition(channelScroll); }
  function scrollUnread(node, direction) {
    const bounds = node?.getBoundingClientRect();
    const candidates = [...(node?.querySelectorAll('[data-unread="true"]') || [])].filter((item) => direction === "above" ? item.getBoundingClientRect().top < bounds.top : item.getBoundingClientRect().bottom > bounds.bottom);
    (direction === "above" ? candidates.at(-1) : candidates[0])?.scrollIntoView({block:"nearest"});
  }
  function observeScroll(node) { const observer = new ResizeObserver(updateUnread); observer.observe(node); node.addEventListener("scroll", updateUnread, {passive:true}); return {destroy() { observer.disconnect(); node.removeEventListener("scroll", updateUnread); }}; }
  async function searchMessages() { showSearch = !showSearch; if (!showSearch) search = ""; else { await tick(); searchElement?.focus(); } }
</script>

{#snippet icon(name)}<svg class="icon" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d={icons[name] || icons.hash}/></svg>{/snippet}
{#snippet navigation()}
  <nav class="servers" aria-label="Servers">
    <a class="brand" href="/" aria-label="Nuschl home"><img src="/nuschl.svg" alt="" width="38" height="38"></a>
    <div class="server-scroll" bind:this={serverScroll} use:observeScroll>
      {#each servers as server, index (server.id)}
        <button class:active={selected?.id === server.id} class="server-icon" data-unread={unreadCount(server) > 0} title={server.name} aria-label={`Browse ${server.name}`} aria-current={selected?.id === server.id ? "true" : undefined} onclick={() => act("browse_server", index)}><span>{initials(server.name)}</span>{#if unreadCount(server)}<span class="badge">{Math.min(99, unreadCount(server))}</span>{/if}<i class:connected={server.status === "connected"} class="connection-dot" aria-label={labels[server.status]}></i></button>
      {/each}
    </div>
    {#if serverUnread.above}<button class="unread-jump rail-jump rail-up" aria-label="Unread servers above" onclick={() => scrollUnread(serverScroll,"above")}>↑</button>{/if}
    {#if serverUnread.below}<button class="unread-jump rail-jump" aria-label="Unread servers below" onclick={() => scrollUnread(serverScroll,"below")}>↓</button>{/if}
    <button class="add server-icon" aria-label="Add server" title="Add server" onclick={addServer}>{@render icon("plus")}</button>
    <button class="icon-button settings-rail" aria-label="Settings" title="Settings" onclick={() => { showSettings = true; drawer = false; }}>{@render icon("settings")}</button>
  </nav>
  <aside class="channels" aria-label="Server and channels">
    <header class="server-header"><div><span class="eyebrow">YOUR SERVER</span><h1>{selected?.name || "Nuschl"}</h1></div>{#if mobile}<button class="icon-button" aria-label="Close navigation" onclick={closeDrawer}>{@render icon("close")}</button>{:else}<button class="icon-button" aria-label="Server settings" title="Server settings" disabled={!selected} onclick={editServer}>{@render icon("settings")}</button>{/if}</header>
    {#if selected}
      <div class="server-connection"><span class:connected={selected.status === "connected"} class="connection-dot"></span><span class="status" role="status">{labels[selected.status]}</span><button class="text-button" onclick={() => act(selected.status === "disconnected" ? "connect" : "disconnect", selectedIndex)}>{selected.status === "disconnected" ? "Connect" : "Disconnect"}</button></div>
      <p class="security" title="TLS protects the connection; this server can read legacy chat.">{@render icon("unlock")}<span>Server can read chat</span><span class="transport">TLS</span></p>
      {#if selected.notice}<div class="notice" role="status"><span>{selected.notice}</span>{#if capabilities.includes("dismiss_notice")}<button class="icon-button" aria-label="Dismiss notice" onclick={() => act("dismiss_notice",selectedIndex)}>{@render icon("close")}</button>{/if}</div>{/if}
      {#if selected.certificate}<div class="certificate" role="alert"><strong>{selected.pin ? "Certificate changed" : "Verify server identity"}</strong><p>Compare this fingerprint with the server owner before trusting it.</p><code>{selected.certificate}</code><button class="primary" onclick={() => act("accept_certificate",selectedIndex,selected.certificate)}>Trust this certificate</button></div>{/if}
      <div class="section-label"><span>CHANNELS</span><span>{selected.channels.length}</span></div>
      {#if channelUnread.above}<button class="unread-jump" onclick={() => scrollUnread(channelScroll,"above")}>↑ Unread channels above</button>{/if}
      <nav class="channel-tree" aria-label="Channels" bind:this={channelScroll} use:observeScroll>
        {#each rows as row (row.id)}
          <div class="channel-row" style:--depth={Math.min(row.depth,8)}>
            {#if row.hasChildren}<button class="expand" aria-label={`${row.open ? "Collapse" : "Expand"} ${row.name}`} aria-expanded={row.open} onclick={() => toggle(row)}>{row.open ? "▾" : "▸"}</button>{:else}<span class="spacer"></span>{/if}
            <button class:current={timeline?.channel === row.id} class:voice-current={ownUser?.channel === row.id && selected.status === "connected"} class="channel" onpointerdown={(event) => lastChannelPointer = event.pointerType} data-unread={unreadCount(row) > 0} title={`${row.name || "Root"} — click for chat, double-click to join voice`} aria-label={row.name || "Root"} aria-current={timeline?.channel === row.id ? "page" : undefined} onclick={() => openChannel(row.id)} ondblclick={() => lastChannelPointer === "mouse" && !row.direct && joinVoice(row.id)}><span class="channel-symbol" aria-hidden="true">{row.direct ? "@" : "#"}</span><span class="channel-name">{row.name || "Root"}</span>{#if ownUser?.channel === row.id && !row.direct && selected.status === "connected"}<span class="voice-marker" title="Your voice channel">{@render icon("voice")}</span>{/if}{#if unreadCount(row)}<span class="badge">{Math.min(99,unreadCount(row))}</span>{/if}</button>
          </div>
          {#each usersByChannel.get(row.id) || [] as user (user.session)}<button class:talking={user.talking} class:own-user={user.session === selected.own_session} class="user" title={user.name} aria-label={user.session === selected.own_session ? `${user.name} (you)` : `Private message ${user.name}`} style:--depth={Math.min(row.depth,8)} disabled={user.session === selected.own_session} onclick={async () => { rememberDraft(); await act("open_direct",selectedIndex,user.session); if (mobile) drawer = false; }}><span class={`mini-avatar ${avatarColor(user.name)}`}>{initials(user.name)}</span><span class="user-name">{user.name}</span>{#if user.self_deaf || user.deafened}<span title="Deafened">{@render icon("headphones")}</span>{:else if user.self_mute || user.muted}<span title="Muted">{@render icon("muted")}</span>{/if}</button>{/each}
        {/each}
      </nav>
      {#if channelUnread.below}<button class="unread-jump" onclick={() => scrollUnread(channelScroll,"below")}>↓ Unread channels below</button>{/if}
      <p class="navigation-hint">Click to read · double-click for voice</p>
      <div class="profile"><span class={`avatar ${avatarColor(selected.username)}`} aria-hidden="true">{initials(selected.username)}</span><div><strong>{selected.username}</strong><small>{selected.status === "connected" ? selectedVoiceChannel?.name || "Connected" : "Offline"}</small></div><button class="icon-button" aria-label="Settings" onclick={() => { showSettings = true; drawer = false; }}>{@render icon("settings")}</button></div>
    {:else}<div class="nav-empty"><h2>A place for your people.</h2><p>Bring your Mumble server. Keep your conversations.</p><button class="primary" onclick={addServer}>Add your first server</button></div>{/if}
  </aside>
{/snippet}

<main>
  {#if !mobile}<div class="navigation">{@render navigation()}</div>{/if}
  <section class="chat" aria-label="Chat">
    <header class="chat-header">
      {#if mobile}<button class="icon-button menu-button" bind:this={navigationButton} aria-label="Open navigation" aria-expanded={drawer} onclick={() => drawer = true}>{@render icon("menu")}</button>{/if}
      <span class="header-hash" aria-hidden="true">{currentChannel?.direct ? "@" : "#"}</span><div class="channel-title"><h2>{timeline?.name || "Welcome to Nuschl"}</h2><span>{currentChannel?.direct ? "Private conversation" : selected?.name || "Your voice. Your space."}</span></div>
      <div class="header-actions">
        {#if timeline && !currentChannel?.direct}<button class="join-button" disabled={selected?.status !== "connected"} onclick={() => joinVoice()} title="Join this voice channel" aria-label="Join voice">{@render icon("voice")}<span>Join voice</span></button>{/if}
        <button class:pressed={showSearch} class="icon-button" aria-label="Search messages" aria-expanded={showSearch} title="Search loaded messages" disabled={!timeline} onclick={searchMessages}>{@render icon("search")}</button>
        <button class:pressed={membersOpen} class="icon-button members-toggle" aria-label="Show members" aria-expanded={membersOpen} disabled={!timeline} onclick={() => membersOpen = !membersOpen}>{@render icon("people")}</button>
        {#if !mobile}<button class="icon-button" aria-label="Add server" title="Add server" onclick={addServer}>{@render icon("plus")}</button>{/if}
      </div>
    </header>
    {#if showSearch}<form class="search-bar" onsubmit={(event) => event.preventDefault()}><label for="search-messages">Search this loaded history</label><input id="search-messages" bind:this={searchElement} bind:value={search} type="search" placeholder="Message text or person…" onkeydown={(event) => { if (event.key === "Escape") { showSearch = false; search = ""; } }}><small>{displayedMessages.length} results</small><button class="icon-button" aria-label="Close search" onclick={() => { showSearch = false; search = ""; }}>{@render icon("close")}</button></form>{/if}
    {#if error}<div class="error" role="alert"><span>{error}</span><button class="icon-button" aria-label="Dismiss error" onclick={() => error = ""}>{@render icon("close")}</button></div>{/if}
    <div class="conversation">
      <div class="messages" bind:this={messagesElement} onscroll={() => { if (messagesElement) follow = messagesElement.scrollHeight - messagesElement.scrollTop - messagesElement.clientHeight < 80; }}>
        {#if timeline?.has_older}<div class="history-start"><button class="text-button" disabled={timeline.loading} onclick={older}>{timeline.loading ? "Loading history…" : "Load older messages"}</button></div>{/if}
        {#if !timeline}<div class="welcome"><div class="welcome-logo"><img src="/nuschl.svg" alt="" width="74" height="74"></div><p class="eyebrow">A LITTLE CLOSER, FROM ANYWHERE</p><h2>Your voice. Your space.</h2><p>Drop into your server, find your people, and pick up the conversation.</p><button class="primary" onclick={addServer}>Add a server</button><small>Compatible with Mumble · history saved in this browser</small></div>
        {:else if !displayedMessages.length}<div class="channel-welcome"><span class="welcome-hash" aria-hidden="true">{currentChannel?.direct ? "@" : "#"}</span><h2>{search ? "No messages found" : `Welcome to ${timeline.name}`}</h2><p>{search ? "Try another word, or load older history." : timeline.loading ? "Loading your conversations…" : "This is the start of the history saved in this browser. Say hello."}</p></div>{/if}
        {#each displayedMessages as message (message.serial)}<Message {message}/>{/each}
      </div>
      {#if membersOpen && timeline}<aside class="members" aria-label="Channel members"><div class="section-label"><span>IN THIS CHANNEL</span><span>{selectedUsers.length}</span></div>{#each selectedUsers as user (user.session)}<button class="member" disabled={user.session === selected.own_session} onclick={() => act("open_direct",selectedIndex,user.session)}><span class={`mini-avatar ${avatarColor(user.name)}`}>{initials(user.name)}</span><span>{user.name}{user.session === selected.own_session ? " (you)" : ""}</span></button>{/each}{#if !selectedUsers.length}<p class="muted">No one here yet.</p>{/if}</aside>{/if}
    </div>
    {#if audioState?.error}<div class="audio-notice" role="status">{audioState.error}</div>{/if}
    <form class="composer" onsubmit={send}>
      <div class="composer-input"><label class="sr-only" for="message">Message</label><textarea id="message" bind:this={composerElement} bind:value={body} rows="2" maxlength="65536" oninput={rememberDraft} placeholder={timeline ? `Message ${timeline.name}` : "Choose a channel to start chatting"} disabled={!timeline || selected?.status !== "connected"} onkeydown={(event) => { if ((event.ctrlKey || event.metaKey) && event.key === "Enter") { event.preventDefault(); send(); } }}></textarea><button class="send-button" type="submit" aria-label="Send" title="Send (Ctrl/⌘ Enter)" disabled={!body.trim() || !canSend}>{@render icon("send")}</button></div>
      <div class="composer-footer"><small>Enter adds a newline · Ctrl/⌘ Enter sends</small><span class="local-history">History stays in this browser</span></div>
    </form>
  </section>
</main>

{#if mobile && drawer}<dialog class="navigation-drawer" use:modal onclose={closeDrawer} oncancel={closeDrawer} aria-label="Server navigation" onpointerdown={(event) => { if (event.target === event.currentTarget) closeDrawer(); }}><div class="navigation">{@render navigation()}</div></dialog>{/if}
{#if showAdd}<dialog class="modal server-modal" use:modal onclose={() => showAdd = false} oncancel={() => showAdd = false} aria-labelledby="server-form-title"><header class="modal-header"><div><p class="eyebrow">MAKE YOURSELF AT HOME</p><h2 id="server-form-title">Add a server</h2></div><button class="icon-button" aria-label="Close server form" onclick={() => showAdd = false}>{@render icon("close")}</button></header><form class="connection" onsubmit={connect}><p class="muted">Connect to your Mumble server. Your bookmarks and observed chat history are saved in this browser.</p><label>Display name <input bind:value={serverName} maxlength="128" autocomplete="off" placeholder="Friday night"></label><div class="form-row"><label>Server <input bind:value={host} required autocomplete="off" placeholder="voice.example.org"></label><label class="port-field">Port <input bind:value={port} type="number" min="1" max="65535" required></label></div><label>Your name <input bind:value={username} required maxlength="128" autocomplete="nickname" placeholder="How your friends know you"></label><details><summary>Certificate and connection</summary><label>Certificate SHA-256 (optional)<input bind:value={pin} autocomplete="off" spellcheck="false" placeholder="64 hexadecimal characters"></label><small class="relay">Relay: {relay}. TLS runs in this browser.</small></details><div class="modal-actions"><button type="button" onclick={() => showAdd = false}>Cancel</button><button class="primary" type="submit" disabled={!loaded || !!link.error}>Connect</button></div></form></dialog>{/if}
{#if showSettings}<dialog class="modal settings-modal" use:modal onclose={() => showSettings = false} oncancel={() => showSettings = false} aria-labelledby="settings-title"><header class="modal-header"><div><p class="eyebrow">MAKE IT YOURS</p><h2 id="settings-title">Settings</h2></div><button class="icon-button" aria-label="Close settings" onclick={() => showSettings = false}>{@render icon("close")}</button></header><section class="settings-section"><h3>Appearance</h3><label>Theme<select bind:value={settings.theme} onchange={persistSettings}><option value="system">Match device</option><option value="dark">Dark</option><option value="light">Light</option></select></label><label>Message spacing<select bind:value={settings.density} onchange={persistSettings}><option value="comfortable">Comfortable</option><option value="compact">Compact</option></select></label><label>Text size<select bind:value={settings.fontSize} onchange={persistSettings}><option value={14}>Small · 14px</option><option value={16}>Default · 16px</option><option value={18}>Large · 18px</option><option value={20}>Larger · 20px</option></select></label></section><section class="settings-section"><h3>Privacy & storage</h3><p>Bookmarks, trusted certificates and observed chat history are stored locally in this browser. Legacy servers can read chat; the relay only forwards encrypted traffic.</p><p>Links and received images never cause automatic requests to another site.</p></section>{#if selected}<section class="settings-section"><h3>{selected.name}</h3><p class="muted">{selected.host}:{selected.port} · {selected.username}</p><button onclick={() => { showSettings = false; editServer(); }}>Edit server bookmark</button>{#if capabilities.includes("remove_server")}<button class="danger" onclick={async () => { await act("remove_server",selectedIndex); showSettings = false; }}>Remove server</button>{/if}</section>{/if}<footer class="settings-footer"><span class="brand-word">nuschl<span>web</span></span><small>Built for conversations.</small></footer></dialog>{/if}
