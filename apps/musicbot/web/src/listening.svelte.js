// SPDX-License-Identifier: EUPL-1.2
// Listening in this browser: one BrowserAudio for the whole page, so the
// player bar and the big screen control the same stream.
import { store } from "./store.svelte.js";
import { BrowserAudio } from "./browser-audio.js";

class Listening {
  /** "off", "starting", "waiting" or "playing". */
  mode = $state("off");
  /** Browser volume in percent; independent of the Mumble volume. */
  volume = $state(50);
  /** Last failure message, cleared when the user toggles listening again. */
  error = $state("");
  active = $derived(this.mode !== "off");
  /** Text for the player status line, or null when there is nothing to say. */
  status = $derived(this.error || (this.mode === "starting" ? "Starting browser audio…"
    : this.mode === "waiting" ? "Waiting for browser audio…"
    : this.mode === "playing" ? "Playing in this browser"
    : null));

  #audio = new BrowserAudio(
    (state) => { this.mode = state; },
    (failure) => {
      this.error = failure instanceof Error ? failure.message : String(failure);
      store.toast(this.error);
    },
  );

  /** True when queue `player` can be streamed to this browser right now. */
  available(player) {
    return store.connection === "open" && !store.invalid
      && store.browseKey !== null && !!store.snapshot?.players[player];
  }

  /**
   * Call once, during App's initialisation: follows the selected queue and
   * stops when listening is no longer possible or the page goes away.
   */
  attach() {
    $effect(() => { this.select(store.selected); });
    $effect(() => { if (!this.available(store.selected)) this.stop(); });
    $effect(() => {
      const stop = () => this.stop();
      window.addEventListener("pagehide", stop);
      return () => {
        window.removeEventListener("pagehide", stop);
        stop();
      };
    });
  }

  // Must run directly in the click handler: the browser only allows audio
  // to start from a user gesture.
  toggle(player) {
    this.error = "";
    if (this.active) this.stop();
    else if (this.available(player)) this.#audio.start(player, store.browseSignal);
  }

  setVolume(volume) {
    this.volume = volume;
    this.#audio.setVolume(volume);
  }

  select(player) {
    this.#audio.select(player);
  }

  stop() {
    this.#audio.stop();
  }
}

export const listening = new Listening();
