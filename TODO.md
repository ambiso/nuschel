# Nuschl TODO

Current focus: **iOS, starting with a Linux-hosted macOS/Xcode/Simulator proof**
(user, 2026-11-11).
The desktop and server support stock Mumble interoperability; shared multi-server
audio, key-transparency witnesses, the browser foundation and experimental MLS
text/history are implemented. Live WebRTC/video, SFrame and the remaining encrypted
history proofs are later work. The rename, profile migration and build-cache cleanup
are complete; package, cross-build and all four VM checks have passed.

## Execution order

1. **Rename checkpoint — verified:** native, migration, accessibility, browser,
   Windows, universal macOS app, all eight Nix checks and all four VM suites passed.
   Build-cache cleanup is complete. Push this checkpoint before new feature work.
2. **iOS is the first implementation track after that checkpoint**, ahead of Android,
   completing the browser UI, video/SFrame and additional encrypted-history features.
   Swift + SwiftUI over the shared Rust SDK through UniFFI remains the chosen approach.
   Build a small usable client, rather than requiring every remaining SDK feature first:
   - [ ] **Linux-hosted macOS VM first** (user, 2026-11-11): no owned Apple
         hardware or paid macOS runner is a development prerequisite. Evaluate
         Docker-OSX/QEMU-KVM with a disk-backed image, pinned guest/Xcode/SDK versions
         and the appropriate Intel simulator target. Prove a minimal SwiftUI app
         builds, boots in Simulator, renders and runs an automated UI test before
         committing to this environment. Check graphics support explicitly; a booted
         macOS guest alone is insufficient. Keep CI as an optional alternative.
   - [ ] Swift bindings for connect/disconnect, bookmarks, channel/user snapshots,
         text/history and mute/deafen/join-voice commands; an iOS Rust build and a
         minimal simulator/device app that exercises the bindings.
   - [ ] SwiftUI server list, channel tree, chat and connection/error states, using
         SDK state and preserving multiline text. Keep local history by default.
   - [ ] iOS audio adapter and codec/isolation review, permissions, capture/playback,
         platform echo cancellation, route changes and interruption/reconnect handling.
         Do not copy the desktop subprocess topology or automatically allow its
         unsafe dependencies in-process; prefer memory-safe implementations and OS
         services, and explicitly review remaining mobile exceptions.
   - [ ] First app milestone: the iOS Simulator in the Linux-hosted macOS guest
         joins a private legacy Mumble
         server and a compatible Nuschl server, exchanges text and voice, and
         reconnects without losing bookmarks or local history.
   - [ ] Background/locked-phone audio, Bluetooth/headset routes, call integration,
         accessibility and a reproducible device build/signing/testing workflow.
         Physical-device audio, Bluetooth, battery and background checks remain
         separate validation work, potentially with external testers; acquiring a
         Mac or iPhone must not gate the SDK, UI or simulator milestones.
3. **Desktop + phone with one linked identity:** authenticated per-device server
   sessions must avoid the current ghost kick before enabling simultaneous linked
   logins or synced auto-connect. This can follow the initial iOS client; it does
   not block testing with a separate device account/certificate. Never clone private
   device keys or MLS state.
4. **Follow-on tracks:** optional recovery and encrypted preference sync, remaining
   MLS history proofs, Android, full browser UI, and video/SFrame. Their safety
   requirements remain, but they do not gate the first iOS client.

Dependency path: rename → shared bindings/build foundation → SwiftUI and iOS audio
in parallel → on-device voice/chat → mobile reliability. Per-device server sessions
join that path before linked desktop/phone use; recovery/sync and production MLS
history remain separate dependencies for their own features.

Ground rules (see `SECURITY.md`, `tests/README.md`, `docs/architecture.md`):

- Tests start their own servers on loopback or private NixOS VMs only.
- Memory-safe implementation first; otherwise an isolated, capability-limited worker.
  No C++ or Go WebRTC stacks.
- `nix build/flake check --cores 2`; commit incrementally with `jj`.
- Remove inactive per-agent Cargo targets after integration. Keep source snapshots,
  profiling data and verification logs apart from disposable build caches; preserve
  current runnable binaries when pruning a shared target directory.

---

## Now: open reports and review findings

Reported while testing (2026-11-10/11):

- [x] Persistent relay-only privacy: accessible/managed setting, no direct peer
      sockets, STUN or peer-IP hints, configured transfer relays only, and active
      direct-capable transfer workers revoked when enabled. Server sessions
      reconnect to apply the privacy preference before publishing new presence.

- [x] Private conversations refresh their online recipient automatically on send,
      receive and presence changes, using the same certificate identity rather than
      a recycled session number. Queued images/files resolve the current presence
      before sending. Private-send failures clear on recovery; temporary notices
      expire after ten seconds and have an accessible Dismiss button (2026-11-11).

- [x] Off-screen unread messages: the server rail and channel browser have named,
      clickable above/below indicators, red for mentions. They open the nearest
      unread text conversation without joining voice; collapsed branches roll up
      hidden unread counts and expand when a jump targets a hidden child.

- [x] Rapidly pressing Deafen could leave SDK/visible state deafened but unmuted:
      sparse Mumble snapshots now infer mute from deafness, and delayed server
      echoes cannot overwrite newer local privacy choices. Routing revisions and
      capture replacement discard buffered speech when talk destinations change;
      tests cover rapid toggles, sparse/out-of-order echoes and multiple servers.

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
      hover (Nuschl now accepts drops by position).
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
- [ ] Echo reference: Nuschl's canceller records one output device, everything it
      plays (all applications; more than two channels fold into left/right and AEC3
      switches between mono and stereo by itself). Sound on another output device
      (HDMI speakers while Nuschl plays to a headset) is not cancelled. Settings
      now names the recorded output, to check the "wrong output" report; record
      several outputs if that turns out to matter.
- [x] Opening a channel shifted the layout for a frame: it reloaded the latest page
      every time, and the first load flashed the welcome and "Loading local history".
- [x] Settings said "Looking for audio devices…" / "The audio worker stopped" in a
      container whose sound sockets were deleted: the media sandbox now skips them,
      and binds PulseAudio's directory so its restarts show up inside.
- [x] The PipeWire/PulseAudio restart test also runs through the real sandbox
      (`nuschl media-sandbox`; PipeWire restarts rely on the worker being replaced).
