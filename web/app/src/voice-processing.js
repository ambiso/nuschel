// SPDX-License-Identifier: EUPL-1.2
// Inert presentation of the microphone processing a browser reports through
// MediaStreamTrack.getSettings() (recorded by BrowserAudio as state.processing).
// A missing or unrecognised value is "unknown", never assumed to be on.
const UNKNOWN = "unknown (the browser did not report it)";
const FIELDS = [
  ["echoCancellation", "Echo cancellation"],
  ["noiseSuppression", "Noise suppression"],
  ["autoGainControl", "Automatic gain"],
];

function describe(field, value) {
  if (value === true) return {state:"on", text:"on"};
  if (value === false) return {state:"off", text:"off"};
  // Media Capture and Streams allows these echoCancellation modes. "remote-only"
  // covers peer-connection audio, which Nuschl's playback is not.
  if (field === "echoCancellation" && value === "all") return {state:"on", text:"on (all system audio)"};
  if (field === "echoCancellation" && value === "remote-only") return {state:"partial", text:"on for peer-connection audio only"};
  return {state:"unknown", text:UNKNOWN};
}

export function voiceProcessing(processing) {
  const reported = Boolean(processing && typeof processing === "object" && FIELDS.some(([field]) => field in processing));
  if (!reported) return {reported:false, rows:[], hint:null};
  const rows = FIELDS.map(([field, label]) => ({field, label, ...describe(field, processing[field])}));
  const echo = rows[0].state;
  const hint = echo === "off" ? "Echo cancellation is off. Use headphones so others do not hear themselves."
    : echo === "on" ? null
    : "Echo cancellation is not confirmed. Headphones keep others from hearing themselves.";
  return {reported:true, rows, hint};
}
