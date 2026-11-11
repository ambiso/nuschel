# Nuschel TODO

Current focus: **the Nuschel desktop client against stock Mumble servers.**
The Nuschel server, WebRTC/video and MLS are deferred until the client is a
credible everyday Mumble replacement. They are listed under [Later](#later).

Ground rules (see `SECURITY.md`, `tests/README.md`, `docs/architecture.md`):

- Tests start their own servers on loopback or private NixOS VMs only.
- Memory-safe implementation first; otherwise an isolated, capability-limited worker.
  No C++ or Go WebRTC stacks.
- `nix build/flake check --cores 2`; commit incrementally with `jj`.

---

## Now: client compatibility with stock Mumble servers

### 1. UDP voice transport

Voice currently only uses the TLS TCP tunnel; `CryptSetup` is ignored
(`crates/session/src/connection.rs`, message type 15).

- [x] Handle `CryptSetup` (key, client/server nonces) and keep crypto state in the
      session, never in the media worker.
- [x] Implement Mumble's OCB2-AES128 variant exactly as stock Mumble does (not a
      generic modern OCB), memory-safe, with interop test vectors from stock Mumble.
      Done: OCB2 draft vectors plus live interop with stock 1.3/1.4/1.5 servers.
- [x] UDP socket with both packet formats: legacy (pre-1.5) and protobuf (1.5+),
      chosen by the negotiated server version.
- [x] UDP ping, connectivity detection, automatic fallback to the TCP tunnel and
      back to UDP when it recovers.
- [x] Nonce resync on decrypt failures (request/resend `CryptSetup` nonces).
- [x] Track good/late/lost/resync counters for the connection-info UI.

### 2. Playback quality

The mixer is a plain FIFO per speaker; out-of-order packets are dropped and gaps
are not concealed (`apps/media-worker/src/live.rs`).

- [x] Per-speaker jitter buffer keyed by sequence number, with reordering and a
      bounded, adaptive target delay. Drop stale audio instead of building latency.
- [x] Opus packet-loss concealment for gaps; use in-band FEC if `opus-pure` supports it.
- [x] Handle speaker start/stop (`end` flag) and sequence resets cleanly.
- [x] Per-user local volume and local mute (the packet `volume` field is always 1.0).

### 3. Transmission modes and capture processing

- [x] Voice activation as the default, with a threshold, hold time and level meter.
      The client currently transmits continuously while unmuted.
- [x] Push-to-talk with a global hotkey: XDG GlobalShortcuts portal on Wayland, X11,
      Windows, macOS. Keep continuous transmission as an option.
      Done; Windows/macOS hotkeys are cross-compiled but untested on those systems.
- [x] Enable RNNoise by default. It is hard-coded off today
      (`noise_suppression: false` in `apps/desktop/src/backend.rs`).
- [x] Wire Sonora AEC into the capture path with the real playback mix as reference.
      `sonora` is a dependency of the media worker but is not used yet.
- [x] Gain control / limiter stage after noise suppression.
- [x] First-pass duplicate-processing avoidance: detect PipeWire `echo-cancel` sources
      and PulseAudio `module-echo-cancel` on the selected input, bypass Nuschel's AEC
      and route Nuschel playback into that canceller's reference. Unknown stays unknown,
      with a per-device override.
- [x] Upstream processing found by walking PipeWire's graph from the microphone
      (mic → echo-cancel → RNNoise filter-chain → default source): a system echo
      canceller anywhere in the chain gets Nuschel's playback as its reference and
      turns Nuschel's off; a noise suppressor turns Nuschel's suppressor off.
- [x] Microphone and echo tests in Settings without a server or unmuting.
- [x] Nuschel's canceller works from a recording of the output device (all
      applications; PipeWire sink capture, PulseAudio monitor, WASAPI loopback, macOS
      process tap), stamped on one clock with the microphone and fed with a 25 ms lead.
      PipeWire room on one clock: 40-41 dB of its own echo removed in 6 of 6 runs, another
      application's stereo sound by about 50 dB; double talk ducks 0-2 dB. Fixed on the
      way: far-end audio was handed over in playback-ring bursts, automatic gain raised
      residual echo, and the gain hold ducked the user.
