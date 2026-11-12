# Sparse server configuration and browser send limits

`ServerConfig` optional fields merge into the current connection's values. The
session adapters emit a complete `ServerLimits` snapshot when a limit field is
present, because the shared client replaces that snapshot. A welcome-only or
empty configuration does not emit Limits or change audio policy.

| Field | Fresh connection / initial value | Explicit update |
| --- | --- | --- |
| HTML | true | Preserve false until an explicit true |
| Message length | 5000 | Zero means unlimited, bounded by the application's 256 KiB ceiling |
| Image length | 131072 | Zero means unlimited, bounded by the application's 1 MiB ceiling |
| Maximum users | Unknown | Preserve the supplied number, including zero |
| Recording | Allowed | Preserve false until an explicit true |
| Bandwidth | Effective ServerSync value, existing 72000 bit/s fallback | Preserve the supplied value across unrelated updates |

Fresh session instances reset all fields. Native/default message and image lengths
match the pinned Mumble source (`35dce9e4801fe52b2217f0d9f421d17b0aaf3bb0`,
`Global.cpp:119` and `MainWindow.cpp:3589`). Its `Messages.cpp::msgServerConfig`
changes only present fields. `Server.cpp::isTextAllowed` treats zero as unlimited.
The bounded application ceilings above retain Nuschl's existing zero semantics.

Browser text now checks the UTF-8 bytes of the actual escaped `TextMessage.message`
value before TLS submission or outgoing/Sent events. The envelope/header is not
part of this count. Escapes and newline markup count toward the cap; plain text
uses its UTF-8 bytes. This matches the existing native enforcement policy, not a
claim of identical stock-Mumble Unicode/HTML acceptance thresholds: Qt uses
QString units and may inspect parsed HTML content. Nuschl's wire-byte policy is
more conservative. Existing native/browser formatting choices are unchanged.
The browser retains image limits without adding an image-send API.

Scoped checks pass: 82 native session tests (one existing ignored), 29 browser-SDK
tests and six local relay integrations. Unit cases cover all retained fields,
explicit zero/false, changes, conservative defaults, reconnects and UTF-8/markup
expansion. The actual compiled App/SDK passes `tests/web_server_config.py` under
production CSP with an owned loopback TLS endpoint and independently encoded
protobuf changes. A private file coordinator changes server fields while the
browser keeps its draft; no App state or worker result is injected. Independent
received string-value lengths are exactly 129,17,4,5001,5000 bytes, covering sparse
bandwidth/max-user updates, raised caps, HTML versus plain newlines, zero and
reconnect defaults. Rejected drafts stay editable; successful legacy Sent keeps
its existing transport semantics, and the fixture independently confirms wire
receipt. No host capture or external server is used.

The original live-welcome `--check-send-cap` diagnostic now passes. Its earlier
129/128 failure remains at `~/.cache/nuschl-live-welcome/browser-final/`. Exact
artifacts, source hashes, commands and current proofs are recorded in
`~/.cache/nuschl-server-config/handoff.json`. These checks do not claim new GPUI,
physical audio/device or broad browser parity verification.

## Precise browser refusal messages

The separate browser-wrapper correction preserves the core's refusal categories.
Nothing-to-send/pending and global-too-long return distinct fixed reasons. Only
Refused may read the fresh notice assigned by that synchronous core attempt,
and only for the same selected server ID and generation. It never substitutes
an old limit notice for an empty, oversized-global or disconnected attempt.
No core API, App or SDK Worker behavior changed.

The real App fixture checks the exact 128/16/5000-byte reasons, their visible
alert, and retained editable drafts. It also checks empty, global 256 KiB and
disconnected diagnostics after an earlier limit notice. Both this fixture and
the original live-welcome diagnostic pass on the precise-error artifact.
`~/.cache/nuschl-server-config/precise-final/cap-refusal-390.png` was visually
reviewed; `precise-handoff.json` records exact source/artifact/test identity.

A separate shared-model edge remains source-audited: Client::connect does not
clear `ServerState.limits`. If a fresh server connection never sends ServerConfig,
the adapter correctly enforces its fresh defaults but the shared/displayed
limits can remain from the previous connection. Resetting or initially
republishing that model needs a generation-aware regression; no shared-client
source was changed in this work.
