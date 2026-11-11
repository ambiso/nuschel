# Clients on every platform

Decisions with the user (2026-11-11) and the architecture that follows from them. The
desktop (GPUI) exists; the terminal, Android, iOS and web clients are queued.

## Decisions

- **One headless client SDK** (`crates/client`) owns everything that is not drawing:
  sessions and reconnects, server and channel state, history merging and the local
  cache, unread counts, edit ordering, typing, identity and trust, transfers. It takes
  commands and emits events and snapshots. The GPUI desktop becomes one view over it,
  and every other client is another view. Logic is written once.
- **Native UI per platform**: Kotlin + Jetpack Compose on Android, Swift + SwiftUI on
  iOS, both over UniFFI bindings to the SDK. Native toolkits give the best
  accessibility and the platform integration voice needs (foreground service, CallKit,
  background audio, push).
- **Terminal client** (ratatui), the SDK's second consumer after the desktop: cheap,
  usable over SSH, and the test that the SDK really is UI-independent.
- **Web client in Svelte**, with the SDK compiled to WASM in a Web Worker.
- **The web client is served from one domain the project runs**, not by each server.
  Users already trust the project for the software they run, so the web client adds no
  new party to trust, and an origin that stays the same lets the browser keep one
  cross-server identity. Others can host the same web client on their own domain.
  Signal-like client version requirements apply to the web client as to the apps.
- **NixOS first** for everything the project or operators host: a NixOS module for the
  web client and share-link host using the usual `services.nginx.virtualHosts` with
  `enableACME = true` (optionally on a subdomain), next to the existing server module.
- **Share links** (`https://<web host>/join/…`) open the installed app through Android
  App Links, iOS Universal Links or the desktop's `nuschel://` scheme, and otherwise
  join in the browser. The project domain is optional; a server or operator can host
  the same page.

## The SDK (`crates/client`)

The SDK is `nuschel-client`. It owns every decision that is not drawing; each client
(GPUI desktop, ratatui, Compose, SwiftUI, Svelte) is a view over it. Decided while
extracting it (2026-11-11); "Status" below says what has moved so far.

### Layers

| Layer | Where | Builds for | Holds |
|---|---|---|---|
| Session values | `nuschel-core::model` | everything, `wasm32-unknown-unknown` | channels, users, text targets, activity lines, ACLs, bans, limits: plain data the session reports |
| SDK core | `nuschel-client` (no features) | everything, `wasm32-unknown-unknown` | timelines, conversations, servers, the channel tree, unread counts, typing, direct conversations, the rules for editing and deleting; the inputs it takes and the updates it reports |
| Native platform | `nuschel-client`, feature `native` (default) | Linux, Windows, macOS; later Android, iOS | the driver on tokio: connection supervision and reconnects (over `nuschel-session`), the content and storage workers, the media worker per connection, history paging and the local cache |
| Platform services the app provides | the app | per platform | notifications, update checks, the audio check in Settings, website icons, image decoding for a viewer, files the user picks |

`just wasm` checks that the core builds for `wasm32-unknown-unknown` (it is part of
`just check`). The core depends only on `nuschel-core`, `nuschel-protocol` and serde.

### Modules

- `model` — the session values (re-exported from `nuschel-core::model`).
- `timeline` — one conversation's messages: `Timeline`, `Message`, `MessageStatus`,
  `IncomingMessage`, `Change`, `Deletion`, `Page`. Merging the local cache and server
  pages by id (seq on Nuschel servers) and observation time; the latest page starting
  over while keeping what arrived since; older pages only if they continue the
  timeline; windows around a search result; trimming the oldest; our pending sends
  until the server acknowledges their idempotency key; edits and deletions that wait
  for their message and never overtake it, undo a deletion or a newer edit.
- `conversation` — a channel or direct conversation: names, target, timeline, unread
  count and mention, the read marker, who is typing, activity lines, the collapsed runs
  of activity as data.
- `server` — one server's state: connection status, users, permissions, capabilities
  and policy, direct conversations, limits, the channel tree (`tree_rows`), and the
  rules (`may_edit`, `may_delete`, `typing_conversation`, `durable_directs_with`, …).
- `navigation` — which conversation is browsed and the tickets that drop late pages.
- `client` — the `Client` (below).
- `native` (feature) — `Driver`, today's backend: connections, workers, storage.

### API

```text
Client                      commands in             events in (from the driver)
  ├─ servers() / server(id) / conversation(ref)     read access, cheap, no copies
  ├─ connect, disconnect, open, load_older, jump_to, send, edit, delete,
  │  mark_read, typing, open_direct, search, set_focus     commands
  ├─ handle(Input)                                   driver events
  └─ take_updates() -> Vec<Update>                   what changed, per entity
```

- **Commands in**: methods on `Client`. None blocks: anything slow is a request to the
  driver, whose answer comes back as an `Input`. Failures a caller must show at once
  (not connected, too long) are returned as `Result<_, String>`.
- **Events out**: `Update` names what changed: a server, a conversation's timeline,
  unread counts, typing, a notice, an alert (sound, notification) for the platform to
  play or show, a send acknowledged. A UI marks itself dirty on updates and reads state
  through the getters (GPUI: `cx.notify()`; ratatui: redraw; UniFFI: a callback that
  Kotlin and Swift turn into flows; WASM: `postMessage` of the update and a snapshot).