- [ ] The PulseAudio media_live rig still varies (8-44 dB): its echo path is a loopback
      between two independently clocked null sinks, whose delay drifts. Check on real
      hardware with Settings → Echo test; consider AEC3's clock-drift settings.
- [ ] macOS: the process tap asks for system audio permission; check the prompt and
      what happens when it is refused (the own-playback reference should take over).
- [ ] cpal's PulseAudio client stops a stream playing into a sink while the same
      connection records that sink's monitor (worked around with a second connection);
      reduce and report upstream.
- [ ] Nuschel records nothing from PulseAudio's `module-sine-source`, and its own
      playback is silent while it records the monitor of the sink it plays into.
      Both found in test rigs; check the PulseAudio client's stream setup.
- [ ] Desktop notifications on Windows (toasts need an AppUserModelID) and macOS
      (UNUserNotificationCenter); Linux uses the freedesktop service. Clicking a
      notification does not open the conversation yet.
- [ ] Decide whether mute/deafen can avoid replacing the media worker (it currently
      restarts it so a blocked native stream teardown cannot keep the microphone open).
      Keep the restart as fallback; measure the audible gap.
      Decided: keep replacing the worker (no in-process stream teardown). The audible gap is not measured yet.

### 4. Session robustness and identity

- [x] Automatic reconnection with backoff; restore channel and mute/deafen state.
- [x] Gap markers in the chat timeline for disconnected intervals (documented in
      the architecture, not implemented).
- [x] Specific handling for `Reject` reasons (wrong server/user password, username in
      use, invalid username, server full) with a retry prompt.
- [x] Show actor and reason for kicks/bans (`UserRemove`) and server mute/deafen/suppress.
- [x] Import/export Mumble client certificates (PKCS#12), so users with registered
      accounts on existing servers can keep their identity.
      New identities are RSA-2048 so stock Mumble can import them.
- [x] Self-registration on a server and access tokens (`Authenticate.tokens`).
- [x] Enforce `ServerConfig` limits (message length, image message length, HTML
      allowed) before sending.

### 5. Protocol coverage

Handled today: Version, UDPTunnel, Ping, Reject, ServerSync, ChannelRemove,
ChannelState, UserRemove, UserState, TextMessage, PermissionDenied, CodecVersion,
ServerConfig.

- [x] `PermissionQuery` (20): cache per-channel permissions; disable join, speak,
      text and create-channel actions the user lacks.
- [x] `VoiceTarget` (19): whisper/shout to users and channels (with links/children),
      bound to push-to-talk keys.
      Targets are chosen from context menus; dedicated whisper hotkeys are not done.
- [x] Channel links and channel listening (Mumble 1.4+ `listening_channel_add`).
- [x] Channel descriptions, user comments and avatars through `RequestBlob` (23);
      parse in the content worker; decode images with a memory-safe decoder in a worker.
- [x] Inline images in legacy HTML messages (currently discarded): same worker path.
- [x] `UserStats` (22) for the connection-info dialog.
- [x] `SuggestConfig` (25): respect the server's suggested PTT/version hints.
- [x] `ContextActionModify` / `ContextAction` (16/17): server-defined context menu entries.
- [x] Recording indicator and priority speaker from `UserState`.
- [x] CELT-only servers: decide between explicit "unsupported codec" notice (preferred,
      no memory-safe CELT exists) and support.
      Decided: an explicit notice; text chat keeps working.
- [x] Admin features: `UserList` (18), `BanList` (10), `ACL` (13), `QueryUsers` (14),
      channel editing and moving users.
- [ ] `PluginDataTransmission` (26), lower priority.
- Positional audio: skipped for now (nobody we know uses it).

### 6. Desktop UI for these features

- [x] Users listed inside the channel tree with talking indicators and mute/deafen/
      suppress icons (currently a text line under the channel header).
- [x] Context menus: user (direct message, local volume/mute, info, whisper), channel
      (join, create sub-channel, links, description).
- [ ] cpal's PipeWire host skips `Audio/Source/Virtual` nodes (e.g. filter-chain or
      EasyEffects outputs), so they can be used only as the system default, not picked
      in Nuschel. Fix upstream in cpal or list them through the PulseAudio API.
