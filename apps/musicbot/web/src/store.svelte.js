// SPDX-License-Identifier: EUPL-1.2
// Live state from the bot, the commands that change it, and the client-side
// guesses (optimistic values) shown until the server confirms them.
import { getJSON, postJSON, HttpError } from "./api.js";
import { applyMove, browseItemIsCurrent, buildRoute, clamp, debounce, libraryForNamespace, parseRoute, queueStorageKey, routeForLibrary, routeForNamespace, seekSettled, selectedQueue } from "./logic.js";

const storageKey = queueStorageKey(location.pathname);

function storedQueue() {
  try {
    return localStorage.getItem(storageKey);
  } catch {
    return null;
  }
}

class Store {
  /** Latest State from the bot (see the API contract), or null before the first one. */
  snapshot = $state.raw(null);
  /** performance.now() when `snapshot` arrived; the base for position interpolation. */
  receivedAt = $state(0);
  /** "connecting" before the first state, "open", or "reconnecting" after an error. */
  connection = $state("connecting");
  /** True when the bot answered 404: the link is unknown or revoked. */
  invalid = $state(false);
  route = $state(parseRoute(location.hash));
  /** Library index used by the browse links. */
  library = $state(0);
  /** Changes only at a connection/library boundary, never on progress snapshots. */
  browseEpoch = $state(0);
  browseKey = $state(null);
  browseSignal = $state.raw(null);
  toasts = $state([]);
  /** Optimistic values by key (`queue:N`, `volume:N`, `ducking:N`): `{ value, version, settled }`. */
  overrides = $state.raw({});
  /** Seeks not yet reflected by the server, by queue index: `{ target, at, entry, version, sent }`. */
  holds = $state.raw({});
  #stored = $state(storedQueue());

