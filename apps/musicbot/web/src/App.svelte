<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { onMount, tick } from "svelte";
  import { store } from "./store.svelte.js";
  import { postJSON } from "./api.js";
  import { listening } from "./listening.svelte.js";
  import { buildRoute, handlesSpace, routeForLibrary } from "./logic.js";
  import Browse from "./components/Browse.svelte";
  import Channels from "./components/Channels.svelte";
  import QueuePanel from "./components/QueuePanel.svelte";
  import HistoryPanel from "./components/HistoryPanel.svelte";
  import PlayerBar from "./components/PlayerBar.svelte";
  import BigScreen from "./components/BigScreen.svelte";
  import Icon from "./components/Icon.svelte";

  let menuOpen = $state(false);
  let menuButton = $state();
  let search = $state("");
  let bigScreen = $state(false);
  let bigScreenOpener = null;
  let confirmForget = $state(false);
  let forgetting = $state(false);
  let forgetOpener = $state();
  const route = $derived(store.route);
  const browseRouteKey = $derived(`${store.browseEpoch}:${store.browseKey}:${buildRoute(route)}`);
  const libraries = $derived(store.snapshot?.libraries ?? []);
  const links = $derived([
    { name: "Queue", icon: "queue", route: { view: "queue" }, views: ["queue"] },
    { name: "History", icon: "queue", route: { view: "history" }, views: ["history"] },
    { name: "Albums", icon: "album", route: { view: "albums", library: store.library, list: "newest" }, views: ["albums", "album", "song"] },
    { name: "Artists", icon: "artist", route: { view: "artists", library: store.library }, views: ["artists", "artist"] },
    { name: "Playlists", icon: "playlist", route: { view: "playlists", library: store.library }, views: ["playlists", "playlist"] },
    { name: "Channels", icon: "channel", route: { view: "channels" }, views: ["channels"] },
  ]);

  onMount(() => store.start());
  listening.attach();
  $effect(() => {
    if (store.invalid || !store.snapshot) bigScreen = false;
  });
  $effect(() => {
    const currentRoute = store.route;
    menuOpen = false;
    if (currentRoute.view === "search") search = currentRoute.q;
  });

  function changeLibrary(event) {
    if (store.browseKey === null) return;
    const library = Number(event.currentTarget.value);
    store.library = library;
    store.navigate(routeForLibrary(route, library));
  }

  function submitSearch(event) {
    event.preventDefault();
    store.navigate({ view: "search", q: search.trim() });
    menuOpen = false;
  }

  function openBigScreen(event) {
    bigScreenOpener = event.currentTarget;
    menuOpen = false;
    bigScreen = true;
  }

  // After a link inside the overlay, focus goes to the new page; otherwise back to the opener.
  async function closeBigScreen(navigated) {
    if (!bigScreen) return;
    bigScreen = false;
    await tick();
    const target = navigated || !bigScreenOpener?.isConnected ? document.getElementById("main-content") : bigScreenOpener;
    bigScreenOpener = null;
    target?.focus();
  }

  function keydown(event) {
    if (event.key === "Escape" && menuOpen && !bigScreen) {
      menuOpen = false;
      menuButton?.focus();
    }
    if (event.code !== "Space" || event.repeat || event.altKey || event.ctrlKey || event.metaKey || handlesSpace(event.target) || store.invalid || !store.player) return;
    event.preventDefault();
    store.togglePause(store.selected);
  }

  // Clears the remembered link; the bot's root then asks for a new `!web` link.
  async function forgetBrowser() {
    if (forgetting) return;
    forgetting = true;
    try {
      await postJSON("api/forget", {});
      location.assign("./");
    } catch (error) {
      forgetting = false;
      store.toast(`Could not forget this browser: ${error instanceof Error ? error.message : String(error)}`);
    }
  }

  async function cancelForget() {
    confirmForget = false;
    await tick();
    forgetOpener?.focus();
  }

  function focusOnMount(node) {
    node.focus();
  }
</script>

<svelte:window onkeydown={keydown} />