- [ ] The room echo check flaked once under heavy load (gate open 43 % for another
      application's sound while the level dropped 40 dB).
- [x] "scrolling over the microphone or speakers list while hovering the cursor over a
      fixed screen location messes up the list. it then shows some entries twice or
      in a different order": each wheel step moved the list and Settings itself, so
      the speakers slid under the parked pointer ("System default" again). A run of
      wheel steps now moves one container, the list under the pointer while it can
      move, as browsers latch a wheel transaction; the font picker too. The
      `devices` scene of `tests/screenshots.py` checks it over AT-SPI.

Later (reported):

- [x] Typing indicators on Nuschl servers (`typing/1`, 2026-11-11): "Ada is typing…"
      in the composer's footer line (no layout shift), a polite live region; Settings →
      Privacy turns sending off. Stock Mumble servers: none (`PluginDataTransmission`
      is forwarded without channel authorisation, docs/extension.md §7).
- [x] LaTeX/math in messages: `$…$`, `$$…$$` and ```` ```math ```` as tokens of the
      Markdown grammar, rendered by Typst in the sandboxed math worker (docs/math.md).
      Remaining: the worker on Windows has only process separation; `\text` in
      other scripts than Latin lacks glyphs (one font).
- [ ] Large file transfers peer to peer, not stored on the server: the server only
      relays (perhaps a small chunk cache, content-defined chunking); resumable
      across restarts and lost connections; clearly shown while still uploading.
      On Mumble servers (compatibility mode): done — a magic-wormhole receive code
      in the message, refreshed after each pickup, in a sandboxed transfer worker;
      Nuschl shows offers as file cards. Nuschl servers get their own, end-to-end
      encrypted design (chunked, resumable; magic-wormhole cannot resume).
- [ ] Files on Nuschl servers: designed in docs/files.md (2026-11-11), the user's
      decisions in §11 folded in: *kept* files on the server (100 MiB / 10 GiB
      defaults), *live* files sent magic-wormhole style (direct first everywhere, the
      server as relay with a piece cache) from the sender or from recipients who help
      share them (a private swarm of members; deleting the message ends every source);
      shares cost nothing at rest (no announcements at login, a lazily read share
      index, §3.5); wormhole codes for stock users re-offered after a restart only for
      recent files, as one digest (§7); one encrypted format hashed with BitTorrent v2's
      tree (BEP 52), prototyped in `crates/files`. rqbit is not embedded for now (no
      BEP 52, no pluggable transport; §2.10). Next: implement (§8).
- [ ] File transfer follow-ups: Settings controls for the rendezvous/relay servers,
      `wss://` rendezvous (the crate's TLS option pulls an old rustls), a rendezvous
      keepalive for codes waiting for hours, post "no longer available" when Nuschl
      quits, retry automatically when a newer code arrives, test on macOS/Windows,
      drop `--offer` if unwanted.
- [x] Nuschl-only servers and a current client (user, 2026-11-11). Decided: end-to-end
      mode turns stock clients away at the connection stage. Experimental
      `security = "e2ee"` now starts; compatible mode remains the default while
      encrypted history/media work is unfinished. Done: `minimum_client`
      with a grace date refuses older Nuschl versions, `recommended_client` shows an
      update notice.
- [ ] Guests on a server with `history.guests = none` see a red "Server history: No
      such conversation or message…" error in the conversation; show a calm "History
      is not available to guests" notice instead (and suggest registering).
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
      managed bookmarks keep runtime state. Home Manager `programs.nuschl.settings`
      and `servers` write it.
- [x] Broken `settings.json` / `servers.json` are set aside as `*.invalid` with an
      error notice instead of stopping the app or being overwritten.
- [ ] Managed config follow-ups: a NixOS option for `/etc/xdg/nuschl/managed.json`;
      system-wide managed config on Windows/macOS; evaluation-time checks for nested
      settings; a way to dismiss the startup error notice.
- [x] XDG Base Directory layout (crates/session/src/layout.rs, docs/desktop.md "Where Nuschl
      keeps its files"): configuration only in `$XDG_CONFIG_HOME/nuschl` (settings,
      bookmarks, managed.json); identity, end-to-end keys, history and security records
      in `$XDG_DATA_HOME/nuschl`, each worker's folder apart; last channels and the
      update state in `$XDG_STATE_HOME`; channel trees and icons in `$XDG_CACHE_HOME`.
      The first start moves an earlier profile file by file (never over a newer one,
      interruptible, reported); servers.json is split. The Home Manager VM test checks
      that `~/.config/nuschl` holds no identity after a session.
- [ ] XDG follow-ups: revoke the Modify grant earlier versions gave their Windows
      storage container on the whole data folder (needs a revoke step in
      nuschl-sandbox-windows; see SECURITY.md, "Where secrets live"); a lock of the
      crypto worker's own, so two instances on one profile cannot both run one on
      `keys/` (true before the move too).
- [x] Unread indicators (channel and DM counts, red for mentions; a pill and a red
      count on the server rail). Remember the last channel and rejoin it on start.
- [x] Runs of routine activity lines (joins, moves, connects, disconnects) collapse into
      one line that expands on click or Enter; warnings stay on their own.
- [ ] A direct-messages home above the servers, as in Discord. Mumble identities are
      certificate hashes (Nuschl, like stock Mumble, uses one identity for every
      server), but each server only claims a user's hash, so a hostile server can
      present someone else's: group by server, never merge by name; linking by hash
      must say which servers vouch for it. Nuschl servers with end-to-end
      identities can do better.
- [x] Emacs-style word deletion in every text field (user, 2026-11-11): Ctrl+W deletes
      the word before the cursor, Alt+D the word after it (not on macOS, where
      Option+D types a character), beside Ctrl+Backspace and Ctrl+Delete.
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
      app goes idle; root cause in GPUI or AccessKit not found. Under Xvfb the first
      snapshot could also miss "Disconnect" when taken before the voice card had come;
      it now waits for the voice controls (2026-11-11).
- [ ] Orca's structural navigation by list item ("No more list items") needs AT-SPI
      Collection support in AccessKit (upstream).
- [ ] "Chat processing is overloaded; some received messages could not be saved"
      (received messages past the content queue are dropped, by design) is cleared by
      the next message that is saved (`HistoryRecovered` clears every "Local
      history" error), so the loss passes unnoticed. Keep it until dismissed, or
      mark the gap in the timeline as reconnects do. Found 2026-11-11, when a slow
      VM dropped 47 of 150 messages; received messages are now prepared in batches
      (one content worker per burst), which keeps up with such a burst.
- [ ] AccessKit's AT-SPI adapter (accesskit_atspi_common 0.19) leaves out two things
      GPUI hands it, so Orca cannot know them (found 2026-11-11 by the AT-SPI desktop
      suites): a text field's contents (GPUI-kit sets the value; no Text interface is
      offered) and whether a channel-tree branch is open (`aria_expanded` becomes no
      EXPANDABLE/EXPANDED state). Upstream, or a patched adapter.
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
- [x] Proper Windows sandboxing for the workers: an AppContainer per worker kind
      (storage and crypto per directory), a job each (memory, one process, UI limits),
      an explicit handle list, no network except transfer, the offered file by handle,
      less privileged AppContainers for content, math, storage and crypto
      (docs/windows-sandbox.md). Checked from inside each worker in the Windows 11 VM
      (`crates/sandbox-windows/tests/confinement.rs`); media works in its AppContainer
      with the microphone capability, so no restricted-token fallback.
- [ ] Windows sandbox follow-ups: a separate desktop or win32k lockdown (workers can
      still create windows on the user's desktop), Arbitrary Code Guard, less
      privileged AppContainers for media and transfer too (COM and Winsock refuse
      them today, so both can read `HKCU\Environment`; try `lpacCom` and friends), an
      audit of what AppContainers may read (registry, system folders), removing a
      deleted Nuschl profile's AppContainer profiles, a run on physical hardware.
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
      and PulseAudio `module-echo-cancel` on the selected input, bypass Nuschl's AEC
      and route Nuschl playback into that canceller's reference. Unknown stays unknown,
      with a per-device override.
- [x] Upstream processing found by walking PipeWire's graph from the microphone
      (mic → echo-cancel → RNNoise filter-chain → default source): a system echo
      canceller anywhere in the chain gets Nuschl's playback as its reference and
      turns Nuschl's off; a noise suppressor turns Nuschl's suppressor off.
- [x] Microphone and echo tests in Settings without a server or unmuting.
- [x] Nuschl's canceller works from a recording of the output device (all
      applications; PipeWire sink capture, PulseAudio monitor, WASAPI loopback, macOS
      process tap), stamped on one clock with the microphone and fed with a 25 ms lead.
      PipeWire room on one clock: 40-41 dB of its own echo removed in 6 of 6 runs, another
      application's stereo sound by about 50 dB; double talk ducks 0-2 dB. Fixed on the
      way: far-end audio was handed over in playback-ring bursts, automatic gain raised
      residual echo, and the gain hold ducked the user.
- [ ] The PulseAudio media_live rig still varies (8-44 dB): its echo path is a loopback
      between two independently clocked null sinks, whose delay drifts. Check on real
      hardware with Settings → Echo test; consider AEC3's clock-drift settings. It is
      what fails vm-smoke under host load (2026-11-11, other builds on a 24-core
      host): at loads of ~27-47 the better of two runs removed 3, 14, 4 and 8 dB
      (sent 38%, 27%, 16%, 22%) in four runs; at ~11-25 it passed twice. The loopback
      (`adjust_time=0`) glitches when the VM is starved. The check now prefers an
      attempt meeting both limits, rather than choosing by "sent" alone and losing
      a passing attempt to one with worse echo reduction.
- [ ] macOS: the process tap asks for system audio permission; check the prompt and
      what happens when it is refused (the own-playback reference should take over).
- [ ] cpal's PulseAudio client stops a stream playing into a sink while the same
      connection records that sink's monitor (worked around with a second connection);
      reduce and report upstream.
- [ ] Nuschl records nothing from PulseAudio's `module-sine-source`, and its own
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
      in Nuschl. Fix upstream in cpal or list them through the PulseAudio API.
- [x] Audio settings: input/output device, transmission mode, VAD threshold with meter,
      PTT key, processing toggles and per-device override.
- [x] Connection status: ping, UDP/TCP indicator, packet loss.
- [x] Error/reject dialogs from section 4.
- [x] Screen-reader pass over these flows with Orca (GPUI AccessKit), early rather
      than at the end.
      Done over AT-SPI (tests/accessibility.py, in the VM); Orca itself was not run.

### 7. Tests (own servers only)

- [x] Drive the stock Mumble GUI client in the VM: stock/stock baseline, then stock
      client and Nuschl client in the same channel with two-way voice and chat.
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
- [x] Upstream PipeWire echo-cancel present: Nuschl must not stack its own AEC and must
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
      in the copy for stock clients (ten seconds under nuschld's lock per 64 KiB
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
      macOS Seatbelt profiles (media is a deny list for now), not yet run on macOS;
      Windows AppContainers, checked in the Windows 11 VM.
- [ ] Platform effect detection: Windows `IAudioEffectsManager` /
      `IAcousticEchoCancellationControl`, macOS Voice Processing (check its ducking).
      Not done: needs unsafe COM/Objective-C and native validation. Documented in docs/architecture.md; users have the per-microphone override.
- [x] Full adaptive audio controller: continuous residual-echo / double-talk / clipping /
      underrun monitoring with hysteresis, touching only Nuschl-owned graph objects.
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

### Nuschl server (`nuschld`) — queued (2026-11-11), in dependency order

`nuschld` implements the listener, compatibility mode and experimental encrypted
text mode. Decisions so far (user):
end-to-end mode is the default for new Nuschl servers and turns stock clients
away at the connection stage ("compatible" mode admits them, without E2EE); new
members see earlier history by default (per-channel setting); read markers sync
across devices; retention defaults to forever (docs/extension.md, "Decisions");
OpenMLS with the libcrux provider (pure Rust; hybrid post-quantum X-Wing key exchange,
Ed25519 signatures until ML-DSA suites exist); trust on first use with optional
out-of-band verification; key transparency, as in Signal's automatic key
verification (https://signal.org/blog/automatic-key-verification/); possibly
Nuschl-only servers and a minimum client version.

Security review of the server plan and first code: docs/reviews/2026-11-11-server-security.md
(P0 requirements must be met before the server listens beyond loopback).

1. **Server for stock Mumble clients** (the base everything else extends) — done
   (2026-11-11): see docs/server.md; the `vm-server` check runs the protocol suites
   and the stock GUI against it. Open phase-1 items:
   - [x] Import of a Murmur database (2026-11-11): `nuschld admin import-murmur`
         (docs/server.md), both layouts (`ServerDB.cpp` of 1.2.4–1.5 and 1.6's),
         read-only by the storage worker and validated by the server's rules;
         PBKDF2-SHA384 and SHA-1 passwords become argon2id at the next login;
         settings to `murmur-import.toml`; `services.nuschl.importMurmur` (its own
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
         Opus). Not done: a live metrics endpoint (`nuschld admin` works offline).
   - [x] Chat in the stock-client VM check (2026-11-11): xdotool types into the stock
         window's chat bar and the Nuschl peer receives it from StockUser; the peer's
         message is read back from the stock client's log (F6, copy, xclip).
   - [x] Fresh full `vm-smoke` passed on integrated `761565c0` (2026-11-11,
         452.24 seconds), including the new private-recipient checks, audio,
         PipeWire recovery, accessibility and stock 1.4/1.3 interoperability.
         The earlier red run exposed one product bug: on a slow machine a burst of
         received messages outran the content worker (one sandbox start per
         message) and what overflowed its queue was dropped; they are now prepared
         in batches (fix(client), see "Chat processing is overloaded" above). The
         rest were stale tests: workers.py and rtc_probe.py (the RTC probe's
         package); OCR could not read the welcome screen's "Add a server" since
         the system theme became the default
         (fbc0df5: under Xvfb that is light, and the binarisation was made for dark
         text); the new-server form says "Add and connect", connects at start unless
         switched off and an offline server's Connect connects directly (b8e8961);
         Enter sends (e040e3c), so lines are joined with Shift+Enter; occupied
         branches open on connect; the fixture's microphone was the speakers'
         monitor, so the canceller rightly removed the test's voice as echo since
         it cancels everything the speakers play (d758149); desktop_recovery's
         `pgrep` never matched a sandboxed worker (run as /worker), so its worker
         crashes killed nothing; accessibility.py's GrabFocus check (7c6e06a) never
         ran under Xvfb, where no window manager gives the window focus; the
         client VM lacked the xclip and xwd that stock_client.py uses (2296781). The
         desktop suites now find controls over AT-SPI (`accessibility.py`'s helpers
         on a private session bus); OCR reads only the long-name tooltip.
   - [x] Run every VM check on integrated changes before publishing: `vm-smoke`,
         `vm-server` (315.06 seconds), `vm-home-manager` (48.87 seconds), and
         `vm-web` (21.86 seconds) all passed on `761565c0` (2026-11-11).
         VM disks and artifacts were kept on disk, outside RAM-backed `/tmp`.
   - [x] `stock_client.py` under heavy host load (2026-11-11): the "0 packets" came
         from counting in a slice of a deque capped at 1000 events, which stays empty
         once full; packets now come from the event queue, the tone plays until the
         first packet (up to 180 s), then is counted for 60 s. Passed in vm-server at
         host load ~6-11; not yet seen at a load like the failing run's ~47.
   - [x] Listen revoked ends the listener (user, 2026-11-11: "no", a listener must not
         keep receiving or reading; review D3.4): after every authorisation change a
         listener whose user lost Listen ends, with Murmur's `UserState`
         (`listening_channel_remove`) to everyone and a `PermissionDenied` for Listen
         to the user, whose reason Nuschl shows; its stored row goes too; live
         delivery stops and `can_read` through it ends (pages in flight fail).
         docs/server.md, "Listeners follow Listen".
   - [ ] A deleted channel's archive can be read by SuperUser but not moderated
         (`Action::Moderate` covers live channels); only `nuschld admin history
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
         deployable (`services.nuschl`: hardened unit, credentials, local
         PostgreSQL): vm-server runs it on both databases across restarts.
   - [x] Rate limits configurable as Murmur's (`message_limit`/`message_burst`,
         `plugin_message_limit`/`plugin_message_burst`; also flood, query and
         extension buckets) and counted in the `statistics` line; the VM matrix:
         stock client → nuschld (vm-server: the stock GUI and the client suites),
         Nuschl → nuschld (vm-server: history, key transparency), Nuschl → stock
         Murmur (vm-smoke).
2. **Nuschl extension** (negotiated; stock clients keep working) — done
   (2026-11-11), see docs/extension.md "Implementation notes": ALPN + hello with a
   sticky, fail-closed client policy (and a reset), `nuschl.v1` messages, account
   ids, conversation ids, durable idempotent sends with per-conversation seq,
   ACL-checked history pages, `can_read`, guests without history, `since_access`
   intervals, subscriptions, edits/deletions with tombstones, read markers, typing,
   stock-client notices, `nuschld --erase-user`; the desktop shows server history and
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
         on a Nuschl server (tests/accessibility.py runs against Murmur).
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
   - [ ] A burst of sends stalls: 30 messages queued at once by `nuschl-cli` against
         a loopback `nuschld` (Murmur's default text limit) get 3–8 acknowledgements,
         then nothing for 20 s, neither an acknowledgement nor `chat_failed`; the
         same with binaries from before 2026-11-11's desktop work. One at a time
         works. Not investigated (the session's outbox pacing and retries against the
         server's text bucket and its 16 requests in flight).
   - [x] Capability negotiation in the Mumble handshake: ALPN `nuschl/1` plus the
         hello in `Version` (field 7200), frozen at `Authenticate`, fail-closed
         (extension.rs, the session's sticky policy), with the protocol version in
         the hello.
   - [x] Minimum-version policy (docs/extension.md "Minimum versions"): the client
         reports its version in the hello; below `recommended_client` the desktop
         shows a dismissible update notice, below `minimum_client` a warning with
         the date (`minimum_client_from`), from which `nuschld` refuses it with
         "This server requires Nuschl X or newer"; stock clients are never
         refused for their version. The notices offer the in-app updater on
         Windows and macOS.
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
         `nuschl.v1` messages, descriptor in the hello, signing key and VRF secret
         as credentials (`services.nuschl.kt.*`), per-type size and rate limits,
         epochs every 10 s while changes wait, hourly re-signed head.
   - [x] Client: publishes and monitors after login on `kt/1` servers; profiles show
         not verified / verified / "verified (same identity as on X)" / identity
         changed, the 60-digit safety number and "Mark as verified"; a red banner on
         a contact's key change; a dialog when your own key is misused; identity and
         recovery key in Settings; `nuschl-cli --identity-dir`.
   - [x] Trust on first use; a changed key is shown, never silently accepted.
   - [x] The same identity on every Nuschl server: per-server account statements,
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
4. **End-to-end encryption (MLS)** — 4a (delivery service), 4b (direct
   conversations, multi-device linking), and 4c (channel/SDK text and experimental
   history) are in (2026-11-11) on OpenMLS 0.9.2 with the
   provisional suite 0x004F and a patch for its vulnerable libcrux pins; see
   docs/e2ee.md "Phase 4 prototype". End-to-end is a server-level switch
   (`security = "e2ee"`, capability `e2ee/1`; docs/e2ee.md "Phase 4c and the answers
   as implemented"); compatible servers offer no `mls/1`.
   Complete authenticated historical ordering/membership proofs and encrypted media
   remain open. Independent review found and fixed five archive/grant/replay/
   plaintext/attribution bugs, with 20 independently rerun regressions.
   - [ ] **Remove the libcrux patch and move to the official OpenMLS release** that
         carries the fixed libcrux crates (hpke-rs-libcrux 0.8, libcrux-kem ≥ 0.0.10,
         libcrux-hmac-drbg ≥ 0.0.2) and the draft's ciphersuite code points, when
         possible: delete `vendor/hpke-rs-libcrux` and `vendor/openmls_libcrux_crypto`
         with their `[patch.crates-io]` and `exclude` entries, then follow the bump
         steps in docs/e2ee.md "Phase 4 prototype" (suite constant, API changes known
         from OpenMLS `main`, tests, `just crate2nix`). Groups and key packages made
         before the bump break (new code point and X-Wing key derivation); the user
         accepted that while Nuschl is in early development, so the bump discards
         MLS state instead of migrating it.
   - [ ] The user's answers to docs/e2ee.md §10 (2026-11-11), to implement:
         - [x] Remove the 1000-device cap (`nuschl_e2ee::MAX_GROUP_DEVICES`, the
           server's `mls.max_devices`, the client check); bound hostile trees by
           message size and process limits instead (prevalidate.rs's node bound
           follows from the input size). Done 2026-11-11. Open: today's 2 MiB tree and
           commit limits cap groups near 1,500 X-Wing devices; raise them with join
           info and server group state split across frames and records.
         - [ ] End-to-end is a server-level switch: on an end-to-end server every channel,
           direct message, whisper and shout is end-to-end encrypted (shouts and
           whispers: the sender joins the target's voice group when it sets up the
           voice target); no unencrypted channels, and every Nuschl feature
           (WebRTC, video, files, several images, typing) is available. Done
           (2026-11-11): `security = "e2ee"` starts, `e2ee/1`, no plaintext text, no
           `mls/1` on compatible servers. Open: voice, whispers and shouts (SFrame,
           4e), typing and read markers inside MLS, files.
         - On compatible servers Nuschl clients keep the unencrypted extras
           (history, edits, typing, durable DMs), labelled "not end-to-end
           encrypted" wherever they appear.
         - [x] Registration required on end-to-end servers; ACLs apply in every case.
           Unregistered connections may register, and otherwise read, write, speak
           and hear nothing (2026-11-11).
         - [x] The device-signing key on every linked device, phones included; a
           bundle without it is refused (2026-11-11).
         - Remove devices inactive for 90 days.
         - History grants to every admitted member by default in `members` channels.
         - Channel membership: subscribers with lazy external joins; every device
           with `can_read` still gets the channel's latest `seq` (unread, activity)
           without being a member.
         - Voice: one MLS group per voice channel of the devices present or
           listening (SFrame keys from it).
         - Key transparency witnesses, option B: **built** (2026-11-11,
           docs/witness.md, docs/e2ee.md §5.9). Each server keeps an epoch log of
           its akd heads, submits it to the witnesses and serves their ML-DSA-44
           cosignatures; `nuschl-witness` audits every akd step from epoch 0 and
           has a NixOS module (`nixosModules.witness`, nginx and ACME); clients
           take `key_transparency` from the settings (and Home Manager's
           `programs.nuschl.keyTransparency`); `reg:` and `name:` labels are
           published, checked on first contact and monitored by their owners.
           **Off until the user deploys the auditor** and puts its key and URL in
           `DEFAULT_WITNESSES` (one entry; docs/witness.md "Switching it on").
           Still open: the user's answer on the proposed 24 h bound and on
           blocking only new devices (both settings now); a desktop settings
           page for witnesses; history-proof growth (review G7).
   - [x] deny.toml wrapper rules: openmls only in `nuschl-e2ee`, which only the
         crypto worker and `nuschld` link; no advisory ignores (patched instead).
         SECURITY.md: libcrux SIMD `unsafe` in the exceptions table.
   - [x] OpenMLS + libcrux in the crypto worker (sealed `mls` file); key packages
         published to the server.
   - [x] Delivery service in the server: `PublicGroup` per group, epoch fencing,
         one `seq` stream with commits, key package directory, Welcome mailboxes,
         join info, crash-consistent commits, permissions through authz.rs.
   - [x] Client SDK (`crates/client`): automatic encrypted send/display and
         authenticated conversation locks on `e2ee/1`, with local history.
   - [x] One MLS group per conversation; lazy channel external joins, server-driven
         ACL/inactive-device removals, and direct conversations.
   - [ ] Device linking over the server's mailbox (`DeviceLink*`, §3.4) and its UI;
         the worker links devices and publishes multi-device rosters today, with the
         messages relayed by the caller. Device revocation and inactive-device
         removal.
   - [ ] Self-updates (§6.5), key-transparency head gossip in MLS, typing and read
         markers as MLS messages, the log-structured worker store (§8.3), a binary
         column for MLS rows, eviction of idle groups on the server.
   - [x] Encrypted text and authenticated channel history, with bounded grant pages
         covering all retained authorized keys and explicit historical-access policy.
   - [ ] 4d: authenticated historical roster/membership evidence (revoked or
         unconfirmed historical signers currently fail closed), periodic key
         rotation/self-updates, own-device self blobs, and files.
   - [x] Strict text mode: no legacy clients or plaintext chat on encrypted servers,
         no downgrades; a green conversation lock only after authenticated MLS
         establishment. Plain voice, attachments and edits are refused until their
         encrypted implementations exist.
   - [ ] Media later: SFrame with MLS-exported keys (with the WebRTC work below).
5. **Moderation and operations**
   - [ ] Moderation tools on top of E2EE (reports, membership removal), metrics.
   - [ ] **Reporting messages** (user, 2026-11-11). Moderators are users with Kick/Ban
         in the channel (server-wide for server-wide reports). The reported user is
         never told. Deniable MLS is not a goal (MLS messages are signed, so a report
         proves authorship). Flow: "Report" on a message (optionally with the few
         before it as context); the report carries the plaintext, the MLS signature
         and framing, the sender's credential, and the server's **delivery stamp** (a
         server MAC over the ciphertext hash, sender, conversation, seq and time, made
         at delivery); attachments add their file key, which the files/1 root commits
         to. Moderators' clients verify signature, credential (KT) and stamp, then act
         (delete for everyone, kick/ban, remove from the group). **A reporter gains no
         key that opens other reports**: the moderators form an MLS group and derive
         an HPKE (X-Wing) report key pair per epoch from it; the server publishes the
         public key signed by a moderator; reporters encrypt each report to it
         without joining anything; a moderator losing Kick/Ban is removed and the key
         rotates. **The removal race** (a report encrypted to the old key while a
         moderator loses Kick/Ban): the server delivers reports only to devices with
         current Kick/Ban (checked at delivery and again after the lock); from the
         moment the permission changes it refuses reports to the superseded key
         (`stale_epoch`), and reporters queue them until a moderator's client commits
         the removal and publishes the new key. Against a malicious server: the report
         key carries the group epoch and a moderator-signed timestamp with a
         freshness bound (24 h; later the KT log), and moderators flag a report
         encrypted to an epoch older than a removal they know of. Residual risk: a
         malicious server colluding with a removed moderator, for at most the bound.
         On compatible servers a report references the stored message.
   - [ ] **Spam holds and a moderation queue** (user, 2026-11-11): against scam bots
         that join and post in every channel. Light, configurable server-side
         signals that work without content on end-to-end servers: a new account
         posting in ≥ 3 channels within a minute, bursts of near-identical sizes
         across channels, a new member's first messages; on compatible servers also
         identical text across channels and link-heavy first messages. A flagged
         message is **held**: hidden from members (end-to-end: delivered marked held,
         since MLS cannot decrypt it after the epoch moves on, and clients hide it;
         stock clients receive it only once allowed); the sender sees it as sent.
         Moderators get a **queue** with Allow (released at its original place) and
         Ban user; a ban resolves all of that user's held items so the queue stays
         clear; every decision stays retrievable in a **moderation log** (who, what,
         when, which signal), filterable by user.
   - [x] Retention policies (per server and channel, see phase 2).
   - [x] Server admin CLI: `nuschld admin` (users, channels and retention, bans,
         history export and erasure, check-config), offline under an operator
         lock (docs/server.md, "Administration").
   - [ ] Admin CLI follow-ups: ACL and group editing; a NixOS wrapper that runs
         `nuschld admin` as the unit's dynamic user with its credentials; the
         Murmur import under the operator lock; `--json` output for lists.

### WebRTC and video (follow-ups to the str0m trial)

- [ ] Evaluate enriching legacy Mumble servers (user proposal, 2026-11-11):
      bounded, authenticated WebRTC signaling over `PluginDataTransmission` for
      direct Nuschl-to-Nuschl calls; an optional companion SFU on nuschl.eu or a
      self-hosted endpoint for group video and browser watch links. Mumble signaling
      does not replace STUN/TURN; relay-only skips peer discovery and candidate
      disclosure. Define independent call authorization: plugin forwarding or a
      copied link does not prove Mumble channel membership/ACLs. A browser may also
      participate in P2P through web-accessible signaling/the Mumble web tunnel;
      stock Mumble itself cannot render video. Queued after the rename, not built.

- [ ] High-quality video without transcoding (user, 2026-11-11), docs/clients.md
      "Video": the sender encodes once with scalable layers (AV1/VP9 SVC, e.g. L3T3;
      simulcast as fallback); the SFU forwards per receiver only the layers its
      bandwidth allows, from the RTP dependency descriptor, never decoding; direct
      two-person calls over plain WebRTC; SFrame with MLS keys keeps layer selection
      possible; codecs in sandboxed workers, hardware codecs where available.

- [ ] Authenticated signaling over the Nuschl session; server capability negotiation.
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
- [ ] Store `UPDATE_SIGNING_KEY` as a repository secret and back the key up. It is
      the minisign secret key that release.yml uses to sign `update.json`, whose
      public half (`crates/update/update-key.pub`) is built into the app: it protects
      in-app updates and the workflow cannot publish without it. It is not operating
      system code signing (below), and is needed either way.
- [ ] Windows installer (Start menu entry, uninstall); today the zip is portable and
      updates itself where it was unpacked.
- Deferred (user, 2026-11-11: **no Windows or macOS code signing for now**). The
  README and docs/install.md explain the first start without it.
  - [ ] macOS Developer ID signing and notarisation with `rcodesign` (needs an Apple
        Developer account); until then the app is only ad-hoc signed and the first
        start needs **Privacy & Security → Open Anyway**.
  - [ ] Windows Authenticode signing; until then SmartScreen asks on the first start
        (**More info → Run anyway**) and Smart App Control blocks Nuschl.
- [ ] Third-party notices and attribution; walk through the release gates in `SECURITY.md`.
- [ ] Mobile, web and terminal clients (user, 2026-11-11): decided, see
      docs/clients.md. Then:
  - [x] The headless client SDK in `crates/client` (commands in, updates out; the
        core builds for wasm32) with the GPUI desktop moved onto it, and a headless
        client proving it (`examples/headless.rs`).
  - [ ] What the SDK does not model yet (docs/clients.md, "Status"): identity, key
        transparency and trust; administration requests; transfers and
        attachments; split `native.rs`; UniFFI wrappers. The wasm-bindgen wrapper
        exists (`crates/client-wasm`, 2026-11-11).
  - [x] **TUI** (ratatui), the SDK's second consumer: `apps/tui` (`nuschl-tui`),
        connections, certificate pins, the channel tree, timeline with paging,
        composer, voice through the media worker, edits, deletions, typing, unread,
        direct conversations, Ctrl+K and Ctrl+F, plain-line mode, no audio over SSH
        (docs/clients.md, "The terminal client"). The switcher, search, typing line
        and `mumble://` links moved into the SDK for it.
  - [ ] TUI, left: identity and key transparency (warnings, verification),
        end-to-end direct conversations, file transfers (offers show as text),
        administration (ACLs, bans, moving people), whisper and push-to-talk (a
        terminal has no key-up), Settings beyond the command line, image alt text
        (the content worker keeps none).
  - [ ] **iOS — next after the rename:** Swift + SwiftUI over UniFFI; staged
        milestones are in "Execution order" above. Review mobile processing and
        codec boundaries; CallKit, background audio, push extension.
  - [ ] **Android — after the initial iOS client**: Kotlin + Compose over UniFFI;
        full protocol natively; workers as `isolatedProcess` services; platform
        echo cancellation; foreground service.
  - [ ] **Web** (Svelte, SDK in WASM in a Web Worker), served from one domain the
        user hosts (others may host it), so one cross-server identity lives in the
        browser; Signal-like version requirements apply. Reaches Mumble and Nuschl
        servers through an untrusted WebSocket/WebTransport tunnel with TLS inside the
        browser, so legacy Mumble servers work too; the relay is no open proxy.
    - [x] The tunnel relay (`apps/web-relay`, 2026-11-11, docs/web.md): WebSocket
          to TCP and UDP, TLS in the client, no open proxy (public addresses only,
          Mumble-like ports, a ping or TLS handshake first, origins), limits per
          client address and per tunnel, no payload logs; fuzz target `tunnel`,
          loopback tests (ciphertext only, Murmur), `vm-web`.
    - [x] The SDK in WebAssembly (`crates/client-wasm`, `just wasm-client`): TLS with
          rustls/ring in wasm32, login, channels and users; a Svelte 5 skeleton in
          `web/app` (`just web`) that shows whether it is connected;
          `tests/web_client.py` runs it in Node.js through the relay to Murmur.
    - [ ] The client itself: text, history and the extension through the browser
          session; voice (WebAudio, getUserMedia, datagrams through the tunnel);
          IndexedDB storage and identity in a Worker; its version requirement; the
          Svelte build in the Nix site package; a real-browser test; WebTransport.
  - [x] NixOS module for the web host, share links and tunnel relay
        (`services.nuschl-web`, 2026-11-11: nginx with `enableACME`, optionally a
        subdomain; static files, the landing page, app-link files, the relay over a
        Unix socket with `IPAddressDeny`; `vm-web`).
  - [ ] Push for mobile: content-free pushes (Signal-style), UnifiedPush on Android.
- [x] **Voice on several servers at once** (user, 2026-11-11): listen anywhere, speak
      in one place by default (docs/clients.md, "Voice on several servers"). One media
      worker for the app (`native/media.rs`): every server that is not deafened mixed
      into one output (so the fallback echo reference is complete too), per-server
      volume (the server's menu), one capture encoded once for the talk servers. One
      talk server by default (the server whose channel you last joined, or "Use voice
      here"); unmuting on a second asks once, "Speak on two servers?"; push-to-talk and
      the mute shortcuts act on the talk servers; a listening server sees you muted.
      Rail: green microphone ring, headphone badges that breathe while someone talks;
      the voice card says "Talking on X #c · listening to Y #d" and takes you there.
      `tests/voice_servers.py` checks it end to end against two loopback Murmurs.
      Open: a breathing badge redraws the window 25 times a second while someone
      talks there (GPUI redraws whole windows); per-server volume has only the menu's
      ±10 % steps; on the tests' private PulseAudio a worker that records takes ~2 s
      to open (the PulseAudio client waits ~1.8 s per recording stream; not yet
      investigated), so unmuting is heard late there. The terminal client has
      `/speak here|both` controls and keeps voice disabled under `--no-audio`.
- [x] **Pre-rename integration checkpoint** (2026-11-11): all five resumed tracks,
      private-recipient/deafen/unread fixes and relay-only privacy are integrated.
      Native workspace: 814 passed, zero failures, 11 baseline ignored; fmt/Clippy,
      WASM/browser build and live private-server smoke, deny/audit, crate2nix,
      Home Manager evaluation, server-only package/tests and Nix Clippy passed.
      All four VM suites passed (details above). Independent E2EE and server
      address-privacy reviews are clear; encrypted history remains experimental
      with its explicit 4d/4e limitations. This was the checkpoint before the rename.
- [x] **Rename Nuschel → nuschl** (user, 2026-11-11; "nuschl is mumbled nuschel"),
      completed on a quiet tree. Native workspace: 821 passed, zero failures,
      11 baseline ignored, then all 113 affected-crate tests passed after the final
      three regressions were added (824 unique native tests covered). All eight Nix
      package/tooling checks and all four private VM suites passed; Windows and both
      Darwin architectures cross-built, including the universal Nuschl.app bundle.
      WASM/browser, real loopback relay, screenshot and accessibility passed; Windows
      sandbox tests passed under Miri for Linux and Windows (20 each). Vendor files
      retain their original hashes. Checklist carried out:
      - text: `nuschl`, `Nuschl`, `NUSCHL` (env vars) in text files only (no PNGs,
        fonts or other binaries; never inside `vendor/`); German prose about
        *nuscheln* stays;
      - paths: `fd nuschl` renames (crates/apps dirs, desktop file, icons, nix files,
        `nuschl-policy.json`, …);
      - regenerate, never edit: `Cargo.lock`, `fuzz/Cargo.lock`, crate2nix output;
      - re-record frozen test vectors deliberately (identity fingerprint context,
        KT labels, MLS binding context, backup and file key derivation strings),
        noting the change in each crate's docs;
      - user data: migrate `$XDG_*/nuschel` (and the macOS/Windows folders) to
        `nuschl` on first start, extending the XDG move; Windows AppContainer
        profiles get new names (remove the old ones);
        preserve Mumble identity, bookmarks, history and security requirements;
        archive old experimental crypto stores and start the approved new namespace.
        Client SQL migration bytes stay unchanged, including old-name comments:
        a pre-rename database opens through the real renamed storage worker;
      - Nix: `services.nuschel` / `programs.nuschel` keep working through
        `mkRenamedOptionModule` aliases with a warning;
      - wire: ALPN `nuschel/1` → `nuschl/1`, `nuschel.v1` → `nuschl.v1` (old and new
        builds stop interoperating; acceptable in early development);
      - repository: GitHub is `ambiso/nuschl`, the jj remote is updated, and README,
        docs and the screenshot use the new name;
      - domain: **nuschl.eu** is the project domain (user, 2026-11-11): passkey
        relying party, app links, share links, the web client's origin and the
        default witness and backup service. Changing it later breaks passkeys and
        browser identities. Registered as a private individual, with auto-renew, a
        multi-year term and 2FA on the registrar account (.eu deletes at expiry and
        needs EU/EEA citizenship or residence);
      - verify: workspace build, clippy, tests, fmt, deny, crate2nix check, every VM
        check, screenshots, the accessibility test, and a migration test from an
        existing Nuschel profile.
- [x] **Build-cache cleanup** (user, 2026-11-11): removed the five idle agent
      targets, pruned 220 Cargo profiles including stranded incremental/build-script
      caches, and removed the temporary Miri target. Recovered about 479 GiB of
      filesystem space. Source snapshots, profiling data, verification logs and all
      14 current runnable Nuschl binaries were retained; binary inode/size/mtime
      checks and executable startup checks passed. No active build was interrupted.
- [ ] **Passkey-backed identity backup** (user, 2026-11-11): skip writing down a
      recovery key by tying the identity to a passkey. WebAuthn's PRF extension
      (CTAP2 `hmac-secret`) yields a stable secret per passkey and salt; encrypt the
      identity root under `HKDF(PRF(passkey, "nuschl identity backup v1"))` and keep
      the blob on an identity backup service (untrusted storage suffices: the PRF
      output cannot be guessed). A new device signs in with the synced passkey,
      derives the key, decrypts the root and issues a new device-signing key; the
      identity and its verifications survive. Relying party: the project domain
      (assetlinks.json / apple-app-site-association for the apps). The recovery key stays as an
      optional fallback; the identity's safety becomes the passkey provider
      account's. Decided (user): the project domain is the passkey domain, and the
      identity backup service runs there. Both browser and genuine native recovery
      are intended (user, 2026-11-11): a shared encrypted backup format and key
      derivation, with cross-route test vectors for each supported credential and
      provider. Browser recovery uses WebAuthn PRF. Native recovery performs the
      passkey operation and derivation outside page JavaScript (Windows WebAuthn,
      macOS/iOS AuthenticationServices; Linux CTAP2 security keys, synced-provider
      access still to investigate). Platform/provider PRF support must be detected
      and tested, with no silent fallback between trust boundaries. Thin reviewed
      OS adapters may need the existing system-service unsafe exception; identity
      decryption stays in the crypto worker. Mobile uses native credential APIs
      where PRF is supported.
      Required user choices (confirmed 2026-11-11): browser-based passkey backup,
      writing down a recovery key, or skipping recovery setup entirely. Native is
      an additional route, never a prerequisite. Identity creation/normal use must
      not require a provider, native API, passkey or completed backup. If a chosen
      browser/provider lacks PRF support, retain the recovery-key and skip choices
      without blocking the user; do not substitute a weaker derivation. Skipping
      must not block chat or repeatedly reopen recovery setup.
      A passkey stored in desktop Chrome/Google Password Manager is not necessarily
      exposed to native OS APIs; Android's Credential Manager is a different case.
      Credential access and PRF support are separate compatibility requirements.
      Keep browser recovery first-class; offer native or phone/hybrid recovery
      only for tested combinations, and explain an unavailable route clearly.
      The encrypted storage API can be untrusted, but the browser recovery code
      cannot: a compromised host can serve JavaScript that captures a PRF result.
      Opening that page from the desktop and returning through localhost does not
      create the native boundary. Protect and separate recovery deployment/signing
      infrastructure from chat/media hosting, and finalize the recovery origin/RP
      ID before issuing real credentials (a shared parent RP ID also authorizes
      qualifying subdomains). macOS signing/associated-domain prerequisites remain
      deferred; browser recovery is still required.
- [ ] **Encrypted preferences sync** (user, 2026-11-11): an optional personal vault
      on nuschl.eu (self-hostable) stores ciphertext for server bookmarks, appearance,
      friends/nicknames and selected settings under the device-sync key (§3.7 of
      docs/e2ee.md). Signed/versioned changes, per-field merges and deletion records
      must handle offline devices and concurrent desktop/mobile edits. Keep audio
      devices, microphone/active-call state, auto-connect choices, private device
      keys, MLS ratchets and system-managed policy local. Device linking can enable
      sync without backup; browser passkey recovery or a written key can restore
      access, and opting out of recovery remains allowed.
      **Concurrent sessions are a prerequisite:** one Nuschl account must admit
      distinct authenticated devices without the current Mumble-style ghost kick
      (apps/server/src/auth.rs). Do not clone an installation's crypto state or
      private device keys. Legacy registered accounts remain single-connection;
      offer explicit single-device use or separate device accounts/certificates
      and names, with their permission implications. Never silently auto-connect
      synced bookmarks on every device or start a reconnect/kick loop. Queued after
      the rename; the vault and concurrent-device login are not implemented yet.
- [ ] Knocking (user, 2026-11-11): someone who may not enter a voice or chat channel
      knocks; online users with **Move and Enter** in that channel get a notice with
      Admit / Decline (knocks rate-limited). Admitting grants temporary Enter, Speak,
      text and read in that channel, shown as "admitted by X", **until the person
      leaves the channel or disconnects**; **no earlier history** (in end-to-end
      channels the device joins the group without a history grant). Stock admins admit
      by moving the person in; a stock client's refused Enter can knock for it.
- [x] Channel share links (user, 2026-11-11; built 2026-11-11, docs/web.md "Share
      links"): `https://<web host>/join/<token>#<client-only data>`; nuschld issues,
      resolves and revokes tokens (`share/1`, 0x4E90–0x4E97: expiry, access token for
      private channels); stock Murmur links carry the target in the fragment; the
      landing page (open the app, join in the browser, get Nuschl), app-link files
      in the web module, `nuschl://` on the Linux desktop, "Copy share link" in a
      channel's menu; copied links use nuschl.eu unless the server or the user names
      another web host. Open: deploy the web module on nuschl.eu (until then links
      to it open only in Nuschl); register `nuschl://` on macOS
      (GPUI's `on_open_urls`) and Windows; list and revoke links in the desktop; an
      E2EE invite secret in the fragment; a Settings field for `share_web_host`.