- [x] Audio settings: input/output device, transmission mode, VAD threshold with meter,
      PTT key, processing toggles and per-device override.
- [x] Connection status: ping, UDP/TCP indicator, packet loss.
- [x] Error/reject dialogs from section 4.
- [x] Screen-reader pass over these flows with Orca (GPUI AccessKit), early rather
      than at the end.
      Done over AT-SPI (tests/accessibility.py, in the VM); Orca itself was not run.

### 7. Tests (own servers only)

- [x] Drive the stock Mumble GUI client in the VM: stock/stock baseline, then stock
      client and Nuschel client in the same channel with two-way voice and chat.
- [x] UDP voice and forced TCP separately; block UDP mid-call and verify fallback and
      recovery.
- [x] Loss, reordering and jitter injection (`tc netem` in the VM) for the jitter buffer.
- [x] Older stock servers (1.3.x, 1.4.x) for the legacy UDP format, plus current 1.5.
- [x] Whisper/shout routing isolation and permission-gated UI.
- [x] Reconnect after server restart and network drop; history gap markers.
      Server restart is tested; a network drop without a restart is not separately tested.
- [x] PipeWire and pipewire-pulse restarts (only native PulseAudio is tested now),
      device switch and format changes during a call.
      tests/pipewire_recovery.py (private PipeWire session, in the VM): restarts, the
      audio-server pin, and PipeWire echo-cancel reuse. Device switches and format changes
      mid-call are not tested.
- [x] Upstream PipeWire echo-cancel present: Nuschel must not stack its own AEC and must
      not touch unrelated graph objects (snapshot before/after).
      Tested with PulseAudio module-echo-cancel; the snapshot covers defaults and levels.
- [x] Kill/stall content, media and storage workers independently.
      Media and storage kills are tested in the desktop. Content-worker kills and stalls
      of any worker are not tested (the content worker has a 5 s deadline in code).
- [x] Hostile inputs for legacy HTML and fenced-code parsing (fuzzing).

---

## Next: client on all desktop platforms

- [x] Linux→Darwin cross build following `../opendeck-akp05` (pinned SDK,
      cargo-zigbuild, x86_64 + aarch64 slices, `llvm-lipo`). The flake only builds
      natively per system today.
      `.#macos-aarch64` builds (SDK 14.5, minimum macOS 14.2, checked per binary; cargo-zigbuild
      is patched to honour MACOSX_DEPLOYMENT_TARGET). Not yet run on a Mac.
- [x] Windows cross build.
      `.#windows` via mingw builds; it found a real `!Send` hotkey bug. The workers run under
      Wine (codec, content, storage IPC); the GUI needs Windows' system ICU, which Wine lacks.
- [x] Audit GPUI build scripts for cross builds (`xcrun` shader compilation vs
      `runtime_shaders`, `fxc.exe` on Windows).
      Vendored patches: Apple build script checks the target OS; Windows compiles HLSL at run time.
- [ ] Native Windows and macOS audio validation on real hardware.
      Windows 11 in a KVM guest (`tests/windows_vm.py`): GUI, auto-connect muted and
      deafened, WASAPI device lists, microphone and echo tests, autostart, updates. It
      found the console window beside the app. Real hardware and macOS remain.
