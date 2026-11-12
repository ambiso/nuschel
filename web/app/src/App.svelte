<!-- SPDX-License-Identifier: EUPL-1.2 -->
<script>
  import { onMount, tick } from "svelte";
  import { flip } from "svelte/animate";
  import { serverIndices, moveServer, sameOrder, edgeSpeed } from "./server-order.js";
  import { inviteFragment } from "./invite.js";
  import { channelRows, initials, avatarColor, conversationKey, filterMessages, appearance } from "./presentation.js";
  import Message from "./Message.svelte";
  import TypedDocument from "./TypedDocument.svelte";
  import { welcomeDocuments } from "./welcome.js";
  import { SdkRpc, SdkUnavailableError } from "./sdk-rpc.js";
  import { BrowserAudio } from "./audio.js";
  import { ErrorLifetime } from "./error-lifetime.js";
  const link = inviteFragment(location.hash);
  let relay = $state(`${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/relay`);
  let host = $state(link.host), port = $state(link.port), username = $state(link.username), pin = $state(link.pin), serverName = $state(""), password = $state("");
  let loaded = $state(false), clientUnavailable = $state(false), error = $state(link.error), servers = $state([]), timeline = $state(null);
  const errorLifetime = new ErrorLifetime();
  function setError(message, conversation = null) {
    errorLifetime.replace(conversation);
    error = message;
  }
  let showAdd = $state(false), editingIndex = $state(null), showSettings = $state(false), drawer = $state(false), mobile = $state(false);
  let body = $state(""), expanded = $state(new Map()), search = $state(""), showSearch = $state(false);
  let messagesElement = $state(), composerElement = $state(), navigationButton = $state(), searchElement = $state();
  let settings = $state(appearance(null)), voice = $state(null), audioState = $state(null), audioActionPending = $state(null), capabilities = $state([]);
  // The requested voice bitrate (voice-bitrate.js); the SDK worker resolves it per server.
  import { VOICE_BITRATES, initialVoiceBitrate, storeVoiceBitrate, bitrateLine, bitrateExplanation, kbps } from "./voice-bitrate.js";
  let voiceBitrate = $state(initialVoiceBitrate()), sentVoiceBitrate = null;
  $effect(() => {
    // Once the SDK is ready, and after each choice; again after a restart.
    if (!loaded) { sentVoiceBitrate = null; return; }
    if (sentVoiceBitrate === voiceBitrate) return;
    sentVoiceBitrate = voiceBitrate;
    void act("set_voice_bitrate", voiceBitrate);
  });
  let serverScroll = $state(), channelScroll = $state(), serverUnread = $state({above:false,below:false}), channelUnread = $state({above:false,below:false});
  let welcomes = $state(new Map());
  const welcome = $derived(selected?.status === "connected" && selected.welcome_token ? welcomes.get(selected.id) : null);
  let membersOpen = $state(false), selectedFallback = $state(null);
  let arrangeServers = $state(false), orderPending = $state(false), previewIds = $state(null), draggingId = $state(null), pointerGhost = $state(null), orderStatus = $state(""), reducedOrderMotion = $state(false);
  let orderDrag = null, orderFrame = 0, orderTime = 0, suppressBrowseUntil = 0;
  const railServers = $derived.by(() => {
    if (!previewIds) return servers;
    const byId = new Map(servers.map(server => [server.id, server]));
    return previewIds.map(id => byId.get(id)).filter(Boolean);
  });
  let lastChannelPointer = "";
  let initialState = true, follow = true, worker, rpc, audio, activateAudio = () => {}, disposeAudio = () => {};
  const drafts = new Map();
  const selected = $derived(servers.find((server) => server.id === timeline?.server) || servers.find(server => server.id === selectedFallback) || servers[0]);
  const selectedIndex = $derived(selected?.runtime_index ?? -1);
  const voiceServer = $derived(servers.find(server => server.id === voice?.server_id));
  const voiceIndex = $derived(voiceServer?.runtime_index ?? -1);
  const voiceChannel = $derived(voiceServer?.channels.find(channel => channel.id === voice?.channel_id));
  const microphoneMuted = $derived(voiceServer?.muted !== false || voiceServer?.deafened === true);
  const playbackDeafened = $derived(voiceServer?.deafened !== false);
  const voiceStatus = $derived(!voice?.joined ? "Joining…" : voice.deafened ? "Sound off"
    : microphoneMuted ? "Microphone muted" : audioState?.phase === "permission-pending" ? "Waiting for microphone permission"
    : audioState?.phase === "denied" ? "Microphone permission denied" : audioState?.phase === "unavailable" ? "Audio unavailable"
    : audioState?.microphone && voice.can_transmit ? "Microphone on" : voice.muted ? "Microphone blocked" : "Microphone off");
  const voiceBitrateLine = $derived(bitrateLine(voice)), voiceBitrateWhy = $derived(bitrateExplanation(voice));
  const rows = $derived(selected ? channelRows(selected.channels, expanded, selected.id) : []);
  const currentChannel = $derived(selected?.channels.find((channel) => channel.id === timeline?.channel));
  const ownUser = $derived(selected?.users.find((user) => user.session === selected.own_session));
  const selectedVoiceChannel = $derived(selected?.channels.find((channel) => channel.id === ownUser?.channel));
  const displayedMessages = $derived(filterMessages(timeline?.messages || [], search));
  const selectedUsers = $derived((selected?.users || []).filter((user) => user.channel === timeline?.channel));
  const canSend = $derived(loaded && !!timeline && selected?.status === "connected" && !timeline.sending);
  const usersByChannel = $derived.by(() => {
    const groups = new Map();
    for (const user of selected?.users || []) { const group = groups.get(user.channel) || []; group.push(user); groups.set(user.channel, group); }
    return groups;
  });
  const labels = { connected:"Connected", connecting:"Connecting…", disconnected:"Disconnected", unavailable:"Unavailable" };
  const icons = {
    menu:"M4 6h16M4 12h16M4 18h16", close:"m6 6 12 12M6 18 18 6", plus:"M12 5v14M5 12h14", hash:"M10 3 8 21M17 3l-2 18M4 9h17M3 15h17",
    voice:"M11 5 6 9H3v6h3l5 4V5Zm4 3a6 6 0 0 1 0 8m3-11a10 10 0 0 1 0 14", mic:"M12 15a3 3 0 0 0 3-3V5a3 3 0 0 0-6 0v7a3 3 0 0 0 3 3Zm-6-4v1a6 6 0 0 0 12 0v-1M12 18v4M8 22h8", muted:"m3 3 18 18M9 9v3a3 3 0 0 0 5 2M9 5a3 3 0 0 1 6 0v5M6 11v1a6 6 0 0 0 10 4M18 11v1M12 18v4M8 22h8",
    headphones:"M4 14v-3a8 8 0 0 1 16 0v3M4 13h4v8H4v-8Zm12 0h4v8h-4v-8", search:"m21 21-5-5M10 18a8 8 0 1 0 0-16 8 8 0 0 0 0 16", people:"M16 21v-2a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v2M16 3a4 4 0 0 1 0 8m6 10v-2a4 4 0 0 0-3-3.87M13 7a4 4 0 1 1-8 0 4 4 0 0 1 8 0",
    settings:"m9 3-.7 3-2.8 1-2.5-1.5-2 3.5 2.3 2V14L1 16l2 3.5L5.5 18l2.8 1L9 22h4l.7-3 2.8-1 2.5 1.5 2-3.5-2.3-2v-3L21 9l-2-3.5L16.5 7l-2.8-1L13 3H9ZM15 12a4 4 0 1 1-8 0 4 4 0 0 1 8 0", send:"m22 2-7 20-4-9-9-4 20-7ZM11 13 22 2", leave:"M10 17l5-5-5-5M15 12H3M15 3h5v18h-5", lock:"M5 10h14v11H5V10Zm3 0V6a4 4 0 0 1 8 0", unlock:"M5 10h14v11H5V10Zm3 0V6a4 4 0 0 1 7-3", chevron:"m9 5 7 7-7 7",
  };
  function command(method, ...args) {
    if (!worker || !rpc || rpc.closed) return Promise.reject(clientUnavailable
      ? new SdkUnavailableError() : new Error("The client is still starting."));
    if (["connect","connect_with_password","disconnect","remove_server","edit_server"].includes(method) && servers.find(server => server.runtime_index === args[0])?.id === voice?.server_id) {
      audio?.syncVoice(null); voice = null; audioActionPending = null;
    }
    return rpc.request(method,args);
  }
  async function act(method, ...args) { try { return await command(method, ...args); } catch (reason) { setError(String(reason?.message || reason)); } }
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
    try { localStorage.setItem("nuschl.appearance.v1", JSON.stringify(settings)); } catch { setError("Appearance preferences could not be saved in this browser."); }
  }
  function chooseVoiceBitrate() {
    let stored = false;
    try { stored = storeVoiceBitrate(localStorage, voiceBitrate); } catch { /* reported below */ }
    if (!stored) setError("The voice bitrate could not be saved in this browser; it applies until you reload.");
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
    const motion = matchMedia("(prefers-reduced-motion: reduce)");
    const updateMotion = () => { reducedOrderMotion = motion.matches; };
    updateMotion(); motion.addEventListener("change", updateMotion);
    const escapeOrder = event => {
      if (event.key === "Escape" && (orderDrag || arrangeServers)) {
        cancelOrder(); arrangeServers = false; event.preventDefault(); event.stopPropagation();
      }
    };
    window.addEventListener("keydown", escapeOrder, true);
    window.addEventListener("resize", cancelOrder);
    const viewport = window.visualViewport;
    const resizeViewport = () => { if (orderDrag) cancelOrder(); document.documentElement.style.setProperty("--app-height", `${viewport?.height || window.innerHeight}px`); };
    resizeViewport(); viewport?.addEventListener("resize", resizeViewport);
    const cleanup = () => { cancelOrder(); window.removeEventListener("keydown", escapeOrder, true); window.removeEventListener("resize", cancelOrder); motion.removeEventListener("change", updateMotion); media.removeEventListener("change", updateMobile); viewport?.removeEventListener("resize", resizeViewport); rpc?.close("Client closed.",{notify:false}); worker = null; disposeAudio(); };
    const unavailable = reason => {
      worker = null; clientUnavailable = true; loaded = false; cancelOrder(); arrangeServers = false;
      // Keep conversation text, unsent drafts, bookmarks and history. Only
      // transport-derived presence is invalid once its owner is retired.
      audio?.syncVoice(null); voice = null; audioActionPending = null;
      servers = servers.map(server => ({...server,status:"unavailable",muted:true,deafened:true,
        users:server.users.map(user => ({...user,talking:false}))}));
      if (timeline?.sending) timeline = {...timeline,sending:false};
      setError(reason);
    };
    try { worker = new Worker(new URL("./sdk.worker.js", import.meta.url), { type:"module" }); }
    catch { unavailable("The browser client could not be created. Reload to try again."); return cleanup; }
    audio = new BrowserAudio({
      onPacket: packet => command("voice_packet",packet),
      onReady: ready => command("audio_ready",ready),
      onState: state => { audioState = state; },
    });
    const ownedWorker = worker;
    rpc = new SdkRpc(ownedWorker,{onClose:reason => {
      if (worker === ownedWorker) unavailable(reason);
    }});
    activateAudio = options => audio.activateGesture(options);
    disposeAudio = () => audio.dispose();
    ownedWorker.onmessage = async ({ data }) => {
      if (worker !== ownedWorker || rpc.closed || !data || typeof data !== "object") return;
      if (data.kind !== "ready" && data.kind !== "error" && rpc.phase !== "ready") return;
      if (data.kind === "voice_packet") {
        // Audio packets never replace reactive chat/server state.
        audio.receive(data.packet); return;
      }
      if (data.kind === "result") {
        rpc.accept(data);
      } else if (data.kind === "error") setError(data.message);
      else if (data.kind === "ready") {
        if (!rpc.acceptReady()) return;
        loaded = true; capabilities = data.capabilities || [];
        if (link.host && link.username && !link.error) await connect();
      } else if (data.kind === "state") {
        const prior = timeline, priorKey = conversationKey(timeline, selected);
        rememberDraft();
        selectedFallback = selected?.id || null;
        try { servers = serverIndices(data.servers, capabilities.includes("server_order")); }
        catch (reason) { audio.setLocalPolicy({muted:true,deafened:true}); audio.syncVoice(null); voice = null; loaded = false; setError("The server list could not be read safely. Reload to reconnect."); return; }
        welcomes = welcomeDocuments(welcomes, servers, data.updates);
        if (orderDrag && !sameOrder(orderDrag.baseline, servers.map(server => server.id))) cancelOrder();
        timeline = data.timeline;
        voice = data.voice || null;
        audio.syncVoice(voice);
        if (audioActionPending) {
          const state=servers.find(server=>server.id===audioActionPending.server);
          if (!voice || !state || (state.muted===audioActionPending.muted && state.deafened===audioActionPending.deafened)) audioActionPending=null;
        }
        for (const update of data.updates || []) {
          if (update.kind === "error") setError(update.message);
          if (update.kind === "sent") {
            const target = servers.find(server => server.id === update.server);
            const key = conversationKey({server:update.server,channel:update.channel},target);
            if (drafts.get(key) === update.body) drafts.delete(key);
            if (key === conversationKey(timeline,selected) && body === update.body) body = "";
          }
        }
        if (initialState) { initialState = false; if (!servers.length && !link.host) showAdd = true; }
        const changed = priorKey !== conversationKey(timeline, selected);
        if (changed) {
          body = drafts.get(conversationKey(timeline, selected)) || ""; search = ""; follow = true;
          if (messagesElement) delete messagesElement.dataset.previousHeight;
        }
        await tick();
        if (worker !== ownedWorker || rpc.closed) return;
        if (messagesElement?.dataset.previousHeight && prior?.loading && !timeline?.loading) {
          const difference = messagesElement.scrollHeight - Number(messagesElement.dataset.previousHeight);
          messagesElement.scrollTop = Number(messagesElement.dataset.previousTop || 0) + difference;
          delete messagesElement.dataset.previousHeight; delete messagesElement.dataset.previousTop;
        } else if (messagesElement && follow && !search) messagesElement.scrollTop = messagesElement.scrollHeight;
        updateUnread();
      }
    };
    ownedWorker.onerror = event => {
      if (worker !== ownedWorker) return;
      event.preventDefault(); rpc.close("The browser worker stopped. Reload to reconnect.");
    };
    ownedWorker.onmessageerror = () => {
      if (worker === ownedWorker) rpc.close("The browser worker sent an unreadable response. Reload to reconnect.");
    };
    rpc.initialize(relay);
    return cleanup;

  });
  async function connect(event) {
    event?.preventDefault();
    if (link.error) { setError(link.error); return; }
    try {
      setError("");
      const fields = [serverName.trim(),host.trim(),Number(port),username.trim(),pin.trim() || undefined];
      const index = editingIndex === null ? await command("add_server",...fields) : await command("edit_server",editingIndex,...fields);
      if (password) await command("connect_with_password",index,password); else await command("connect", index);
      password = ""; await command("open", index, 0); showAdd = false;
    } catch (reason) { setError(String(reason?.message || reason)); }
  }
  function addServer() { editingIndex = null; password = ""; serverName = ""; host = ""; port = 64738; username = selected?.username || ""; pin = ""; showAdd = true; drawer = false; }
  function editServer() { if (!selected) return; editingIndex = selectedIndex; password = ""; serverName = selected.name; host = selected.host; port = selected.port; username = selected.username; pin = selected.pin || ""; showAdd = true; drawer = false; }
  async function send(event) {
    event?.preventDefault();
    const sent = body;
    if (!sent.trim() || !canSend) return;
    // Shared SDK Sent identifies both the original conversation and body.
    // A command being queued never clears text typed during an earlier send.
    const conversation = conversationKey(timeline, selected);
    const previousError = errorLifetime.capture(conversation);
    try {
      await command("send_text", sent);
      if (errorLifetime.accepts(previousError)) setError("");
    } catch (reason) { setError(String(reason?.message || reason), conversation); }
  }
  async function openChannel(id) { rememberDraft(); await act("open", selectedIndex, id); if (mobile) drawer = false; }
  function joinVoice(channel = timeline?.channel) {
    if (!selected || selected.status !== "connected" || channel == null) return;
    // Revoke the old destination before touching an asynchronous command.
    audio?.syncVoice(null); voice = null;
    void activateAudio({microphone:false}); void act("join", selectedIndex, channel);
  }
  function toggleMicrophone() {
    if (voiceIndex < 0 || !voice?.joined || audioActionPending) return;
    audioActionPending={server:voice.server_id,muted:!microphoneMuted,deafened:false};
    if (microphoneMuted) void activateAudio({microphone:true});
    else audio?.setLocalPolicy({muted:true});
    void command("toggle_mute",voiceIndex).catch(reason=>{audioActionPending=null;setError(String(reason?.message || reason));});
  }
  function togglePlayback() {
    if (voiceIndex < 0 || !voice?.joined || audioActionPending) return;
    audioActionPending={server:voice.server_id,muted:true,deafened:!playbackDeafened};
    if (playbackDeafened) void activateAudio({microphone:false});
    else audio?.setLocalPolicy({muted:true,deafened:true});
    void command("toggle_deaf",voiceIndex).catch(reason=>{audioActionPending=null;setError(String(reason?.message || reason));});
  }
  function stopAudio() {
    audio?.setLocalPolicy({muted:true,deafened:true}); audio?.syncVoice(null);
    voice = null; audioActionPending=null; void act("stop_audio");
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
  function cancelOrder() {
    if (orderFrame) cancelAnimationFrame(orderFrame);
    orderFrame = 0; orderTime = 0; orderDrag = null; previewIds = null; draggingId = null; pointerGhost = null;
  }
  function beginOrder(event, id, pointer = false) {
    if (orderPending || !capabilities.includes("server_order") || !serverScroll) { event.preventDefault(); return; }
    if (pointer && event.button !== 0) return;
    const baseline = servers.map(server => server.id), slot = serverScroll.querySelector(".server-order-slot");
    if (!baseline.includes(id) || !slot) return;
    cancelOrder();
    const style = getComputedStyle(serverScroll);
    orderDrag = {id, baseline, rect:serverScroll.getBoundingClientRect(), pitch:slot.offsetHeight + (parseFloat(style.rowGap) || 0), padding:parseFloat(style.paddingTop) || 0, pointer, pointerId:event.pointerId, point:{x:event.clientX,y:event.clientY}};
    previewIds = baseline.slice(); draggingId = id;
    if (pointer) {
      event.preventDefault(); event.stopPropagation(); serverScroll.setPointerCapture(event.pointerId);
      pointerGhost = {...orderDrag.point};
    } else { event.dataTransfer.effectAllowed = "move"; event.dataTransfer.setData("application/x-nuschl-server-order", "1"); }
    orderStatus = "Move the server, then release to save. Escape cancels.";
  }
  function orderPosition() {
    if (!orderDrag) return;
    const {rect,point,pitch,padding,id} = orderDrag;
    if (point.x < rect.left || point.x > rect.right || point.y < rect.top || point.y > rect.bottom) return;
    const target = Math.floor((point.y - rect.top + serverScroll.scrollTop - padding) / pitch);
    const next = moveServer(previewIds,id,target);
    if (next !== previewIds) previewIds = next;
  }
  function scheduleOrderScroll() {
    if (orderFrame || !orderDrag) return;
    const step = now => {
      orderFrame = 0;
      if (!orderDrag || !serverScroll) return;
      const {rect,point} = orderDrag;
      const speed = edgeSpeed(point.y - rect.top,rect.height,point.x >= rect.left && point.x <= rect.right);
      const before = serverScroll.scrollTop;
      serverScroll.scrollTop += speed * Math.min(.05,orderTime ? (now-orderTime)/1000 : .016);
      orderTime = now;
      orderPosition();
      if (speed && serverScroll.scrollTop !== before) orderFrame = requestAnimationFrame(step);
    };
    orderFrame = requestAnimationFrame(step);
  }
  function moveOrder(event, pointer = false) {
    if (!orderDrag || orderDrag.pointer !== pointer || (pointer && event.pointerId !== orderDrag.pointerId)) return;
    event.preventDefault();
    if (!pointer && event.dataTransfer) event.dataTransfer.dropEffect = "move";
    orderDrag.point = {x:event.clientX,y:event.clientY};
    if (pointer) pointerGhost = {...orderDrag.point};
    orderPosition(); scheduleOrderScroll();
  }
  function cancelPointerOrder(event) {
    // Native HTML dragging cancels its mouse pointer stream. Only our captured
    // touch/handle gesture owns these cancellation/capture-loss events.
    if (!orderDrag?.pointer || event.pointerId !== orderDrag.pointerId) return;
    if (event.type === "lostpointercapture" && event.target !== serverScroll) return;
    cancelOrder();
  }
  async function saveOrder(ids) {
    if (orderPending || !capabilities.includes("server_order") || sameOrder(ids,servers.map(server => server.id))) return;
    orderPending = true; orderStatus = "Saving server order…";
    try { await command("set_server_order",ids); orderStatus = "Server order saved in this browser."; }
    catch (reason) { setError(String(reason?.message || reason)); orderStatus = "Server order was not saved."; }
    finally { orderPending = false; await tick(); updateUnread(); }
  }
  function dropOrder(event, pointer = false) {
    if (!orderDrag || orderDrag.pointer !== pointer || (pointer && event.pointerId !== orderDrag.pointerId)) return;
    event.preventDefault(); event.stopPropagation();
    const {point,rect,baseline} = orderDrag, ids = previewIds.slice();
    const valid = point.x >= rect.left && point.x <= rect.right && point.y >= rect.top && point.y <= rect.bottom && sameOrder(baseline,servers.map(server=>server.id));
    cancelOrder(); suppressBrowseUntil = performance.now() + 250;
    if (pointer && serverScroll?.hasPointerCapture(event.pointerId)) serverScroll.releasePointerCapture(event.pointerId);
    if (valid) void saveOrder(ids);
  }
  async function moveOrderKey(id, delta) {
    const current = servers.map(server => server.id), from = current.indexOf(id);
    if (from < 0) return;
    cancelOrder(); await saveOrder(moveServer(current,id,from+delta));
    await tick();
    const slot = [...(serverScroll?.querySelectorAll(".server-order-slot") || [])].find(node=>node.dataset.serverId===id);
    slot?.scrollIntoView({block:"nearest",behavior:reducedOrderMotion ? "instant" : "smooth"});
  }
  function orderKey(event,id,handle = false) {
    if ((handle || event.ctrlKey && event.shiftKey) && (event.key === "ArrowUp" || event.key === "ArrowDown")) {
      event.preventDefault(); event.stopPropagation(); void moveOrderKey(id,event.key === "ArrowUp" ? -1 : 1);
    }
  }
  async function searchMessages() { showSearch = !showSearch; if (!showSearch) search = ""; else { await tick(); searchElement?.focus(); } }
</script>

{#snippet icon(name)}<svg class="icon" width="20" height="20" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d={icons[name] || icons.hash}/></svg>{/snippet}
{#snippet navigation()}
  {#if pointerGhost && draggingId}<div class="server-order-ghost" aria-hidden="true" style:left={`${pointerGhost.x - 23}px`} style:top={`${pointerGhost.y - 23}px`}>{initials(servers.find(server=>server.id===draggingId)?.name || "")}</div>{/if}
  <nav class="servers" class:server-order-enabled={capabilities.includes("server_order")} aria-label="Servers">
    <a class="brand" href="/" aria-label="Nuschl home"><img src="/nuschl.svg" alt="" width="38" height="38"></a>
    <div class="server-scroll" role="presentation" bind:this={serverScroll} use:observeScroll ondragover={event => moveOrder(event)} ondrop={event => dropOrder(event)} onpointermove={event=>moveOrder(event,true)} onpointerup={event=>dropOrder(event,true)} onpointercancel={cancelPointerOrder} onlostpointercapture={cancelPointerOrder}>
      {#each railServers as server (server.id)}
        <div class="server-order-slot" data-server-id={server.id} class:server-order-source={draggingId === server.id} animate:flip={{duration:reducedOrderMotion ? 0 : 180}}>
          <button class:active={selected?.id === server.id} class="server-icon" data-unread={unreadCount(server) > 0} title={server.name} aria-label={`Browse ${server.name}`} aria-current={selected?.id === server.id ? "true" : undefined} draggable={capabilities.includes("server_order") && !arrangeServers && !orderPending} ondragstart={event => beginOrder(event,server.id)} ondragend={cancelOrder} onkeydown={event=>orderKey(event,server.id)} onclick={() => { if (performance.now() >= suppressBrowseUntil && !orderDrag) void act("browse_server",server.runtime_index); }}><span>{initials(server.name)}</span>{#if unreadCount(server)}<span class="badge">{Math.min(99, unreadCount(server))}</span>{/if}<i class:connected={server.status === "connected"} class="connection-dot" aria-label={labels[server.status]}></i></button>
          {#if arrangeServers}<button class="server-order-handle" aria-label={`Move ${server.name}; use arrow keys or drag`} title="Drag to move; arrow keys also work" disabled={orderPending} onpointerdown={event=>beginOrder(event,server.id,true)} onkeydown={event=>orderKey(event,server.id,true)} onclick={()=>orderStatus="Use Up and Down to move this server, or drag its handle."}>⋮</button>{/if}
        </div>
      {/each}
    </div>
    {#if serverUnread.above}<button class="unread-jump rail-jump rail-up" aria-label="Unread servers above" onclick={() => scrollUnread(serverScroll,"above")}>↑</button>{/if}
    {#if serverUnread.below}<button class="unread-jump rail-jump" aria-label="Unread servers below" onclick={() => scrollUnread(serverScroll,"below")}>↓</button>{/if}
    {#if capabilities.includes("server_order")}<button class="icon-button server-order-toggle" aria-label={arrangeServers ? "Finish arranging servers" : "Reorder servers"} aria-pressed={arrangeServers} title="Reorder servers" disabled={orderPending} onclick={() => { cancelOrder(); arrangeServers = !arrangeServers; orderStatus = arrangeServers ? "Drag a handle, or focus it and use arrow keys." : ""; }}>↕</button>{/if}
    <span class="sr-only" role="status" aria-live="polite">{orderStatus}</span>
    <button class="add server-icon" aria-label="Add server" title="Add server" onclick={addServer}>{@render icon("plus")}</button>
    <button class="icon-button settings-rail" aria-label="Settings" title="Settings" onclick={() => { showSettings = true; drawer = false; }}>{@render icon("settings")}</button>
  </nav>
  <aside class="channels" aria-label="Server and channels">
    <header class="server-header"><div><span class="eyebrow">YOUR SERVER</span><h1>{selected?.name || "Nuschl"}</h1></div>{#if mobile}<button class="icon-button" aria-label="Close navigation" onclick={closeDrawer}>{@render icon("close")}</button>{:else}<button class="icon-button" aria-label="Server settings" title="Server settings" disabled={!selected} onclick={editServer}>{@render icon("settings")}</button>{/if}</header>
    {#if selected}
      <div class="server-connection"><span class:connected={selected.status === "connected"} class="connection-dot"></span><span class="status" role="status">{labels[selected.status]}</span>{#if !clientUnavailable}<button class="text-button" disabled={!loaded} onclick={() => act(selected.status === "disconnected" ? "connect" : "disconnect", selectedIndex)}>{selected.status === "disconnected" ? "Connect" : "Disconnect"}</button>{/if}</div>
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
  {#if !mobile}<div class="navigation" class:server-order-layout={capabilities.includes("server_order")}>{@render navigation()}</div>{/if}
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
    {#if clientUnavailable}<div class="error" role="alert"><span>{error || "The client is unavailable. Reload to reconnect."}</span><button class="text-button" onclick={() => location.reload()}>Reload client</button></div>{:else if error}<div class="error" role="alert"><span>{error}</span><button class="icon-button" aria-label="Dismiss error" onclick={() => setError("")}>{@render icon("close")}</button></div>{/if}
    {#if welcome}
      {#key welcome.token}<section class="server-welcome" aria-label="Server welcome">
        <header><h3 title={`Welcome to ${selected.name}`}>Welcome to {selected.name}</h3><button onclick={() => act("dismiss_welcome", selectedIndex, welcome.token)} aria-label="Dismiss server welcome">Dismiss</button></header>
        <div class="server-welcome-body" tabindex="0" role="region" aria-label="Welcome message"><TypedDocument document={welcome.document} /></div>
      </section>{/key}
    {/if}
    <div class="conversation">
      <div class="messages" bind:this={messagesElement} onscroll={() => { if (messagesElement) follow = messagesElement.scrollHeight - messagesElement.scrollTop - messagesElement.clientHeight < 80; }}>
        {#if timeline?.has_older}<div class="history-start"><button class="text-button" disabled={timeline.loading} onclick={older}>{timeline.loading ? "Loading history…" : "Load older messages"}</button></div>{/if}
        {#if !timeline}<div class="welcome"><div class="welcome-logo"><img src="/nuschl.svg" alt="" width="74" height="74"></div><p class="eyebrow">A LITTLE CLOSER, FROM ANYWHERE</p><h2>Your voice. Your space.</h2><p>Drop into your server, find your people, and pick up the conversation.</p><button class="primary" onclick={addServer}>Add a server</button><small>Compatible with Mumble · history saved in this browser</small></div>
        {:else if !displayedMessages.length}<div class="channel-welcome"><span class="welcome-hash" aria-hidden="true">{currentChannel?.direct ? "@" : "#"}</span><h2>{search ? "No messages found" : `Welcome to ${timeline.name}`}</h2><p>{search ? "Try another word, or load older history." : timeline.loading ? "Loading your conversations…" : "This is the start of the history saved in this browser. Say hello."}</p></div>{/if}
        {#each displayedMessages as message (message.serial)}<Message {message}/>{/each}
      </div>
      {#if membersOpen && timeline}<aside class="members" aria-label="Channel members"><div class="section-label"><span>IN THIS CHANNEL</span><span>{selectedUsers.length}</span></div>{#each selectedUsers as user (user.session)}<button class="member" disabled={user.session === selected.own_session} onclick={() => act("open_direct",selectedIndex,user.session)}><span class={`mini-avatar ${avatarColor(user.name)}`}>{initials(user.name)}</span><span>{user.name}{user.session === selected.own_session ? " (you)" : ""}</span></button>{/each}{#if !selectedUsers.length}<p class="muted">No one here yet.</p>{/if}</aside>{/if}
    </div>
    {#if voice}
      <section class="voice-bar" aria-label="Voice controls">
        <div class="voice-description"><strong>{voiceChannel?.name || "Voice channel"}</strong><small>{voiceServer?.name} · {voiceStatus}</small>{#if voiceBitrateLine}<small class="voice-bitrate" title={voiceBitrateWhy}>{voiceBitrateLine}</small>{/if}</div>
        <div class="voice-actions">
          <button class:pressed={microphoneMuted} class="icon-button" disabled={!voice.joined || !!audioActionPending} aria-label={microphoneMuted ? "Unmute microphone" : "Mute microphone"} aria-pressed={microphoneMuted} title={microphoneMuted ? "Unmute microphone" : "Mute microphone"} onclick={toggleMicrophone}>{@render icon(microphoneMuted ? "muted" : "mic")}</button>
          <button class:pressed={playbackDeafened} class="icon-button" disabled={!voice.joined || !!audioActionPending} aria-label={playbackDeafened ? "Enable sound" : "Deafen"} aria-pressed={playbackDeafened} title={playbackDeafened ? "Enable sound" : "Deafen (also mutes microphone)"} onclick={togglePlayback}>{@render icon("headphones")}</button>
          {#if !voice.deafened && (audioState?.contextState !== "running" || audioState?.phase === "deafened")}<button class="text-button" onclick={() => activateAudio({microphone:false})}>Enable audio</button>{/if}
          <button class="icon-button" aria-label="Stop audio" title="Stop audio; stay connected for chat" onclick={stopAudio}>{@render icon("leave")}</button>
        </div>
      </section>
    {/if}
    {#if audioState?.message && voice}<div class="audio-notice" role="status">{audioState.message}</div>{/if}
    <form class="composer" onsubmit={send}>
      <div class="composer-input"><label class="sr-only" for="message">Message</label><textarea id="message" bind:this={composerElement} bind:value={body} rows="2" maxlength="65536" oninput={rememberDraft} placeholder={timeline ? `Message ${timeline.name}` : "Choose a channel to start chatting"} readonly={clientUnavailable} disabled={!timeline || (selected?.status !== "connected" && !clientUnavailable)} onkeydown={(event) => { if ((event.ctrlKey || event.metaKey) && event.key === "Enter") { event.preventDefault(); send(); } }}></textarea><button class="send-button" type="submit" aria-label="Send" title="Send (Ctrl/⌘ Enter)" disabled={!body.trim() || !canSend}>{@render icon("send")}</button></div>
      <div class="composer-footer"><small>Enter adds a newline · Ctrl/⌘ Enter sends</small><span class="local-history">History stays in this browser</span></div>
    </form>
  </section>
</main>

{#if mobile && drawer}<dialog class="navigation-drawer" use:modal onclose={closeDrawer} oncancel={closeDrawer} aria-label="Server navigation" onpointerdown={(event) => { if (event.target === event.currentTarget) closeDrawer(); }}><div class="navigation" class:server-order-layout={capabilities.includes("server_order")}>{@render navigation()}</div></dialog>{/if}
{#if showAdd}<dialog class="modal server-modal" use:modal onclose={() => showAdd = false} oncancel={() => showAdd = false} aria-labelledby="server-form-title"><header class="modal-header"><div><p class="eyebrow">MAKE YOURSELF AT HOME</p><h2 id="server-form-title">{editingIndex === null ? "Add a server" : "Edit server"}</h2></div><button class="icon-button" aria-label="Close server form" onclick={() => showAdd = false}>{@render icon("close")}</button></header><form class="connection" onsubmit={connect}><p class="muted">Connect to your Mumble server. Your bookmarks and observed chat history are saved in this browser.</p><label>Display name <input bind:value={serverName} maxlength="128" autocomplete="off" placeholder="Friday night"></label><div class="form-row"><label>Server <input bind:value={host} required autocomplete="off" placeholder="voice.example.org"></label><label class="port-field">Port <input bind:value={port} type="number" min="1" max="65535" required></label></div><label>Your name <input bind:value={username} required maxlength="128" autocomplete="nickname" placeholder="How your friends know you"></label><label for="server-password">Server password (optional)</label><input id="server-password" type="password" bind:value={password} maxlength="1024" autocomplete="current-password"><details><summary>Certificate and connection</summary><label>Certificate SHA-256 (optional)<input bind:value={pin} autocomplete="off" spellcheck="false" placeholder="64 hexadecimal characters"></label><small class="relay">Relay: {relay}. TLS runs in this browser.</small></details><div class="modal-actions"><button type="button" onclick={() => showAdd = false}>Cancel</button><button class="primary" type="submit" disabled={!loaded || !!link.error}>Connect</button></div></form></dialog>{/if}
{#if showSettings}<dialog class="modal settings-modal" use:modal onclose={() => showSettings = false} oncancel={() => showSettings = false} aria-labelledby="settings-title"><header class="modal-header"><div><p class="eyebrow">MAKE IT YOURS</p><h2 id="settings-title">Settings</h2></div><button class="icon-button" aria-label="Close settings" onclick={() => showSettings = false}>{@render icon("close")}</button></header><section class="settings-section"><h3>Appearance</h3><label for="theme-choice">Theme</label><select id="theme-choice" bind:value={settings.theme} onchange={persistSettings}><option value="system">Match device</option><option value="dark">Dark</option><option value="light">Light</option></select><label for="density-choice">Message spacing</label><select id="density-choice" bind:value={settings.density} onchange={persistSettings}><option value="comfortable">Comfortable</option><option value="compact">Compact</option></select><label for="size-choice">Text size</label><select id="size-choice" bind:value={settings.fontSize} onchange={persistSettings}><option value={14}>Small · 14px</option><option value={16}>Default · 16px</option><option value={18}>Large · 18px</option><option value={20}>Larger · 20px</option></select></section><section class="settings-section voice-quality"><h3>Voice quality</h3><label for="voice-bitrate-choice">Voice bitrate</label><select id="voice-bitrate-choice" bind:value={voiceBitrate} onchange={chooseVoiceBitrate}>{#each VOICE_BITRATES as choice (choice)}<option value={choice}>{kbps(choice)}{choice === 128000 ? " · default" : ""}</option>{/each}</select><p>Mono Opus, 64 to 256 kb/s. A server's bandwidth limit can lower it, never raise it.</p>{#if voice}<p class="voice-bitrate" role="status">{voiceServer?.name}: {voiceBitrateWhy}</p>{/if}</section><section class="settings-section"><h3>Privacy & storage</h3><p>Bookmarks, trusted certificates and observed chat history are stored locally in this browser. Legacy servers can read chat; the relay only forwards encrypted traffic.</p><p>Links and received images never cause automatic requests to another site.</p></section>{#if selected}<section class="settings-section"><h3>{selected.name}</h3><p class="muted">{selected.host}:{selected.port} · {selected.username}</p><button onclick={() => { showSettings = false; editServer(); }}>Edit server bookmark</button>{#if capabilities.includes("server_order")}<div class="server-order-actions"><button disabled={orderPending || servers[0]?.id === selected.id} onclick={()=>moveOrderKey(selected.id,-1)}>Move server earlier</button><button disabled={orderPending || servers.at(-1)?.id === selected.id} onclick={()=>moveOrderKey(selected.id,1)}>Move server later</button></div>{/if}{#if capabilities.includes("remove_server")}<button class="danger" onclick={async () => { await act("remove_server",selectedIndex); showSettings = false; }}>Remove server</button>{/if}</section>{/if}<footer class="settings-footer"><span class="brand-word">nuschl<span>web</span></span><small>Built for conversations.</small></footer></dialog>{/if}