- **Plain data only** in the public surface: owned strings, enums with data, no
  generics or lifetimes, no GPUI types, ids as values (`server: String`, channel ids,
  16-byte conversation ids, message serials). Rendered content is
  `nuschel_core::MessageDocument`; each UI converts it to its own representation and
  caches that by message serial (the desktop: GPU images and laid-out text).
- **Time is passed in**: the core never reads a clock (`std::time::Instant` panics on
  `wasm32-unknown-unknown`). Inputs carry wall-clock milliseconds; expiring typing
  indicators and waiting edits take the platform's monotonic milliseconds.
- **No runtime in the core**: it is a state machine. The native driver runs on tokio,
  as `nuschel-session` does; a browser driver will run on `wasm-bindgen-futures` and
  talk to the same core.

### What stays in each UI

Views, input states, focus, scrolling and virtualisation, animations, images in the
GPU atlas, menus, dialogs, theme; per-message presentation (laid-out document, height,
"Show more"), the expansion of collapsed activity runs, drafts and attachments being
composed, "new messages" below the fold.

### Status

1. Session values moved to `nuschel-core::model` (re-exported by `nuschel-session`).
2. Timelines.
3. Conversations, servers and the channel tree.
4. The native driver.
5. `Client` applies driver events and takes the commands.
6. A headless client (`crates/client/examples/headless.rs`) proves the API.

Remaining after these: identity, key transparency and trust state (`identity.rs`) and
the version notice still live in the desktop; settings and bookmarks stay in
`nuschel-session::config` until the core needs them on the web; search requests and
transfer offers are issued by the desktop.

## Reuse by layer

| Layer | Crates | Desktop | TUI | Android | iOS | Web |
|---|---|---|---|---|---|---|
| Domain types, policies | `core`, `client` | ✓ | ✓ | ✓ | ✓ | ✓ (WASM) |
| Protocol, session | `protocol`, `session` | ✓ | ✓ | ✓ | ✓ | ✓ over a tunnel (below) |
| Content (Markdown/HTML, images) | `content`, `legacy-html` | worker | worker | isolated service | in-process | WASM in a worker |
| Storage | `storage` | worker (SQLite) | worker | isolated service | in-process | IndexedDB backend |
| Identity, KT, MLS | `identity`, `kt`, e2ee | crypto worker | crypto worker | isolated service | in-process | WASM worker |
| Audio | media worker (cpal, Opus, DSP) | ✓ | ✓ | AAudio, platform echo cancellation | CoreAudio voice processing | WebAudio/getUserMedia (browser echo cancellation) |

Workers keep one message contract everywhere. Android runs them as `isolatedProcess`
services (a real OS sandbox, as Chrome does); iOS cannot start processes, so the same
boundary runs in-process there, which is weaker and documented as such.

## The web client's network path

Browsers cannot open TCP or UDP sockets. The web client therefore reaches servers
through a WebSocket (later WebTransport) tunnel, and keeps the tunnel untrusted:

- TLS to the Mumble or Nuschel server runs **inside the browser** (rustls in WASM)
  over the tunnel, so the tunnel relay sees only ciphertext and the server's
  certificate is checked by the client itself, as in the apps.
- Voice datagrams are already OCB2-encrypted with keys from the TLS channel; the relay
  forwards them without being able to read them (UDP over WebTransport datagrams, or
  Mumble's TCP tunnel over the WebSocket as a fallback).
- This works for **legacy Mumble servers** too, so the web client can join them.
- The relay is part of the project-run web host (and of the module for others). It is
  not an open proxy: destinations must answer a Mumble ping first, private and
  link-local address ranges are refused (no SSRF), and connections are rate-limited
  per client address.
- Nuschel servers may offer the tunnel endpoint themselves, so the extra hop is
  optional, and video and group voice later use WebRTC to the server's SFU.

## Video (queued)

High-quality video without transcoding, so servers stay cheap:

- **Scalable video coding (SVC)**: the sender encodes once into layers (AV1 or VP9
  temporal and spatial layers, e.g. L3T3). The server's SFU forwards to each receiver
  only the layers that receiver's bandwidth allows, reading the RTP dependency
  descriptor, and never decodes. Simulcast (2–3 independent encodings) is the fallback
  for encoders without SVC.
- **Who picks the bitrate**: in group calls each receiver's bandwidth estimate decides
  at the SFU which layers it gets, and the sender adapts its top layer to its own
  uplink. In a direct two-person call over WebRTC without the SFU, congestion control
  between the two peers makes the sender encode for that one receiver.
- End-to-end encryption with SFrame keyed from MLS still allows layer selection,
  because the dependency descriptor stays outside the encrypted payload.
- Encoders and decoders run in sandboxed workers; platform hardware codecs where
  available (VA-API, VideoToolbox, MediaCodec, WebCodecs), memory-safe software codecs
  otherwise.

## Order of work

1. The SDK extraction, with the desktop moved onto it.
2. The terminal client.
3. Android, then iOS.
4. The web client, the tunnel relay and the NixOS module for the web host and share
   links.
5. Video, with the WebRTC work.