- [ ] Automate the Windows VM checks as a script (they were driven by hand so far)
      and add UDP (QEMU's guest forwards carry only TCP, so voice used the tunnel).
- [x] Windows and macOS sandbox profiles for media, storage and content workers.
      macOS Seatbelt profiles (media is a deny list for now); Windows is only a kill-on-close job, not a sandbox. Neither has run on its OS.
- [ ] Platform effect detection: Windows `IAudioEffectsManager` /
      `IAcousticEchoCancellationControl`, macOS Voice Processing (check its ducking).
      Not done: needs unsafe COM/Objective-C and native validation. Documented in docs/architecture.md; users have the per-microphone override.
- [x] Full adaptive audio controller: continuous residual-echo / double-talk / clipping /
      underrun monitoring with hysteresis, touching only Nuschel-owned graph objects.
- [x] Evaluate quality and robustness of the Rust DSP ports (Sonora, nnnoiseless,
      opus-pure).
      docs/dsp-evaluation.md. Perceptual quality on recorded speech is still open.
- [x] Audio callback without allocations or locks; binary IPC for audio if profiling
      warrants it.
      Allocation-free callbacks are tested; binary IPC measured as not warranted.
- [x] Show more based on measured layout height (currently 12 lines / 2,000 chars).
- [x] Local history search.
- [ ] Accessibility tests with NVDA (Windows) and VoiceOver (macOS).
      Needs Windows/macOS; the Linux AT-SPI test covers the same tree.

---

## Later

### Nuschel server (`nuscheld`)

`apps/server/src/main.rs` refuses to start a listener.

- [ ] Mumble protocol listener for stock clients: TLS, authentication, ACLs, channel
      state, voice routing (no decoding), UDP and TCP tunnel.
- [ ] Capability-negotiated extension: server message IDs, persistent history,
      network pagination with ACL checks on every page, idempotency keys, independent
      text-channel subscriptions. Client side of the extension.
- [ ] Retention, moderation, edits and deletion/tombstones applied to history.
- [ ] Make the NixOS module deployable (VM test currently runs config-only).
- [ ] VM matrix rows: stock client → Nuschel server, Nuschel → Nuschel.

### WebRTC and video (follow-ups to the str0m trial)

- [ ] Authenticated signaling over the Nuschel session; server capability negotiation.
- [ ] SFU forwarding in the server; Mumble ↔ WebRTC Opus bridge without re-encoding.
- [ ] TURN client, ICE restart, renegotiation, multiple tracks per kind.
- [ ] Adaptive jitter buffer / playout for WebRTC audio.
- [ ] Real video encoder with bitrate adaptation, keyframe requests, frame dependency tracking.
- [ ] Camera and screen capture, video rendering in GPUI, A/V sync, calling UI.
- [ ] Browser interoperability and long-call quality tests; str0m vs webrtc-rs benchmark.
- [ ] Resolve AWS-LC pulled in by str0m certificate generation.

### MLS / end-to-end encryption

- [ ] Choose and review a Rust MLS implementation; dedicated crypto worker.
- [ ] One MLS group per conversation; membership follows ACL changes.
- [ ] SFrame with MLS-exported keys for media; replay/sender/epoch handling.
- [ ] Encrypted archive with explicit historical-access policy.
- [ ] Strict mode rejecting legacy clients and downgrades; green lock only after
      authenticated MLS establishment.

### Release and other targets

- [x] Release workflow (one Linux runner, cross builds) with signed in-place updates
      for Windows and macOS.
- [ ] Store `UPDATE_SIGNING_KEY` as a repository secret and back the key up.
- [ ] Windows installer (Start menu entry, uninstall); today the zip is portable and
      updates itself where it was unpacked.
- [ ] macOS Developer ID signing and notarisation with `rcodesign` (needs an Apple
      Developer account); until then first start needs "Open Anyway".
- [ ] Third-party notices and attribution; walk through the release gates in `SECURITY.md`.
- [ ] Mobile and web clients (renderer open; shared core already compiles to WASM).