<div class="app-shell" inert={bigScreen}>
  <a class="skip-link" href="#main-content" onclick={(event) => { event.preventDefault(); document.getElementById("main-content")?.focus(); }}>Skip to content</a>
  <header class="app-header">
    {#if !store.invalid}
      <button class="icon-button menu-toggle" bind:this={menuButton} aria-label={menuOpen ? "Close navigation" : "Open navigation"} aria-expanded={menuOpen} aria-controls="navigation" onclick={() => (menuOpen = !menuOpen)}><Icon name={menuOpen ? "close" : "menu"} /></button>
    {/if}
    <a class="brand" href={buildRoute({ view: "queue" })}><Icon name="note" /> nuschl musicbot</a>
    {#if store.snapshot && !store.invalid}<span class="viewer muted">Listening as {store.snapshot.you}</span>{/if}
    <span class="live-status" class:online={store.connection === "open" && !store.invalid} role="status">{store.invalid ? "Link expired" : store.connection === "open" ? "Live" : store.connection === "reconnecting" ? "Reconnecting…" : "Connecting…"}</span>
  </header>

  {#if store.invalid}
    <main id="main-content" class="invalid-link" tabindex="-1">
      <h1>This link is not valid (any more).</h1>
      <p>Send <code>!web</code> to the music bot in Mumble for a new one.</p>
    </main>
  {:else}
    {#if store.connection === "reconnecting"}
      <div class="connection-banner" role="status">Connection interrupted. Reconnecting automatically; displayed playback state may be out of date.</div>
    {/if}
    <div class="workspace">
      {#if menuOpen}<button class="nav-backdrop" aria-label="Close navigation" onclick={() => (menuOpen = false)}></button>{/if}
      <aside id="navigation" class="sidebar" class:mobile-open={menuOpen} aria-label="Library navigation">
        <form class="search-form" role="search" onsubmit={submitSearch}>
          <label for="library-search" class="visually-hidden">Search all libraries</label>
          <input id="library-search" type="search" placeholder="Search music" bind:value={search} autocomplete="off" />
          <button class="icon-button" type="submit" aria-label="Search"><Icon name="search" /></button>
        </form>
        <label class="library-choice" for="library-choice">Library</label>
        <select id="library-choice" value={store.library} disabled={store.browseKey === null || !libraries.length} onchange={changeLibrary}>
          {#each libraries as name, index (index)}<option value={index}>{name}</option>{/each}
        </select>
        <nav aria-label="Music">
          {#each links as link (link.name)}
            <a href={buildRoute(link.route)} class:active={link.views.includes(route.view)} aria-current={link.views.includes(route.view) ? "page" : undefined} onclick={() => (menuOpen = false)}><Icon name={link.icon} />{link.name}</a>
          {/each}
        </nav>
        <p class="sidebar-help muted">Music plays in Mumble, or use the headphones button to hear it in this browser. Changes here are live for everyone sharing the queue.</p>
        <p class="sidebar-help"><a href="licenses">Licences</a></p>
        <div class="forget sidebar-help">
          {#if confirmForget}
            <p class="muted" id="forget-explanation">This browser will drop its link to the bot. To come back, send <code>!web</code> again.</p>
            <div class="forget-actions">
              <button aria-describedby="forget-explanation" disabled={forgetting} onclick={forgetBrowser}>Forget</button>
              <button disabled={forgetting} use:focusOnMount onclick={cancelForget}>Cancel</button>
            </div>
          {:else}
            <button class="link-button" bind:this={forgetOpener} onclick={() => (confirmForget = true)}>Forget this browser</button>
          {/if}
        </div>
      </aside>

      <main id="main-content" class="main-content" tabindex="-1">
        {#if !store.snapshot}
          <h1>Music</h1><p class="notice muted" role="status">Connecting to the music bot…</p>
        {:else if route.view === "queue"}
          <QueuePanel />
        {:else if route.view === "history"}
          <HistoryPanel />
        {:else if route.view === "channels"}
          <Channels />
        {:else}
          {#key browseRouteKey}<Browse {route} />{/key}
        {/if}
      </main>
      {#if store.snapshot && route.view !== "queue"}
        <aside class="queue-aside" aria-label="Live queue"><QueuePanel compact /></aside>
      {/if}
    </div>
    {#if store.snapshot}<PlayerBar onexpand={openBigScreen} />{/if}
  {/if}
</div>

{#if bigScreen}<BigScreen onclose={closeBigScreen} />{/if}

<div class="toast-list" aria-live="polite" aria-atomic="false">
  {#each store.toasts as toast (toast.id)}
    <div class="toast"><p>{toast.text}</p><button class="icon-button" aria-label="Dismiss notification" onclick={() => store.dismiss(toast.id)}><Icon name="close" size={18} /></button></div>
  {/each}
</div>

<style>
  .app-shell { height: 100dvh; min-height: 0; display: flex; flex-direction: column; }
  .app-header { display: flex; flex: none; align-items: center; gap: 1rem; min-height: 3.5rem; padding: 0.5rem 1rem; background: var(--surface); border-bottom: 1px solid var(--border); }
  .brand { display: flex; align-items: center; gap: 0.5rem; font-weight: 700; white-space: nowrap; }
  .viewer { margin-left: auto; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 0.85rem; }
  .live-status { color: var(--warning); font-size: 0.8rem; flex: none; margin-left: auto; }
  .viewer + .live-status { margin-left: 0; }
  .live-status.online { color: var(--accent); }
  .connection-banner { flex: none; padding: 0.5rem 1rem; color: var(--warning); background: var(--surface); font-size: 0.85rem; }
  .workspace { position: relative; flex: 1; min-height: 0; display: flex; }
  .sidebar { width: 13.5rem; flex: none; padding: 1rem; background: var(--surface); border-right: 1px solid var(--border); overflow-y: auto; }
  .search-form { display: flex; align-items: center; gap: 0.25rem; margin-bottom: 1.25rem; }
  .search-form input { min-width: 0; flex: 1; width: 100%; }
  .search-form button { padding: 0.3rem; }
  .library-choice { display: block; color: var(--muted); font-size: 0.8rem; margin-bottom: 0.35rem; }
  .sidebar select { width: 100%; }
  nav { display: grid; gap: 0.25rem; margin-top: 1rem; }
  nav a { display: flex; align-items: center; gap: 0.65rem; padding: 0.65rem 0.5rem; border-radius: 6px; color: var(--muted); }
  nav a:hover, nav a.active { background: var(--surface-hover); color: var(--text); }
  nav a.active { color: var(--accent); }
  .sidebar-help { font-size: 0.75rem; line-height: 1.5; margin-top: 2rem; }
  .forget { margin-top: 0.75rem; }
  .forget p { margin: 0 0 0.5rem; }
  .forget-actions { display: flex; gap: 0.5rem; }
  .link-button { min-height: 0; padding: 0; border: 0; background: none; color: inherit; cursor: pointer; }
  .link-button:hover:not(:disabled) { background: none; text-decoration: underline; }
  .main-content { flex: 1; min-width: 0; overflow: auto; padding: 1.5rem; scroll-behavior: smooth; }
  .queue-aside { width: 22rem; flex: none; overflow: auto; padding: 1rem; border-left: 1px solid var(--border); background: var(--surface); }
  .menu-toggle, .nav-backdrop { display: none; }
  .invalid-link { padding: 2rem; max-width: 50rem; margin: auto; }
  .toast-list { position: fixed; z-index: 50; right: 1rem; bottom: 7rem; width: min(26rem, calc(100% - 2rem)); display: grid; gap: 0.5rem; pointer-events: none; }
  .toast { pointer-events: auto; display: flex; align-items: flex-start; gap: 0.5rem; padding: 0.75rem; border: 1px solid var(--warning); border-radius: 8px; background: var(--surface); box-shadow: var(--shadow); }
  .toast p { margin: 0; flex: 1; overflow-wrap: anywhere; }
  @media (max-width: 1100px) { .queue-aside { display: none; } }
  @media (max-width: 760px) {
    .app-header { gap: 0.5rem; padding: 0.5rem 0.75rem; }
    .menu-toggle { display: grid; }
    .viewer { display: none; }
    .brand { font-size: 0.95rem; }
    .sidebar { display: none; position: absolute; z-index: 25; inset: 0 auto 0 0; width: min(18rem, 85vw); box-shadow: var(--shadow); }
    .sidebar.mobile-open { display: block; }
    .nav-backdrop { display: block; position: absolute; z-index: 24; inset: 0; background: #0009; border: 0; border-radius: 0; cursor: default; }
    .main-content { padding: 1rem 0.75rem; }
    .toast-list { bottom: 12rem; }
  }
</style>
