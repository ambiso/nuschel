// SPDX-License-Identifier: EUPL-1.2
import { getJSON } from "./api.js";

class Resource {
  data = $state.raw(null);
  error = $state(null);
  loading = $state(true);
  revision = $state(0);

  reload() {
    this.revision++;
  }
}

/**
 * Loads JSON from the URL returned by `url()` and reloads whenever it changes;
 * a null URL loads nothing. An optional namespace signal aborts and clears old data
 * synchronously on invalidation, before a reconnect can expose another library.
 */
export function resource(url, signal = () => null) {
  const r = new Resource();
  $effect(() => {
    r.revision;
    const path = url();
    const namespaceSignal = signal();
    if (path === null || namespaceSignal?.aborted) {
      r.data = null;
      r.error = null;
      r.loading = false;
      return;
    }
    const controller = new AbortController();
    const abort = () => {
      controller.abort();
      r.data = null;
      r.error = null;
      r.loading = false;
    };
    namespaceSignal?.addEventListener("abort", abort, { once: true });
    r.loading = true;
    r.error = null;
    r.data = null;
    getJSON(path, controller.signal).then(
      (data) => {
        if (controller.signal.aborted) return;
        r.data = data;
        r.loading = false;
      },
      (error) => {
        if (controller.signal.aborted) return;
        r.error = error instanceof Error ? error.message : String(error);
        r.loading = false;
      },
    );
    return () => {
      namespaceSignal?.removeEventListener("abort", abort);
      controller.abort();
    };
  });
  return r;
}
