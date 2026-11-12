// SPDX-License-Identifier: EUPL-1.2
// The requested voice bitrate (nuschl_core::bitrate) and how the voice view's
// effective rate is explained. The SDK worker resolves the request against
// the authenticated server's budget; this only stores and words it.
export const VOICE_BITRATES = [64000, 96000, 128000, 192000, 256000];
export const DEFAULT_VOICE_BITRATE = 128000;
export const VOICE_BITRATE_KEY = "nuschl.voice.bitrate.v1";
const OVERHEAD = 24000;

export function kbps(bits) {
  return `${bits % 1000 === 0 ? bits / 1000 : (bits / 1000).toFixed(1)} kb/s`;
}

/** A stored request, or the default for anything else. */
export function storedVoiceBitrate(storage) {
  try {
    const value = Number(storage?.getItem(VOICE_BITRATE_KEY));
    return VOICE_BITRATES.includes(value) ? value : DEFAULT_VOICE_BITRATE;
  } catch {
    return DEFAULT_VOICE_BITRATE;
  }
}

/** The stored request in this browser, the default without storage. */
export function initialVoiceBitrate() {
  try { return storedVoiceBitrate(globalThis.localStorage); } catch { return DEFAULT_VOICE_BITRATE; }
}

/** False when the browser refuses to store it (private browsing). */
export function storeVoiceBitrate(storage, value) {
  if (!VOICE_BITRATES.includes(value)) return false;
  try { storage.setItem(VOICE_BITRATE_KEY, String(value)); return true; } catch { return false; }
}

function valid(voice) {
  return voice && [voice.bitrate, voice.requested_bitrate, voice.server_bandwidth].every(Number.isSafeInteger);
}

/** "Voice 48 of 128 kb/s · server limit", for the voice bar. */
export function bitrateLine(voice) {
  if (!valid(voice)) return "";
  const {bitrate, requested_bitrate: requested, bitrate_limit: limit} = voice;
  if (limit === "no_server_budget" || bitrate === 0) return `Voice not sending (requested ${kbps(requested)}) · server allows too little for voice`;
  const rate = bitrate === requested ? kbps(bitrate) : `${bitrate / 1000} of ${kbps(requested)}`;
  const reason = {server_budget:"server limit",shared_microphone:"shared with another server"}[limit];
  return reason ? `Voice ${rate} · ${reason}` : `Voice ${rate}`;
}

/** Why, in one sentence. */
export function bitrateExplanation(voice) {
  if (!valid(voice)) return "";
  const {bitrate, requested_bitrate: requested, server_bandwidth: budget, bitrate_limit: limit} = voice;
  if (limit === "server_budget") return `${kbps(bitrate)}: this server allows ${kbps(budget)} per user, ${kbps(OVERHEAD)} of which is kept for Mumble packet overhead.`;
  if (limit === "no_server_budget") return `Not sending: this server allows only ${kbps(budget)} per user, too little for voice after ${kbps(OVERHEAD)} of packet overhead.`;
  if (limit === "shared_microphone") return `${kbps(bitrate)}: another server you speak on allows less, and one encoder serves both.`;
  return `${kbps(bitrate || requested)}, as requested.`;
}
