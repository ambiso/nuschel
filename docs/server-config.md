# Server configuration and send limits

`nuschl_core::model::ServerLimits` owns the connection defaults and sparse merge
policy. `LimitsUpdate` has optional domain fields, without a protocol dependency.
Both session adapters translate `ServerConfig` into it and publish complete
snapshots. Omitted fields keep their prior values; explicit zero and false remain
meaningful. A welcome-only or empty update emits no Limits or audio-policy change.

| Field | Fresh connection | Explicit update |
| --- | --- | --- |
| HTML | true | Preserve false until explicit true |
| Text length | 5000 UTF-16 code units | Zero removes the server's text-component limit |
| Total HTML/image length | 131072 UTF-16 code units | Zero removes the server's whole-HTML limit |
| Maximum users | Unknown | Preserve the supplied number, including zero |
| Recording | Allowed | Preserve false until explicit true |
| Bandwidth | ServerSync value, or 72000 bit/s fallback | Preserve it across unrelated updates, including explicit zero |

Raw server limits are retained, including `u32::MAX`; application byte ceilings
are not disguised as server values. A successful explicit `Client::connect` and
current-generation `Input::Reconnecting` clear the shared/displayed limits.
ServerSync publishes the full initial snapshot even when no ServerConfig follows.
Stale-generation events cannot restore the previous connection's values.
Negotiated Opus availability is a separate optional fact: it stays unknown until
an actual CodecVersion arrives and resets with the connection. A private routing
default of true is not evidence that a server negotiated Opus.

The defaults and optional-field behavior follow the pinned Mumble source
`35dce9e4801fe52b2217f0d9f421d17b0aaf3bb0` (`Global.cpp`,
`MainWindow.cpp`, `Messages.cpp`). Its `Server.cpp::isTextAllowed`, and nuschld's
`apps/server/src/text.rs`, count Qt/UTF-16 units. One `é` or `漢` counts as one unit;
`🙂` counts as two. Notices call these characters, rather than UTF-8 bytes.

For ordinary text, both clients use the existing first-party `legacy_body`
formatter: literal markup is escaped, newlines become `<br/>` in HTML mode, links
become anchors, and an opening code fence gets a leading newline. The same
formatted wire string is checked by the shared SDK and the session adapter before
submission. Escapes, anchors and newline markup count. For example, `<&>` followed
by a newline becomes `&lt;&amp;&gt;<br/>`, 18 UTF-16 units.

HTML mode always enforces the total HTML/image limit, even when the text limit
is zero or larger than the image limit. Ordinary text contains no inline images,
so it must fit both nonzero caps. With HTML disabled only the text cap applies.
Both server caps can be zero while Nuschl's independent `MAX_CONTENT_BYTES`
ceiling still bounds the raw text and formatted message to 256 KiB of UTF-8.
Refusals identify the actual server character cap or the local byte ceiling.
Avatar binary payloads retain their separate existing 1 MiB local bound.

Inline-image preparation has a finite byte budget from
`inline_image_byte_budget()`: ASCII image markup is bounded by the nonzero image
cap and the existing 256 KiB message envelope, or just that local envelope when
the server declares zero. Raw `SendHtml` also checks the total image cap; it no
longer substitutes `max(text,image)`. It does **not** apply the text cap to the
entire base64 image. Full rich-text parity remains queued: Murmur checks an
XML-normalized message with image `src` values removed against the text cap.
HTML-disabled XML filtering can also change the server's accepted text. This
scalar shared helper does not claim to implement either XML transformation.

`SendError::Refused(String)` returns its precise reason directly. The browser and
TUI do not scrape the current notice or rely on an unreachable selected-ID guard.
Every synchronous Refused gets an owned notice that expires after ten seconds.
All production `ServerState` notice writers use its setter/dismissal helpers,
which invalidate the prior ownership revision even for identical text. A matching
own `Input::Sent` clears only the old refusal captured by that send, never a newer
unrelated notice. Explicit dismissal, expiry and stale/mismatched Sent events are
covered separately. Empty/pending and raw-body-too-long keep distinct variants.

The App has the same small ownership rule for its global error, scoped to the
conversation. A successful `send_text` command clears only its captured prior
send error. This is **local acceptance**, not a delivery acknowledgement. Legacy
Sent itself reports local submission; Mumble provides no message-delivery ACK.
The fixture holds the owned server's reply until the alert/status/draft clear,
then releases it and independently verifies actual wire receipt. Its async helper
tests cover newer identical errors, unrelated errors and different conversations.

One pre-existing native attribution gap remains separate: generic actor
Notice/PermissionDenied events can clear a pending send, and are not correlated
to a particular target/body. That includes a limit shrinking after SDK enqueue.
This change preserves that generic behavior; it does not claim a complete
correlated refusal/attachment-recovery protocol. See the tracked follow-ups in
[TODO.md](../TODO.md).

Scoped checks pass: 35 core tests, 95 native session tests, 114 shared-client tests,
31 browser-SDK tests plus six private TLS relay integrations, 87 desktop tests and
27 TUI tests. Session/client each retain one existing ignored test. The compiled
WASM/Svelte/notices package passes [web_server_config.py](../tests/web_server_config.py)
under production CSP with an independently encoded loopback TLS endpoint. It
checks sparse updates, refusal/draft preservation, raised-cap success clearing,
UTF-16 Unicode boundaries, HTML total with unlimited text, independent local byte
ceilings, exact link/fence wire formatting and reconnect defaults. Received
UTF-8-byte/UTF-16-unit pairs are independently recorded; no App state or worker
result is injected. Private browser profiles and denied capture avoid host
devices and external servers.

The earlier failing diagnostics and prior proofs remain preserved in
`~/.cache/nuschl-live-welcome/browser-final/` and `~/.cache/nuschl-server-config/`.
Current source identities, package, commands, proof limits and retained build/test
failures are recorded in `~/.cache/nuschl-next/server-limits/handoff.json`.
