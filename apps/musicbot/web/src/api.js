// SPDX-License-Identifier: EUPL-1.2
// HTTP helpers. Every URL is relative to the page (`/u/<token>/`), so the app
// also works when the bot sits behind a proxy under a path.

export class HttpError extends Error {
  constructor(status, message) {
    super(message);
    this.status = status;
  }
}

async function failure(response) {
  let message = `${response.status} ${response.statusText}`.trim();
  try {
    const body = await response.json();
    if (typeof body?.error === "string" && body.error) message = body.error;
  } catch {
    // The body was not JSON; keep the status text.
  }
  return new HttpError(response.status, message);
}

/** GETs JSON from `path`; throws HttpError with the server's `error` text on failure. */
export async function getJSON(path, signal) {
  const response = await fetch(path, { signal, headers: { Accept: "application/json" } });
  if (!response.ok) throw await failure(response);
  return response.json();
}

/** POSTs a command body to `api/command`; throws HttpError on failure. */
export async function postCommand(body, signal) {
  const response = await fetch("api/command", {
    method: "POST",
    signal,
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) throw await failure(response);
}

/** Query string for library calls. */
export function query(path, params) {
  return `${path}?${new URLSearchParams(Object.entries(params).map(([key, value]) => [key, String(value)]))}`;
}

/** URL of a cover image `size` pixels wide. */
export function artUrl(library, coverArt, size) {
  return `art/${library}/${encodeURIComponent(coverArt)}?size=${size}`;
}
