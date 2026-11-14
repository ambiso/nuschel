# Screen sharing plan: Nuschl and stock Mumble servers, with browser watch links

Status: plan, 2026-11-13. Nothing is implemented, and effort figures are rough.
It extends the frozen #139 signalling design
(`83a5b5a7:docs/legacy-signalling-plan.md`) and the frozen #93 plugin-data work
(`0e40c7ac`), and covers #35, #135 and #139–#147. **Owner** marks decisions the
owner made on 2026-11-13; the rest are recommendations (§8).

## 0. Summary

- **Topology:** a forward-only companion SFU at stream.nuschl.eu relays for link
  viewers, relay-only users and sharers whose upload is too small. Direct paths
  come later, natively, only between verified Nuschl identities (§4.5). TURN at
  turn.nuschl.eu is a reachability fallback that can relay only to the SFU.
- **Quality (Owner):** up to 4K60 where devices and network measurably sustain
  it. Only resolution, frame rate and bitrate adapt.
- **Phases:** browser capture launched from Nuschl first (§2), then native Linux
  capture and viewing. End-to-end-mode servers refuse sharing until last.
- **Key agreement on stock servers:** each viewer runs a fresh X-Wing HPKE
  exchange with the sharer, authenticated by a PSK derived from the link secret.
  A server that only reads the link cannot decrypt. To watch, it must join, and
  so appear in the viewer list, or intercept browser viewers with the stream
  host's help.

## 1. What exists

- `nuschl-rtc`: a str0m 0.25 trial with Opus and VP8, one offer/answer and host
  candidates only; no TURN, ICE restart or SFrame. Its 256 KiB frame cap and 10
  Mbit/s budget are too small for 4K. Only this crate may use str0m; C++ and Go
  WebRTC stacks are excluded.
- str0m packetizes AV1 and detects AV1 key frames, but has no Dependency
  Descriptor (DD) support and no TURN client. DD parsing, per-viewer layer
  selection and sequence/timestamp rewriting are Nuschl code.
- No first-party capture, encoder or decoder. GPUI's capture code runs in the
  desktop shell, so it can't be used. The media worker holds microphone and
  audio-socket capabilities, so video needs a new worker role.
- The browser client does audio only. It has no RTCPeerConnection or
  getDisplayMedia.
