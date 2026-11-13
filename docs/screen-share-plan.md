# Screen sharing plan: Nuschl and stock Mumble servers, with browser watch links

Status: plan, 2026-11-13, against main 8781be09. It folds in the frozen #139 signalling design (`83a5b5a7:docs/legacy-signalling-plan.md`), the frozen #93 plugin-data work (`0e40c7ac`), #35, #135 and #139–#147, the owner's request for link-secret key agreement that only an active server can defeat, and the domain decisions (stream.nuschl.eu/watch, turn.nuschl.eu). Nothing here is implemented; effort figures are rough estimates.

## 0. Bottom line

- **Nothing user-visible exists yet.** What exists is a str0m transport trial with synthetic frames, the #139 design, #93 plugin data (frozen, off main), and reusable post-quantum crypto (HPKE with X-Wing) in `nuschl-identity`. There is no capture, encoder, decoder, SFU, watch page or SFrame code.
- **Topology:** for one sharer and many viewers, use a forward-only **companion SFU at stream.nuschl.eu**. I don't recommend mesh. TURN at turn.nuschl.eu is a reachability fallback, restricted so it can only relay to the SFU. It is not on the critical path.
- **Fastest route to "working":**
  - **Phase 1:** browser-captured sharing launched from Nuschl. Nuschl opens `stream.nuschl.eu/share` in the system browser, and viewers use `stream.nuschl.eu/watch#…`. It works on stock Mumble and on compatible-mode Nuschl servers.
  - **Phase 2, in parallel lanes:** native Linux capture and AV1 encoding in a new isolated video worker; native viewing comes after.
  - **End-to-end-mode Nuschl servers come last.** Until then they refuse sharing (fail closed).
