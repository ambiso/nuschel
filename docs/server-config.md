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

Fresh connections reset all fields. Native/default message and image lengths
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
