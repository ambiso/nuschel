# Clients on every platform

Decisions with the user (2026-11-11) and the architecture that follows from them. The
desktop (GPUI) and the terminal client (`apps/tui`) exist; the Android, iOS and web
clients are queued.

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
- **The project domain is nuschl.eu** (user, 2026-11-11), after the rename to nuschl.
- **The web client is served from one domain the project runs**, not by each server.
  Users already trust the project for the software they run, so the web client adds no
  new party to trust, and an origin that stays the same lets the browser keep one
  cross-server identity. Others can host the same web client on their own domain.
  Signal-like client version requirements apply to the web client as to the apps.
- **NixOS first** for everything the project or operators host: a NixOS module for the
  web client and share-link host using the usual `services.nginx.virtualHosts` with
  `enableACME = true` (optionally on a subdomain), next to the existing server module.
- **Share links** (`https://<web host>/join/…`) open the installed app through Android
  App Links, iOS Universal Links or the desktop's `nuschl://` scheme, and otherwise
  join in the browser. The project domain is optional; a server or operator can host
  the same page.

## The SDK (`crates/client`)

The SDK is `nuschl-client`. It owns every decision that is not drawing; each client
(GPUI desktop, ratatui, Compose, SwiftUI, Svelte) is a view over it. Extracted from
the desktop on 2026-11-11; "Status" says what moved and what is left.

### Layers

| Layer | Where | Builds for | Holds |
|---|---|---|---|
| Plain data | `nuschl-core` (`model`, `bookmark`) | everything, `wasm32-unknown-unknown` | what a session reports (channels, users, text targets, activity, administration, limits, transport statistics) and saved servers |
| SDK core | `nuschl-client` without features | everything, `wasm32-unknown-unknown` | the `Client`; timelines, conversations, servers, the channel tree; unread, mentions, read markers, typing, direct conversations; the rules for editing, deleting, typing and sounds; the `Platform` trait with its requests and inputs |
| Native platform | `nuschl-client`, feature `native` (default) | Linux, Windows, macOS; later Android and iOS | `native::Driver`, the `Platform` on tokio: one supervised connection per server over `nuschl-session` with reconnects and backoff, the one media worker they share, the content and storage workers, server history pages, edits and deletions kept in order |
| The app's own services | the app | per platform | Desktop: notifications, updates, the Settings audio check, website icons, the image viewer, attached images (`apps/desktop/src/services.rs`) |

`just wasm` (part of `just check`) lints `nuschl-core` and `nuschl-client` without
features for `wasm32-unknown-unknown`. The core depends on `nuschl-core`,
`nuschl-protocol` and serde only; no runtime, no clock, no I/O.

### Modules

- `client` — `Client`, `Focus`, `Update`, `Notification`, `SendError`.
- `platform` — the `Platform` trait the platform implements; `Request` (to one
  server's connection), `ChangeRequest`, `CacheKey`, `RenderKey`.
- `input` — `Input`, what a platform reports (connection lifecycle, rendered
  messages, history pages, edits, audio), and `ServerEvent`, what a server said.
- `timeline` — one conversation's messages: `Timeline`, `Message`, `MessageStatus`,
  `IncomingMessage`, `Change`, `Deletion`, `Page`, `WaitingChanges`. Merging the
  local cache and server pages by id (seq on Nuschl servers) and observation time;
  the latest page starting over while keeping what arrived since; older pages only
  if they continue the timeline; windows around a search result and the newer pages
  after them; trimming the oldest; pending sends settled by their idempotency key;
  edits and deletions that wait for their message and never overtake it, undo a
  deletion or a newer edit.
- `conversation` — a channel's or direct conversation's state: timeline, unread
  count and mention, "new below the fold", read marker, typing, activity lines, and
  its entries in order (activity folded into runs, messages grouped by author).
- `server` — `ServerState`: status, users, permissions, capabilities and policy,
  direct conversations, limits, audio route; the rules (`may_edit`, `may_delete`,
  `typing_conversation`, `durable_directs_with`, …).
- `tree` — the channel tree as rows, closed branches counting who is inside.
- `names` — nicknames and friends (`Names`, `Preferences`), mentions.
- `finder` — the quick switcher's results (Ctrl+K): channels, people and direct
  conversations of every server, ranked by recent visits, unread state and a fuzzy
  match.
- `search` — what a message search covers (`Scope`), its results as a UI lists them
  (`Hit`: place, author, snippet), and where a result's message is now (`Located`,
  `Client::show_message`).