- `privacy/1` relay-only and `PeerPolicy` exist; `rtc/1` does not (#141).
- `nuschl-identity` has X-Wing HPKE (hpke-rs with libcrux). cargo-deny confines
  that crate to kt, e2ee, nuschld and the crypto worker, so reuse the pattern,
  not the crate. There is no SFrame code.

## 2. Phases

**Phase 1, the first release:**
- **Sharer:** a Nuschl desktop user clicks "Share screen". Nuschl generates the
  link secret S and a publisher secret P, opens
  `https://stream.nuschl.eu/share#v=1&k=S&p=P` in the system browser, and posts
  `https://stream.nuschl.eu/watch#v=1&k=S` to the channel. P is never posted.
- **Capture:** the browser's picker (getDisplayMedia), video only. Realistically
  up to 1080p30 with software AV1; higher only where the browser encodes AV1 in
  hardware and keeps up. 4K60 is mainly a Phase 2 target.
- **Viewers:** anyone with the link, in a browser, Nuschl users included, always
  through the SFU.
- **Servers:** stock Murmur 1.x and compatible-mode nuschld (interim link mode
  until `rtc/1`). End-to-end-mode nuschld refuses with "not available yet".
- **Caps:** 1 publisher and 16 viewers on the project instance.

Browser-launched capture is the owner-approved intermediate step towards native
capture and viewing.

**Phase 2, in parallel:** native Linux sharer (portal, isolated video worker
with VA-API AV1 and a software fallback, str0m in the SDK driver); native viewer
(isolated decoder #35, GPUI #145, identity-bound key exchange #139 B,
plugin-data cards #93 and #139 C), then direct paths; end-to-end servers (#139
I0/I, #135).

**Later:** Windows and macOS capture, audio (#146, #144), scalable video (#140),
nuschld's own SFU and `rtc/1` (#142, #141), long-call validation (#147).

**Why an SFU:** link viewers and relay-only users need one anyway, a browser
sharer re-encodes per peer connection, and few uploads cover many 4K streams (16
× 25 Mbit/s = 400 Mbit/s).

## 3. Key agreement on stock servers

This replaces #139 §5.2's `sframe_base(g)`, which was derived from S, and #139's
"Not end-to-end encrypted" labels. Record it as a new revision of the #139 plan;
frozen `83a5b5a7` stays as it is.

**Key schedule.** S is 32 bytes from the OS RNG. PRK =
HKDF-Extract("nuschl/stream/v2", S). From PRK: `stream_id` (16 B, the SFU room),
`admit_key`, `mac_key`, `psk` and `signal_key`.

**Joining:**
- The viewer makes a fresh X-Wing keypair (sk_v, pk_v) for every join and sends
  `KeyRequest{version, suite, stream_id, viewer_id, nonce_v, not_after, pk_v,
  [binding]}`, MACed under `mac_key` so non-holders are dropped cheaply.
- The HPKE `info` is the hash of one canonical transcript: protocol label and
  version, role labels (`viewer→sharer`, `sharer→viewer`), suite IDs, stream_id,
  viewer_id, both nonces, not_after and pk_v. psk and psk_id come from S.
- The sharer replies `KeyGrant{nonce_s, enc, ciphertext, confirm_s}`: an HPKE
  PSK-mode seal to pk_v of {epoch, K_epoch, kid, not_after}, and a transcript
  MAC under a key exported from the HPKE context. The viewer answers
  `KeyAck{confirm_v}` under a second exported key.
- The context also exports a per-viewer channel key. Rekeys are sealed under it
  with a counter.
- Replays (bounded nonce cache, not_after), reflections, any other suite,
  expired requests and bad confirmations are rejected. All fail closed, with no
  classical-only fallback.
- **Passive-holder claim:** someone with S and every recording still cannot
  decrypt, because every sk_v and every K_epoch is fresh and random. It holds
  only while neither is ever derived from S or an earlier key.

**Message sizes and credentials:**
- **Key messages ≤16 KiB (Owner: "larger limit then").** Our own bound on the
  SFU WebSocket, sized so identity credentials fit inline: a KeyRequest is
  ≤5,124 B without them and about 12.1 KB with the root key and signing
  certificate inline. No multipart.
- **Credentials by fingerprint (Owner: "clients can cache keys and we only send
  a fingerprint").** A binding names the identity root key and the current
  `DeviceSigningCertificate` by the SHA-256 of their bytes. On a cache miss the
  receiver answers `NeedCredentials{fingerprints}` and gets them inline, or
  fetches them from key transparency. Mismatching bytes are rejected. The cache
  is per device and bounded by entries and bytes; revocation and
  key-transparency status are still checked.
- Bindings, fingerprints and credentials travel only inside the `signal_key`
  encryption, so the SFU never learns who is watching.
- A policy that requires the hybrid binding refuses the Ed25519 one.
- Suite: X-Wing / HKDF-SHA256 / ChaCha20-Poly1305, as in
  `crates/identity/src/hpke.rs`.

**Fan-out and rekeying:**
- Key messages travel over the SFU WebSocket as opaque blobs. Plugin data would
  need 2–3 chunks per key under Murmur's 4 messages/s budget.
- **Join: rekey first.** A newcomer never gets an epoch that carried media
  before it joined. The sharer sends K_{e+1} to the existing viewers, grants it
  to the newcomer, then forces a key frame. Joins may be batched for up to 2 s.
- **Leave or kick:** rekey within 2 s; receivers keep the previous epoch for 2
  s. Rekey every 15 minutes regardless. A kicked viewer can rejoin through the
  link until "Rotate link" (new S, new room).

**SFrame:**
- RFC 9605 `AES_128_GCM_SHA256_128`, unmodified, with one KID per encoding.
- **Implementation (recommended):** our own RFC 9605 layer in `crates/stream`, a
  few hundred lines tested against the RFC's vectors, on the RustCrypto
  `aes-gcm`, `hkdf` and `sha2` crates already in `Cargo.lock`. Not the `sframe`
  crate: it is not in the lockfile and has tens of thousands of downloads,
  against hundreds of millions for those crates.
- **AV1 envelope.** Whole-frame SFrame over AV1 fails in every browser, because
  the packetizer parses the payload as OBUs. What works in Chromium and Firefox:
  one byte per frame, `0x08` (an AV1 sequence-header OBU header without a size
  field) on key frames and `0x28` or `0x08` on deltas, then the ciphertext. The
  byte is authenticated as SFrame metadata. Key frames must carry `0x08`, or
  receivers without the DD (Firefox) never detect one. This relies on
  unspecified libwebrtc behaviour, so it needs a regression test per supported
  browser version.
- If the sender transform throws or its worker dies, sending stops; no plaintext
  reaches the wire.

**Why not a PAKE:** a PAKE only matters for short, human-typed secrets, and S
has 256 bits. SPAKE2 is also classical, and post-quantum PAKEs have no reviewed
Rust implementation.

**SFU admission:**
- The SFU holds stream_id, `admit_key` and a publisher verifier derived from P,
  never S, `mac_key`, `psk`, `signal_key` or a K_epoch. It sees only HPKE and
  SFrame ciphertext.
- The share page reserves the room before Nuschl posts the link. If the room
  exists, sharing fails with "link already in use".
- A viewer proves `admit_key` by an HMAC over an SFU nonce and gets only the
  viewer role. Publishing needs the P verifier, so a new publisher needs a new
  link.

**Who can do what:**
- An active stock server can join as an extra viewer, which shows in the roster,
  or swap the posted link and relay, which also needs a join.
- A stock server colluding with the SFU or page code can intercept browser
  viewers undetected.
- Identity-bound Nuschl viewers detect a substituted pk_v through `CallBinding`
  ("Identity check failed"). Browser viewers have no identity key and are shown
  as "unverified viewer".
- A leaked link plus recorded SFU traffic does not decrypt later, which fixes
  #139 §3.1's retention problem.

**Where it runs:**
- One portable crate, `crates/stream`, built natively and for WASM; no JS crypto
  library.
- Browser: one dedicated worker that is also the encoded-transform worker. The
  main thread never holds K.
- Native: the crypto worker runs the HPKE exchange and `CallBinding`, and hands
  only K_epoch and the channel keys to the SDK driver, which does SFrame. Keys
  never enter capture or codec workers.
- **Provider (Owner: "libcrux is verified so we should probably prefer it to
  RustCrypto"):** libcrux with hpke-rs on both targets, as identity uses. The
  browser stream worker uses libcrux's portable wasm32 build, which matched
  native byte for byte. That needs a new `SECURITY.md`/cargo-deny row for that
  worker only, proposed and not approved until reviewed. Verification covers the
  ML-KEM and X25519 cores, not the wrappers, so versions stay pinned to patched
  releases.

**Labels:** the sharer card says "Encrypted against passive servers. Anyone with
the link can watch; everyone watching is listed here. A server that saw the link
could join, or, with the stream host's help, intercept browser viewers." Short
form "Link-encrypted", never the green lock, which end-to-end-server streams get
only under `docs/e2ee.md` §7.4.

## 4. Quality, bandwidth and paths

**Owner target:** "ideally we can support 4K 60fps at a good bitrate, with the
client opting into relay forwarding if their upload bandwidth doesn't suffice
for direct streaming (and clients would request more bandwidth) and downgrading
the bitrate automatically to what can be sent over the connection (or what the
clients can process)."

Quality and path changes never change encryption, key agreement, labels or
relay-only.

### 4.1 Tiers

Realtime AV1 estimates from published guidance, not measured here (±50 %):

| Tier | Screen content | Motion | Key frame |
| --- | --- | --- | --- |
| 4K60 | 6–15 Mbit/s | 20–40 Mbit/s | 0.4–2.5 MB |
| 4K30 | 4–10 | 12–25 | 0.4–2.5 MB |
| 1440p60 | 3–8 | 10–20 | 0.2–1 MB |
| 1080p60 | 2–5 | 6–12 | 0.1–0.6 MB |
| 1080p30 | 1.5–3 | 3–6 | 0.1–0.6 MB |
| 720p30 | 0.8–1.5 | 1.5–3 | 40–250 KB |
| 720p15 (floor) | 0.4–0.8 | 0.8–1.5 | 40–250 KB |

At 4K60 the default is 12 Mbit/s for screen content and 25 Mbit/s for motion,
never above 40.

### 4.2 Frame bounds and key frames

Today's trial bounds would reject 4K key frames. Proposed, checked by S0f:
- **Encoder caps:** key frames ≤1 MiB from 1440p up, ≤512 KiB below; deltas ≤
  half that. An oversized frame is dropped, never split or queued, and a capped
  key frame is requested.
- **Admission:** a key frame may take 500 ms at the leg's estimate, a delta 120
  ms. A leg too slow for the tier's key frame steps down.
- **SFU queues:** about 300 ms at the leg's estimate, clamped to 32 KiB–2 MiB.
  RTX caches hold about 1 s, ≤ about 100 MB for a 4K room of 16.
- **Key-frame requests** are coalesced and rate-limited: one per viewer per 2 s,
  and at 4K at least 1 s apart per room. They never change the tier. Native
  senders repair loss with rolling intra refresh.

### 4.3 What each device may do

Measured at the start and re-checked during the share:
- **Sharer at 4K60:** a hardware AV1 encoder with encode p95 well under 16.7 ms,
  fed without a CPU copy (natively, PipeWire DMA-BUF into VA-API).
- **Software encoders:** at most 1080p30. Apple has no AV1 hardware encoder, so
  Apple sharers are software-only.
- **Viewer at 4K60:** hardware AV1 decoding, or a software decoder that measures
  it. Single-threaded rav1d without assembly reaches roughly 25–40 fps at 4K.
- **WASM rav1d:** at most 1080p30, 1080p15 by default.
- **Browsers:** `mediaCapabilities` is only a hint; the page goes by `getStats`.
- Below the 720p15 floor, a device gets a clear refusal.

### 4.4 Requests and adaptation

- **Viewers request** a tier with a fixed-size `QualityRequest`, at most once a
  second. "Auto" asks for the player's size.
- **Viewers report capacity** (Owner: "or what the clients can process"), sealed
  to the sharer under the channel key. A viewer steps down after 3 s below 90 %
  of the target fps or with decode p95 above 80 % of the frame interval, and up
  after 30 s with p95 below 50 %.
- **A viewer gets min(request, decode capacity, network capacity, policy),** and
  the player says which limit applied.
- **The sharer encodes for the largest effective demand,** bounded by its
  upload, encoder and policy. The SFU tells the sharer only that aggregate.
- **Layers:** the SFU picks layers per viewer from the DD, so a slow viewer
  never lowers anyone else's quality. Hardware encoders usually offer temporal
  layers only; per-viewer resolution needs spatial layers or simulcast (decided
  after S0f).
- **Without layers** (Firefox sharers today): one stream for the slowest
  admitted viewer, never below the floor.
- **Congestion control:** GCC/TWCC on every leg; new legs start low and probe
  up. The SFU drops layers within a frame, and the encoder follows within 200 ms
  with target = min(upload estimate × 0.85 − audio reserve, largest demand).
- **Step-down order:** bitrate to the tier's floor, then frame rate (60 → 30 →
  15, by dropping temporal layers), then resolution, at most once per 10 s since
  it needs a key frame. For motion content, resolution may go first.
- **Hysteresis:** step down after 2 s below the floor. Step up only with ≥1.3×
  the next floor for 10 s (frame rate) or 30 s (resolution); a failed step-up
  doubles the wait, up to 2 min.

### 4.5 Direct or relayed

- **Relayed:** one upload to the SFU, which fans out. **Direct:** a native
  sharer encrypts once and sends a copy per viewer, choosing layers with the
  SFU's code. Browser sharers re-encode per connection, so they stay relayed.
- **Who may go direct:** only verified Nuschl identities (`CallBinding` checked)
  that both allow it; never a relay-only user (§7) or a link viewer on a stock
  server, since a direct connection reveals each side's IP address.
- **Candidates** go only to the verified peer, sealed under its channel key, not
  `signal_key`, which every link holder has.
- **Choosing by upload:** probe the upload against the SFU before sharing (≤2 s,
  bounded bytes) and go direct if 80 % of the estimate covers the direct
  viewers. Move viewers to the relay, largest first, if the direct legs back off
  together or demand exceeds 85 % of the estimate for 5 s. Return only with 50 %
  headroom for 60 s, at most once per 2 min per viewer.
- **Opt-in (Owner: "the client opting into relay forwarding"):** "When my upload
  is too slow: relay automatically (default) / lower the quality instead". The
  cards always show the path: "Direct", "Relayed: upload limit", "Relayed:
  privacy".
- **Switching never touches encryption** and never rekeys. Make before break:
  start the new path at the base layer, run both for at most 2 s (the replay
  window drops duplicates), then close the old one.

### 4.6 SFU and TURN sizing

Project instance, 1 sharer and 16 viewers:

| Viewers get | SFU egress | Per hour |
| --- | --- | --- |
| all 16: 4K60 motion, 25 Mbit/s | 400 Mbit/s | 180 GB |
| all 16: 4K60 screen content, 12 Mbit/s | 192 Mbit/s | 86 GB |
| 4 at 4K60 motion, 12 at 1080p30 | 148 Mbit/s | 67 GB |
| all 16: 1080p30, 4 Mbit/s | 64 Mbit/s | 29 GB |

**Caps:** 4K60 only when a viewer asks for it, ≤25 Mbit/s per viewer, ≤150
Mbit/s egress per room, ≤70 % of the host's upload. A full budget is shared
max-min by dropping layers; a join is refused only if the base layer doesn't
fit.

**TURN:** a relay-only browser viewer's stream passes through coturn, so its
per-allocation `max-bps` is the tier cap plus 20 % (30 Mbit/s for a 25 Mbit/s
viewer), under a total cap. coturn runs on the SFU host with its own IP. A
relay-only browser sharer also uploads through TURN; native relay-only clients
connect straight to the SFU. When TURN is full, relay-only users get a lower
tier, never a direct path.

## 5. Slices

Phase 1 order: S0 → K1 → K2 → W1 → B1 → T1, with E1 (the long pole) and Q1 in
parallel and D1 and I1 alongside. About 3–3.5 weeks with 5–6 people in parallel
(uncertain).

| ID | Goal | Depends on | Size |
| --- | --- | --- | --- |
| S0 | Spikes: HPKE and RustCrypto on wasm32, browser AV1/SVC/encoded-transform matrix, str0m forwarding of SFrame'd AV1 with DD layer dropping. Results on #139 | — | done |
| S0f | 4K60 measurements: VA-API AV1, key-frame sizes and rate curves per tier, 4K decoding, GPU browsers, spatial layers and simulcast | — | M, 3–5 d |
| K1 | `crates/stream`: link parse, key schedule and messages, HPKE-PSK exchange, rekeying, SFrame (later shared with #135) | S0 | M, 4–6 d |
| K2 | `crates/stream-wasm` for the browser worker | K1 | S |
| Q1 | `crates/stream-quality` (portable, no I/O or clock): request and report codec, tier ladder, the four limits, layer choice, budgets | S0 | M, 4–5 d |
| E1 | Companion SFU: rooms, admission, AV1 forwarding with per-viewer layers, PLI coalescing, key relay, TURN credentials, caps, NixOS unit | S0 | L, 10–13 d |
| W1 | Watch page: decrypt in the transform worker, refuse without support, roster, quality menu | K2, E1 | M, 6–8 d |
| B1 | Share page: video only, AV1 or refuse, measured tiers, grants and rekeys, Stop, monitor warning (#139 J) | W1, E1 | M, 4–5 d |
| D1 | Desktop handoff: consent dialog, S and P, open the share page, post the line, rotate | K1 | S–M, 2–4 d |
| I1 | Infrastructure (§6) | owner hosts and DNS | S–M |
| T1 | End-to-end evidence and documentation (#147, partial) | Phase 1 | M |
| V1 | `nuschl-video-worker --screen-encode` (Linux): portal PipeWire FD only, VA-API AV1 with DMA-BUF import, software fallback, live rate control, ≤2 queued frames | — | M–L, 6–8 d |
| R1 | Driver RTC: UDP in the network process, `PeerPolicy` before gathering, SFrame between V1 and rtc, bandwidth estimate → bitrate | K1, E1, V1 | L, 8–10 d |
| U1 | Native share UX: portal picker, indicator, stop rules, privacy curtain | V1, R1 | M, 4–6 d |
| N1 | Native viewer: decode role with pixel caps, GPUI surface, `CallBinding` | V1, R1, #93 | L |
| X1 | Direct paths (§4.5) | R1, N1, Q1 | L, 8–10 d |
| M1 | End-to-end servers: S over an MLS `Signal` envelope, native viewers only | N1 | M–L |

**Dependencies:**
- K1: RustCrypto crates already in the graph; hpke-rs with libcrux (Owner).
- E1: str0m through a new cargo-deny wrapper `nuschl-sfu`; tokio-tungstenite (in
  the graph).
- V1: VA-API bindings (for example cros-libva) as a new scoped FFI row of the
  video worker; a software AV1 encoder chosen by S0f. rav1e is preferred if its
  realtime screen-content quality suffices, though it has no live bitrate
  setter; libaom or SVT-AV1 would need a new SECURITY row. Encoders have had
  exploited bugs
  ([CVE-2023-5217](https://nvd.nist.gov/vuln/detail/CVE-2023-5217), a heap
  overflow in libvpx's VP8 encoder), so encoding is isolated too.
- N1: rav1d (Owner: "we can use rav1d for software decoding"), after reviewing
  its features, assembly and unsafe code.
- I1: coturn (C, under the infrastructure exception). A Rust TURN server is
  evaluated under #143.

**Required tests:**
- K1: native and WASM known answers agree; HPKE and RFC 9605 vectors; a passive
  observer with S and every recording fails; a required binding or a missing S
  rejects an interceptor; replay, stale epoch and a removed viewer fail;
  fuzzing.
- Q1: the result never exceeds any of the four limits and doesn't flap under
  oscillating input.
- E1 and T1, in a VM with headless Chromium and Firefox, against owned Murmur
  1.4 and 1.5 with a real stock Mumble client: an oversized frame is dropped; 16
  viewers at 4K stay within the memory bounds and get a fair share; a throttled
  leg loses frame rate before resolution; logs contain no secrets or link
  fragments.
- Relay-only, by packet capture: no host or srflx candidates and no peer
  traffic, even with ample bandwidth. Link viewers never receive candidates.
- X1: a throttled sharer upload moves viewers to the relay with no rekey and no
  decryption failure.
- W1, B1: the envelope regression per supported browser; a CPU-limited viewer
  steps down without affecting others.
- V1, N1: a synthetic PipeWire producer in a VM; malformed AV1 corpus; pixel
  caps; sandbox denials; kill and reap.

## 6. Infrastructure

**Hosts and DNS** (owner to provide):
- `stream.nuschl.eu`: nginx serves the static `/watch` and `/share` pages
  (access logs off) and the `/sfu` WebSocket on the same origin; SFU media on
  one UDP port.
- `turn.nuschl.eu`, preferably its own IP: 3478 UDP/TCP, 5349 TLS, a relay port
  range.
- ACME TLS for both, CAA records, HSTS includeSubDomains on the apex.

**TURN:** coturn, with `use-auth-secret` from `LoadCredential`. Peers are
restricted to the SFU's addresses, so it can't become an open relay even if
credentials leak. No TCP relay, no CLI, quotas, minimal logs with short
retention. Credentials are 10-minute REST credentials issued by the SFU only
after link admission, rate-limited per IP and room, and never derived from the
link secret; nuschld issues the same format later (#141, #143).

**Project SFU caps:** 1 publisher and 16 viewers per room, the budgets in §4.6,
a 4 h room lifetime and limited room creations per IP (IPv6 per /64). Operators
self-host with the same NixOS module.

**Same site.** app.nuschl.eu, stream.nuschl.eu and nuschl.eu are one site:
- The passkey rpId is **auth.nuschl.eu (Owner)**, never the apex, so the watch
  page can't request an assertion or PRF output (the identity backup key).
- The watch and share pages run no identity code and deny WebAuthn by
  Permissions-Policy. Only the share page may capture the display.
- No cookies on the stream host; elsewhere only `__Host-` cookies, and no
  reliance on SameSite for CSRF protection.
- The exact headers (a `'self'`-only CSP, COOP/COEP, Permissions-Policy) live
  once, in `nix/stream-module.nix` (#227). The SFU checks Origin.

**What nuschl.eu learns:** IP addresses, user agents, timing, the room id (which
links a stream's viewers), viewer counts, bitrates, requested tiers and TURN
allocations. **Not:** S, K, media, the Mumble server, channels or names, as long
as the page is honest. A malicious stream host could serve JavaScript that
steals S and K from browser users; native viewers never run its code.

## 7. Security and privacy invariants

- **Consent:**
  - Every capture needs a fresh picker (no portal restore token) and a visible
    indicator, plus Nuschl's banner. Sharing is never automatic and never
    restored after a reconnect. It stops on Stop, disconnect, channel change, a
    portal or track end, and app exit.
  - **Phase 1 publisher lease.** Nuschl can't stop a browser tab's capture, so
    it keeps the credential derived from P and its own control connection to the
    SFU, and the page publishes under a lease Nuschl renews (TTL about 10 s).
    Nuschl revokes it on any stop event; a crash lets it expire. The SFU then
    stops forwarding, even if the page keeps capturing.
  - Watching never opens the microphone or camera.
- **Own windows:** window sharing is preselected. Sharing a monitor shows a
  warning and, natively, a privacy curtain over conversations.
- **Encryption:** stock and compatible servers use §3. End-to-end servers use
  MLS only, never links or plugin data. A browser without encoded transforms
  refuses to play or share.
- **Authenticity:** `CallBinding` signs the full transcript, including pk_v, enc
  and the DTLS fingerprints (#139 §5.5). Unknown identities are never pinned
  from a stock server. Results are bound to {connection generation, stream id,
  epoch, signalling id, request}.
- **Isolation:** native codecs run only in `nuschl-video-worker` roles, with
  decoding and encoding in separate roles and hardware video APIs as scoped FFI
  exceptions of that worker. Keys live only in the crypto worker, the SDK driver
  and the browser stream worker. Neither the desktop nor the SFU decodes. Worker
  credit is held until the process is reaped.
- **Relay-only** keeps the existing `privacy/1` and `PeerPolicy` definition:
  configured servers and relays only, no local-interface or STUN candidates, no
  direct peer connections, and peer-supplied endpoints can't override that.
  - Native: no host or srflx candidates; connect only to the configured SFU or
    TURN.
  - Browser (Owner: "A makes sense for relay-only people"): `iceTransportPolicy:
    'relay'` through turn.nuschl.eu, so local addresses are never gathered.
    Without relay-only the browser connects straight to the SFU, which never
    forwards anyone's address.
  - Relay-only users are always relayed, whatever the bandwidth. If the
    permitted path fails, the call ends.
- **Bounds:** §4.2 and §4.6; key messages ≤16 KiB, ≤32 pending key requests, ≥1
  s between epochs, bounded replay windows; quality messages fixed-size, ≤1/s
  per viewer; decoder pixel, memory and CPU caps.

## 8. Decisions

**Made by the owner (2026-11-13):**
- **AV1 only:** "AV1 on hw only, av1 in software as a fallback"; no H.264 ("I
  don't want to touch h.264 with a 10 meter pole") and no VP8. Hardware encoding
  first on every platform (VA-API, Media Foundation, MediaCodec, or the
  browser's encoder where it reports power-efficient), software AV1 otherwise.
  The SFU never transcodes. A browser without AV1 in WebRTC (WebKit today) is
  refused with a clear message until a WASM rav1d path is designed.
- **rav1d** for software decoding, natively and in WASM.
- **4K60 target**: each viewer gets min(request, client capacity, network,
  policy), and relaying is the client's opt-in when its upload doesn't suffice.
- **Relay-only users** are always relayed, in a browser through TURN.
- **libcrux with hpke-rs** for the X-Wing HPKE-PSK exchange on both targets.
- **Key messages ≤16 KiB**, with credentials sent by fingerprint and cached.
- **Domains:** stream.nuschl.eu/watch, turn.nuschl.eu, and auth.nuschl.eu as the
  passkey rpId.

**Open, with the recommended default:**
1. **Phase 1 sharer:** browser capture, with native Linux in parallel; this
   approves #139 J. Native Linux first would add 1–2 weeks.
2. **Topology:** the SFU relays, plus direct paths per decision 11; 16 viewers
   on the project instance, configurable when self-hosted.
3. **Key agreement:** HPKE PSK mode with X-Wing, not a PAKE (§3).
4. **Viewer policy:** anyone with the link by default, approval available; rekey
   on join and leave (batched within 2 s) and every 15 minutes.
5. **Labels:** a new "Link-encrypted" state, never green.
6. **SFrame:** our own RFC 9605 layer on the RustCrypto crates already locked,
   not the `sframe` crate.
7. **Hosting:** coturn limited to SFU peers now, a Rust TURN server later; SFU
   and pages on one host.
8. **End-to-end servers:** S over an MLS `Signal` envelope first, exporter keys
   (#135) later with voice.
9. **cargo-deny:** a str0m wrapper `nuschl-sfu` and HPKE rows for the stream
   exchange.
10. **Tiers and budgets:** §4.1 and §4.6, floor 720p15; sharers offer "Auto, up
    to 4K60" and viewers ask for "Auto".
11. **Direct paths:** as in §4.5, with "Allow direct connections with verified
    people" on by default. Alternatives: off by default, or direct for anyone in
    the channel, which exposes addresses to strangers on stock servers.
12. **Sharers without layers (Firefox today):** one stream for the slowest
    viewer, with a hint to use Chromium or the app for 4K. Alternative: refuse
    Firefox sharers above 1080p.
13. **TURN at 4K for relay-only users:** `max-bps` at the tier cap plus 20 %, a
    lower tier when full. Alternative: cap relay-only browser users at 1080p,
    cheaper but worse for them.

K1, K2, Q1, W1, B1, V1, X1, S0f and the infrastructure need issues; #140–#147
are approved migrated scope.

## 9. Open questions

Not yet verified: libwebrtc's future handling of the AV1 envelope and str0m
forwarding of it; encoded-transform support per engine (Safari has no AV1
encoder, so it can't share in Phase 1); hardware AV1 encoding and 4K60 capture
in browsers; spatial layers and simulcast with the envelope; the §4.1 figures on
real content; VA-API AV1 per GPU vendor and under the production 1 GiB
address-space limit; which software AV1 encoder; native 4K60 decoding; WASM
rav1d on Apple devices; the ScreenCast portal on X11; Rust TURN servers; the
publisher-lease stop deadline; all effort estimates.
