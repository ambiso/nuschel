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
