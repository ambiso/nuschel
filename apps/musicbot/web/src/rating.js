// SPDX-License-Identifier: EUPL-1.2
import { getJSON } from "./api.js";

// Visible song rows share two read slots with the player and history controls.
const waiting = [];
let active = 0;

function drain() {
  while (active < 2 && waiting.length) {
    const job = waiting.shift();
    job.signal.removeEventListener("abort", job.cancel);
    active++;
    getJSON(job.path, job.signal).then(job.resolve, job.reject).finally(() => {
      active--;
      drain();
    });
  }
}

export function getRating(path, signal) {
  return new Promise((resolve, reject) => {
    if (signal.aborted) {
      reject(signal.reason);
      return;
    }
    const job = { path, signal, resolve, reject, cancel: null };
    job.cancel = () => {
      const index = waiting.indexOf(job);
      if (index < 0) return;
      waiting.splice(index, 1);
      signal.removeEventListener("abort", job.cancel);
      reject(signal.reason);
    };
    signal.addEventListener("abort", job.cancel, { once: true });
    waiting.push(job);
    drain();
  });
}