  selected = $derived(selectedQueue(this.#stored, this.snapshot?.players.length ?? 0));
  player = $derived(this.snapshot?.players[this.selected] ?? null);

  #browseSession = crypto.getRandomValues(new Uint32Array(4)).join("-");
  #browseController = null;
  #libraryNames = [];
  #libraryKey = null;
  #source = null;
  #retry = null;
  #stateRequest = null;
  #running = false;
  #onHashChange = () => this.#routeChanged();
  #version = 0;
  #states = 0;
  #debouncers = new Map();
  #toastId = 0;

  start() {
    this.#running = true;
    window.addEventListener("hashchange", this.#onHashChange);
    this.#routeChanged();
    this.#connect();
    const request = new AbortController();
    const epoch = this.browseEpoch;
    this.#stateRequest = request;
    getJSON("api/state", request.signal).then(
      (state) => {
        if (this.#running && !request.signal.aborted && epoch === this.browseEpoch && !this.snapshot) this.#receive(state);
      },
      (error) => {
        if (!this.#running || request.signal.aborted || this.#checkInvalid(error)) return;
        this.#disconnected();
        this.toast(error instanceof Error ? error.message : String(error));
      },
    );
    return () => this.stop();
  }

  stop() {
    this.#running = false;
    window.removeEventListener("hashchange", this.#onHashChange);
    this.#stateRequest?.abort();
    this.#source?.close();
    this.#source = null;
    clearTimeout(this.#retry);
    this.#invalidateBrowse();
    for (const pending of this.#debouncers.values()) {
      if (!this.invalid) pending.flush();
      pending.cancel();
    }
  }

  #routeChanged() {
    let route = parseRoute(location.hash);
    let library = typeof route.library === "number" ? route.library : this.library;
    if (this.browseKey !== null && library >= this.#libraryNames.length) library = 0;
    route = routeForNamespace(route, library, this.browseKey);
    if (typeof route.library === "number") this.library = route.library;
    this.#replaceRoute(route);
  }

  navigate(route) {
    const library = typeof route.library === "number" ? route.library : this.library;
    location.hash = buildRoute(routeForNamespace(route, library, this.browseKey));
  }

  #replaceRoute(route) {
    this.route = route;
    const hash = buildRoute(route);
    if (location.hash !== hash) history.replaceState(history.state, "", hash);
  }

  #invalidateBrowse() {
    this.#browseController?.abort();
    this.#browseController = null;
    this.browseSignal = null;
    this.browseKey = null;
    this.browseEpoch++;
    let overrides = null;
    for (const key of Object.keys(this.overrides)) {
      if (!key.startsWith("queue:")) continue;
      overrides ??= { ...this.overrides };
      delete overrides[key];
    }
    if (overrides) this.overrides = overrides;
    this.#replaceRoute(routeForLibrary(this.route, this.library));
  }

  #disconnected() {
    if (this.connection !== "reconnecting") {
      this.#stateRequest?.abort();
      this.#invalidateBrowse();
    }
    this.connection = "reconnecting";
  }

  browseItemCurrent(item) {
    return !this.invalid && this.connection === "open"
      && browseItemIsCurrent(item, this.browseKey, this.#libraryNames.length);
  }

  #connect() {
    if (!this.#running || this.invalid) return;
    clearTimeout(this.#retry);
    this.#source?.close();
    const source = new EventSource("api/events");
    this.#source = source;
    source.addEventListener("state", (event) => {
      if (!this.#running || this.#source !== source || source.readyState !== EventSource.OPEN || this.invalid) return;
      try {
        this.#receive(JSON.parse(event.data));
      } catch (error) {
        console.error("bad state event", error);
      }
    });
    source.addEventListener("error", () => {
      if (this.#source !== source) return;
      this.#disconnected();
      // The browser retries by itself unless the response was not a stream
      // (for example a 404); then find out why and retry later.
      if (source.readyState === EventSource.CLOSED) {
        getJSON("api/state").then(
          () => { if (this.#running && this.#source === source) this.#scheduleReconnect(); },
          (error) => {
            if (this.#running && this.#source === source && !this.#checkInvalid(error)) this.#scheduleReconnect();
          },
        );
      }
    });
  }

  #scheduleReconnect() {
    clearTimeout(this.#retry);
    if (!this.#running || this.invalid) return;
    this.#retry = setTimeout(() => this.#connect(), 3000);
  }

  #checkInvalid(error) {
    if (error instanceof HttpError && error.status === 404) {
      this.invalid = true;
      this.#invalidateBrowse();
      this.#source?.close();
      this.#source = null;
      clearTimeout(this.#retry);
      return true;
    }
    return false;
  }

  #receive(state) {
    const libraryKey = JSON.stringify(state.libraries);
    const newNamespace = this.browseKey === null || libraryKey !== this.#libraryKey;
    if (newNamespace) {
      const library = libraryForNamespace(this.#libraryNames, state.libraries, this.library);
      if (this.browseKey !== null) this.#invalidateBrowse();
      this.#libraryNames = state.libraries;
      this.#libraryKey = libraryKey;
      this.library = library;
      this.#replaceRoute(routeForLibrary(this.route, library));
    }
    this.#states++;
    this.snapshot = state;
    this.receivedAt = performance.now();
    this.connection = "open";
    if (newNamespace) {
      this.#browseController = new AbortController();
      this.browseSignal = this.#browseController.signal;
      this.browseKey = `${this.#browseSession}:${this.browseEpoch}`;
    }
    let overrides = null;
    for (const [key, override] of Object.entries(this.overrides)) {
      if (!override.settled) continue;
      overrides ??= { ...this.overrides };
      delete overrides[key];
    }
    if (overrides) this.overrides = overrides;
    let holds = null;
    for (const [index, hold] of Object.entries(this.holds)) {
      if (!hold.sent) continue;
      const current = state.players[index]?.current ?? null;
      if (seekSettled(this.holdPosition(hold, current), hold.entry, current, current?.position_ms ?? 0)) {
        holds ??= { ...this.holds };
        delete holds[index];
      }
    }
    if (holds) this.holds = holds;
  }

  toast(text) {
    const id = ++this.#toastId;
    this.toasts.push({ id, text });
    setTimeout(() => this.dismiss(id), 6000);
  }

  dismiss(id) {
    const index = this.toasts.findIndex((toast) => toast.id === id);
    if (index >= 0) this.toasts.splice(index, 1);
  }

  selectQueue(index) {
    this.#stored = String(index);
    try {
      localStorage.setItem(storageKey, String(index));
    } catch {
      // Storage may be disabled; the selection then lasts for this page only.
    }
  }

  /** Sends a command; on failure shows the server's error and returns false. */
  async command(body, signal) {
    if (signal?.aborted) return false;
    try {
      await postJSON("api/command", body, signal);
      return true;
    } catch (error) {
      if (signal?.aborted) return false;
      this.#checkInvalid(error);
      this.toast(error instanceof Error ? error.message : String(error));
      return false;
    }
  }

  // Optimistic values

  #setOverride(key, value) {
    const version = ++this.#version;
    this.overrides = { ...this.overrides, [key]: { value, version, settled: false } };
    return version;
  }

  #dropOverride(key, version) {
    if (this.overrides[key]?.version !== version) return;
    const overrides = { ...this.overrides };
    delete overrides[key];
    this.overrides = overrides;
  }

  /**
   * Sends `body` for the override `key` at `version`. When it succeeds, the
   * override goes away with the next state; if a state already arrived while
   * the request ran, it goes away now. When it fails, it goes away at once.
   */
  async #confirm(key, version, body) {
    const states = this.#states;
    const ok = await this.command(body);
    const override = this.overrides[key];
    if (override?.version !== version) return;
    if (!ok || this.#states !== states) this.#dropOverride(key, version);
    else this.overrides = { ...this.overrides, [key]: { ...override, settled: true } };
  }

  #debounced(key, ms, fn) {
    let debounced = this.#debouncers.get(key);
    if (!debounced) {
      debounced = debounce(fn, ms);
      this.#debouncers.set(key, debounced);
    }
    return debounced;
  }

  queue(index) {
    return this.overrides[`queue:${index}`]?.value ?? this.snapshot?.players[index]?.queue ?? [];
  }

  volume(index) {
    return this.overrides[`volume:${index}`]?.value ?? this.snapshot?.players[index]?.volume ?? 0;
  }

  ducking(index) {
    return this.overrides[`ducking:${index}`]?.value ?? this.snapshot?.players[index]?.ducking ?? null;
  }

  // Commands

  togglePause(player) {
    const p = this.snapshot?.players[player];
    if (p) this.command({ command: p.paused ? "resume" : "pause", player });
  }

  skip(player) {
    this.command({ command: "skip", player });
  }

  clear(player) {
    const key = `queue:${player}`;
    this.#confirm(key, this.#setOverride(key, []), { command: "clear", player });
  }

  remove(player, entry) {
    const key = `queue:${player}`;
    const list = this.queue(player).filter((item) => item.id !== entry);
    this.#confirm(key, this.#setOverride(key, list), { command: "remove", player, entry });
  }

  /** Moves `entry` to `index`, counted in the queue without the entry. */
  move(player, entry, index) {
    const key = `queue:${player}`;
    const current = this.queue(player);
    const from = current.findIndex((item) => item.id === entry);
    if (from < 0) return;
    this.#confirm(key, this.#setOverride(key, applyMove(current, from, index)), { command: "move", player, entry, index });
  }

  add(player, item, index = null) {
    if (!this.browseItemCurrent(item)) {
      this.toast("The library changed or is reconnecting. Choose an item from the refreshed library.");
      return;
    }
    if (!this.snapshot?.players[player]) {
      this.toast("There is no queue to add to.");
      return;
    }
    this.command({ command: "add", player, kind: item.kind, library: item.library, id: item.id, index }, this.browseSignal);
  }

  attach(channel, player) {
    return this.command({ command: "attach", channel, player });
  }

  setVolume(player, volume) {
    const key = `volume:${player}`;
    const value = clamp(Math.round(volume), 0, 100);
    const version = this.#setOverride(key, value);
    this.#debounced(key, 150, (version, value) => this.#confirm(key, version, { command: "volume", player, volume: value }))(version, value);
  }

  setDucking(player, ducking) {
    const key = `ducking:${player}`;
    const value = {
      enabled: Boolean(ducking.enabled),
      level: clamp(Math.round(ducking.level), 0, 100),
      hold_ms: clamp(Math.round(ducking.hold_ms), 0, 10000),
      sensitivity: clamp(Math.round(ducking.sensitivity), 0, 100),
    };
    const version = this.#setOverride(key, value);
    this.#debounced(key, 300, (version, value) => this.#confirm(key, version, { command: "ducking", player, ...value }))(version, value);
  }

  // Seeking

  /** Position shown for a pending seek: the target, advancing from when it was set if the song plays. */
  holdPosition(hold, current, now = performance.now()) {
    const elapsed = current?.advancing ? Math.max(0, now - hold.at) : 0;
    const duration = current?.entry.track.duration;
    const ms = hold.target + elapsed;
    return typeof duration === "number" && duration > 0 ? Math.min(ms, duration * 1000) : ms;
  }

  /**
   * Seeks queue `player` to `ms`. The target shows at once; keyboard seeks
   * are debounced, a released pointer sends at once (`immediate`).
   */
  seek(player, ms, immediate = false) {
    const current = this.snapshot?.players[player]?.current;
    if (!current) return;
    const version = ++this.#version;
    this.holds = { ...this.holds, [player]: { target: ms, at: performance.now(), entry: current.entry.id, version, sent: false } };
    const send = this.#debounced(`seek:${player}`, 300, (version, ms) => this.#sendSeek(player, version, ms));
    send(version, ms);
    if (immediate) send.flush();
  }

  async #sendSeek(player, version, ms) {
    const pending = this.holds[player];
    if (pending?.version !== version) return;
    if (this.snapshot?.players[player]?.current?.entry.id !== pending.entry) {
      this.#dropHold(player, version);
      return;
    }
    const ok = await this.command({ command: "seek", player, position_ms: Math.round(ms) });
    const hold = this.holds[player];
    if (hold?.version !== version) return;
    if (!ok) {
      this.#dropHold(player, version);
      return;
    }
    this.holds = { ...this.holds, [player]: { ...hold, sent: true } };
    setTimeout(() => this.#dropHold(player, version), 2000);
  }

  #dropHold(player, version) {
    if (this.holds[player]?.version !== version) return;
    const holds = { ...this.holds };
    delete holds[player];
    this.holds = holds;
  }
}

export const store = new Store();
