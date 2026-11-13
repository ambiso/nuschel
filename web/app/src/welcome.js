// SPDX-License-Identifier: EUPL-1.2
// Keep typed documents only for the SDK's current connected welcome lease.
// Documents arrive once; ordinary state snapshots carry only opaque tokens.
import {contentDocument} from "./content-bridge.js";
export function welcomeDocuments(previous, servers, updates = []) {
  const current = new Map(servers.filter(s => s.status === "connected" && typeof s.welcome_token === "string").map(s => [s.id, s.welcome_token]));
  const result = new Map([...previous].filter(([id, view]) => current.get(id) === view.token));
  for (const update of updates || []) {
    if (update.kind === "welcome" && current.get(update.server) === update.token && Array.isArray(update.document?.blocks)) {
      result.set(update.server, {token:update.token, get document(){ return contentDocument(update.content_token) ?? update.document; }});
    }
  }
  return result;
}