- `sounds` — when a sound effect plays.
- `voice` — where your voice goes on several servers: the talk servers, each
  server's `Role`, and the question before a second one ("Voice on several servers").
- `navigation` — the open conversation, voice, and tickets that drop late pages.
- `native` (feature) — `Driver`, its `Event`s and their conversion to `Input`;
  `Client::open_url` for `mumble://` links (their parser and the servers' security
  records are `nuschl-session`'s); `native::media`, the one media worker's supervisor.

### API

```rust
// Native wiring (crates/client/examples/headless.rs is a whole client):
let (driver, mut events) = Driver::new(runtime, profile, settings);
let driver = Arc::new(driver);
let mut client = Client::new(Box::new(driver.clone()), bookmarks, native::preferences(&settings));
client.connect(0, None, false);
while let Some(event) = events.recv().await {
    client.set_time(monotonic_ms);
    if let Ok(input) = event.into_input() { client.handle(input) }  // else: identity events
    client.settle();                         // once per batch: sort channels, apply waiting edits
    for update in client.take_updates() { /* redraw, scroll, notify, save bookmarks… */ }
}
```

- **State is read in place**: `client.servers` (each a `ServerState` with its
  conversations and their timelines), `client.navigation`, `client.focus`. Cheap: no
  copies; a UI redraws what an update names.
- **Commands** are methods: `connect`, `disconnect`, `trust_certificate`,
  `open_url`, `add_server`, `replace_server`, `remove_server`, `open` (browse a
  conversation; loads its latest page), `load_history`, `load_page`, `send_text`,
  `edit`, `delete`, `change_message`, `composing` (typing), `mark_read`, `set_mute`
  (which may return the question "Speak on two servers?", answered with
  `speak_here`), `toggle_voice` (the mute shortcuts), `use_voice_here`, `join`,
  `open_direct`, `search` (with `search_scope`, `search_hits`, `locate`,
  `show_message`), `request` (to a server, for what is not modelled yet), `log_gap`. None blocks: slow work is a `Platform` call answered later as an
  `Input`. Failures to show at once (not connected, too long, a full queue) come back
  as `Result`s; the rest as the server's `notice` or an `Update`.
- **Inputs** (`Client::handle`): what the platform reports, each with the server and
  the connection generation it belongs to; a stale generation's reports are dropped,
  as are history pages whose ticket is no longer current.
- **Updates** (`Client::take_updates`): `BookmarksChanged`/`…Now` (save),
  `Browse` (open this conversation), `LoginRefused`, `Sent` (a composer can clear),
  `PageShown` (scroll to the newest, or to a search result), `HistoryFailed`,
  `HistoryRecovered`, `OwnMessage`, `Notify`, `Negotiated`, `Gap`,
  `MessageDeleted`, `SearchResults`, `Rendered` and `Avatar` (content for the UI's
  own images). GPUI: apply, then `cx.notify()`; ratatui: redraw; UniFFI: a callback
  per update that Kotlin and Swift turn into flows; WASM: `postMessage` of the update.
- **Plain data**: owned strings, enums with data, ids as values (server ids, channel
  ids, 16-byte conversation ids, message serials); no generics or lifetimes on the
  `Client`'s methods, no GPUI types. Rendered content is
  `nuschl_core::MessageDocument`; a UI converts it once per message serial and
  revision (the desktop: `presentation.rs`).
- **Time is passed in**: `set_time` with the platform's monotonic milliseconds
  (`std::time::Instant` panics on `wasm32-unknown-unknown`); inputs carry wall-clock
  milliseconds.
- **No runtime in the core**: `Client` is a state machine behind a `Platform` trait
  object. The native driver runs on tokio; a browser platform will run on
  `wasm-bindgen-futures` and talk to the same core.

### What stays in each UI

Views, input states, focus, scrolling and virtualisation, animations, images in the
GPU atlas, menus, dialogs, theme; per-message presentation (laid-out document,
height, "Show more"), which activity runs are open, drafts and attachments being
composed, where the reader is scrolled (it tells the client through `Focus`).

### Status

Moved, in this order, each step leaving the desktop working:

1. Session values and saved servers to `nuschl-core` (`model`, `bookmark`), re-exported
   by `nuschl-session`.
2. Timelines.
3. Conversations, servers and the channel tree.
4. The native driver (the desktop's former `backend.rs`) and the sound policy.
5. `Client`: applying what the driver reports, and the commands. The desktop is a
   view over it.
6. `examples/headless.rs` with `tests/sdk_headless.py`: three clients without a UI
   connect to a loopback nuschld, register, meet in a channel, send, receive, edit,
   and load the server's history through the SDK alone.
7. For the terminal client, what it would otherwise have copied from the desktop: the
   switcher's ranking (`finder`), search scopes, result naming and jumps (`search`),
   the typing line (`ServerState::typing_line`), opening `mumble://` links
   (`Client::open_url`, which the desktop uses too) and trusting a certificate
   (`Client::trust_certificate`). The desktop keeps writing the servers before it
   reconnects after a trust (it rolls the pin back if that fails).

Left, in the order they should move:

- **Identity, key transparency and trust** (`apps/desktop/src/identity.rs`): the
  driver reports them as `native::Native` events outside `Input`, because their
  types live in `nuschl-ipc::crypto`, which needs tokio. Move the crypto worker's
  message types to a crate that builds for the browser, then the state to the SDK.
  The version notice (`version_notice.rs`) goes with it.
- **End-to-end direct conversations** (`mls/1`, opt-in): the driver reports
  `nuschl_session::mls::MlsEvent`s as `native::Native::Mls`; the desktop keeps each
  conversation's lock (`ServerUi::e2ee`) and opens groups with `MlsCommand`s. In the
  SDK they become a conversation's lock state in `ConversationState`, sends through
  the crypto worker as a `Request` settled like pending sends (by key, then seq), and
  decrypted rows as `IncomingMessage`s into the same timeline (plaintext the server
  pushes into a pinned conversation stays out). The `mls` capability is already
  `ServerState::mls`.
- **Administration** (ACLs, bans, registered users, channel edits, user actions,
  comments and avatars, listening, whisper targets, context actions): the desktop
  still sends `nuschl_session::connection::Command`s to the driver itself; each
  becomes a `Request`.
- **Settings**: `ClientSettings` stays in `nuschl-session::config` (its recovery
  policy is an `nuschl-ipc` type); the client sees `names::Preferences`.
- **Transfers** (wormhole offers, `transfer.rs`) and **attachments**: a platform
  service and SDK state each.
- **Search**: requests, scopes, results and jumps go through the client; the
  highlight of a jumped-to message is each UI's.
- **The driver** is `native.rs` and its media supervisor (`native/media.rs`); split
  the pieces needed by mobile into connection, content and storage adapters as
  that work needs them; completing every SDK feature is not an iOS prerequisite.
- **Bindings**: wasm-bindgen in `crates/client-wasm` (docs/web.md): `WebClient`
  wraps `Client`, servers and updates come out as JSON, and a browser `Platform`
  connects through the web relay with TLS in WebAssembly (login, channels and users
  so far). UniFFI is not adopted yet; a thin crate will wrap `Client` the same way.

## The terminal client (`apps/tui`)

`nuschl-tui` (ratatui on crossterm) is the SDK's second view, built on 2026-11-11 to
prove the SDK is UI-independent: everything in it that is not drawing or keys is a
call into `nuschl-client`, and what the desktop had kept to itself moved into the SDK
first (Status, step 7). Decisions:

- **Same profile and workers.** `Profile` (or `--profile-dir`), the saved servers,
  local history, identity and the sandboxed workers beside the executable, through the
  native driver; a `mumble://` link on the command line or with `/url`.
- **One state, two outputs.** The full screen (channels and people, the conversation,
  who is typing, the composer, two status lines, dialogs) and `--plain`, which prints
  each change once as a line and reads commands and messages from standard input. Both
  run the same `App` and the same commands (`/join`, `/edit`, `/search`, …), so
  whatever a key does, a command does too. Plain lines are chosen automatically when
  standard output is not a terminal or `TERM=dumb`.
- **Screen readers.** Nothing is said by colour alone: states are words ("talking",
  "muted", "3 unread, mention", "(edited)", "(deleted by a moderator)"), the row or
  message the keyboard is on starts with `>`, and the terminal cursor sits on it (or in
  the field being typed in), so readers that follow the cursor read the right line.
  Plain lines suit readers that read new output (Speakup, Orca or NVDA in a terminal).
- **Untrusted text.** Every string from a server goes through one cleaning step:
  control characters (escape sequences) and bidirectional overrides never reach the
  terminal. Images show as `[image, W×H]` (the content worker keeps no alternative
  text), display formulas as `[formula]` with their source; links are numbered under
  the message and open only on request (`o`, `/open N`), web links with the system's
  opener (`--opener`; none over SSH, where they are shown), `mumble://` links in the
  client itself.
- **Voice over SSH.** Over SSH (or with `--no-audio`) every connection stays muted
  and deafened, so no media worker opens audio devices on the remote machine; voice
  elsewhere goes through the same media worker as the desktop (`--audio` over SSH).
  Unmuting on a second server asks first; `/speak both` confirms sharing the
  microphone and `/speak here` moves it. Both commands respect `--no-audio`.
  Most terminals report no key releases, so there is no push-to-talk yet (the kitty
  keyboard protocol would allow it where a terminal speaks it).
- **Keys.** Tab moves between the panes; Ctrl+K, Ctrl+F, F1 (and `?` outside the
  composer), Alt+M and Alt+D work everywhere; single letters act only outside the
  composer. `keys.rs` holds the bindings and the help that lists them.

Not yet in the terminal client: identity and key transparency beyond a new
identity's recovery key and the worker's failures, which show as warnings (the
driver's other `Native` events are dropped), end-to-end direct conversations, file
transfers (an offer shows as text), administration, whisper, and settings beyond the
command line.

## Voice on several servers

Voice is currently available on compatible servers only. An `e2ee/1` server needs
SFrame before media can be enabled; its chat encryption does not protect Mumble Opus.
The SDK refuses voice controls there, the desktop says "Voice unavailable", and the
shared worker neither sends microphone packets to it nor plays raw audio from it.
Compatible servers can still use the one microphone and playback mix independently.

Decided with the user on 2026-11-11: they often listen to several voice channels at
once and very rarely speak in more than one. So: **listen anywhere, speak in one place
by default.**

- **One media worker for the app**, owned by the native driver (`native/media.rs`),
  not one per connection. Each connected server attaches to it as a *stream*; the
  worker keys its speakers by (stream, session), since session numbers are per server,
  and mixes every server that is not deafened into one output ("Voice chat" in volume
  mixers, as before). One output also makes the canceller's fallback reference (its
  own playback, where the output device cannot be recorded, as on macOS and Windows
  today) complete. Each server has a playback volume (`server_volumes` in the
  settings; the server's menu), applied with the per-user volume to each packet.
- **One capture, encoded once**, handed to every server you are unmuted on. The
  encoder takes the lowest bitrate and the highest measured loss among those servers.
  The microphone still opens and closes with a new worker process (a native stream's
  teardown can block); the worker is replaced when the microphone's state or its
  speaking destinations change, discarding speech buffered for the previous
  listeners. Per-connection routing revisions reject packets queued before a
  mute/unmute round trip, even while another server keeps speaking. Until a new one
  has its devices open, every server's status says "Starting audio…". Changes settle
  for 30 ms first, so moving your voice (one server muted, another unmuted) is one
  change, and it sounds like what the microphone did, not like a mute and an unmute.
  With every server deafened no worker runs; sounds then play through a worker that
  opens only the output, as before.
- **Talk servers** (`voice::Voice` in the SDK) own the microphone. There is one by
  default: the server whose channel you last joined, or the one you chose with "Use
  voice here" (its menu); a new connection becomes it unless your voice is live
  somewhere. Joining a channel elsewhere moves a live microphone along (the other
  server then sees you muted) and keeps a muted one muted. Unmuting on another server
  while your voice is live asks once, "Speak on two servers?": *Speak on both* adds it
  to the talk servers (later unmutes there do not ask again), *Only on …* moves the
  voice, *Cancel* changes nothing.
- **Push-to-talk and the mute shortcuts act on the talk servers only.** Push-to-talk
  gates the one capture, which reaches only them; the mute shortcut toggles them
  together (a talk server you deafened is left alone). The footer's buttons stay per
  server.
- **What each server sees stays true**: one you only listen to sees you self-muted, a
  deafened one sees you deafened, and only the talk servers ever receive your voice
  (the driver routes by what each server was told, and each connection checks again).
- **Where it shows** (desktop): the talk server has a green microphone ring in the
  server rail, servers you listen to a small headphone badge that breathes while
  someone talks there (eased, `motion::Ease`, drawn 25 times a second rather than at
  the display's rate, since GPUI redraws the whole window; lit without motion when
  motion is reduced). The voice
  card reads "Talking on Beta #Lobby · listening to Alpha #Lobby", each place a
  button that takes you there; the user panel says "Listening" for a server that
  only plays. Rail buttons and the card are named for screen readers.

Tested by `voice::tests` and `client::tests` (the policy), `native::media::tests`
(routing), the media worker's `mixer::tests`, and `tests/voice_servers.py`: the real
desktop over AT-SPI against two loopback Murmurs with a private PulseAudio.

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

- TLS to the Mumble or Nuschl server runs **inside the browser** (rustls in WASM)
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
- Nuschl servers may offer the tunnel endpoint themselves, so the extra hop is
  optional, and video and group voice later use WebRTC to the server's SFU.

Built (2026-11-11): the relay (`apps/web-relay`), the NixOS module for the web host
(`services.nuschl-web`), share links (`share/1`, the landing page, the desktop's
`nuschl://`) and the SDK in WebAssembly with a Svelte skeleton; docs/web.md has
the details and what is left.

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

1. The SDK extraction, with the desktop moved onto it (done).
2. The terminal client (done: `apps/tui`).
3. Finish and push the verified Nuschel → Nuschl rename.
4. **iOS first** (user priority, 2026-11-11): SwiftUI over UniFFI. Start with the
   Linux-hosted macOS VM feasibility check (Docker-OSX/QEMU-KVM), bindings and iOS
   build/app foundation; implement the server/channel/chat view
   and the iOS audio adapter in parallel. The first usable milestone is text and
   voice against private Mumble and compatible Nuschl servers in iOS Simulator,
   with bookmarks, local history and reconnects. Then harden background audio,
   routes, interruptions, accessibility and distribution. Review the in-process
   codec/content boundary explicitly, preferring memory-safe code and OS services.
   No owned Apple hardware is assumed. Pin a compatible guest/Xcode/simulator
   combination and prove SwiftUI rendering/UI tests, including the Intel simulator
   build of the Rust library. Docker-OSX documents Xcode and headless operation,
   but lists GPU acceleration as unfinished, so actual simulator compatibility is
   an early experiment, not an assumed capability ([project documentation](https://github.com/sickcodes/Docker-OSX)).
   Record physical-device audio/background validation separately; simulator results
   are not evidence of hardware behavior ([Apple's testing guidance](https://developer.apple.com/documentation/xcode/testing-a-release-build)).
5. Authenticated per-device Nuschl sessions before simultaneous linked desktop/phone
   logins; do not clone private device keys. Optional recovery and preferences sync
   follow their own prerequisites and do not block the initial iOS client.
6. Android and the rest of the browser client; the browser relay, SDK foundation and
   web-host module already exist. Additional MLS history features and video/SFrame
   are separate tracks, not prerequisites for the first iOS client.