- **Stock-server key agreement (owner's request): use (b), not a PAKE.**
  - Each viewer runs a fresh X-Wing (ML-KEM-768 + X25519) exchange with the sharer, authenticated by a pre-shared key derived from the link secret (HPKE PSK mode, RFC 9180).
  - The SFrame key is random and sent to each viewer over that viewer's own channel.
  - A stock server that only reads the link cannot decrypt the video.
  - To watch, the server has to act: either join as a viewer (it then shows up in the sharer's viewer list), or intercept browser viewers' key exchanges, which needs help from the stream host (SFU or page code). Identity-bound Nuschl viewers detect interception.

## 1. Inventory: what is code, what is design, what is missing

| Item | State | Where |
| --- | --- | --- |
| `nuschl-rtc` | **Code, trial.** Socket-independent str0m 0.25 with the `rust-crypto` feature (DTLS adapter patched, `vendor/str0m-rust-crypto/NUSCHL-PATCH.md`). Opus and VP8 only (AV1 must be added for screen sharing; str0m supports it). Up to 2 audio + 2 video tracks, roles bound to track IDs (`Purpose::ScreenVideo`). One offer/answer only. Host candidate only: no TURN, no ICE restart, no SFrame. Limits: SDP 32 KiB, datagram 2 KiB, video frame 256 KiB. 17 tests (12 transport, 5 signalling). | `crates/rtc/src/lib.rs:1-13, 27-31, 43-49, 157-190, 204, 421, 488-520`; `signaling.rs:7-13, 48-57`; `crates/rtc/tests/` |
| `apps/rtc-probe` | **Code, trial only.** Two loopback UDP endpoints, synthetic Opus and VP8 keyframes, delay/BWE readout. Not an application call path. | `apps/rtc-probe/src/main.rs:1-60` |
| WebRTC stack | **str0m only.** C++/Go stacks are excluded even behind bindings or workers (`docs/architecture.md:200-206`). str0m has no TURN client (sans-IO, its `lib.rs:470-489`). It has an RTP mode for SFU forwarding. Its VP8 keyframe detection reads the first payload byte (`packet/vp8.rs:341, 403`), so SFrame must leave the VP8 header unencrypted. str0m 0.25 also packetizes and depacketizes AV1 and detects AV1 keyframes (`packet/av1.rs`); it only names the Dependency Descriptor extension in SDP, so layer selection from it would be Nuschl code. cargo-deny allows only `nuschl-rtc` to use str0m (`deny.toml:60-63`). | — |
| Capture, encode, decode | **Nothing first-party.** GPUI ships capture code (`vendor/gpui-pre/src/platform/scap_screen_capture.rs`, via zed-scap: X11/xcb, ScreenCaptureKit, windows-capture). It runs inside the desktop shell, so it can't be used: capture APIs belong in a capture worker (`docs/screen-share-audio.md:61-66`), and AGENTS.md forbids extending the GPUI exception by analogy. `rav1e` is in the lockfile only through image→ravif. #35 (decoder isolation) is open. | `Cargo.lock` |
| Media worker isolation | **Code.** One media worker per application: CPAL, opus-pure, Sonora, nnnoiseless, pipewire. Linux bubblewrap profile: audio sockets, `/dev/snd`, no network/home/D-Bus, 1 GiB address space. Windows: AppContainer with the microphone capability. macOS: deny-list Seatbelt. Supervised with generations and revisions. **Not suitable for video:** it holds microphone and audio-socket capabilities. Precedent for a new role: the music-bot codec worker's fixed roles (`SECURITY.md:43`). | `crates/ipc/src/host.rs:773-829`; `SECURITY.md:28-29, 79-84, 228-231`; `crates/client/src/native/media.rs:1-13` |
| Browser media paths | **Audio only, over the WebSocket TLS tunnel:** `crates/client-wasm`, separate Opus WASM worker (`web/app/audio-codec.worker.js`), AudioWorklet. No RTCPeerConnection, no getDisplayMedia. Web CSP allows only the microphone (`nix/web-module.nix:64-83`). Video and screen sharing are listed as unsupported (`docs/web.md:376-385`). | — |
| `docs/screen-share-audio.md` | **Design only**, for #146. Separate capture worker; the portal grants video, not audio (`:128-136`); exclusion barrier; SFrame keys never in capture or codec workers (`:251-256`); relay-only rules (`:246-250`). | — |
| PluginDataTransmission (#93) | **Code, frozen `0e40c7ac`, not on main.** 13 files, +2,704 lines: `crates/protocol/src/plugin.rs`, session `connection.rs` (+900), `client/plugin.rs`, `client.rs`, `native.rs`, server `handlers.rs`. Opt-in, bounded, never on end-to-end servers, reserved `nuschl/` dataID namespace. Loopback interop passed with Murmur 1.5.939 and nuschld. No browser support, no UI. **Collides with Codex's `client.rs` work.** Not needed for Phase 1. | — |
| Capability negotiation | **Code.** ALPN `nuschl/1` plus a `NuschlHello` capability list, frozen per connection, sticky policy (`docs/extension.md:24-98`). `privacy/1` relay-only (`:613-641`) and `PeerPolicy` (`crates/core/src/network.rs:15-26`). **No `rtc/1` yet** (#141; #139 §6.2 proposes 0x4EA0–0x4EAF). | — |
| SFrame / MLS exporter (#135) | **Design plus a spike.** `docs/e2ee.md:1071-1088, 1141, 1182`; `spikes/mls/src/lib.rs:409-414` exports "SFrame 1.0 Base Key". End-to-end servers block voice (`SECURITY.md:198-203`). No SFrame implementation exists. | — |
| Reusable crypto | **Code.** HPKE with X-Wing in PSK and base modes (`crates/identity/src/hpke.rs:1-66`). Device linking already uses SPAKE2 plus X-Wing HPKE-PSK (`link.rs:1-35`). X-Wing comes from `libcrux-kem` `XWingKemDraft06` (`crypto.rs:319-345`). `spake2` 0.4 is also magic-wormhole's PAKE. **cargo-deny confines `nuschl-identity`** to kt, e2ee, nuschld and the crypto worker (`deny.toml:173-178`), so reuse the pattern, not the crate. RustCrypto `x-wing` 0.1.1 and `ml-kem` 0.3.4 are already in the lockfile (via hpke-rs-rust-crypto). | — |
| #139 signalling design | **Design, frozen `83a5b5a7`.** One typed signalling format over three transports, `CallBinding`, plugin framing, SFU/TURN rules, slices A–J, decisions D1–D14. No code. | — |
| Web host | **Code.** Share links (`docs/web.md:204-266`), `PROJECT_WEB_HOST = nuschl.eu` (`crates/core/src/share.rs:27`). Precedent for posting a structured offer as chat text: `apps/desktop/src/transfer.rs:530-553`. | — |

## 2. MVP definition (recommended)

**Phase 1, the first shippable release:**
- **Who shares:** a Nuschl desktop user clicks "Share screen" in a channel. Nuschl generates the link secret S and a separate publisher secret P, opens `https://stream.nuschl.eu/share#v=1&k=S&p=P` in the system browser, and posts the legacy line containing `https://stream.nuschl.eu/watch#v=1&k=S`. P is never posted.
- **Capture:** the browser's own picker and recording indicator, via getDisplayMedia. Video only, at most 1080p and 15 fps, up to 2.5 Mbit/s. **AV1 only** (owner, 2026-11-13): hardware encoding wherever the device has it, software AV1 as the fallback, no VP8 and no H.264 (§7.6). Screen-content tuning and an L1T3 scalability mode where the browser offers them.
- **Viewers:** anyone with the link, in a browser. Nuschl users get the same link (no in-app player yet).
- **Media path:** browser → companion SFU → browsers. SFrame with a random key per epoch, delivered per viewer by the X-Wing PSK exchange.
- **Servers:** stock Murmur 1.x and compatible-mode nuschld (both use the chat-link path). End-to-end-mode nuschld refuses with "not available yet" (fail closed; #139 §2).
- **Viewer cap:** 1 publisher and 16 viewers on the project instance.

**Phase 1 is an intermediate milestone, not the end state.** Native capture and native viewing remain the requested result; browser-launched capture is only the owner-approved step towards it.

**Phase 2, in parallel lanes:**
- Native Linux sharer: portal picker, `nuschl-video-worker` with VA-API AV1 hardware encoding and a software AV1 fallback (§7.6), the SDK driver's str0m connection to the SFU.
- Native in-app viewer: isolated decoder (#35), GPUI rendering (#145), identity-bound key exchange (#139 B), plugin-data stream cards (#93 + #139 C).
- End-to-end servers (#139 I0/I, #135).

**Later:** Windows/macOS capture, audio (#146/#144), scalable video (#140), nuschld's own SFU and `rtc/1` (#142/#141), long-call validation (#147).

**Why an SFU:** a sharer acting as the hub uploads N copies, and a browser re-encodes once per peer connection. The browser viewers also need a rendezvous at nuschl.eu anyway. str0m's frame and RTP-mode forwarding is the shared core that #142 reuses later.

## 3. Stock-server key agreement (owner's PAKE request)

**Key schedule** (replaces #139 §5.2's `sframe_base(g)`, which was derived from S):
- S is 32 bytes from the OS RNG. PRK = HKDF-Extract("nuschl/stream/v2", S).
- From PRK: `stream_id` (16 B, the SFU room), `admit_key` (SFU challenge), `mac_key`, `psk`, `signal_key`.

**Joining:**
- The viewer makes a **fresh random** X-Wing keypair (sk_v, pk_v) for every join, never reused, and sends `KeyRequest{version, suite, stream_id, viewer_id, nonce_v, not_after, pk_v, [binding]}`, with an HMAC under `mac_key` so non-holders are dropped cheaply.
- **Transcript.** Both sides hash one canonical encoding of the protocol label and version, the role labels (`viewer→sharer`, `sharer→viewer`), the fixed suite IDs (KEM, KDF, AEAD), stream_id, viewer_id, both nonces, not_after and pk_v. HPKE `info` is that hash, and psk/psk_id come from S.
- The sharer replies `KeyGrant{nonce_s, enc, ciphertext, confirm_s}`. The ciphertext is the HPKE PSK-mode seal to pk_v carrying {epoch, K_epoch, kid, not_after}. `confirm_s` is a MAC over the transcript under a key **exported from the HPKE context**. The viewer answers `KeyAck{confirm_v}` under a second exported key with a different label.
- **Rejected:**
  - a replayed request (nonce cache bounded per room and epoch, plus not_after);
  - a reflected message (role labels differ);
  - a suite other than the one fixed by the link version (downgrade);
  - an expired request;
  - a missing or wrong confirmation.

  All of these fail closed, with no classical-only fallback.
- The HPKE context then exports a per-viewer channel key. Rekeys are sealed with that key plus a counter.
- **The passive-holder claim holds only under these conditions:** a party with S and every recording still cannot decrypt, because every sk_v is fresh and random and every K_epoch is independent random (never derived from S or from an earlier K). If any of those were derived from S, the claim would not hold.

**Sizes and authentication profile (to be frozen with S0a's exact numbers):**
- **Key-message cap: 16 KiB.** The owner first raised it to 8 KiB, then (2026-11-13: "larger limit then") further, so the hybrid signing certificate travels inline as well. It is a Nuschl-chosen bound on the SFU WebSocket, not an external constraint. S0a measured, in one binary frame: binding plus hybrid certificate 8,908 B, KeyRequest with it about 10.3 KB; without the certificate the worst case is 5,124 B.
- **Both profiles fit in one message, certificate included.** The default profile is an Ed25519 leaf binding (about 334 B) plus pk_v (1,216 B). The hybrid profile is a hybrid binding (3,471 B) plus its signing certificate plus pk_v (1,216 B) or enc (1,120 B). No multipart is needed, and no separate certificate fetch.
- **Certificates by fingerprint (owner, 2026-11-13: "clients can cache keys and we only send a fingerprint").** A binding names the signing certificate by its fingerprint (SHA-256 of the exact certificate bytes), not inline.
  - **Cache hit:** the receiver already caches it, keyed by identity and signing generation, so only the fingerprint travels.
  - **Cache miss:** the receiver answers `NeedCertificate{fingerprint}`, and the sender then sends the certificate inline (within the 16 KiB cap), or the receiver fetches it from key transparency. A certificate whose hash doesn't match the fingerprint is rejected.
  - **Bounds:** the cache is bounded by entries and bytes, with eviction, and is per device.
  - **Revocation:** revocation and key-transparency status are still checked for cached certificates.
  - **Privacy:** bindings, fingerprints and certificates travel only inside the S-keyed signalling encryption (`signal_key`), so the SFU never learns who is watching.
- **No silent downgrade:** a policy that requires the hybrid binding never falls back to classical; it refuses.
- Configuration: the same suite as `crates/identity/src/hpke.rs:35-43` (X-Wing / HKDF-SHA256 / ChaCha20-Poly1305).

**Fan-out and rekeying:**
- The key exchange travels over the SFU WebSocket as opaque blobs of at most 16 KiB. Plugin data is a poor fit: pk_v is 1,216 B and the ciphertext 1,120 B, which is 2–3 chunks each under Murmur's 4 messages/s budget.
- **Join, rekey first.** A newcomer never receives an epoch that carried media before they joined, because otherwise recorded pre-join media would decrypt. On a join the sharer creates K_{e+1}, sends it to the existing viewers over their channels, then grants K_{e+1} to the newcomer, then forces a keyframe under K_{e+1}. Batching joins (at most 2 s) delays the newcomer; it never hands out the old epoch.
- **Leave exposure is bounded separately.** On a leave or kick the sharer rekeys, batched within 2 s. A departed viewer can decrypt at most until that rekey takes effect plus the previous-epoch grace (about 2 s). Every 15 minutes there is a rekey regardless.
- Receivers keep the previous epoch for about 2 s.
- A kicked viewer can rejoin via the link until "Rotate link" (new S, new room).
- The SFU sees only HPKE and SFrame ciphertext.

**SFrame profile:** RFC 9605 AES_128_GCM_SHA256_128, with an unmodified cipher core. Encrypt the whole AV1 frame if the packetizers allow it. If they need clear bytes, a versioned profile adapter carries them as SFrame metadata, which RFC 9605 already authenticates as additional data. That needs no cipher fork, but the clear bytes leak what they contain (for example frame type and dimensions). A codec bitstream prefix is not the same as an RTP payload descriptor; the adapter has to name which one it keeps clear. AV1 (the only codec, §7.6): whether browsers' AV1 packetizers accept an encrypted payload (the OBU headers are inside it), or need a clear prefix as VP8 would, or the generic SFrame packetization, is spike S0b's question. An SFU can pick AV1 layers from the Dependency Descriptor header extension without reading the payload; its integrity then rests on SRTP to the SFU, not on SFrame.

**Options evaluated:**
- **(a) SPAKE2** is reusable (spake2 0.4; magic-wormhole 0.8.1; `link.rs`) but classical: a quantum-capable recorder who later learns S breaks it.
- **(b) HPKE PSK mode with X-Wing — recommended.** Post-quantum against passive observers, and no man-in-the-middle without S. It is the confidentiality half of `link.rs` without the human-code half.
- **(c) Post-quantum PAKEs** (CAKE/OCAKE, CPace+OQUAKE drafts) have no reviewed Rust implementation and are immature.
- **When a PAKE proper matters:** only when the secret is short or typed by a human, because the transcript then allows offline guessing. With 256 bits of S that can't happen.
- **Passive password holders:** even with a PAKE, a passive party that knows the password can't derive the session key, because it lacks the ephemeral secret (SPAKE2: g^xy is still a Diffie–Hellman problem).

**What the SFU knows, and admission:**
- **What the SFU holds:** stream_id (the room), `admit_key` (to check viewer admission) and a publisher verifier derived from P.
- **What it never holds:** S, `mac_key`, `psk`, `signal_key` or any K_epoch.
- **Reserve the room before posting.** The share page reserves the room atomically at the SFU (stream_id, admit_key, publisher verifier) *before* Nuschl posts the watch link. If the room already exists, the share fails closed ("link already in use"). This removes the room-squatting window.
- **Admission is viewer-only.** A viewer proves admit_key by an HMAC over an SFU-issued nonce, and admission grants only the viewer role. The publisher role needs the P verifier. Admission can never replace the publisher; a new publisher needs a new link (Rotate).

**Who can do what:**
- Passive stock server with S: cannot decrypt (needs sk_v).
- The SFU without S: can neither forge a MAC nor open a grant.
- Active stock server alone: can only join as an extra viewer, which appears in the roster, or swap the posted link and relay the stream, which also needs it to join.
- Stock server colluding with the SFU or page code: can intercept browser viewers undetected.
- Identity-bound Nuschl viewers: any substitution of pk_v breaks the `CallBinding` transcript, giving "Identity check failed".
- A leaked link plus recorded SFU traffic no longer decrypts later. This fixes #139 §3.1's retention problem.
- Browser viewers have no identity key, so they are shown as "unverified viewer".

**Where it runs:**
- One portable crate, `crates/stream`, compiled natively and to WASM, so no JS crypto library is added.
- Browser: one dedicated worker that is also the encoded-transform worker, so the main thread never holds K.
- **Native (Phase 2): the crypto worker.** It already holds libcrux and the identity HPKE stack (hpke-rs with hpke-rs-libcrux) under the existing `SECURITY.md` exception. It performs the HPKE PSK exchange and the exports, and hands only K_epoch and the per-viewer channel keys to the SDK driver, the network process, which does SFrame.
- Never in capture, encode or decode workers.
- The crypto worker also signs and verifies `CallBinding`.
- **Provider (owner, 2026-11-13: "libcrux is verified so we should probably prefer it to RustCrypto"):** libcrux with hpke-rs, the same code identity uses, on both targets. There is no first-party HPKE implementation. The browser stream worker uses libcrux's portable wasm32 build; S0a showed there are no SIMD intrinsics there and that it matches native byte for byte. That needs a new, narrow `SECURITY.md`/cargo-deny row for the browser stream worker only, written by Root. Formal verification covers libcrux's ML-KEM and its HACL*-derived X25519 cores, not every wrapper: this year's advisories were in libcrux-kem and libcrux-hmac-drbg, so versions stay pinned to the patched ones.

**Proposed labels:**
- Sharer card: "Encrypted against passive servers. Anyone with the link can watch; everyone watching is listed here. A server that saw the link could join, or, with the stream host's help, intercept browser viewers." Short form "Link-encrypted". **Never the green lock.**
- End-to-end-server streams turn green only under `docs/e2ee.md` §7.4.

**Supersedes in #139:** the §0 row "ephemeral keys + PSK: refuted", §5.2 `sframe_base`, and the §3.1/§8 "Not end-to-end encrypted" labels. Record this as a new descendant revision of the plan; don't rewrite frozen `83a5b5a7`.

## 4. Critical path

Phase 1 order: S0 → K1 → K2 → W1 → B1 → T1, with E1 (the long pole) in parallel and D1 and I1 alongside. Rough estimate for Phase 1 with 5–6 lanes: about 2.5–3 weeks (uncertain).

| ID | Slice | Issue | Depends on | Files / crates | New dependencies (safety / licence) | Runs in | Tests | Size | Lane |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| S0 | Spikes, run in parallel: (a) wasm32 build of the HPKE/X-Wing backend plus RustCrypto `hkdf`/`hmac`/`sha2`/`aes-gcm`; (b) browser support matrix, sender and receiver: AV1 WebRTC encode (hardware or software, via `mediaCapabilities.encodingInfo` `powerEfficient`) and decode, AV1 `scalabilityMode` (L1T3, L2T3), screen-content hints, encoded-transform SFrame, and a WASM AV1 decoder (rav1d) for browsers without native AV1 decode; (c) str0m RTP-mode forwarding of SFrame'd AV1, Dependency Descriptor parsing for AV1 layer dropping, PLI behaviour | new (under #139/#147) | — | owned scratch only | — | — | results recorded | S, 1–2 d | A/B/C |
| K1 | `nuschl-stream`: link v1 strict parse, key schedule, request/grant/update codec, HPKE-PSK exchange, per-viewer channel, rekey manager, SFrame with replay window | new (child of #139; SFrame later shared with #135) | S0a | `crates/stream/*`, test vectors, fuzz targets | RustCrypto (in graph, MIT/Apache; same `aes` as the OCB2 exception, `SECURITY.md:35`). KEM/HPKE: hpke-rs + hpke-rs-libcrux, the same code as identity (owner decision). Native runs in the crypto worker under the existing row; the browser stream worker uses portable wasm32 under a new narrow row | browser worker; crypto worker (native) | native = WASM known answers; HPKE PSK vectors; **passive observer** (S + every recording, all S-derived keys fail); **MITM** (anonymous viewer intercepted, documented; with binding required, rejected; without S, rejected); replay; stale epoch; removed viewer can't decrypt; fuzz | M, 4–6 d | A |
| K2 | `crates/stream-wasm` | new | K1 | wasm-bindgen (pinned, in graph) | — | dedicated worker | Node and headless-browser known-answer equality | S | A |
| E1 | Companion SFU: rooms in memory, admission HMAC, publisher verifier from P, browser SDP under bounds, AV1 RTP forwarding (temporal-layer dropping per viewer from the Dependency Descriptor), PLI coalescing, opaque key relay, TURN REST credentials, caps, NixOS unit, Origin check | #142 core + #139 E | S0c | `crates/sfu`, `apps/sfu`, `nix/stream-module.nix` | str0m (**new wrapper `nuschl-sfu`**, `deny.toml:60-63`); tokio-tungstenite (in graph); HMAC-SHA1 for coturn REST credentials | systemd-hardened service; no keys, no decoding | VM with headless Chromium/Firefox; fuzz the WebSocket protocol and SDP; limits; log scan | L, 8–10 d | B |
| W1 | Watch page: strict fragment, `replaceState`, `/sfu`, encoded-transform decrypt, fail closed without support, roster, accessibility | #139 F (new issue) | K2, E1, S0b | `web/stream/{watch.html, watch.js, stream-worker.js}`, `packages.stream-site` | none (plain JS) | browser | `tests/web_stream.mjs`; CSP; no fragment in logs | M, 5–7 d | C |
| B1 | Share page: getDisplayMedia video only, `selfBrowserSurface:'exclude'`, `systemAudio:'exclude'`, AV1 via `setCodecPreferences` (refuse with a clear message where the browser cannot encode AV1), maxBitrate, L1T3, SFrame encrypt, grants and rekeys, viewer roster, Stop, warning when a whole monitor is shared | new (#139 J, **needs owner approval**) | W1, E1 | `web/stream/share.*` | — | browser (its own encoder and sandbox) | headless Chromium with fake capture; rekey on leave | M, 3–4 d | C |
| D1 | Desktop handoff: consent dialog, S/P from OS RNG held in memory, open share page, post the HTML or text line as `transfer.rs` does, "ended" line, rotate | #145 UI / #139 G | K1 link format | `crates/core/src/stream.rs` (new), `apps/desktop/src/stream.rs` (new), one menu hunk | — | desktop shell | units; private GNOME screenshots; AT-SPI | S–M, 2–4 d | E |
| I1 | Infrastructure (§5) | #143 + new infra issue | owner hosts and DNS | `nix/stream-module.nix`, coturn config | coturn (C; infrastructure exception, D7) | servers | TURN relays only to the SFU; relay-only browser run | S–M | F |
| T1 | End-to-end evidence and documentation | #147 (partial) | Phase 1 | `tests/stream_vm.nix`; docs (Root merges) | — | VM | owned Murmur 1.4/1.5 plus a real stock Mumble client showing the line; two browsers; passive and MITM; rotate/kick; secret-free logs | M | F + all |
| V1 | `nuschl-video-worker --screen-encode` (Linux): only the inherited portal PipeWire FD; SHM buffers; AV1 hardware encoding through VA-API (render node only; PipeWire DMA-BUF imported without a CPU copy where possible), software AV1 fallback with BGRx→I420 and downscaling; realtime screen-content mode (palette, intra block copy where the encoder has them), L1T3; at most 2 queued frames | #145 + #140 (single layer) | host.rs hunk via Root | `apps/video-worker`, `crates/ipc/src/video.rs` (new), `crates/ipc/src/host.rs` (`video_sandbox_command`) | pipewire 0.10.1 (existing FFI row); **VA-API** bindings (e.g. cros-libva, BSD-3; a new scoped FFI row) plus the system's libva and vendor driver; a **software AV1 encoder** chosen by spike: rav1e (Rust, already in the lockfile via ravif; preferred if realtime screen content is good enough), libaom or SVT-AV1 (C, new SECURITY row and deny wrapper). Encoders have had exploited bugs (libvpx VP8 encoder, CVE-2023-5217, from memory), so encoding is isolated too | new worker: no audio sockets, D-Bus, network or home; fails closed | private PipeWire with a synthetic producer node in a VM; round-trip decode; sandbox denials; kill and reap | M–L, 6–8 d | D |
| R1 | Driver RTC: UDP owned by the network process, `PeerPolicy` before gathering, WSS to the SFU, SFrame between V1 and rtc, BWE → bitrate, PLI → keyframe, results bound to generation | #143 + #145 | K1, E1, V1 | `crates/client/src/native/rtc.rs` (new), `native.rs` hunk, `crates/rtc` (send-only screen mode, ICE-TCP) | — | SDK driver | loopback SFU; packet capture shows no host/srflx candidates in relay-only mode | L, 8–10 d | **wait for Codex** |
| U1 | Native share UX: ashpd `screencast` (window preselected, `multiple=false`, no persist token), worker spawn with the FD, indicator, stop on disconnect / channel change / portal Closed / exit, privacy curtain | #145 | V1, R1 | `apps/desktop/src/stream.rs`, `Cargo.toml` feature (Root) | ashpd feature (zbus 5 advisories already accepted, `deny.toml:18-20`) | desktop shell (portal broker only) | private headless GNOME with portal; AT-SPI | M, 4–6 d | E |
| N1 | Native viewer: `--decode` role (strict profile, pixel caps), GPUI surface, `CallBinding`, plugin announce | #35, #145, #139 B/C, #93 | V1, R1, #93 merged | video worker, desktop, `crates/identity/src/call.rs`, `apps/crypto-worker/src/call.rs` | AV1 decoder: **rav1d** (owner, 2026-11-13: "we can use rav1d for software decoding"; Rust port of dav1d; review enabled features, asm and unsafe); hardware decoding only as a later opt-in | decode worker (separate role from encode: no capture FD, no GPU encode access); crypto worker | malformed AV1 corpus; pixel caps; identity mismatch | L | later |
| M1 | End-to-end servers: MLS ephemeral `Signal` with S over MLS (or exporter keys), native viewers only | #139 I0/I, #135 | N1 | e2ee envelope, crypto-worker `mls.rs`, server `mls.rs` | — | crypto worker | e2ee flow tests | M–L | later |

## 5. Infrastructure the owner must provide or approve

**Hosts and DNS:**
- `stream.nuschl.eu` (A/AAAA): nginx serves `/watch` and `/share` (static, access logs off, no other content) and the `/sfu` WebSocket (same origin, so CSP stays `'self'`).
- SFU media: UDP 443 if nginx QUIC is off, otherwise one fixed UDP port.
- `turn.nuschl.eu` (A/AAAA, preferably its own IP): 3478 UDP/TCP, 5349 TLS (443 TLS if a dedicated IP is available), relay range for example 49152–49999/udp.
- TLS for both via ACME; coturn needs read access to its certificate.
- CAA records; HSTS includeSubDomains on the apex.

**TURN:**
- coturn for now, using `use-auth-secret` with the secret delivered through `LoadCredential`.
- Peer addresses restricted to the SFU's IPs only (`allowed-peer-ip`), with private, loopback, link-local, CGNAT and multicast ranges denied. That makes it impossible to use as an open relay even if credentials leak.
- `no-tcp-relay`, `no-cli`, per-user and total quotas, `max-bps` about 4 Mbit/s, minimal logging with short retention.
- Credentials are 10-minute REST/HMAC credentials, **issued by the SFU only after link admission**, rate-limited per IP and room.
- Later: nuschld issues the same format for Nuschl servers (#141/#143).
- Credentials derived from the link secret, or an unauthenticated public TURN: rejected.
- A Rust TURN server (webrtc-rs `turn`, librice) is to be evaluated under #143; maturity and licences unverified.

**Project SFU caps:** 1 publisher and 16 viewers per room, 2.5 Mbit/s, 4 h room lifetime, room creations per IP (IPv6 by /64), a total egress budget. A refused join fails only that join. Operators can self-host through the same NixOS module.

**Same-site implications** (app.nuschl.eu, stream.nuschl.eu and nuschl.eu are separate origins but the same site):
- Passkey rpId: **auth.nuschl.eu** (owner, 2026-11-13), the dedicated identity-recovery origin; never the apex. With rpId `nuschl.eu`, the watch page could request an assertion or PRF output (the identity backup key).
- The watch and share pages run no identity code and send `Permissions-Policy: publickey-credentials-get=(), publickey-credentials-create=()`.
- No cookies on the stream host; elsewhere only `__Host-` cookies, never `Domain=nuschl.eu`. The app must not rely on SameSite for CSRF protection.
- Headers on the watch and share pages:
  - `Origin-Agent-Cluster: ?1`
  - `COOP: same-origin`, `COEP: require-corp`, `CORP: same-origin`
  - CSP: `default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; worker-src 'self'; connect-src 'self'; img-src 'self' blob:; style-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'; require-trusted-types-for 'script'`
  - `Permissions-Policy`: the watch page denies display-capture, camera and microphone; the share page allows `display-capture=(self)` only.
  - `no-referrer`, `nosniff`.
- The SFU checks the Origin header.
- Origin-Agent-Cluster and COOP/COEP only *request* origin-keyed process isolation; Chromium isolates per site by default. A **separate registrable domain** for viewing untrusted media is optional extra hardening.

**What nuschl.eu learns:** IP addresses, user agents and TLS fingerprints, timing, the room id (which links all viewers of one stream), viewer counts, bitrates and packet timing, and TURN allocations.

**What it doesn't learn:** S or K (from an honest page), media content, sealed titles, the Mumble server, channels or names. A malicious or compromised stream host can serve JavaScript that steals S and K from browser users; native viewers never run its code.

## 6. Security and privacy invariants

- **Consent:**
  - Capture needs a fresh picker every time (browser picker, or portal with no restore token) and a visible indicator (the browser's or the portal's, plus Nuschl's banner, voice bar and window title).
  - Never automatic; never restored after a reconnect.
  - Stops on Stop, disconnect, channel change, portal or track ended, and app exit.
  - **Phase 1 external browser capture.** Nuschl cannot stop a browser tab's capture directly, so sharing P with the page alone is not enough.
    - **Revocable delegation.** Nuschl keeps the owner credential (derived from P) and holds its own control connection to the SFU. The page publishes with a short-lived, revocable publisher lease that Nuschl issues and renews: TTL about 10 s, renewed about every 3 s.
    - **Ending the share.** On native Stop, disconnect, channel change or exit, Nuschl revokes the lease. If Nuschl crashes, the lease simply expires. The SFU then stops forwarding within the lease deadline and tells the page, which calls `track.stop()` locally and must stop within a fixed deadline. Forwarding stops at the SFU even if the page doesn't.
  - Worker credit is held until the process has actually been reaped.
  - Joining or watching never opens the microphone or camera.
- **Own windows:** window sharing is preselected. Sharing a monitor shows a warning (`displaySurface === 'monitor'` in the browser, the portal's `source_type` natively) and, natively, a privacy curtain over conversations. Linux portals have no exclude-window API. Windows' exclude-from-capture flag would need new unsafe code, so it is not planned.
- **Encryption:**
  - Stock and compatible servers: link-encrypted as in §3, honestly labelled.
  - End-to-end servers: MLS only, never links or plugin data.
  - No plaintext fallback: a browser without encoded transforms refuses to play or share.
- **Authenticity:**
  - `CallBinding` signs the full transcript, including pk_v, enc and DTLS fingerprints (#139 §5.5).
  - Unknown identities are never pinned from a stock server.
  - Results are bound to {connection generation, stream id, epoch, signalling id, request}.
- **Isolation:**
  - Native codecs only in `nuschl-video-worker` roles.
  - Keys only in the SDK driver or browser stream worker.
  - The desktop never decodes.
  - The SFU never decodes and holds no keys.
- **Relay-only** uses the owner's existing definition (`docs/extension.md` `privacy/1`; `crates/core/src/network.rs` `PeerPolicy`), neither strengthened nor weakened:
  - **What it allows and forbids:** configured servers and relays may be contacted. Peer discovery, local-interface/STUN candidate gathering and direct peer connections are not allowed, and peer-supplied endpoints cannot override this. The configured server necessarily sees the source IP.
  - **Native:** with `relay_only`, the SDK driver gathers no host or srflx candidates and connects only to the configured SFU (or TURN).
  - **Browser (owner, 2026-11-13: "A makes sense for relay-only people"):** with relay-only on, the page uses `iceTransportPolicy: 'relay'` through turn.nuschl.eu, so local addresses are never gathered and a VPN user's non-VPN interface is never tried. Without relay-only, the browser connects straight to the SFU. The page signals no local candidates, and the SFU never forwards anyone's address. Sharers and viewers never connect to each other in either case.
  - **No fallback:** if the permitted path fails, the call ends.
- **Bounds:**
  - Video: at most 1920×1080 and 15 fps by default (30 opt-in), 2.5 Mbit/s, frames ≤256 KiB (a large keyframe may need an intra-bitrate cap or 512 KiB; to verify), ≤2 queued frames.
  - Key messages ≤16 KiB, ≤32 pending key requests, at least 1 s between epochs, bounded replay windows.
  - Viewer and SFU caps as in §5.
  - Decoder pixel, memory and CPU caps.

## 7. Owner decisions, with recommended defaults

1. **Phase 1 sharer:** browser capture launched from Nuschl, with native Linux in parallel. Approves #139 J. The alternative, native Linux first, adds about 1–2 weeks.
2. **Topology:** companion SFU at stream.nuschl.eu, not mesh. **16 viewers** on the project instance; configurable when self-hosted.
3. **Key agreement:** option (b), HPKE PSK mode with X-Wing. The KEM backend is chosen by spike S0a.
4. **Viewer policy and rekeying:** Link policy by default (Approve available); rekey on leave and join (batched within 2 s) and every 15 minutes.
5. **Labels:** a new "Link-encrypted" state with the §3 wording, never green.
6. **Codec (owner, 2026-11-13): AV1 only.** "AV1 on hw only, av1 in software as a fallback"; no H.264 ("I don't want to touch h.264 with a 10 meter pole") and no VP8. Encoding is hardware-first on every platform: VA-API on Linux, Media Foundation or D3D12 video encoding on Windows, MediaCodec on Android, the browser's encoder where `encodingInfo` reports it power-efficient; software AV1 otherwise. Apple has no AV1 hardware encoder, so macOS and iOS sharers encode AV1 in software. The SFU never transcodes. A browser that cannot decode AV1 natively gets a WASM AV1 decoder (rav1d) in the watch page's worker if S0b shows it fast enough; otherwise it is refused with a clear message, never downgraded. AV1 brings screen-content tools (palette, intra block copy), better compression and spatial plus temporal scalability with the Dependency Descriptor (#140). Software decoding uses rav1d (owner decision), natively and as the WASM fallback. Natively, decoding and encoding run in separate isolated roles; hardware APIs (VA-API, Media Foundation, VideoToolbox decode, MediaCodec) are scoped FFI exceptions of the video worker only.
7. **Hosting:** coturn at turn.nuschl.eu limited to SFU peers, now; a Rust TURN server later. SFU and pages on the same host. Minimal logs and a stated retention period.
8. **Domains:** stream.nuschl.eu/watch (decided); the passkey rpId is auth.nuschl.eu (owner, 2026-11-13); a separate viewing domain is optional.
9. **End-to-end servers:** S delivered over an MLS `Signal` envelope (fast), with exporter keys (#135) later for voice.
10. **cargo-deny:** add str0m wrapper `nuschl-sfu`, a KEM/HPKE wrapper for `nuschl-stream`, and a vpx wrapper for the video worker (Root reviews).
11. **Compatible-mode nuschld in Phase 1:** use the interim link mode, until `rtc/1` (#141) brings a server-made line.

The coordinator should file new issues for K1, W1, B1, V1 and the infrastructure, with `source:user-request` quoting today's requests. #140–#147 are approved migrated scope.

## 8. Lanes for the next 1–2 days (no conflicts with current work)

- **L1, stream crypto:** S0a, then K1, in new `crates/stream`.
- **L2, SFU:** S0c, then the E1 skeleton, in new `crates/sfu` and `apps/sfu`.
- **L3, web stream:** S0b, then the W1 skeleton, in new `web/stream/`. Not `web/app`, `web/site` or `crates/client-wasm`.
- **L4, native capture/worker/codec review: owned by the coordinator's own lane.** It covers VA-API AV1 encoding on a VCN 4 GPU (bindings, DMA-BUF import, rate control, temporal layers), software AV1 encoder comparison (rav1e, libaom, SVT-AV1), rav1d review, synthetic PipeWire fixture design, a draft of new `crates/ipc/src/video.rs`. Dry runs only; the `host.rs` hunk is proposed to Root.
- **L5, desktop share:** D1 in new `crates/core/src/stream.rs` and `apps/desktop/src/stream.rs`. The menu hunk needs agreement with the desktop-next lane.
- **L6, infrastructure and docs:** `nix/stream-module.nix`, coturn config, the owner's DNS/TLS checklist, the #139 plan amendment as a new revision, draft SECURITY/architecture/web rows for Root.
- **Root only:** `Cargo.toml` members, `Cargo.lock`, crate2nix, `deny.toml`, `SECURITY.md`.
- **On hold:**
  - R1 and the #93 merge (they touch `client.rs`, `native.rs` and session `connection.rs` while Codex works on #108/#118).
  - Anything in `apps/server` (#115).
  - `crates/client-wasm` `session.rs`/`browser.rs` (#13).
  - `web/app` (browser broker staging).

**Unverified, needs checking:** browser encoded-transform support (Chromium's RTCRtpScriptTransform vs `createEncodedStreams`; Safari); libcrux on wasm32 and X-Wing draft parity; str0m forwarding of SFrame'd AV1; browser AV1 encode/decode and SVC support per engine (Safari is expected to have no AV1 encoder, so Safari cannot share in Phase 1); WASM rav1d decode speed; AV1 packetization of encrypted payloads; VA-API AV1 encode per GPU vendor and which binding crate; which software AV1 encoder; ScreenCast portal on X11 sessions and SHM fallback; Rust TURN servers; the publisher-lease protocol and its stop deadline; all effort estimates.
