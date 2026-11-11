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

## Now: open reports and review findings

Reported while testing (2026-11-10/11):

- [ ] Code blocks: a copy-to-clipboard button. (In progress.)
- [ ] Message text is selectable; right-click a message for Copy message (and Copy
      image / Save image as for images). (In progress.)
- [ ] Clickable links, and Mumble's HTML formatting on par with stock Mumble (bold,
      italic, lists, headings, colour, tables), safely: no remote resources, link
      scheme allowlist, deceptive-anchor tooltip, libFuzzer targets. (In progress.)
- [x] Paste images into the composer (Ctrl+V): re-encoded in the sandboxed content
      worker to fit the server's image limit, sent like stock Mumble.
- [x] Attachment tray: paste, paperclip or drop images and files; Enter sends text and
      images (one image per message, paced under the server's rate limit: Murmur
      drops excess messages silently) and hands files to the transfer.
- [ ] Text is held silently while that server's images are still being prepared;
      say so. A per-bookmark rate-limit override. WebP/HEIC go as files for now.
- [ ] Report upstream: GPUI only drops files on a hovered element, and typing clears
      hover (Nuschel now accepts drops by position).
- [x] Emoji picker on Ctrl+. / Ctrl+; (GNOME's chooser is GTK-only), in colour on
      Linux via a one-line GPUI patch (vendor/gpui-pre-wgpu).
- [ ] Colour emoji in messages: text runs that are emoji should use the colour emoji
      font (DejaVu Sans has black-and-white glyphs for the classic smileys).
- [x] Sensitivity slider; dialogs scroll smoothly; echo test stops cleanly and fails
      fast without a sound system; codec shown only when known.
- [x] Add-server form: the name follows the address; Enter adds and connects. A
      refused connection says no Mumble server listens on that port.
- [ ] DNS SRV records (`_mumble._tcp.<host>`) like stock Mumble.
- [x] Mixers (pavucontrol, PipeWire) show what each stream is for; Windows/macOS
      session names are not set (cpal does not expose them).
- [ ] Echo reference: Nuschel's canceller records one output device, everything it
      plays (all applications; more than two channels fold into left/right and AEC3
      switches between mono and stereo by itself). Sound on another output device
      (HDMI speakers while Nuschel plays to a headset) is not cancelled. Settings
      now names the recorded output, to check the "wrong output" report; record
      several outputs if that turns out to matter.
- [x] Opening a channel shifted the layout for a frame: it reloaded the latest page
      every time, and the first load flashed the welcome and "Loading local history".
- [x] Settings said "Looking for audio devices…" / "The audio worker stopped" in a
      container whose sound sockets were deleted: the media sandbox now skips them,
      and binds PulseAudio's directory so its restarts show up inside.
- [x] The PipeWire/PulseAudio restart test also runs through the real sandbox
      (`nuschel media-sandbox`; PipeWire restarts rely on the worker being replaced).
- [ ] The room echo check flaked once under heavy load (gate open 43 % for another
      application's sound while the level dropped 40 dB).

Later (reported):

- [x] Typing indicators on Nuschel servers (`typing/1`, 2026-11-11): "Ada is typing…"
      in the composer's footer line (no layout shift), a polite live region; Settings →
      Privacy turns sending off. Stock Mumble servers: none (`PluginDataTransmission`
      is forwarded without channel authorisation, docs/extension.md §7).
- [ ] LaTeX/math in messages: e.g. a Typst compiler in its own sandboxed worker
      rendering to an image, with resource limits.
- [ ] Large file transfers peer to peer, not stored on the server: the server only
      relays (perhaps a small chunk cache, content-defined chunking); resumable
      across restarts and lost connections; clearly shown while still uploading.
      On Mumble servers (compatibility mode): done — a magic-wormhole receive code
      in the message, refreshed after each pickup, in a sandboxed transfer worker;
      Nuschel shows offers as file cards. Nuschel servers get their own, end-to-end
      encrypted design (chunked, resumable; magic-wormhole cannot resume).
- [ ] Files on Nuschel servers: designed in docs/files.md (2026-11-11): *kept* files
      on the server, *live* files relayed from the sender's device, one encrypted
      format with per-chunk Merkle proofs. Decisions needed from the user:
      docs/files.md §11 (default size boundary and quota, live share lifetime, direct
      connections, wormhole codes for stock users, recipients as sources).
- [ ] File transfer follow-ups: Settings controls for the rendezvous/relay servers,
      `wss://` rendezvous (the crate's TLS option pulls an old rustls), a rendezvous
      keepalive for codes waiting for hours, post "no longer available" when Nuschel
      quits, retry automatically when a newer code arrives, test on macOS/Windows,
      drop `--offer` if unwanted.
- [ ] Decide whether Nuschel servers accept only Nuschel clients (legacy Mumble
      clients cannot take part in MLS/E2EE features such as encrypted file sharing),
      and whether servers require a current client like Signal does (a minimum
      protocol version with a grace period), to avoid ecosystem fragmentation.
- [x] WebP images: decoded (inline, viewer, server icons) with the `image` crate's
      pure-Rust decoder; pasted or attached WebP is sent re-encoded as JPEG/PNG.
- [ ] AVIF images and video: their decoders are C or heavily `unsafe` and belong in
      a separate capability-limited worker.
- [x] Server icons from the server's website favicon (Settings → Server icons).
- [x] Mute and deafen animate (a slash draws across the icon as it turns red),
      reversing smoothly under rapid clicks.
- [x] Home Manager module (`homeModules.default`): install, autostart, seed-once
      initial settings and servers; checked by evaluation and a NixOS VM test.
- [x] Declarative settings: a read-only `managed.json` (user and `$XDG_CONFIG_DIRS`)
      merged at start; managed controls show "Set by your system configuration";
      managed bookmarks keep runtime state. Home Manager `programs.nuschel.settings`
      and `servers` write it.
- [x] Broken `settings.json` / `servers.json` are set aside as `*.invalid` with an
      error notice instead of stopping the app or being overwritten.
- [ ] Managed config follow-ups: a NixOS option for `/etc/xdg/nuschel/managed.json`;
      system-wide managed config on Windows/macOS; evaluation-time checks for nested
      settings; a way to dismiss the startup error notice.
- [x] Unread indicators (channel and DM counts, red for mentions; a pill and a red
      count on the server rail). Remember the last channel and rejoin it on start.
- [x] Runs of routine activity lines (joins, moves, connects, disconnects) collapse into
      one line that expands on click or Enter; warnings stay on their own.
- [ ] A direct-messages home above the servers, as in Discord. Mumble identities are
      certificate hashes (Nuschel, like stock Mumble, uses one identity for every
      server), but each server only claims a user's hash, so a hostile server can
      present someone else's: group by server, never merge by name; linking by hash
      must say which servers vouch for it. Nuschel servers with end-to-end
      identities can do better.
- [x] Keyboard shortcuts (Settings → Shortcuts): push to talk, toggle mute, toggle
      deafen, push to mute; in the window and system-wide through the GlobalShortcuts
      portal (tested end to end on GNOME), Windows and macOS hotkeys (compiled only).
- [x] Ctrl+K finder (channels, people, direct conversations across servers, never
      merging people across servers) and Ctrl+F / Ctrl+Shift+F full-text search with
      "exact phrases", `from:`, paging and jumping to a result (SQLite FTS5,
      PostgreSQL tsvector; older history indexed in the background).
- [x] Ctrl+K, Ctrl+F and Ctrl+Shift+F are configurable in Settings → Shortcuts as
      in-app actions (defaults, rebind, clear; never registered system-wide).
- [ ] Finder/search follow-ups: direct conversations from earlier sessions are not in
      the finder; a result in a DM opens only if that person is online; diacritics are
      significant (PostgreSQL would need `unaccent`); CJK text matches only by prefix.
- [ ] Shortcuts follow-ups: a plain-letter binding is also taken from the message box
      while typing; GNOME keeps its own trigger after re-recording (needs portal v2
      for "change in desktop settings"); X11 backend; KDE untested; optional actions
      (focus window, next server, whisper).
- [ ] `tests/accessibility.py --gnome-shell` is flaky on main: whole subtrees (the
      "Disconnect" button, groups of on-screen messages) are sometimes missing once the
      app goes idle; root cause in GPUI or AccessKit not found.
- [ ] Orca's structural navigation by list item ("No more list items") needs AT-SPI
      Collection support in AccessKit (upstream).
- [x] Links inside messages from the keyboard: Tab walks the current message's links
      (the composer's footer line names each and where it leads, as a polite status),
      Enter opens it, or a message's only link, as a click would; Space still expands.
      A printable key in the timeline goes to the composer and is typed there.
- [ ] An optional Orca step in `tests/accessibility.py` (Orca 51 has a D-Bus test
      interface; a probe exists in the 2026-11-11 session's scratchpad).
- [ ] With a screen reader, 1000 loaded messages cost about 4–5 ms per frame (labels
      plus AccessKit work); still 60 fps.
- [x] Friends and local nicknames by certificate hash (heart marker, nickname shown
      everywhere with the real name in tooltips, friends' presence server-wide,
      optional "friend connected" notification).
- [ ] Store the author's certificate hash in history so nicknames apply to offline
      authors; sort the tree by shown name; a friends list/filter, "Restrict
      whispers to friends" and whisper-to-friends.
- [ ] Screenshot harness: the `menus` scene breaks after `server` in `--scenes all`
      (collapsed tree coordinates).
- [x] Light mode, the system theme by default (live switching), interface and code
      fonts, text size, and Ctrl+= / Ctrl+− / Ctrl+0 zoom (a vendored gpui-pre patch
      scales the whole window; drop it once upstream has a window scale).
- [ ] The dark palette's faint text is about 3.6:1 on the main background (below
      AA); Ctrl+scroll / pinch zoom; IME placement at zoom ≠ 100 % untested.
- [x] Smooth resizing: only messages near the viewport are laid out (1000-message
      resize: 113 ms → 7 ms per frame), full frame rate during drag-resizes.
- [x] Off-screen messages are in the accessibility tree again (2026-11-11): every
      entry is a list item, off-screen ones as label-only 1×1 px elements with the same
      id, only while a screen reader listens.
- [ ] The channel tree is now the largest per-frame cost (~2.4 ms for 80
      rows); report upstream: wgpu/Mesa capability queries on every reconfigure, and
      GPUI's 30 fps cap for windows without focus during compositor grabs.
- [ ] Proper Windows sandboxing for the workers: **ready on the unmerged bookmark
      `windows-sandbox`, waiting for the user's decision** (2026-11-11). Every worker
      runs in an AppContainer (LPAC for content, storage and crypto), job limits,
      explicit handle list; verified in the Windows 11 VM. It needs the first
      first-party `unsafe` (crates/sandbox-windows, ~50 commented blocks over
      windows-sys, only the workers may depend on it) because no maintained safe
      wrapper fits (comparison in that branch's docs/windows-sandbox.md). Today on
      main the workers only run in a kill-on-close job object.
- [ ] The desktop shows Murmur's HTML welcome text as stripped source: tags vanish
      and images become U+FFFC boxes (seen on Windows against a stock Murmur).

Limits review (user, 2026-11-11): hard limits are themselves an availability risk
when chosen too low or handled badly (cf. sshd skipping work for long passwords).
Prefer process resource limits on the sandboxed workers (memory, CPU) and treating a
worker that runs out as "this message cannot be shown" for that message only; where
a limit stays, it must page or say so visibly, never silently drop.

- [ ] Search returns one page within a 96 KiB budget: page it (cursor) like history.
- [ ] Session budgets (4,096 channels, 16,384 users, 64 MiB of blobs): evict cached
      blobs by use rather than refusing new ones; review the counts against the
      largest real servers; keep the visible notice.
- [ ] Content worker decode caps (4096×4096, 64 MiB, 8 images, 640 KiB): lean on the
      worker's memory and CPU limits instead, and show a placeholder ("could not be
      displayed") when the worker fails on a message.

Security review (2026-11-11):

- [x] M1 macOS: history gets its own subfolder where data and config coincide.
- [ ] M2 The media worker gets the PulseAudio socket on PipeWire hosts too.
- [ ] M3 The macOS media Seatbelt profile allows by default (deny list).
- [x] L1 Notification thumbnails check their size without u32 overflow.
- [x] L2 No microphone audio leaves after mute: the terminator carries silence.
- [x] L3 Budgets for channels, users and server blobs (see the limits review).
- [x] L4 A channel cache can no longer block saving the bookmarks.
- [x] L5 Search over visible text (`message_search`), within a byte budget (see the
      limits review); pages seek the index.
- [x] L6 Signed release times; replayed older manifests are refused. Open: a client
      that never saw the newer manifest can still be replayed to, and withheld
      releases go unnoticed (needs expiring, regularly re-signed manifests).
- [x] L7 Exported certificates are written 0600, atomically.
- [ ] Sanitise control characters in names for display.
- [ ] L8 The Windows VM harness's HTTP control has no authentication (test-only).

Code quality and performance review (2026-11-11):

- [x] Content worker: lazy syntax highlighting, one worker per history page (a
      40-message page: 1.3 s → 10 ms in debug), pages off the storage loop.
- [x] Settings changes keep the capture chain; bitrate/loss only touch the encoder.
- [x] Level events only in the Settings check.
- [x] Channel tree built in one pass (2,000 channels / 5,000 users: 74 ms → 1 ms).
- [ ] Each `ChannelState` still does two linear searches; the "Linked with" tooltip
      looks names up linearly every frame.
- [ ] Message text and code highlights are copied every frame; avatar lookup per
      message scans all users.
- [ ] Image decoding runs on the UI thread. (Bookmark saving moved off it.)
- [x] Decoded images leave GPUI's texture atlas once nothing uses them.
- [ ] `server, generation` repeated in ~17 event types; `connect_once` and
      `drain_events` are too large.
- [x] Events are applied as they arrive (no 20 ms polling).
- [x] Media worker cleanups (idle wakeups, per-packet allocations, duplicated
      dispatch, unused `JitterStats`).
- [x] Storage: pages seek the index; search no longer scans image data.
- [x] One bwrap builder for all sandboxes; `ContentRequest` is an enum.
- [ ] Smaller: duplicated activate-key checks and `data:` URI parsing; test-only
      worker modes in release binaries.
- [ ] Vendored crates to drop once upstream catches up: `vendor/cpal` (stream names,
      RustAudio/cpal#1409), `vendor/gpui-pre-wgpu` (emoji fonts).

From the timeline and icon work:

- [ ] Memory while scrolled far up is unbounded and every loaded message renders each
      frame: a virtualised list, or paging towards newer messages.
- [x] Keyboard and screen-reader navigation of the timeline (2026-11-11): one Tab
      stop, ↑/↓, Page Up/Down, Home/End (Home loads older history), Enter/Space
      expands; AT-SPI ScrollTo and GrabFocus on any entry.
- [ ] The channel description shows only at the beginning of local history.
- [ ] A screenshot scene for the 1000-message limit.
- [ ] Installs without Nix get no desktop entry (no icon on Wayland); no icon for MSVC
      builds; no AppStream metainfo.

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
- [x] libFuzzer targets for every untrusted-input parser, run with `just fuzz <target>`
      from the flake's nightly shell (SECURITY.md, "Fuzzing"). First campaign
      (2026-11-11, 15–45 minutes per target, 18 targets, every corpus then replayed
      under AddressSanitizer without findings): the fuzzer found a GIF that decoded
      to an image without pixels; writing the targets and reviewing what they
      cannot see (super-linear time in large inputs) found quadratic link detection
      in the copy for stock clients (ten seconds under nuscheld's lock per 64 KiB
      message), in bare URLs and in the website-icon page scan, unmatched end tags
      that fooled the HTML depth limit, code blocks past the block limit, and voice
      sequence numbers that overflowed the jitter buffer. All fixed, with regression
      tests.
- [ ] Fuzzing, still open: no scheduled campaigns or kept corpora (only the small
      seed sets are committed); the server harness's frames are at most 255 bytes, so
      large messages rely on the timing tests; direct conversations (random ids) are
      out of the server harness's reach; no target for the update manifest's
      signature parsing (minisign-verify), the storage worker's requests, or
      magic-wormhole's own protocol code; the media worker's live path is covered
      through its jitter buffer and decoder only (the mixer and devices are not);
      the client harness goes on after a frame that would end a real session.
      Mumble's OCB2 header keeps 24 bits of the tag, so one random datagram in 2^24
      decrypts without the key (the crypt target finds one within a minute): the
      protocol's, but worth weighing where a decrypting datagram rebinds a
      session's UDP address (docs/server.md, "UDP binding").

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

### Nuschel server (`nuscheld`) — queued (2026-11-11), in dependency order

`apps/server/src/main.rs` refuses to start a listener. Decisions so far (user):
end-to-end mode is the default for new Nuschel servers and turns stock clients
away at the connection stage ("compatible" mode admits them, without E2EE); new
members see earlier history by default (per-channel setting); read markers sync
across devices; retention defaults to forever (docs/extension.md, "Decisions");
OpenMLS with the libcrux provider (pure Rust; hybrid post-quantum X-Wing key exchange,
Ed25519 signatures until ML-DSA suites exist); trust on first use with optional
out-of-band verification; key transparency, as in Signal's automatic key
verification (https://signal.org/blog/automatic-key-verification/); possibly
Nuschel-only servers and a minimum client version.

Security review of the server plan and first code: docs/reviews/2026-11-11-server-security.md
(P0 requirements must be met before the server listens beyond loopback).

1. **Server for stock Mumble clients** (the base everything else extends) — done
   (2026-11-11): see docs/server.md; the `vm-server` check runs the protocol suites
   and the stock GUI against it. Open phase-1 items:
   - [x] Import of a Murmur database (2026-11-11): `nuscheld admin import-murmur`
         (docs/server.md), both layouts (`ServerDB.cpp` of 1.2.4–1.5 and 1.6's),
         read-only by the storage worker and validated by the server's rules;
         PBKDF2-SHA384 and SHA-1 passwords become argon2id at the next login;
         settings to `murmur-import.toml`; `services.nuschel.importMurmur` (its own
         oneshot unit). The reader runs confined by Landlock (best effort; bwrap is
         unavailable under the unit's `RestrictNamespaces`). Left out: Murmur on
         MySQL/PostgreSQL (export to SQLite), murmur.ini, e-mail addresses, disabled
         listeners, the log table; a test that runs the reader process itself (the
         VM check does).
   - [x] Persist channel listeners for registered users (2026-11-11): stored with
         their volume, restored after `ServerSync` with Listen and the limits checked
         again (Murmur does not check), removed with the user or channel.
   - [x] TLS reload on SIGHUP and `minimum_tls_version = "1.3"` (2026-11-11): new
         handshakes get the new certificate, a broken one keeps the old one. The
         NixOS module copies certificate credentials at start, so there a renewal
         still needs a restart.
   - [x] IP obfuscation in logs (2026-11-11): `obfuscate = true`, HMAC-SHA256 tokens
         under a per-start key (Murmur's 32-bit key is reversible).
   - [x] CA-verified ("strong") certificates (2026-11-11):
         `strong_certificate_authorities` ("webpki", "system" or PEM files; none by
         default) as Murmur's `bVerified`: the client's chain verified with webpki
         after the handshake (self-signed still log in; R1 unchanged), any purpose;
         the `strong` group, `UserStats.strong_certificate`, the login log line. Not
         done, deliberately: Murmur's e-mail login with strong certificates.
   - [x] A per-user cap on temporary channels and a global budget for shout fan-out
         (2026-11-11): `temporary_channels_per_user` (16), `whisper_fanout_per_second`
         (500,000); generous, configurable, failing only the request with a reason
         (docs/server.md, "Limits").
   - [x] Rolling statistics and clients older than 1.2.2 (2026-11-11): a `statistics`
         log line per `statistics_interval`; clients reporting a version below 1.2.2
         are refused with WrongVersion (they cannot fetch blobs by hash and have no
         Opus). Not done: a live metrics endpoint (`nuscheld admin` works offline).
   - [x] Chat in the stock-client VM check (2026-11-11): xdotool types into the stock
         window's chat bar and the Nuschel peer receives it from StockUser; the peer's
         message is read back from the stock client's log (F6, copy, xclip).
   - [x] `stock_client.py` under heavy host load (2026-11-11): the "0 packets" came
         from counting in a slice of a deque capped at 1000 events, which stays empty
         once full; packets now come from the event queue, the tone plays until the
         first packet (up to 180 s), then is counted for 60 s. Passed in vm-server at
         host load ~6-11; not yet seen at a load like the failing run's ~47.
   - [ ] Listen revoked (an ACL edit, a token gone) does not end an existing channel
         listener, as in Murmur: it keeps hearing voice and text, and keeps
         `can_read` through the listener (review D3.4). Decide whether re-evaluation
         should drop listeners that lost Listen (stock clients are told by a
         `UserState` with `listening_channel_remove`).
   - [ ] A deleted channel's archive can be read by SuperUser but not moderated
         (`Action::Moderate` covers live channels); only `nuscheld admin history
         erase-user` removes messages there. Decide whether SuperUser moderates it.
   - [x] Review deviations (2026-11-11): every permission decision goes through one
         `State::authorize(actor, action)` (authz.rs) over the ACL engine, which only
         that module can evaluate (a test guards the rest of the server); three
         decisions made two ways now agree (temporary tokens, stock direct messages
         to the archive, key-transparency histories re-checked when answered). The
         mutex stays as the single owner of authorisation state: no check-to-effect
         gap that an actor would close (docs/server.md, "Design"). Left: the UDP path
         takes the lock per packet (an immutable routing snapshot would avoid it).
   - [x] TLS listener (rustls), certificate and password authentication, user
         registration, server and channel passwords, ban list: tls.rs (the R1
         verifier), auth.rs, handlers.rs (`register`, `UserList`, `BanList`),
         access tokens as `#token` groups kept as digests; tests/protocol.rs (login,
         every reject reason, a certificate without its key, registration, kick),
         protocol_features.py (the ban list).
   - [x] Channels (create, move, links, temporary channels), user state, ACLs and
         groups with Mumble's permission model (PermissionQuery, PermissionDenied):
         handlers.rs over authz.rs and its engine (Murmur's `ACL.cpp`/`Group.cpp`);
         tests/protocol.rs, the authz tables, protocol_features.py in vm-server.
   - [x] Text messages to channels, trees and users; blobs (comments, descriptions,
         avatars) with size limits: `text_message`, `RequestBlob`, text.rs
         (Murmur's UTF-16 lengths and `<img>` rule), blobs by hash from 128 bytes.
   - [x] Voice routing without decoding: UDP with OCB2-AES128 (crates/protocol
         `crypt.rs`, shared with the client), the TCP tunnel, whisper/shout targets,
         Opus only: routing.rs, udp.rs; tests/protocol.rs (tunnel, UDP, legacy
         clients, whisper isolation), udp_voice.py with loss and blocked UDP.
   - [x] Persistent state in SQLite/PostgreSQL (the storage worker over
         crates/storage), the TOML config (murmur.ini's names), and the NixOS module
         deployable (`services.nuschel`: hardened unit, credentials, local
         PostgreSQL): vm-server runs it on both databases across restarts.
   - [x] Rate limits configurable as Murmur's (`message_limit`/`message_burst`,
         `plugin_message_limit`/`plugin_message_burst`; also flood, query and
         extension buckets) and counted in the `statistics` line; the VM matrix:
         stock client → nuscheld (vm-server: the stock GUI and the client suites),
         Nuschel → nuscheld (vm-server: history, key transparency), Nuschel → stock
         Murmur (vm-smoke).
2. **Nuschel extension** (negotiated; stock clients keep working) — done
   (2026-11-11), see docs/extension.md "Implementation notes": ALPN + hello with a
   sticky, fail-closed client policy (and a reset), `nuschel.v1` messages, account
   ids, conversation ids, durable idempotent sends with per-conversation seq,
   ACL-checked history pages, `can_read`, guests without history, `since_access`
   intervals, subscriptions, edits/deletions with tombstones, read markers, typing,
   stock-client notices, `nuscheld --erase-user`; the desktop shows server history and
   pending sends. Open:
   - [x] Desktop (2026-11-11): direct messages as durable direct conversations; edit
         and delete UI; typing indicators; fetched server pages kept in the local
         cache by seq (decisions in docs/extension.md, "Desktop client").
   - [ ] Desktop follow-ups: direct conversations are listed once opened or unread
         (the DM home is a later item); a stock client's reply in a durable direct
         conversation takes the stock path and is stored only with
         `archive_stock_direct_messages`; the edit limit is not shown before the
         server refuses; no reply UI yet (`reply_to`); typing has no "stopped" message
         (indicators expire after 6 s); cached server messages are keyed by channel
         id and seq, and Mumble reuses channel ids: a recreated channel's messages
         land on the deleted channel's cached rows (and stay tombstones where those
         were deleted), so key them by conversation id; images in direct
         conversations go the stock way and are not in the server's history;
         an AT-SPI check of the edit and delete menu entries and the typing status
         on a Nuschel server (tests/accessibility.py runs against Murmur).
   - [ ] Desktop: show each channel's retention (`ChannelInfo.retention_days`,
         `live_only`, `own_retention`) and offer it in channel settings to users with
         Write: say what a shortening removes ("messages older than N days", all for
         live-only) and send `confirm_expiry` only after the user agreed; drop
         expired content from the local cache too.
   - [x] Server: per-channel and "none" retention (forever by default; a shortening
         needs confirmation and applies to existing messages; `max_retention_days`).
   - [x] Server: history rate limits per network (IPv4 /24, IPv6 /56) and per
         account, failing the single page with `retry_after_ms`.
   - [x] Server: a PostgreSQL test that runs locally (`just test-postgres`: the
         storage worker on a throwaway PostgreSQL from nixpkgs).
   - [ ] Session and desktop: honour `retry_after_ms` on `rate_limited` and `busy`
         (history pages today fail without a retry).
   - [ ] A burst of sends stalls: 30 messages queued at once by `nuschel-cli` against
         a loopback `nuscheld` (Murmur's default text limit) get 3–8 acknowledgements,
         then nothing for 20 s, neither an acknowledgement nor `chat_failed`; the
         same with binaries from before 2026-11-11's desktop work. One at a time
         works. Not investigated (the session's outbox pacing and retries against the
         server's text bucket and its 16 requests in flight).
   - [x] Capability negotiation in the Mumble handshake: ALPN `nuschel/1` plus the
         hello in `Version` (field 7200), frozen at `Authenticate`, fail-closed
         (extension.rs, the session's sticky policy), with the protocol version in
         the hello.
   - [ ] Minimum-version policy: the hello announces `minimum_client` and
         `recommended_client` (docs/extension.md §1), but the client shows no update
         notice below `recommended`, and the server never refuses below `minimum`
         after an announced grace date (nor may it for stock clients in compatible
         mode).
   - [x] Persistent history: per-conversation seq, pages checked with `can_read` and
         again when sent, idempotency keys on the message row, edits and deletions
         as tombstones, subscriptions, per-channel retention (docs/extension.md
         "Implementation notes").
   - [x] Client side: with `history/1` the desktop's timeline pages from the server
         (`HistorySource::ServerArchive`), channels and durable direct conversations
         alike, and goes back to local observations when offline; the history source
         label is per conversation (stock direct messages say "Local history"), and
         fetched pages are kept in the local cache.
3. **Identity and key transparency** (persistent, cross-server and multi-device;
   user, 2026-11-11) — done without MLS (2026-11-11), see docs/e2ee.md "Phase 3 as
   implemented": identities are created, published to each server's log, looked up,
   pinned on first use and verified; key changes and lying servers are detected.
   - [x] `crates/identity`: RFC 10073 composite ML-DSA-65 + Ed25519 root and
         device-signing keys, a separate Ed25519 credential key (172-byte MLS
         credentials), X-Wing device keys, certificates and revocations by key epoch,
         safety numbers and QR payload, SPAKE2 device linking with a PSK-bound HPKE
         bundle, encrypted keystore and recovery key; account statements (signed by
         the device-signing key, naming the server's log id).
   - [x] `crates/kt`: akd 0.13 behind a swappable interface, signed heads, gossip
         with evidence, lookup verification, self-monitoring, audits, TOFU
         assessment, per-epoch stored audit proofs; malicious-server tests.
   - [x] Crypto worker (`apps/crypto-worker`, sandboxed, no network, one per
         profile): creates the identity on first need, recovery key offered by default
         (setting: offer / require / never), sealed keystore and state, signs account
         statements, renews the device certificate, pins each server's log, audits
         heads, monitors your own binding, daily schedule, trust per contact.
   - [x] Server: the log in `kt_records` through the storage worker, `kt/1` with 12
         `nuschel.v1` messages, descriptor in the hello, signing key and VRF secret
         as credentials (`services.nuschel.kt.*`), per-type size and rate limits,
         epochs every 10 s while changes wait, hourly re-signed head.
   - [x] Client: publishes and monitors after login on `kt/1` servers; profiles show
         not verified / verified / "verified (same identity as on X)" / identity
         changed, the 60-digit safety number and "Mark as verified"; a red banner on
         a contact's key change; a dialog when your own key is misused; identity and
         recovery key in Settings; `nuschel-cli --identity-dir`.
   - [x] Trust on first use; a changed key is shown, never silently accepted.
   - [x] The same identity on every Nuschel server: per-server account statements,
         so "same identity as on X" cannot be forged by copying a binding.
   Open:
   - [ ] The key-change warning inside the direct conversation's timeline (today a
         banner above it).
   - [ ] "This wasn't me: revoke" (needs device linking and multi-device publishing;
         today only a device holding the device-signing key publishes); the
         `DeviceLink*` mailbox; device-sync key rotation.
   - [ ] OS keychain for the store key (today a 0600 file in its own directory); QR
         code rendering (the payload exists).
   - [ ] Carry head gossip (ready in the worker, not yet transported); `kt_head` in
         MLS messages.
   - [ ] A verified contact without a direct conversation is checked by lookup only,
         so a binding that comes and goes between two lookups is missed.
   - [ ] Persist the desktop's map of registration numbers to accounts (the
         worker's is persisted).
   - [ ] Mumble-certificate binding statements; independent auditors (optional).
   - [ ] deny.toml ignores RUSTSEC-2026-0173 (proc-macro-error2, reachable only under
         libcrux's `cfg(hax)` verification builds): confirm.
   Design: docs/e2ee.md. Spike: spikes/mls (own workspace). **Open questions for the
   user** are in docs/e2ee.md §10 (device-key placement, akd vs KEYTRANS, channel
   membership, history grants, size cap, voice groups, inactive devices, auditors,
   release timing, ciphersuite, guests); question 1 (recovery key) became a
   setting, offered by default.
4. **End-to-end encryption (MLS)** — 4a (delivery service) and 4b (direct
   conversations, multi-device linking) are prototyped on the unmerged bookmark
   `mls-prototype` (2026-11-11, OpenMLS 0.9.2, provisional suite 0x004F; VM check
   with an E2EE DM step passes). It merges only after an OpenMLS release with the
   draft code points and fixed libcrux crates; the bump steps are in that branch's
   docs/e2ee.md "Phase 4 prototype". Still open there: the desktop sends and shows
   E2EE DMs through the worker (today only the lock), the `DeviceLink*` mailbox and
   UI, device revocation and inactive-device removal, self-updates (§6.5), gossip in
   MLS, typing and read markers as MLS messages, channels (4c), the log-structured
   worker store, a binary column for MLS rows, eviction of idle groups on the server.
   - [ ] Wait for the next OpenMLS release before shipping persistent groups:
         0.9.2 pins libcrux crates with RUSTSEC-2026-0329/-0330/-0331 (a hostile
         key package can crash the crypto worker) and an X-Wing suite code point
         that is not in the IETF draft; `main` already has both fixed. Prototype
         against 0.9.2 meanwhile.
   - [ ] deny.toml wrapper rules: openmls only in `nuschel-e2ee`, the crypto
         worker and `nuscheld` (akd, `nuschel-kt` and `nuschel-identity` are
         already confined). SECURITY.md: libcrux SIMD `unsafe` in the exceptions
         table.
   - [ ] OpenMLS + libcrux in the crypto worker (which exists since phase 3, with a
         sealed state store); key packages published to the server.
   - [ ] Delivery service in the server: ordered handshake messages per group, key
         package directory, welcome delivery.
   - [ ] One MLS group per conversation; membership follows ACL changes; direct
         messages as two-party groups.
   - [ ] Encrypted messages, files (the file-transfer design for Nuschel servers) and
         history archive with an explicit historical-access policy.
   - [ ] Strict mode: no legacy clients in end-to-end channels, no downgrades; a
         green lock only after authenticated MLS establishment.
   - [ ] Media later: SFrame with MLS-exported keys (with the WebRTC work below).
5. **Moderation and operations**
   - [ ] Moderation tools on top of E2EE (reports, membership removal), metrics.
   - [x] Retention policies (per server and channel, see phase 2).
   - [x] Server admin CLI: `nuscheld admin` (users, channels and retention, bans,
         history export and erasure, check-config), offline under an operator
         lock (docs/server.md, "Administration").
   - [ ] Admin CLI follow-ups: ACL and group editing; a NixOS wrapper that runs
         `nuscheld admin` as the unit's dynamic user with its credentials; the
         Murmur import under the operator lock; `--json` output for lists.

### WebRTC and video (follow-ups to the str0m trial)

- [ ] Authenticated signaling over the Nuschel session; server capability negotiation.
- [ ] SFU forwarding in the server; Mumble ↔ WebRTC Opus bridge without re-encoding.
- [ ] TURN client, ICE restart, renegotiation, multiple tracks per kind.
- [ ] Adaptive jitter buffer / playout for WebRTC audio.
- [ ] Real video encoder with bitrate adaptation, keyframe requests, frame dependency tracking.
- [ ] Camera and screen capture, video rendering in GPUI, A/V sync, calling UI.
- [ ] Browser interoperability and long-call quality tests; str0m vs webrtc-rs benchmark.
- [ ] Resolve AWS-LC pulled in by str0m certificate generation.

### Release and other targets

- [x] Release workflow (one Linux runner, cross builds) with signed in-place updates
      for Windows and macOS.
- [ ] Store `UPDATE_SIGNING_KEY` as a repository secret and back the key up.
- [ ] Windows installer (Start menu entry, uninstall); today the zip is portable and
      updates itself where it was unpacked.
- [ ] macOS Developer ID signing and notarisation with `rcodesign` (needs an Apple
      Developer account); until then first start needs "Open Anyway".
- [ ] Third-party notices and attribution; walk through the release gates in `SECURITY.md`.
- [ ] Mobile, web and terminal clients (user, 2026-11-11), queued; architecture under
      discussion with the user. Proposal: first a headless client SDK, the desktop's
      UI-independent state (`apps/desktop/src/state.rs`/`backend.rs`: unread, edit
      ordering, history merge, typing, trust, reconnects) moved into `crates/client`
      with commands in and events out, so every client shares it and the GPUI desktop
      becomes one view over it. Then:
  - [ ] **TUI** (ratatui), the SDK's second consumer; full protocol and voice.
  - [ ] **Android**: Kotlin + Compose over UniFFI; full protocol natively; workers as
        `isolatedProcess` services; platform echo cancellation; foreground service.
  - [ ] **iOS**: Swift + SwiftUI over UniFFI; workers in-process behind the same
        boundary (no subprocesses on iOS); CallKit, background audio, push extension.
  - [ ] **Web**: SDK in WASM in a Web Worker, DOM UI; needs a gateway in nuscheld
        (WebSocket control with the same framing, WebRTC voice via str0m without
        re-encoding); content in a sandboxed iframe on its own origin; trust model for
        E2EE in a served web app to decide.
  - [ ] Push for mobile: content-free pushes (Signal-style), UnifiedPush on Android.
  Start each sub-agent once the SDK and the protocol interfaces are semi-stable.
- [ ] Channel share links (user, 2026-11-11): a link you send to someone that opens
      Nuschel if installed, else joins in the browser. Proposal:
      `https://<server>/join/<opaque token>#<client-only data>` served by nuscheld (token
      revocable, expiring, optionally an access token; the fragment carries the
      certificate pin and later an E2EE invite secret, never logged), App Links and
      Universal Links for the apps, a `nuschel://` scheme for the desktop, "join in
      browser" via the web client; for stock Murmur servers perhaps a static
      project-run page with the target only in the fragment.
