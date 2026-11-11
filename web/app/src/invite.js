// SPDX-License-Identifier: EUPL-1.2
// This first browser client only implements legacy Mumble login. Security
// requirements in an invite must fail closed until modern negotiation exists.
export function inviteFragment(fragment) {
  const empty = { host: "", port: 64738, pin: "", username: "", error: "" };
  if (fragment.length > 8192) return { ...empty, error: "This invite is too long." };
  const params = new URLSearchParams(fragment.replace(/^#/, ""));
  for (const key of ["s", "fp", "policy", "u"]) {
    if (params.getAll(key).length > 1) return { ...empty, error: "This invite repeats an option." };
  }
  const server = params.get("s") || "";
  const match = /^\[?([^\]]*?)\]?(?::(\d{1,5}))?$/.exec(server);
  const pinValue = params.get("fp");
  const pin = /^sha256:([0-9a-fA-F]{64})$/.exec(pinValue || "");
  let error = "";
  if (pinValue !== null && !pin) error = "This invite has an invalid certificate pin.";
  if (params.get("policy")) {
    error = "This invite requires the Nuschl protocol. Open it in the app; this browser client cannot negotiate that protocol yet.";
  }
  return {
    host: match ? match[1] : "",
    port: match && match[2] ? Number(match[2]) : 64738,
    pin: pin ? pin[1].toLowerCase() : "",
    username: (params.get("u") || "").slice(0, 128),
    error,
  };
}
