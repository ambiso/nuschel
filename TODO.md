# Nuschl TODO

Current focus (user, 2026-11-12): **server-operator setup, desktop rail/footer
spacing and the native ACL/group editor**. Browser configuration/send-limit fixes
are integrated with combined checks running; the iOS installer remains quarantined
after a whole-file checksum mismatch. Audio fidelity, unmute latency and hybrid
update readiness retain their remaining gates below.
The relay DNS/kernel checks and explicit-deny correction are integrated.
The measured playback clipping correction and SuperUser bootstrap guide
are integrated. Transmit quality evidence and the exact native welcome-domain
regression are integrated, along with the browser's initial welcome surface
and central text-size correction. Live welcome updates are also integrated.
Full Mumble ACL/group editing parity is explicitly queued below. Native action
authority, content/math worker retirement and
the standalone server/RTC per-crate build migration are integrated with scoped
proofs. Native and browser transmit encoders now use Audio; RestrictedLowDelay
quality equivalence remains open. Browser initialization recovery is integrated.
Native macOS static CI verification is prepared; its actual execution
remains open. SDK request retirement, bounded menus, native packet-gain
ownership and frozen cross archives have scoped proofs integrated. Browser playout, hash-only document delivery, CELT concealment,
Windows profile-preserving updater relaunch and the metadata-refresh procedure
are integrated with scoped proofs; final browser/device/distribution checks stay open.
Server reordering and the desktop layout foundation have scoped proofs integrated;
web image attachments, modern identity/archive flows and full parity remain open.
The rename/build-cache cleanup is complete. Release publication remains behind
signing, notice and packaging gates. The locally tracked `origin/main` is
`36eef419` as of this checkout review; this is not a tagged-release claim.
No push, tag or release is performed by the queue integrations below.

Queue policy (user, 2026-11-12): record every reported or discovered issue here
when it cannot be fixed immediately. Delegation does not close an issue: retain
its remaining work and verification limits until the fix is integrated and its
relevant checks pass. Keep newly discovered blockers visible even when they are
outside the current task's scope.

Current integration checkpoint (2026-11-12):

- **Live welcome updates are integrated through `8d65a1a4`.** Native 81 session
  tests (one existing ignored), browser 27 units/six relay integrations and the
  compiled App with an owned TLS/protobuf fixture pass. Absent/empty/changed/
  repeated sources, stale tokens and reconnects have distinct checks; welcome-only
  updates emit no Limits/audio changes. All eight combined build/check outputs
  and both bundled notice readers pass, and all ten browser runtime assets match
  the tested product exactly. This adds no native GUI or stock-Murmur live-update
  proof. Sparse-limit merging and browser send caps remain separate open fixes.
  Root evidence: `~/.cache/nuschl-integration-20261112/live-welcome/handoff.json`.
- **Initial browser welcome and text-size fixes are integrated through
  `99931b5e`.** The SDK worker produces bounded typed documents; exact-source
  dismissal tokens, reconnect/replacement, inert resource canaries, bold domains,
  code and keyboard dismissal pass against the compiled App and owned Murmur.
  Actual 14/16/20px settings, persisted text size and phone/desktop bounds pass.
  All eight combined distribution/check outputs and both bundled notice readers
  pass; all 11 production source files and ten runtime assets match the tested
  product exactly. The long-name 320px/20px screenshot was also reviewed.
  Live ServerConfig updates and broader browser parity remain open. Root evidence:
  `~/.cache/nuschl-integration-20261112/web-welcome/handoff.json`.
- **Xcode archive verification is integrated through `a12405a9`.** The confined
  read-only verifier checks the Apple Software Update certificate profile,
  signature, current signed CRLs, full payload and Xcode 16.2/SDK 18.2 plists.
  All 18 real-media/parser tests pass, including real tamper and expired-CRL
  refusals. Root independently passes ten parser and 19 capability tests.
  Native Apple acceptance, installed macOS, Xcode execution, Simulator graphics
  and a working iOS app remain open; see docs/ios-vm.md.
- **Filtered native capture is integrated through `ff695f9e`.** The actual
  converter now uses the existing memory-safe mono Speex engine with rational
  clocks and capture-side timestamp compensation. All 31 quality/clock/initial
  marker cases, 81 native units and actual private 44.1/96 kHz capture/unmute/AEC
  tests pass, alongside the original DSP suite. The raw stopband-relative
  diagnostic and justified fixed-input acceptance rails remain explicit in
  docs/capture-quality.md. All eight combined build/check outputs at `46f6a77d`
  and both actual bundled notice readers pass; the packaged worker matches the
  tested worker byte for byte. Browser runtime assets are unchanged from the
  actual App-tested product. Physical/platform/full-pipeline listening and
  matched cold-source timing remain open. Root evidence:
  `~/.cache/nuschl-integration-20261112/capture-quality/handoff.json`.
- **Relay DNS and explicit-deny verification is integrated through `aaff9293`.**
  Six real module configurations/eight invariant categories, nine unit tests,
  seven integration tests and both private NixOS VM cases pass. The VM verifies
  actual cgroup BPF filtering, DNS-dependent stock-Murmur login/UDP, resolver
  tunnel refusal and remove/restore recovery. Explicit denies now precede
  operator allowances; listening denied aliases receive zero relay packets
  against positive direct controls. The root relay binary matches the actual
  VM-tested binary exactly and formatting passes. IPv6 policy has unit/eval
  coverage; these runtime cases use IPv4. No public service was contacted or
  deployed. Root evidence: `~/.cache/nuschl-integration-20261112/relay-dns/handoff.json`.
- **Playback clipping/reference correction is integrated through `89806ef2`.**
  The reviewed wide ring/device clip and actual quantized-output fallback pass
  76 native tests, 20 independent C-reference/clock cases, the unchanged spectral
  gates, private DSP/rate checks and the existing gain regressions. At 44.1 kHz,
  the overload comparison improves from 20.03 to 127.31 dB. All eight combined
  build/check outputs at `700b8d24` and actual native/web notices pass; the bundled
  media worker is byte-identical to the tested final worker. Browser runtime
  assets remain byte-identical to the actual App-tested product. Full physical,
  platform and end-to-end listening equivalence remain open. Root evidence:
  `~/.cache/nuschl-integration-20261112/output-clip-order/handoff.json`.
- **SuperUser bootstrap guidance is integrated at `700b8d24`.** The loopback
  test verifies password setup/rotation, exact fresh ACL preservation, registered
  account promotion, unpromoted denial and restart persistence. The documented
  stock-Mumble group-editor route applies to compatible-mode servers; Nuschl's
  own full editors and E2EE bootstrap remain unfinished. See docs/server.md.
  Relay module tests integrated at `936c9eeb` independently pass six actual
  configurations/eight invariant categories; the later private DNS/BPF proof is
  now integrated above.
- **Shared-checkout relay commits are repaired.** `24a810a0` and `8a84f4b9`
  are consolidated into relay-only `7ed9b7b2`; the accidentally captured TODO
  milestone update is a separate docs-only `f8a69fa1`. A before/after comparison
  proves identical final files. The default kernel exception is only resolved's
  dedicated `127.0.0.53` when enabled, with explicit custom resolver addresses.
  Independent module/runtime validation and clearer dedicated-address guidance
  are now integrated above; no deployment or push was performed by this integration.
- **Native action authority and the Audio transmit profile are integrated through
  `95a1a27f`.** All 17 authority production files match the reviewed private-server
  source exactly. Its 87 desktop, 107 client and 79 session tests and real permission
  shifts, session reuse and stale reconnect prompt proofs pass. The unchanged
  geometry matrix also passes against that authority package. Native encoder
  tests pass 71 with five existing ignored; audio-WASM passes eight and the real
  wasm-bindgen artifact builds. Combined desktop/web/server/signer packages,
  formatting, deny, audit and generated-graph checks pass, as do actual Linux/web
  bundled notice readers. The independent receive spectral artifacts and five
  measurement controls are integrated: nominal component agreement is close,
  while the overload 44.1 kHz stock-order gate deliberately remains failing.
  See `docs/context-menus.md`, `docs/opus-encoding-quality.md` and
  `docs/output-quality.md`. This does not establish complete input/output or
  physical Windows/macOS fidelity. The exact combined web App/SDK passes the
  private-Murmur voice/text/order/privacy test: 21 audio packets produce zero
  state snapshots, audible RMS is 0.06777 and deafen RMS is zero. Source/build
  outputs, unchanged authority hashes and runtime limits are recorded in
  `~/.cache/nuschl-integration-20261112/authority-audio/handoff.json`.
  The exact combined native package also passes private-GNOME/Pulse two-server
  voice checks: PTT destination, explicit multi-server consent, rapid deafen,
  independent resume, mute shortcuts and media-worker crash recovery. Its
  media-worker binary and bundled browser audio WASM match the independently
  encoder/DSP-tested products byte for byte. These remain synthetic private
  runtime proofs, with physical-device and complete fidelity gates still open.
- **Combined Linux/web distributions passed at `39ea7132`.** Actual packages
  include clocked browser playout with both reviewed cross-port fixes, hash-only
  document delivery, loss/CELT/DTX/FEC/framing corrections, stereo/headroom/mono
  folding, filtered conversion, profile relaunch, updater helpers and notices.
  Formatting, deny, audit and generated-graph checks passed; actual Linux/web
  notice readers verified bundled bytes. The native media worker is byte-identical
  to the independently tested 70-pass source product. The exact combined web App
  passes private-Murmur production-CSP voice/text/reorder/privacy checks with its
  new SDK: 21 audio packets produce zero full-state snapshots, audible RMS 0.06762
  and deafen zero. Browser component/controller/worker proofs remain scoped below.
  Complete frozen Windows/macOS archives at the same source tree also pass
  scoped archive checks and actual Windows GUI update/profile continuity. Native
  Apple signature/runtime acceptance remains open. The later combined Linux/web
  checkpoint `a5ff76cd` includes bounded menus, SDK-RPC retirement and native
  encoded-packet gain ownership: all seven build/check outputs and both actual
  bundled notice readers pass. Nine independent native gain fixtures pass on
  the combined mixer source; its packaged worker matches the private DSP/SRC
  tested product byte for byte. All ten web runtime files match the actual
  confined-App-tested SDK-RPC package. This reuses exact-byte evidence; it does
  not claim new physical-device or native Windows/macOS execution tests.
  Source/proof details: `~/.cache/nuschl-integration-20261112/menus-native-gain/handoff.json`.
  No push, release or production trust enrollment.
- **Browser initialization recovery is integrated through `269d4f24`.** All 44
  focused Node checks pass on root. The current web package builds and its ten
  runtime files exactly match the confined Chromium-tested startup package;
  its actual scoped notices pass separately. Zero-server recovery, a held real
  WASM loader with zero ordinary requests/microphone access, retained draft/cache
  and first-ready-only invite connection pass. Actual calling/text/order/privacy
  checks remain green. Scheduled deadlines do not prove synchronous thread exit.
  See `docs/sdk-rpc.md`; content-worker retirement is now integrated separately,
  while the attachment broker remains open.
- **Desktop layout foundation is integrated through `12d6695c`.** Shared finite
  text/action lanes, one bounded notice stack, restored connection indicator,
  wrapped headers/footers, keyboard/pointer dismissal and nested focus reveal
  passed 75 native tests and 12 real private-GNOME width/sidebar/zoom cases.
  First/last server decorations and shared animated icon/outline radii passed
  eight endpoint/zoom cases; the old executable reproduced the clipped first
  outline. README screenshot regenerated. Matched timing records added layout
  cost honestly and show no idle redraw loop; see `docs/desktop-layout.md`.
  The frozen integrated Linux `.#default` package built successfully; its exact
  package control and eight-case rail GUI repeats passed. Native Windows/macOS runtime proof remains
  separate.
- **Native and browser server reordering passed their scoped checks.** Stable-ID
  catalog order leaves live connection indexes, conversations, drafts and voice
  routes unchanged. Drag preview/ghost, cancellation, edge scrolling, keyboard
  alternatives and atomic persistence have native 100/200% and actual browser
  desktop/320/390 touch proofs. The live-App synthetic-voice test also preserved
  generations and microphone state and correctly muted after a permutation.
  SDK tests passed 87 with one pre-existing ignored; native tests passed 79.
  Canonical bookmark barriers cover native/TUI/browser writers. Final integrated
  distribution and physical-device/cross-platform checks remain; see
  `docs/server-order.md`.
- **Real web voice/mobile baseline is integrated through `690fe1f9`.** Shared SDK
  legacy voice, explicit touch Join, desktop double-click, chat-first responsive
  drawer and browser audio were verified with synthetic media/private servers.
  Evidence includes 18 native units, five stock-Murmur integrations, three
  worker tests, nine presentation/invite tests, six production-CSP viewport
  cases, final real-App Opus/IndexedDB/multiline/rapid-deafen/private-cache proof,
  and audio packet handling without timeline snapshots. Codec checks include
  independent 997 Hz libopus decoding. This is a working baseline, not full
  desktop parity or physical Safari/mobile/headset quality proof.
- **Browser content producer is integrated through `e91ff46e`.** Three native,
  eight Node and real confined-worker RGBA/code/canary checks passed with a
  256 MiB maximum. The source-ticket SDK bridge and App image paste/attachment
  controls remain unimplemented; see `docs/browser-content.md`. The pinned,
  isolated Temml 0.14 formula renderer with notices is integrated through
  `309ad791`/`9159eae5`; this does not complete image/attachment parity.
- **Hash-only document delivery is integrated.** The shared SDK requests selected
  descriptions and current-profile comments once per hash/connection; arriving
  comments update the open profile. Private stock-Murmur/GNOME proofs reproduce
  both old gaps and verify first-open, replacement, hidden/reopen and raw hash/body
  frames on the fixed package. SDK tests pass 99 (one existing ignored), desktop
  tests pass 79. Existing-document cache reuse and stale/hidden interest guards
  pass. Separate legacy hash/body normalization and native cross-platform checks
  remain open; see `docs/hash-documents.md`.
- **Modern browser identity/archive flows remain open.** Do not advertise MLS,
  authenticated modern archive parity or other desktop-only capabilities until
  end-to-end browser interoperability and capability checks prove them.
- **ACME/PKI lifecycle is verified through `3364664c`.** The private NixOS VM
  reproduced the removed-target startup blocker, then passed bootstrap rejection,
  automatic recovery, actual UID/group/read-only credential access, renewal via
  the real post-run hook, retained TLS sessions, invalid-renewal retention and
  intentional manual stop. The issuer command is offline; public ACME challenge
  providers remain outside this proof. Trust and history identity checks remain
  independent of server ordering.
- **Audio corrections are packaged through `abe83021`.** Native/browser stereo,
  wide final mixing, actual mono-device folding, Mumble's receive phase setting,
  complete 120 ms packets, filtered playback/reference conversion, Pulse capture
  startup and bounded VAD onset retention have scoped proofs. The mono source
  checkpoint passed 68 media tests (five diagnostic/baseline ignored); welcome
  client tests passed 93 (one baseline ignored). Actual private PCM/DSP checks
  passed; earlier failed AEC trials remain documented. The shared jitter-core
  refactor `76ef6af6` additionally passed native/shared tests and 19,881 exact
  frozen-native trace comparisons; browser activation and its scoped packaged
  App proof are now integrated (see the detailed completed rows below).
  Physical-device latency, complete Mumble output parity, browser recovery and
  remaining conversion/clip-order work stay open below. The requested transmit
  default is 128 kbit/s subject to server budgets; UserStats bandwidth now converts
  bytes/s to bits/s correctly. Configurable bitrate controls remain queued.
- **Hybrid update verification has Linux and Windows VM proofs.** Actual
  packaged Linux helpers passed sandbox/corruption/timeout/install checks.
  `c262ef1b` adds native Windows fixed-engine, LPAC, forged-reply, deadline and
  library download/apply/cleanup evidence. Windows network probing specifically
  observed denied/unavailable Winsock initialization. The fixture desktop GUI
  update/restart additionally passes exact profile/state continuity and every
  signed installed hash; its eight helper/worker EXEs match runtime-tested official
  products. Current-head cross archives and macOS execution remain separate gates;
  see `docs/windows-hybrid-update.md`. Production trust stays empty.
- **Release remains gated.** Independent Linux, Windows, universal macOS and web
  artifacts, hybrid update verification, exact notices and final packaging must
  pass. Production public-key enrollment and CI secret provisioning remain
  operator steps. Do not publish unsigned updates.
- Knocking is integrated through `1f979943` (134 source-branch tests); SDK/UI
  consumers remain. MLS admission is refused. RTC RustCrypto certificates and
  bounded four-track transport are integrated (five signaling and 12 transport
  tests); production calling/capture/signaling and ICE restart remain unfinished.
- Local speech model-cache/isolated CPU Voxtral checkpoints passed private
  synthetic/silence tests on their source branch. GPU backends, Kokoro and
  user-facing controls remain unfinished; no runtime quality claim follows.

## Execution order

1. **Verified foundation:** rename checkpoint `f0116b77` and audited fixes
   `15320136` are pushed. Build-cache cleanup is complete.
2. **Web:** package the real WASM SDK and Svelte UI through crate2nix and locked npm
   dependencies; worker-isolated chat, server/channel navigation and default local
   IndexedDB history; then negotiated server archives, identity, voice and browser
   interoperability. Keep certificate and invite policy checks fail closed.
3. **Server admission/moderation:** knocking and temporary permission/history fences;
   reports, spam holds and durable moderation decisions; administrative follow-ups.
   One owner controls shared protocol/server authorization changes before assigning
   callers. Experimental E2EE must refuse unsupported admission rather than leak
   earlier history or keys.
4. **WebRTC/video:** certificate/DTLS RustCrypto gate passed; next ICE/re-negotiation,
   authenticated signaling and relay-only
   privacy, calling/SFU/media adapters, and SFrame. Transport/codec tests do not
   establish a usable video-call product.
5. **Independent safety and desktop fixes:** native image clipboard and contrast,
   authenticated Windows test control, persistence/resource limits and remaining
   recovery/accessibility coverage. Parallelize only disjoint files and APIs.
6. **Linked devices/recovery:** authenticated per-device server sessions before
   concurrent linked logins or synced auto-connect; optional passkey/recovery-key/
   skip flows and preferences vault retain their requirements. Android remains
   future work. No private device-key or MLS-state cloning.
7. **Optional local speech:** Kokoro TTS and Voxtral Mini 4B Realtime captions;
   isolate inference, download verified models only on demand, target Linux/Windows
   Vulkan, optional Nix ROCm and macOS Metal. See [speech plan](docs/speech.md).

Dependency path: shared protocol/authorization → server handlers → shared SDK
commands/events → desktop/web controls. Web build/worker storage and RTC encoded-media
transport can progress independently until signaling integration. Moderation and
knocking share server authorization/history code and must be serialized there.

## iOS feasibility and app plan

Initially cancelled after Apple access failed; the user subsequently supplied the
Xcode archive on 2026-11-11. Resume at archive verification and guest feasibility.
The earlier experiment demonstrated
Sonoma Recovery 14.6.1/23G93 x86_64 boot and basic GUI rendering over KVM, not an
installed macOS/Xcode/Simulator environment. Experiment processes were stopped;
media/evidence remain on disk. Confined archive checks now prove signature,
current-CRL checks, payload integrity and Xcode/SDK version declarations; an
installed macOS guest, Xcode execution and Simulator graphics remain unproven.

   - [ ] **Linux-hosted macOS VM first** (user, 2026-11-11): no owned Apple
         hardware or paid macOS runner is a development prerequisite. Evaluate
         Docker-OSX/QEMU-KVM with a disk-backed image, pinned guest/Xcode/SDK versions
         and the appropriate Intel simulator target. Prove a minimal SwiftUI app
         builds, boots in Simulator, renders and runs an automated UI test before
         committing to this environment. Check graphics support explicitly; a booted
         macOS guest alone is insufficient. Keep CI as an optional alternative.
   - [x] Verify the supplied Xcode archive's signature, integrity and declared version:
         complete confined XAR/PBZX/CPIO inspection passed without extraction;
         actual bundle is Xcode 16.2/16C5032a, with Simulator SDK 18.2. Local file is
         2,969,019,672 bytes, SHA-256
         `0e367d06eb7c334ea143bada5e4422f56688aabff571bedf0d2ad9434b7290de`.
         Mandatory three-cert Apple Software Update policy and both production
         EKUs, independently retrieved root, RSA signature, payload hashes and
         current signed CRLs passed; SHA-256 remains a local fingerprint.
         `tests/ios_xip.py`, `tests/ios_xip_test.py`, `docs/ios-vm.md` retain exact
         bounds, version/plist hashes and real tamper evidence.
   - [ ] Before extraction/use, recheck current CRLs and retain native Apple `xip`
         acceptance as a separate unproven gate. The linked Apple website root CRL
         was expired; actual certificate distribution-point CRLs passed. Never
         bypass expired revocation data. Keep extraction/guest scratch on disk;
         SDK presence is not an installed Simulator runtime.
   - [x] Fix the iOS container capability diagnostic: normalized Podman CapDrop
         requires exact local OCI evidence of empty sets/NNP, rejects added caps
         and unsafe boundaries; all 19 capability tests passed. The initial
         stopped-container report correctly kept `live_effective_verified=false`;
         the separate controlled-resume evidence follows below.
   - [x] Observe live init capabilities/NNP after backed-up controlled resume:
         all five masks zero, NoNewPrivs=1, KVM enabled; existing Recovery booted,
         then stopped cleanly. Both host qcow2 hashes unchanged; target disk1 is
         blank 80 GiB, persistent (only Recovery/OpenCore use temporary snapshots).
   - [ ] Acquire/authenticate the exact candidate Sonoma 14.8.9/23J631 installer:
         official Apple product 140-88558 metadata identifies a 13,654,790,295-byte
         package, expected catalog SHA-1 recorded in docs/ios-vm.md. No package
         downloaded; actual SHA-256/signature/payload checks remain. Exact profile
         repinned from 14.5; Recovery 14.6.1 is explicitly rejected as that candidate.
         Review target disk/APFS and installation before destructive steps. Recovery
         exposes a basic IONDRV framebuffer, no IOAccelerator; Metal, installed-OS
         graphics, Xcode execution and Simulator remain unproven.
   - [x] **Host feasibility tooling** (`tests/ios_vm.py`, `docs/ios-vm.md`): host and
         confined cached-container KVM creation, disk/RAM budgets and immutable
         registry metadata checks passed; 20 host-side regression tests passed.
         `just ios-preflight` and `just ios-vm-check` expose the checks. A later bounded experiment acquired official signed recovery media and booted
         Recovery only. Installed macOS/Xcode, SwiftUI rendering,
         XCTest and Rust-library linking remain unproven in the milestone above.
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
SwiftUI/UniFFI remains the proposed client direction. Simulator/SwiftUI/XCTest/Rust
linkage and physical-device audio were never proven.

Full source/test audit (2026-11-11): [findings, evidence and conflict ownership](docs/todo-audit.md).
Checked implementation and checked runtime coverage are separate claims; untested
native-platform and failure scenarios remain open below.

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

- [x] Explicit relay destination denies take precedence over configured
      allow networks. The private DNS/BPF VM found `deny_networks` containing
      `192.168.1.254/32` was bypassed by `allow_networks = ["192.168.1.0/24"]`:
      the relay attempted a connection and returned Unreachable instead of
      AddressRefused. The integrated correction separates explicit denies and
      checks them first while retaining operator exceptions for built-in private
      ranges. Nine units/seven integration tests pass, covering overlapping
      IPv4/IPv6 and mapped/boundary cases. Both actual VM cases refuse a listening
      denied alias before any target packets are sent. The failed trial is
      preserved; root source/binary/format checks match the tested product.

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

- [x] Code blocks: an accessible copy-to-clipboard button (`view.rs`, `selection.rs`;
      clipboard assertions in `screenshots.py`).
- [x] Message text is selectable; right-click for Copy message. Image menus offer
      Open image and Save image as (`selection.rs`, `menus.rs`).
- [x] Image menu: Copy image to the clipboard. Linux backend offers real image MIME
      data; private GNOME test compared externally pasted PNG pixels to the fixture.
      Separate native Windows/macOS runtime checks remain open.
- [x] Server rail mute badge: larger microphone glyph and fixed square sizing.
      Connection notices wrap at zoom, bound long text in a scroll area and keep
      a compact accessible Dismiss control inside the banner. Real GNOME tests
      passed at 100/125/150/200% zoom and 1280/900px widths, with pointer and AT-SPI
      dismissal; Linux-only runtime evidence (2026-11-11).
- [x] Clickable links and a safe supported subset of Mumble HTML formatting (bold,
      italic, lists, headings, colour; tables normalized to text rows): no remote resources, link
      scheme allowlist, deceptive-anchor tooltip, libFuzzer targets.
- [x] Paste images into the composer (Ctrl+V): re-encoded in the sandboxed content
      worker to fit the server's image limit, sent like stock Mumble.
- [x] Attachment tray: paste, paperclip or drop images and files; Enter sends text and
      images (one image per message, paced under the server's rate limit: Murmur
      drops excess messages silently) and hands files to the transfer.
- [x] Image preparation status for the current conversation; WebP is decoded and
      re-encoded as an image. HEIC goes as a file.
- [ ] Cross-conversation queue feedback: an image pending in another channel of
      the same server can block text without showing preparation status here.
      A per-bookmark rate-limit override remains open.
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
      Windows uses a less privileged AppContainer (checked in the Windows VM).
      Remaining: `\text` in other scripts than Latin lacks glyphs (one font).
- [ ] Large file transfers peer to peer, not stored on the server: the server only
      relays (perhaps a small chunk cache, content-defined chunking); resumable
      across restarts and lost connections; clearly shown while still uploading.
      On Mumble servers (compatibility mode): done — a magic-wormhole receive code
      in the message, refreshed after each pickup, in a sandboxed transfer worker;
      Nuschl shows offers as file cards. Nuschl servers get their own, end-to-end
      encrypted design (chunked, resumable; magic-wormhole cannot resume).
- [ ] Files on Nuschl servers: designed in docs/files.md (2026-11-11), the user's
      decisions in §11 folded in: *kept* files on the server (100 MiB / 10 GiB
      defaults), *live* files sent magic-wormhole style (direct first only when the
      user's peer policy permits it, with the server as relay and a piece cache;
      relay-only omits peer discovery/IP advertisement) from the sender or recipients who help
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
      settings; verify startup error dismissal when there is no selected conversation
      (a Dismiss button already exists in a selected conversation).
- [x] XDG Base Directory layout (crates/session/src/layout.rs, docs/desktop.md "Where Nuschl
      keeps its files"): configuration only in `$XDG_CONFIG_HOME/nuschl` (settings,
      bookmarks, managed.json); identity, end-to-end keys, history and security records
      in `$XDG_DATA_HOME/nuschl`, each worker's folder apart; last channels and the
      update state in `$XDG_STATE_HOME`; channel trees and icons in `$XDG_CACHE_HOME`.
      The first start moves an earlier profile file by file (never over a newer one,
      interruptible, reported); servers.json is split. The Home Manager VM test checks
      that `~/.config/nuschl` holds no identity after a session.
- [x] Rename migration retires the old Windows containers and revokes their path
      grants, including the broad data-folder Modify grant (`session/rename.rs`,
      sandbox retirement mock regressions; SECURITY.md, "Where secrets live").
- [ ] Validate migration from an actual earlier Windows installation in the VM.
- [x] Crypto worker: exclusive `keys/` ownership through a persistent `store.lock`,
      acquired before either key source is read/created and retained until exit.
      Contention and OS lock failures fail closed. All 49 crypto-worker tests pass,
      including cross-process contention/crash release and drop-before-reopen
      link/MLS fixtures; no new dependencies or unsafe code.
- [ ] Exercise the new crypto-store contention/crash paths on Windows/macOS;
      the current runtime evidence is Linux only.
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
- [x] Dark faint-text contrast raised to 6.04:1 on the main background and 4.68:1
      on selected rows; desktop tests and real GNOME clipboard/UI checks passed.
- [ ] Ctrl+scroll / pinch zoom; IME placement at zoom ≠ 100 % untested.
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
- [x] Shared native welcome document-loss bug (originally seen on Windows as
      stripped tags and U+FFFC boxes): the worker-produced document is retained
      and rendered through the shared document blocks. See the stock-server
      welcome proof below; the packaged runtime proof is Linux, not Windows.

Limits review (user, 2026-11-11): hard limits are themselves an availability risk
when chosen too low or handled badly (cf. sshd skipping work for long passwords).
Prefer process resource limits on the sandboxed workers (memory, CPU) and treating a
worker that runs out as "this message cannot be shown" for that message only; where
a limit stays, it must page or say so visibly, never silently drop.

- [x] Search pages within its 96 KiB budget using a cursor; UI continuation and
      byte-budget/reach-every-match tests exist (`storage/src/lib.rs`, desktop search).
- [ ] Session budgets (4,096 channels, 16,384 users, 64 MiB of blobs): evict cached
      blobs by use rather than refusing new ones; review the counts against the
      largest real servers; keep the visible notice.
- [ ] Content worker decode caps (4096×4096, 64 MiB, 8 images, 640 KiB): lean on the
      worker's memory and CPU limits instead, and show a placeholder ("could not be
      displayed") when the worker fails on a message.
- [ ] Crypto-store availability: sealed files have a 16 MiB read cap but writes do
      not enforce a matching readable bound (`crypto-worker/src/store.rs`). Growing
      retained archive bindings can become unreadable after restart. Resolve with
      bounded/log-structured persistence and explicit failure handling; never
      silently discard keys. Source-level finding, not an oversized-profile repro.

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
- [x] L8 Windows VM HTTP control uses fresh per-boot bearer authentication, bounded
      requests/results and a flat file allowlist. Host tests and two existing-guest
      boots passed, including rejected missing/stale tokens.

Code quality and performance review (2026-11-11):

- [x] Content worker: lazy syntax highlighting, one worker per history page (a
      40-message page: 1.3 s → 10 ms in debug), pages off the storage loop.
- [x] Settings changes keep the capture chain; bitrate/loss only touch the encoder.
- [x] Level events only in the Settings check.
- [x] Channel tree built in one pass (2,000 channels / 5,000 users: 74 ms → 1 ms).
- [ ] Each `ChannelState` still does two linear searches; the "Linked with" tooltip
      looks names up linearly every frame.
- [ ] Visible message rows rebuild StyledText highlights and avatar lookup scans
      users. Documents/text are already shared/cached; profile the remaining work.
- [x] Image-format decoding runs in the content worker; the UI receives bounded
      RGBA and converts pixels for GPUI (`document.rs`). Bookmark saving is off-thread.
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

- [x] Timeline layout renders visible rows using a virtualized list.
- [ ] Memory while scrolled far up remains unbounded: page/evict the active timeline
      in both directions. Current trimming happens on reopening or inactive timelines.
- [x] Keyboard and screen-reader navigation of the timeline (2026-11-11): one Tab
      stop, ↑/↓, Page Up/Down, Home/End (Home loads older history), Enter/Space
      expands; AT-SPI ScrollTo and GrabFocus on any entry.
- [ ] The channel description shows only at the beginning of local history.
- [x] A 1000-message resize/screenshot scene exists (`screenshots.py`).
- [ ] Explicit long-history memory/eviction regression across older/newer paging;
      the screenshot scene does not prove a universal 1000-message retention cap.
- [ ] Installs without Nix get no desktop entry (no icon on Wayland); no icon for MSVC
      builds; no AppStream metainfo.

---

## Now: client compatibility with stock Mumble servers

### 1. UDP voice transport

Voice uses encrypted UDP when reachable, with TLS TCP fallback; crypto state stays
in the session (`crates/session/src/connection.rs`).

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

The media worker uses per-speaker adaptive jitter buffers and Opus loss concealment.

- [x] Per-speaker jitter buffer keyed by sequence number, with reordering and a
      bounded, adaptive target delay. Drop stale audio instead of building latency.
- [x] Opus packet-loss concealment for gaps; use in-band FEC if `opus-pure` supports it.
- [x] Handle speaker start/stop (`end` flag) and sequence resets cleanly.
- [x] Per-user local volume and local mute (the packet `volume` field is always 1.0).

### 3. Transmission modes and capture processing

- [x] Voice activation as the default, with a threshold, hold time and level meter.
- [x] Push-to-talk with global shortcuts through the XDG portal on Wayland and
      native Windows/macOS adapters; continuous transmission remains an option.
      Portal tested on GNOME; Windows/macOS hotkeys are cross-compiled, not exercised.
- [ ] X11 global shortcut adapter.
- [x] RNNoise enabled by default, bypassed when upstream suppression is detected.
- [x] Sonora AEC in the capture path with a playback reference; upstream AEC reuse
      disables Nuschl's own canceller.
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
- [x] Keep replacing the media worker on capture-route/privacy changes so blocked
      native teardown cannot keep the microphone open (decided). In the shared
      multi-server worker, not every server mute/deafen change needs replacement.
- [ ] Investigate unmute → first-speech latency (user, 2026-11-12; active owner).
      Measure worker/device startup, first captured PCM, VAD/encoder admission and
      first transmitted/received audio separately against stock Mumble. Preserve
      microphone release while muted; do not reduce latency by recording without
      explicit unmute consent. Include repeated mute/unmute and route changes.
      Pulse stream startup, the initial supervisor debounce and bounded VAD onset
      retention are corrected through `769c479b`; the detailed remaining physical
      device, cold-graph, noise-classifier and playback-continuity work remains
      listed below. Private synthetic timings do not complete this investigation.
- [ ] Complete receive/output parity against stock Mumble with independent PCM
      and device proofs before claiming 1:1 quality. The stereo/gain source
      checkpoint preserves native/browser LR and volume; independent native
      mono/stereo/overshoot/120 ms fixtures and browser voice/privacy checks pass.
      Native queue duration and variable-packet tick advancement are corrected;
      browser storage retains full 120 ms packet tails across its startup hold.
      The linear-SRC audit found 44.1 kHz passband loss of 3.06 dB and synthetic
      aliasing at -7.14 dB versus reference Speex quality-3 -85.14 dB. Sonora's
      fixed32-tap candidate was measured and rejected. The reviewed scalar Rust
      Speex vendor and exact-ratio adapter now pass direct stereo reference PCM
      at 135.8–156.2 dB, including ten-second 11,025/22,050 Hz clocks and exact
      callback chunks. Callback allocation/underrun/delay units and private
      negotiated 22.05/44.1/96 kHz output pass. A stricter 44.1 kHz-render/48 kHz
      capture AEC reliability gate remains open: separate new-SRC attempts removed
      0.09–2.32 dB, but an unchanged-graph ABBA comparison gave old/new/new/old
      12.88/19.20/32.39/37.20 dB. This exposes baseline variability rather than a
      consistent SRC regression. Keep the 15 dB gate and investigate monitor
      conversion/timestamps; do not claim stable non-48 kHz AEC. The old
      monitor-reference 44.1→48 SRC separately lost 3.65 dB at 15.435 kHz and
      imaged at -14.41 dB versus reference -94.96 dB. Its filtered follow-up now
      passes full-stereo reference PCM at ≥134.5 dB, exact chunks/clocks and
      impulse/timeline delay-sign tests; 58 native units pass. Same-graph ABBA
      old/new/new/old removal is 0.0/45.26/44.12/4.44 dB; a fresh session's
      original best-of-two-on check passes at 27 dB/9% sent. Preserve all earlier
      failures and the unchanged 15 dB gate: this supports a bounded reference
      fix, not general AEC reliability. Microphone CaptureConverter remains linear;
      audit its non-48 kHz passband/aliasing and exact clocks separately without
      changing startup/consent. Integrated distribution checks,
      browser sequence-clock jitter/PLC, actual browser stereo destination capture,
      matched complete reference-Mumble multispeaker/loss playback and physical
      listening remain open. See `docs/output-quality.md`; bitrate and synthetic
      tone SNR alone do not establish perceptual equivalence.
- [x] Correct the independent ordered SILK/DTX decoder mismatch (2026-11-12).
      The frozen FEC-enabled stream contains valid padded one-byte coded DTX
      frames at packet 2/22. libopus conceals those; Rust incorrectly range-decoded
      them as ordinary SILK. The bounded safe-Rust correction makes all 46,080
      samples and 24 entropy ranges bit-exact, versus the preserved 6.30 dB
      failure. Full FEC framing validation now precedes fallback/leading PLC,
      malformed `09 00` preserves state, FEC reports its actual canonical range,
      and accepted packet mode stays distinct from DTX synthesis mode. Scoped
      synthetic 48 kHz regressions cover ordered/FEC/PLC, durations/channels,
      padding/mixed frames and mode/reset transitions. See `docs/opus-silk.md`;
      preserve `~/.cache/nuschl-output-quality/codec-lbrr-failure`. This does not
      complete browser/native pipeline verification or full Mumble parity.
- [ ] Fix direct `opus-pure` 8/12 kHz decoder-API SILK resampling mismatch.
      The independent broader synthetic audit matches entropy but yields only
      about 1.7–8.5 dB PCM agreement for ordered WB/MB SILK downsampling at those
      rates; FEC/PLC inherit it. The strict broad `--require-parity` gate remains
      failing, with baseline/fixed per-case evidence retained. Production
      native/browser receivers decode at 48 kHz and convert device rates
      separately; do not broaden this into their conversion paths or claim full
      codec conformance. See `tests/opus_silk_fec.py` and `docs/opus-silk.md`.
- [ ] Audit direct `OpusDecoder::last_packet_duration` metadata compatibility.
      The current Rust API documents retaining the last real packet duration
      through PLC/FEC; libopus updates `OPUS_GET_LAST_PACKET_DURATION` after
      those calls. This pre-existing API difference is separate from the fixed
      PCM/range behavior. Check consumers before changing its public semantics.
- [x] Bound shared/native loss advancement by the nearer known future packet.
      After 120 ms followed by a 10/20 ms sequence hole, conceal only that exact
      span instead of skipping the queued packet. Independent frame/gain/next
      sequence checks and regression units pass; no target/catch-up/concealment
      budget retuning. Historical extraction trace equality remains scoped to
      that checkpoint; this correction intentionally changes those bug cases.
- [x] Verify the integrated CELT PLC correction in native/actual-WASM playout
      components. The frozen 120 ms→20 ms loss improves 28.21→81.59 dB against
      independent libopus at fixed gain and unfitted delay; all six independent
      action cases pass in both engines after admission validation. The original
      332 codec cases and prior 241 cases remain separate proofs. Preserve failed
      evidence under `~/.cache/nuschl-output-quality/celt-plc/`; actual browser
      worker/Worklet activation remains the open row below.
- [x] Reject full malformed Opus framing before native/browser jitter or speaker
      admission. Both invoke allocation-free `packet::validate` before time,
      sequence or gain changes. The frozen `09 00` plus subsequent valid packet
      improves 6.34→71.26 dB native / 70.18 dB WASM against libopus; frame sizes,
      gains and rejection results now agree in native/actual-WASM components.
      Unit tests also prove rejection cannot create a native speaker or advance
      its arrival clock. This checks packet packing, not entropy authenticity;
      worker activation and scoped confined-browser checks are completed below.
- [x] Integrate the verified clocked browser playout checkpoint (`1d1cf926`,
      `c5a81933`, `9827d211`). The combined packaged App proof passes too. Shared native
      jitter/FEC/PLC through actual-WASM playout and a direct bounded Worklet
      port, with separate RX authorization and TX epochs. Mic-only changes keep
      listening; route/deafen/connection revocation clears it. Global 16 RX/two
      codec TX credits survive epoch changes until exact ACK; unsolicited or
      stale capture replies cannot grant current credits or transmit.
      Native/actual-WASM six-case independent oracles, 30 main/Worklet tests and
      twelve worker/WASM checks pass. Confined Chromium/private Murmur controller
      passes RX continuity, actual PCM and privacy; exact packaged App proof also
      passes production CSP, live-call reorder/text/privacy and 20 packets with
      zero timeline snapshots. Every playback re-enable uses a fresh RX epoch
      so delayed disabled barriers cannot discard newly authorized data. No
      per-packet UI PCM/state forwarding or stacked startup hold. Sixteen aligned
      non-silent C-reference streams at 10/20/120 ms measure max 0.96/1.66/4.57 ms
      per render here; no universal scheduling/device quality claim. See
      `docs/browser-audio.md` and the source-bound handoff.
- [ ] Review inherited unknown-first-packet duration estimation separately.
      The policy initially assumes 20 ms before a decoder observation; a single
      non-ended 120 ms packet waits for the next packet. Deterministic clocked
      startup is 40/30/130 ms for regular 10/20/120 ms packets, versus 10 ms for
      one ended 120 ms packet. These exclude sender/codec/device delay. Preserve
      native policy equivalence until a separately measured correction is reviewed.
- [x] Integrate the verified bounded SDK-RPC retirement checkpoint (browser
      owner; `3fb219f7`, `d15780fa`, `854582b5` plus final proof/docs). Global two
      SDK audio-send credits now survive epoch changes and the 250 ms capture
      timeout until a real reply or permanently retired owner settlement. The
      generic 128-command adapter closes its owner and requests termination
      before synthetic rejection, revokes voice/consent, ignores stale owned
      callbacks and offers Unavailable/Reload without clearing draft/cache.
      36 scoped unit checks pass. Real private Chromium stall fixture fills
      exactly 128 total/two audio requests, then proves no new posts/results,
      stopped tracks and retained selectable draft/bookmarks/certificate/history
      through the late worker tail. Ordinary calling/text/order/CSP pass too.
      Physical thread exit is not synchronous: the deliberate 17 s tail can run
      after the scheduled 15 s retirement. Preserve all three stronger failed
      assertions and the measured close delay; see `docs/sdk-rpc.md`.
- [ ] Bound any future automatic SDK-worker replacement across retiring owners.
      The current fix permanently closes the old owner and does not auto-spawn.
      `Worker.terminate()` has no physical-exit acknowledgment; a future retry
      must not multiply still-retiring workers/inboxes or restore old consent.
      No guarantee is made about previously submitted network traffic delivery.
- [x] Integrate the verified browser SDK startup watchdog (`8a235afe`,
      `40bd8a5c`; browser owner). An explicit starting/ready/closed owner has a
      separate 30 s initialization deadline even with zero RPCs. Only its first
      ready after one init clears that deadline/autoconnects; duplicate/late
      ready cannot bootstrap again. Constructor/init-post failures and loader
      stalls expose a global persistent Unavailable/Reload action with no
      selected or saved server. Existing permanent retirement, failed-terminate
      credits, cache/draft and microphone-consent rules are preserved.
      All 44 scoped Node checks pass; actual packaged/CSP Chromium covers the
      held WASM loader with zero ordinary RPC/mic, true zero-server 320 px
      recovery, unchanged saved pin/history, pending setup draft and exactly one
      private invite connect despite three duplicate ready events. Constructor/
      post draft checks are N/A because those failures are synchronous. Normal
      App/voice/text/order/privacy regression passes separately. See
      `docs/sdk-rpc.md` and `~/.cache/nuschl-sdk-startup/handoff.json`; no physical
      thread-exit or real-time deadline guarantee and no auto replacement.
- [ ] Complete browser scheduling/device/reference receive parity beyond the
      scoped clocked pipeline fixtures: suspended/background tabs, Safari and
      Firefox, physical device latency/AEC, hostile entropy decode failures,
      long multi-speaker loss/reorder and reference Mumble jitter/PLC behavior.
      The node silence stress and private Chromium results are not universal
      real-time/perceptual guarantees. Keep all retained baseline failures.
- [x] Match Mumble's explicit `OPUS_SET_PHASE_INVERSION_DISABLED(1)` receive
      setting. The pinned, minimally patched opus-pure 0.2.3 exposes its existing
      synthesis control with upstream/generic defaults unchanged; only native
      Speaker/browser VoiceDecoder opt in. Independent 16/24/32/48 kbit intensity
      fixtures and actual native/WASM+Worklet stereo/mono comparisons pass;
      explicit SILK/Hybrid/CELT+channel transitions and reset retain the control,
      with entropy ranges unchanged. Native 67/audio-WASM 4/Node 14/API 30 tests pass.
      Preserve published-default mismatch evidence; no full perceptual parity claim.
- [x] Correct native mono-device clipping/folding before the 48 kHz mix boundary.
      Actual one-channel output folds full f64 LR plus local effects before
      protection, then duplicates heard mono into the existing ring/reference.
      Negotiated private 1ch 48k fixed-gain proof improves decoded overshoot 14.13→
      76.51 dB and formerly silent gain 4 opposing LR→88.53 dB; quiet 66.10 dB and stereo
      regressions pass. 68 native units include allocation-free converter/reference
      correspondence and unchanged exact clocks. Preserve the failing baseline.
- [x] Produce independent native receive/output spectrograms and float32 audio:
      tests/output_spectral.py drives the actual Mixer and PlaybackConverter with
      identical C libopus packets, then compares C decode/Speex 3 at fixed gain and
      unfitted clocks. Multiband/transient/speech-like/stereo-music cases at 48/44.1 kHz
      agree by 127.44–194.26 dB; isolated 44.1 kHz SRC agrees by 145.76–147.64 dB, with
      bit-exact callback chunk changes. Five negative controls protect measurement
      sensitivity. Input/codec lossy error is separate; no native TX/network/device
      listening or full executed-Mumble/perceptual equivalence follows. Actual
      plots/WAV provenance: docs/reviews/2026-11-12-output-spectral.json.
- [x] Replace actual native linear capture SRC with the existing memory-safe
      mono Speex quality 3 engine through a narrow capture helper. Preserve finite
      full-channel average/unit input clamp, rational generated clocks despite ring
      overflow and capture-side filter/phase timestamp compensation; flush actual
      input immediately without whole-callback prefill. The original 21/26 failed
      baseline remains preserved, including dropped 96 kHz impulse and 0 dB alias.
      Final 31 quality/clock/marker cases and five real-PCM negative controls pass;
      ordinary output agreement is 133.60–161.11 dB. Stopband raw output-relative
      100 dB diagnostic remains false (77.98 dB); declared input-relative 139.99 dB,
      −96.61 dB attenuation and unchanged absolute/C-relative rails pass. Native
      81 units, observed private 44.1/96 kHz CPAL capture/unmute/privacy/AEC and
      the whole original DSP suite pass. See docs/capture-quality.md and
      docs/reviews/2026-11-12-capture-filtered.json; no TX/DSP defaults/selector change.
- [ ] Expand capture fidelity proof to physical devices and Windows/macOS,
      matched cold-source timing, multichannel/clock/backend variation and a full
      input→DSP→encoder→network→output listening comparison. Keep the strict
      stopband diagnostic and original failures visible; numerical floor handling
      is not bit-exact or perceptual parity. Two-attempt private AEC convergence
      results do not establish general reliability; original consent and queue
      limitations remain open. No physical host audio was used in these proofs.
- [x] Correct the measured native non-48 kHz clipping/SRC defect together with
      fallback correspondence. Keep f64 mix/mono/FX headroom through the 200 ms
      ring, fixed bounded binary scaling around f32 SRC, then map/clip the device.
      Reconstruct actual clipped/quantized front-LR playback to 48 kHz with rational
      clocks and compensated reverse-filter delay; skip work if its consumer is
      dropped. Actual identical-packet 44.1 kHz overload improves 20.03→127.31 dB;
      all stock-order/component gates pass. Native 76 tests, 20 independent C
      reference/clock cases, mono/i16/overflow/reopen/no-allocation checks pass.
      Direct fallback plus independent C room yields 58 dB/0% sent; actual private
      monitor runs pass 46 dB at 48 kHz and 36 dB at 44.1 kHz, 0% sent. Original failed
      spectral/AEC evidence remains preserved. See docs/output-quality.md and
      docs/reviews/2026-11-12-output-clip-order.json.
- [ ] Extend output/AEC validation beyond this synthetic correction: an actual
      opened CPAL fallback device (current fallback proof directly runs production
      converter/AEC with an independent C room), physical devices, Windows/macOS
      runtime, multivoice/room/overload listening and matched full stock-client
      input→encoder→network→output remain unproven. Short paired callback timings
      and one passing private 44.1 kHz monitor run do not establish general non-48 kHz
      AEC reliability; retain earlier failures and all consent/mute queue TODOs.

- [x] Preserve legitimate per-speaker decoded headroom until the final mix.
      Native and browser queues now store raw finite PCM with per-frame gain,
      multiply/sum in f64, then clip after all contributors (including native
      local effects). Opposing 1.5/-1.25 recovers 0.25; finite f32MAX×4 stays bounded.
      Independent legal-source/two-stream libopus sums match native at 119.21 dB
      and actual Worklet exactly; 60 native/14 Node units pass. Same 200 ms frame
      limits, zero native mix allocations, bounded persistent browser scratch;
      measured CPU/memory cost is documented in `docs/output-quality.md`.
      Full reference-Mumble multispeaker/loss/device parity remains open.
- [x] Integrate verified native encoded-packet gain ownership (`acf7fc66`,
      `d2e7631f`; browser/audio owner). Typed native jitter payload retains each
      sampled gain through reorder; FEC/PLC keeps the preceding known gain.
      The shared jitter policy, decoded-frame/f64 mix, limits and callbacks are
      unchanged. Nine strict independent actual-Mixer/libopus fixtures pass:
      ordered 4.63→326.50dB, loss/reorder 3.74→324.93dB, stale-controls −0.33→322.52dB,
      long-to-short loss 84.42/81.12dB; constant-gain controls remain unchanged.
      All 70 native tests pass (5 diagnostic ignores), including zero callback
      allocations; original private DSP and 22050/44100/96000 stereo SRC pass.
      Preserve failed baselines and scoped timing/RSS observations. This does
      not close live local mute/gain semantics or general quality/AEC parity.
      See `docs/output-quality.md` and `~/.cache/nuschl-native-gain/handoff.json`.
- [ ] Define native live local gain/per-user mute behavior for already queued
      audio. Current IPC carries a product sampled per forwarded packet; local
      mute drops future packets upstream, but no per-user flush/live-gain worker
      command exists. Typed gain fixes packet ownership, not immediate local
      control. Review decoded/device queues and PLC tail before broader IPC work;
      keep stream deafen/disconnect revocation and microphone consent separate.


### Optional local speech (requested 2026-11-11)

- [ ] Voxtral Mini 4B Realtime streaming captions through an isolated inference
      helper. Use `~/Projects/whisper-cpp` as the llama.cpp/model reference; its
      current adapter uses ROCm and file chunks, so live latency remains unproven.
- [ ] Native inference backends: Linux Vulkan and optional Nix ROCm package variant,
      Windows Vulkan, macOS Metal, CPU fallback; benchmark the exact pinned model,
      required audio encoder and runtime on each supported backend.
- [ ] Kokoro TTS: evaluate memory-safe runtime/phonemizer first, otherwise isolated
      native/ONNX backend; local playback and explicit transmit controls.
- [ ] Verified, resumable, on-demand model/voice downloads with size/progress,
      cancellation/removal, disk-backed cache and offline reuse. No default weight
      downloads or downloaded executable/pickle model code.
- [ ] Generation-safe per-speaker PCM taps and bounded partial/final captions;
      ephemeral local transcripts by default, explicit sharing/persistence, no
      audio-callback inference, no TTS feedback, no capture/mute/permission bypass.
- [ ] Sandbox/resource/failure tests and call-quality measurements under GPU load;
      desktop controls and accessible download/caption/TTS states. Full design,
      dependencies and platform evidence requirements: [docs/speech.md](docs/speech.md).

### 4. Session robustness and identity

- [x] Automatic reconnection with backoff; restore channel and mute/deafen state.
- [x] Gap markers in the chat timeline for disconnected intervals.
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

Initial handshake, channels/users, chat and transport are handled, along with the
permission, administration, blob, listening, context-action and statistics messages
listed below. Plugin data remains the explicit gap.

- [x] `PermissionQuery` (20): cache per-channel permissions; disable join, speak,
      text and create-channel actions the user lacks.
- [x] `VoiceTarget` (19): whisper/shout to users and channels (with links/children),
      bound to push-to-talk keys.
      Targets are chosen from context menus; dedicated whisper hotkeys are not done.
- [x] Channel links and channel listening (Mumble 1.4+ `listening_channel_add`).
- [x] Channel descriptions, user comments and avatars through `RequestBlob` (23);
      parse in the content worker; decode images with a memory-safe decoder in a worker.
- [x] Inline images in legacy HTML messages through the content worker.
- [x] Cover the reported join/welcome-notice formatting case (user,
      2026-11-12): `<b>something.de</b>` appears as `bsomething.de/b` on joining
      a server. Preserve supported legacy HTML formatting faithfully: display
      `something.de` in bold, without literal tag names or storage markers.
      Include a domain inside emphasis in regression coverage and inspect the
      complete welcome-rendering path, including safe link detection. The exact
      sample now passes in the packaged native client and compiled browser App
      against private Murmur, including linked emphasis and escaped literal tags.
      Native rendering was already fixed; this adds regression evidence rather
      than claiming a new native correction. The original server's complete raw
      notice was not captured. Sanitization and explicit activation are retained.
- [x] Integrate the implemented web welcome-document checkpoint: bounded
      SDK-worker rendering, generation/exact-source dismissal leases, once-only
      typed document updates and a bounded mobile panel are implemented and
      scoped tests pass. Actual private stock Murmur + compiled App/CSP checks
      cover exact bold domains, inert resources, 320/390/900/1280 bounds,
      keyboard dismissal, reconnect/replacement and stale tokens. Native exact
      domain proof already passes; no new native fix claimed. Parent source,
      runtime-asset, distribution and notice checks pass; see
      `docs/welcome-domain.md` and the integration checkpoint above.
- [x] Integrate the verified live `ServerConfig.welcome_text` implementation:
      both session adapters now apply connected, exact bounded source updates;
      None preserves, empty revokes, repeats remain dismissed and fresh
      connections reset. Welcome-only updates emit no Limits/audio changes.
      Native 81/1 existing ignored, browser 27+6 and an actual compiled App/private
      TLS framing fixture pass (320/390/1280, old-token/old-connection/canaries).
      Parent combined distribution, notice and runtime-asset checks pass.
      No new GPUI proof claimed;
      see `docs/welcome-domain.md`.
- [ ] Integrate verified sparse `ServerConfig` retention/full snapshots and
      browser wire-byte text caps. Absent fields preserve message/image limits,
      max-users, recording false and known bandwidth; welcome-only updates stay
      separate. Native 82/1 existing ignored, browser 29+6 and actual compiled
      App/private TLS tests pass, including zero, escaped/newline expansion,
      retained/retried drafts, sparse changes and reconnect 5000 default. The
      original 129/128 failure remains retained and its diagnostic now passes.
      Source is integrated; parent combined checks remain pending. See
      `docs/server-config.md` and `~/.cache/nuschl-server-config/handoff.json`.
      No image-send API implied.
- [ ] Integrate the precise browser send-refusal wrapper correction: actual
      compiled App tests show exact 128/16/5000-byte reasons and retained editable
      drafts; Nothing/global-too-long/disconnected diagnostics do not reuse old
      notices. Same selected server ID/generation is checked; core/App APIs are
      unchanged. Source is integrated; parent distribution checks remain pending.
      See `~/.cache/nuschl-server-config/precise-handoff.json`.
- [ ] Reset shared `ServerState.limits` on reconnect, or publish fresh initial
      limits, when the new connection sends no ServerConfig. Source review finds
      Client::connect keeps the prior snapshot although each adapter correctly
      resets its enforcement defaults. Add a generation/stale-event model
      regression; this is distinct from the verified adapter sparse-field fix.
- [x] Integrate the separate central browser text-size correction: Settings
      previously changed body font size while rem-sized text kept the root's
      16px. `html` now uses the same preference variable; actual compiled App
      assertions cover 14/16/20px roots, message text 12.6/14.4/18px, persistence
      and largest-setting phone/desktop bounds. Keep the pre-fix measurements
      and distinguish synthetic root-font stress from physical browser zoom.
- [x] Stock-server welcome document rendering (2026-11-12): the native adapter
      discarded the correct worker document and displayed its internal storage
      markers (`bNuschl/b`, link directives) as plain text. It now keeps the typed
      document, renders it with the shared blocks, and revokes its cache across
      disconnect/reconnect/replacement. `tests/welcome_html.py` reproduces the
      old output and checks the fixed packaged desktop against a private stock
      Murmur, alongside a normal-chat HTML control: formatting, explicit breaks,
      sanitized link data and blocked remote images. Worker/client tests cover
      script/link policy, stale generations and cache revocation. This closes
      the duplicate shared-source report above; Windows GUI confirmation remains
      a platform verification gap, and general document-link activation is separate.
- [x] Integrate the verified stock-Murmur hash-only document fetch/render fix.
      The SDK now requests a selected channel's missing description once per hash
      and binds open-profile comment interest to connection/session/presence.
      Arriving comments and hash replacements render without reopening; hidden
      or replaced profiles revoke interest before event processing. Prepared and
      pending sources are reused. The private old-package baseline reproduces
      both gaps; the fixed Linux package passes first-selection/first-open,
      replacement and hidden/reopen checks against stock Murmur, with raw pinned
      loopback hash/body frames retained. SDK tests pass 99 (one existing ignored)
      and all 79 native unit tests pass; the reviewed fix is integrated. See
      `docs/hash-documents.md`. No Windows/macOS or new browser profile-UI claim.
- [ ] Audit native legacy blob hash/body normalization separately (source-level
      finding, not a reproduced exploit). A new small inline description/comment
      can leave a prior long-body hash in normalized session state, while body-only
      blob responses are not independently digest-checked. Validate incoming
      20-byte hash shapes before retaining model metadata as part of that audit.
      Murmur's synchronous current-body replies use ordered TLS; old-body-after-
      new-hash reordering is unproven. Preserve valid inline replacements without
      hash fields, current stored-source checks and HTML sanitization; blindly
      rejecting against an older retained hash would break legitimate updates.
- [x] Shared nonselectable document links (2026-11-12): welcome cards, delivered
      channel descriptions and profile comments reuse `BlockText`'s existing
      layout/hit testing, real retained link focus and the safe `open_link`
      dispatch. Pointer, Tab/Shift+Tab, Enter and assistive activation share
      server/generation/source/document-lease and visible-surface guards.
      `tests/welcome_html.py --document-links --pulse-bin …` checks all three
      callsites with a private stock Murmur, null audio and synthetic URL handler,
      wrapped Unicode, width/200% zoom focus, drag rejection, stale/replaced and
      hidden documents, reconnect and typing. Focus-only navigation reuses cached
      geometry; an eight-second profiler observation checks quiet intervals and
      zero idle geometry work. See `docs/desktop-document-links.md` for evidence
      and limits. Hash-only retrieval/render is now verified above; the separate
      legacy hash/body normalization audit remains open.
- [x] `UserStats` (22) for the connection-info dialog.
      Profile bandwidth units corrected (2026-11-12, `d8f5999d`): the protocol
      statistic is bytes/second, so divide by 125 for kbit/s. Server bandwidth
      limits already use bits/second and retain their separate /1000 conversion.
- [x] `SuggestConfig` (25): respect the server's suggested PTT/version hints.
- [x] `ContextActionModify` / `ContextAction` (16/17): server-defined context menu entries.
- [x] Recording indicator and priority speaker from `UserState`.
- [x] CELT-only servers: decide between explicit "unsupported codec" notice (preferred,
      no memory-safe CELT exists) and support.
      Decided: an explicit notice; text chat keeps working.
- [x] Administration protocol support: `UserList` (18), `BanList` (10), `ACL` (13),
      `QueryUsers` (14), channel editing and moving users. This does not establish
      complete client editing parity; see the ACL/group requirement below.
- [ ] `PluginDataTransmission` (26), lower priority.
- Positional audio: skipped for now (nobody we know uses it).

### 6. Desktop UI for these features

- [ ] Restore desktop rail/footer spacing (user, 2026-11-12): Add-a-server's
      bottom edge currently touches the window, and Settings/mute/deafen have no
      gaps. The rail now has matching top/bottom padding and the compact control
      cluster an explicit gap; source is committed, private runtime proof pending.
      Check positive bottom/control gutters and reachable unclipped controls at
      small heights, narrow widths and real 100–200% zoom, preserving server-list
      scrolling and outline gutters. Assigned to the web owner after send-refusal.
- [ ] Full Mumble feature parity for ACLs and group membership (user, 2026-11-12).
      Provide complete ACL/group editors, including adding/removing/reordering
      rules, user and group targets, permission grant/deny, apply-here/apply-to-
      subchannels flags, channel ACL inheritance, group creation/removal,
      inherit/inheritable flags, and explicit/inherited member additions and
      exclusions. Resolve registered users by stable account/registration ID;
      support bootstrapping a normal account into the root admin group.
      Current desktop only changes existing permission bits, removes rules,
      adds @all rules and displays group member counts; group membership is
      read-only. Preserve unrelated ACLs/groups and reject obsolete drafts or
      retained controls after response, target or connection replacement.
      Cover the supported desktop/web/TUI surfaces and server semantics with
      private stock-Mumble/nuschld interoperability, permission-denial controls,
      inheritance/order cases and persistence across reconnect/server restart.
      Bound automatic registered-name lookup work globally, with IDs retained
      when labels are unavailable; per-group caps must not multiply into an
      unbounded request burst. Virtualize large rule/member lists or show one
      selected item's details so model limits do not become per-frame UI work.
      Track offline admin-CLI ACL/group commands in the existing server follow-up.
      Active native-editor verification also found default Murmur rate limiting
      silently drops rapid ACL requests after save/reconnect. A bounded semantic
      queue with dispatch-time authority checks is under private-server test;
      preserve immediate mute/deafen and the original draft/target. Do not close
      this until the unmodified default-rate, zero-settle matrix passes.
      Root Write must permit ACL editing on children whose local Write is denied,
      as both servers allow, without granting unrelated channel operations.
      Also require explicit apply/discard for typed rule targets before save or
      changing selection: the native prototype can display typed target text
      while the underlying new rule still targets @all. Preserve per-row/open
      identity and reject any stale form action; this correction is not integrated.
      Document the legacy wire ambiguity for an inherited group with default
      flags and empty local membership: it cannot distinguish an explicit empty
      local definition from inherited-only state; stock Mumble omits it on save.
- [x] Users listed inside the channel tree with talking indicators and mute/deafen/
      suppress icons.
- [x] Context menus: user (direct message, local volume/mute, info, whisper), channel
      (join, create sub-channel, links, description).
- [x] Integrate and verify the shared bounded context-menu surface.
      One finite viewport/decorations budget, wrapped labels, retained scroll
      handle, visible overflow scrollbar and focus trap cover every native menu.
      Tab/reverse Tab reveal first/last actions; reading keys scroll the region.
      A shared occluding hitbox prevents wheel events reaching the underlying
      channel browser. Real full-admin-menu baseline clipping and the corrected
      Linux geometry are retained. The complete private full-admin matrix passes
      first/last pointer and keyboard activation, 100/200% wrapping/placement,
      background wheel isolation, reading/dismissal and native prompt editing.
      All 82 native units and exact package/notice checks pass. The eight-second
      menu idle proof has two transport-driven draws and no new focus reveals;
      Root integration preserves the exact tested menu source bytes; native
      Windows/macOS execution remains open. See `docs/context-menus.md`.
- [x] Keep menu focus attached to a semantic action when permissions or server
      context actions change while the menu is open. Semantic keys now retain
      the original action through row insertion/removal. Real private-Murmur
      permission changes move Kick from row 12 to 11 while it stays focused;
      removing the focused Register action does not activate its replacement,
      and Tab recovers inside the menu. Integrated source matches the tested
      package; source and proof details are in `docs/context-menus.md`.
- [x] Bind delayed desktop menu actions and administrative prompts to their
      original server identity, connection generation and user presence instance.
      Menus/prompts/panels now retain saved endpoint, connection generation,
      own presence and primary user/channel lifetime instead of resolving a
      retained vector slot or numeric session alone. Root integrated and reviewed
      the authority/focus implementation and independent private proof:
      87 desktop, 107 client and 79 session units pass; real stock Murmur changes
      permission row positions, reuses a session for another certificate, and
      reconnects with a retained harmless prompt. Fresh controls succeed while
      stale actions refuse. All 17 production source hashes match the tested
      authority source; combined package/notice checks also pass. Separate
      non-menu controls and mutable panel-row findings remain open below.
- [ ] Audit retained non-menu controls for original saved-server binding.
      Source review still finds the whisper Stop banner in `view.rs` captures a
      server vector index; `stop_whisper` resolves the current slot. Removing an
      earlier bookmark can therefore change the target of a retained callback.
      Bind single-server controls to saved ID/generation while preserving
      deliberate persistent/global voice preferences. This is a separate source
      finding; the menu/prompt/panel proof does not reproduce that UI race.
- [ ] Review bounded metadata retry acknowledgement after an actor lease refusal.
      The SDK marks a blob request sent once enqueue succeeds; a later native
      actor refusal currently reports a generic notice. An own-presence change
      without a generation/hash change may leave a selected description/avatar
      request pending. This is a source-level audit, not a stock-Murmur runtime
      reproduction. Preserve strict target guards and bound any own-epoch retry;
      do not add per-frame retry loops.
- [ ] Give mutable administrative panel drafts stable response/row identities.
      Source review finds ACL rules and ban-list controls use vector positions;
      replacing a response for the same server/channel while a panel remains
      open can overwrite an unsaved draft or change which row an old control
      edits. Registered-user controls instead use persistent account IDs, not
      online sessions; review response replacement separately from user-presence
      leases. This is a separate source finding, not covered by menu geometry,
      semantic context-menu focus, or original server/connection target guards.
- [x] Close the enqueue-to-consume authority gap for delayed native actions.
      The former native queue could survive automatic reconnect with only numeric
      target existence checks. Bounded target-instance envelopes now bind work at
      enqueue and revalidate in the session actor, including current known
      permissions. Administrative panels retain the same original-target binding
      as prompts; installed whisper targets stay bound for subsequent voice
      packets. Expired whisper targets suspend voice until explicit reselection,
      with no fallback to channel broadcast. Focused regressions and private
      proofs cover these integrated guards. Known cached numeric permissions are
      rechecked locally; Nuschl edit/delete ownership and moderator authorization
      remain authoritative server decisions, with exact conversation coverage.
      No local message-ownership cache is required or claimed.
- [ ] cpal's PipeWire host skips `Audio/Source/Virtual` nodes (e.g. filter-chain or
      EasyEffects outputs), so they can be used only as the system default, not picked
      in Nuschl. Fix upstream in cpal or list them through the PulseAudio API.
- [x] Audio settings: input/output device, transmission mode, VAD threshold with meter,
      PTT key, processing toggles and per-device override.
- [ ] Shared Opus bitrate controls in desktop, TUI and web for legacy Mumble and
      nuschl servers. Persist the requested target; show the effective encoder
      target and why it is limited. Respect each destination's advertised bandwidth,
      including packet/transport overhead, and recompute on server updates/reconnect.
      The requested mono voice default is now 128 kb/s on native and web; lower
      ServerSync/ServerConfig budgets take precedence, with a 24 kb/s conservative
      Mumble framing/transport reserve. Tiny budgets disable capture, not playback.
      Live changes reject over-budget frames and update isolated encoders;
      browser route epochs also revoke prior queued media.
      Persistent controls/effective-rate UI and the full private-server matrix
      remain open; synthetic codec/budget regressions cover the default slice.
- [ ] Complete input-fidelity encoder verification (user, 2026-11-12).
      Audio activation is integrated as `95a1a27f`: all native/browser production
      TX factories use Audio, retaining the 128k requested target, server budget
      and reserve, mono 20 ms packets, complexity, CVBR, FEC and privacy. No silent
      VoIP fallback when server budgets force 32/48k. Native 71 passed / 5 existing
      ignored, audio-WASM 8 passed, actual 20 native/WASM encode paths and
      independent C decode pass; the full private Pulse DSP suite also passes.
      Queried lookahead is Audio 312 samples versus RLD 120: a 4 ms codec benefit,
      not full call latency. RestrictedLowDelay at 64/128k remains OPEN: Rust 64k
      transient scores fall 13.54→10.01 dB native / 13.77→10.01 browser, unlike
      matched C browser 13.78→14.05; 128k browser speech falls 25.81→23.75 versus
      C 25.79→25.73. Preserve loss/burst and sparse-transient bitrate/window/
      analysis findings before adopting RLD. Do not trade fidelity away for 4 ms.
      Separate portability diagnostic OPEN: native/WASM encoded-output equality
      gives 29.53–47.42 dB; the first 32k speech case gives 39.77 against a new
      60 dB probe. Actual WASM source scores stay close to matched C (9.336 versus
      9.503 there). This establishes neither bit identity, corruption nor
      perceptual equivalence. Requested native FEC 5% also selects hybrid for
      parts of matched C Audio cases while Rust stays CELT; verify actual coded
      FEC/loss behavior. Browser complexity 5 versus native 9 and browser/native
      capture preprocessing remain separate gaps. All 1,008 standalone synthetic
      rows, actual encodes, loss controls, fixed-gain spectrograms/float WAVs and
      the initial failed diagnostic remain in the evidence; see
      `docs/opus-encoding-quality.md`. Keep real capture, non-48k CaptureConverter,
      listening and broad input→output transparency open. Receive/SRC tests alone
      cannot close TX fidelity, and lossy Opus is not identical PCM.
- [ ] Design an optional music preset alongside the voice default: proposed voice
      128 kb/s mono at 48 kHz; music 128 kb/s stereo at 48 kHz, with 96/160/192 kb/s
      choices subject to the same server budget. Music uses `Application::Audio`,
      an explicitly selected continuous source and speech-processing/VAD bypass
      only for that music track. Selecting it must not unmute, acquire microphone
      consent or alter another track's privacy/processing policy. Verify stereo
      send/playback and voice/music isolation on every client; no preset exists yet.
- [x] Avoid blocking native microphone processing on PulseAudio reference-stream
      startup; retain up to 80 ms of processed VAD onset from the current unmuted
      epoch, with bounded/paced output and strict mute/PTT/echo-guard resets.
      Private synthetic worker timings and independent libopus marker proof are
      documented in `docs/unmute-latency.md`; no always-open muted microphone.
      A first authorized talk route skips the supervisor's 30 ms move debounce;
      active-server moves still coalesce with unchanged revision/epoch guards.
- [ ] Measure remaining unmute latency end-to-end on physical Pulse/PipeWire,
      Windows and macOS. A fresh virtual Pulse source still takes ~1.3 s before
      first PCM (independent libpulse reproduces it); classifier evidence takes
      input frames, and onset retention can queue up to 80 ms. No claim of zero
      cold hardware latency or complete GUI-to-peer timing proof.
- [ ] Preserve incoming playback across native microphone mute/unmute and speaking
      destination changes. The combined worker currently restarts, interrupting
      listening; separate playback lifetime/capabilities without retaining a muted
      microphone stream or replaying old capture into another destination.
- [ ] Investigate the phase-sensitive fresh-noise VAD false positive: `tests/unmute_noise.py`
      can transmit deterministic white noise with old and updated workers;
      some old-worker phases stayed closed, so distinguish source/callback phase
      from classifier behavior with identical-frame replay.
      Explicit low-latency controls can pass with zero packets on both paths;
      same-input diagnostic replay can admit noise with zero-reference AEC on.
      Keep strict zero-packet assertions and raw source/reference evidence; do
      not hide the context-dependent result with startup suppression, weaker
      noise tests or another unmute delay.
- [x] Connection status: ping, UDP/TCP indicator, packet loss.
- [x] Error/reject dialogs from section 4.
- [x] AT-SPI accessibility coverage through GPUI AccessKit (`tests/accessibility.py`,
      private VM/GNOME session).
- [ ] Actual Orca pass over these flows; AT-SPI coverage alone does not establish it.

### 7. Tests (own servers only)

- [x] Stock Mumble GUI and Nuschl client in a private VM channel with two-way voice
      and chat (`stock_client.py`).
- [ ] Two-stock-client GUI baseline to independently validate the fixture.
- [x] UDP voice and forced TCP separately; block UDP mid-call and verify fallback and
      recovery.
- [x] Loss and jitter injection (`tc netem` in the VM) for the jitter buffer.
- [ ] Explicit deterministic packet-reordering injection in the live integration test.
- [x] Older stock servers (1.3.x, 1.4.x) for the legacy UDP format, plus current 1.5.
- [x] Whisper/shout routing isolation and permission-gated UI.
- [x] Server-restart reconnect and history gap markers (`desktop_recovery.py`).
- [ ] TLS network drop without server restart; reconnect and history gap markers.
- [x] Native PulseAudio, PipeWire and pipewire-pulse restart recovery in private
      audio sessions; PipeWire replacement worker stays pinned to that backend
      (`pipewire_recovery.py`).
- [ ] Device switch and format changes mid-call.
- [x] Upstream PulseAudio/PipeWire echo-cancel reuse, with Nuschl AEC bypassed;
      tests snapshot defaults/levels, and PipeWire tests include chained RNNoise.
- [ ] Snapshot all unrelated graph objects before/after AEC reuse and recovery.
- [x] Media and storage worker kills preserve the desktop session/history.
- [ ] Content-worker kill and content/media/storage stalls; verify bounded recovery
      (a content deadline in code is not runtime stall coverage).
- [x] Hostile inputs for legacy HTML and fenced-code parsing (fuzzing).
- [x] Initial libFuzzer target set, run with `just fuzz <target>`
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
      cargo-zigbuild, x86_64 + aarch64 slices, `llvm-lipo`); both architectures and
      `.#macos-app` cross-built during the rename checkpoint (SDK 14.5, minimum
      macOS 14.2, checked per binary; cargo-zigbuild honours MACOSX_DEPLOYMENT_TARGET).
      Not yet run on macOS.
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
- [x] Windows VM control harness (`windows_vm.py`) and scripted updater regression
      (`windows_update.py`, historical minisign GUI evidence). The new native
      hybrid fixture passes in Windows 11 build 26200: both fixed engines and
      LPAC roles, strict bound replies/corrupt siblings, bounded stalls/output,
      and guest-loopback library fetch/apply/cleanup. Network initialization is
      denied before a socket exists; distinct SIDs and image deletion are
      supplementary evidence. See docs/windows-hybrid-update.md.
- [x] Correct the Windows GUI updater harness's omitted verifier EXEs/helpers and
      blanket same-name process stopping. It now requires/stages all nine programs
      and packaged notices, rejects operator/test tools and non-fixture trust,
      isolates install/profile/listener/files by run ID, and stops only PIDs at
      the owned executable path. Five host cases and an actual Windows same-name
      foreign-path sentinel test pass (`windows_update_test.py --guest`).
- [x] Build a separate temporary-public-trust source and prove the actual Windows
      GUI update/restart in the private VM. UIA-owned focus and the update-button
      geometry pass; both exact official-feature helper EXEs pass the full native
      adversarial fixture. The GUI verifies all 16 installed signed files, disjoint
      old/new PIDs and retired/staging cleanup. See docs/windows-hybrid-update.md
      for exact app/harness sources; later audio/source changes still need final
      integrated distribution/archive checks.
- [x] Preserve an explicitly selected profile across updater restart. The first
      run caught dropped `--profile-dir`; the fix remembers only that typed path,
      normalizes it absolute and uses native Command arguments without replaying
      URLs, offers or autostart. 82 native units and the actual Windows GUI pass:
      initial relative space/Unicode profile becomes the exact absolute restarted
      profile and its persisted update-state SHA remains unchanged.
- [ ] Investigate the secret-free VM bootstrap Run dialog left behind in one
      restart screenshot after the agent was ready. Focusing the exact owned app
      makes final captures reliable, but does not establish that automatic
      bootstrap dialog cleanup is solved; do not dismiss arbitrary guest windows.
- [ ] Complete unattended Windows desktop/audio/sandbox regression orchestration
      and add UDP (the existing guest-forward voice check used the TCP tunnel).
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
- [x] Show more based on measured layout height.
- [x] Local history search.
- [ ] Accessibility tests with NVDA (Windows) and VoiceOver (macOS).
      Needs Windows/macOS; the Linux AT-SPI test covers the same tree.

---

## Later

### Nuschl server (`nuschld`) — implemented base and remaining phases

`nuschld` implements the listener, compatibility mode and experimental encrypted
text mode. The current Rust and NixOS default is `compatible`; experimental `e2ee`
mode rejects stock clients and disables unprotected voice. Desired behavior (user):
end-to-end mode becomes the default for new Nuschl servers ("compatible" mode
admits stock clients, without E2EE); new
members see earlier history by default (per-channel setting); read markers sync
across devices; retention defaults to forever (docs/extension.md, "Decisions");
OpenMLS with the libcrux provider (pure Rust; hybrid post-quantum X-Wing key exchange,
Ed25519 signatures until ML-DSA suites exist); trust on first use with optional
out-of-band verification; key transparency, as in Signal's automatic key
verification (https://signal.org/blog/automatic-key-verification/); possibly
Nuschl-only servers and a minimum client version.

- [ ] Change the new-server default to E2EE after protected media and the required
      history proofs are ready. Do not silently flip the current compatible default
      while E2EE voice is unavailable. Keep existing explicit server modes intact.

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
   - [ ] Review the supplied combined NixOS deployment and provide a server
         operator guide (user, 2026-11-12). Cover imports, ACME/DNS/80+443,
         interface-scoped TCP+UDP 64738, selected fullchain/renewal, browser relay
         DNS/egress, agenix/runtime SuperUser credentials and named admin bootstrap,
         persistent storage/backups and optional witness two-stage public-key
         setup/client policy. The provided compatible-mode host/origin/share URL
         and ACME selection align. Explain settings assumed elsewhere; cohosted
         witness state shares a failure/trust domain. Private module evaluation
         and source/CLI checks, without changing the separately managed deployment.
   - [x] TLS reload on SIGHUP and `minimum_tls_version = "1.3"` (2026-11-11): new
         handshakes get the new certificate, a broken one keeps the old one. The
         NixOS module's manual `certificateFile` credentials are copied at start,
         so replacing those still needs a restart. The integrated ACME helper
         publishes renewed material for reload; its real systemd lifecycle remains
         a separate verification gate.
   - [x] Prove the integrated ACME certificate lifecycle in an actual NixOS systemd
         service: DynamicUser/UID access, credential replacement, renewal/recovery,
         SIGHUP handoff, restart and intentional manual stop. Module evaluation,
         private helper and server tests passed through `acacf`. The standalone
         cached-package VM passed all installed-unit cases (2026-11-12), including
         a held authenticated TLS stream across HUP and an intentional stop across
         renewal hooks, accelerated timer runs and the restart backoff. The private
         issuer is offline; real public ACME/challenge-provider operation is untested.
         Current pinned nixpkgs (`4fe25e12`, 2026-11-12) no longer generates
         `acme-finished-<host>.target`: `acme-<host>.service` now provides the
         bootstrap pair and `acme-order-renew-<host>.service` performs issuance
         and the post-run hook. The standalone cached-package VM reproduced
         the missing-target failure. The publisher now depends on the generated
         `acme-<host>.service`; the lifecycle test passes with that dependency.
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
   - [ ] General history-page retry/desktop UX: honour `retry_after_ms` on
         `rate_limited` and `busy` instead of failing the page. MLS send/commit/grant
         paths and legacy chat already have bounded retry/backoff handling.
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
   user, 2026-11-11) — crypto primitives implemented (2026-11-11); simultaneous
   linked-device sessions and the end-user linking flow remain open. See docs/e2ee.md
   "Phase 3 as implemented": identities are created, published to each server's log, looked up,
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
   - [ ] Mumble-certificate binding statements.
   - [ ] Deploy the implemented independent witness service, pin its key/URL in
         `DEFAULT_WITNESSES`, and validate default protection before enabling it.
   - [ ] deny.toml ignores RUSTSEC-2026-0173 (proc-macro-error2, reachable only under
         libcrux's `cfg(hax)` verification builds): confirm.
   Design and recorded user decisions: docs/e2ee.md §10; spike: spikes/mls (own
   workspace). Recovery is offered with recovery-key and skip choices. Do not reopen
   the answered design inventory; remaining witness-policy questions are listed
   explicitly below.
4. **End-to-end encryption (MLS)** — 4a (delivery service), 4b (direct
   conversations and worker linking primitives), and 4c (channel/SDK text and
   experimental history) are in (2026-11-11); the server linking mailbox/UI and
   concurrent authenticated device sessions remain open. Uses OpenMLS 0.9.2 with the
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
   - [ ] Remaining implementation of the recorded answers in docs/e2ee.md §10:
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
         - [x] Remove devices inactive for 90 days (server MLS policy and
           `e2ee_channels.rs` regression); user-initiated revocation is separate.
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
         crypto worker and `nuschld` link. Vulnerable runtime libcrux pins are
         patched; scoped advisory exceptions still exist in deny.toml, including
         the `cfg(hax)` verification-only proc-macro chain. SECURITY.md records the
         libcrux SIMD `unsafe` exception.
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
         messages relayed by the caller. User-initiated device revocation and
         associated sync-key rotation remain open; inactivity removal is implemented.
   - [ ] Self-updates (§6.5), key-transparency head gossip in MLS, typing and read
         markers as MLS messages, the log-structured worker store (§8.3), a binary
         column for MLS rows, eviction of idle groups on the server.
   - [x] Experimental encrypted text/archive checks and bounded grant pages over
         retained authorized keys, with explicit historical-access policy. Available
         signer/replay evidence is checked; unknown/revoked historical signers fail
         closed. This does not complete historical membership/order proofs.
   - [ ] 4d: authenticated historical roster/membership evidence (revoked or
         unconfirmed historical signers currently fail closed), first-observation
         historical ordering proofs, periodic key
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
         `nuschld admin` as the unit's dynamic user with its credentials;
         `--json` output for lists. Murmur import already takes the operator lock.

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
- [ ] Optional desktop/application audio with screen sharing, excluding Nuschl's
      voice, received shares, effects and TTS at capture time. Separate microphone
      and system-audio tracks; verified per-stream Linux taps, Windows process-tree
      exclusion and macOS application filters, covering playback workers. Never
      reuse the full-device AEC reference or fall back to unfiltered capture.
      Track negotiation, native adapters and tests remain future work; see
      [screen-share audio design](docs/screen-share-audio.md).
- [ ] Browser interoperability and long-call quality tests; str0m vs webrtc-rs benchmark.
- [x] Remove AWS-LC from the native RTC dependency graph and explicitly select
      dimpl's RustCrypto DTLS provider. Vendored adapter generates P-256 certificates
      without the dimpl rcgen feature; 2 certificate and 6 real transport tests pass.
      Native RTC normal/build graph contains neither AWS-LC nor ring.

### Release and other targets

- [x] Reusable independent Linux/native+web, Windows x86_64 and macOS universal
      build jobs, with separate gated publication. Update v2 requires Ed25519 AND
      ML-DSA-44 in two confined fixed-algorithm processes; raw bytes, full public
      pair, expiry and persisted anti-rollback state are bound. No minisign or
      adjacent writable-key fallback. Actual Linux workers, a loopback install,
      strict signer/artifact regressions and workflow syntax are tested. Native
      Windows hybrid verifier/LPAC/library-install and desktop update/restart proof
      now pass on reviewed fixture sources. macOS native proof, final integrated
      distribution/archive checks and an actual release remain.
- [ ] Enroll the production public pair in `crates/update/update-trust.json`, back
      up the independent private seeds and explicitly provision the matching
      `UPDATE_HYBRID_SIGNING_KEY` repository secret with the reviewed operator
      tool. The embedded store deliberately remains empty; no production key or
      secret is created by tests. This protects updates independently of OS code
      signing (below).
- [x] Source-scoped runtime notice producer/publisher gates and lossless compact
      packaging for native/cross/web artifacts. Known platform mappings select
      authoritative roots/features; Mac requires both slices, native builds do
      not depend on web App/WASM outputs, and CI retains the complete source audit.
      Actual scoped Nix bundles have zero unresolved entries (931-package union
      at the Speex checkpoint), preserve original bytes/hashes and patched MPL
      corresponding source, and use five native/four web files within the existing
      256-file limit. Full final distribution review remains a separate row;
      metadata evidence is not legal-compliance or native-runtime proof. See
      docs/release-notices.md.
- [x] Provide an offline operator metadata refresh helper and manual procedure
      (docs/update-metadata-refresh.md). Authenticate both original raw signatures,
      pin the protected ledger's latest hash, preserve every non-time byte, retain
      binary provenance in an external receipt, require the same pair and normal
      current-time verification of the renewal. Explicit expired recovery never
      waives client clock/expiry/rollback rules. Nine fixture tests, 13 updater unit
      tests and 15 existing release regressions pass; no production operation ran.
- [ ] After production enrollment/publication approval, perform the first actual
      manual metadata renewal, verify the downloaded pair, update the protected
      latest ledger and assign the next deadline before the maximum 30-day expiry.
      No cron/reminder service or automatic publication is implemented or implied.
- [x] Declare the exact VP8 fixture for the isolated per-crate RTC probe using a
      checked Nix-only compile-time path replacement. The original 4,297 bytes
      remain embedded and actual loopback passes 50 Opus frames each direction
      plus all 34 multi-packet VP8 frames; no runtime Rust/capture changes.
- [x] Complete standalone RTC notice scope/original-byte coverage: four explicit
      native scopes select only the probe's real dependency root, main desktop/
      cross roots stay unchanged, and the all audit unions actual standalone roots.
      The initial 98-package inventory exposed is0.12.0/str0m-proto0.8.0/
      str0m-rust-crypto0.7.0 missing notices; exact pinned monorepo MIT terms and
      package/author declarations now close those three gaps. Real ranges/tamper/
      unchanged main-bundle checks pass, with zero unresolved entries; this is not
      legal assurance or extra-platform runtime proof. See docs/build-packages.md.
- [ ] Review the server-mode storage worker's blast-radius boundary separately:
      it is a direct child with cleared environment, with NixOS service hardening
      applied to parent and children, rather than the client storage bwrap path.
      Standalone CLI use and authorized PostgreSQL/file access need a reviewed
      narrow worker boundary; this builder migration does not redesign it.
      A separate process alone does not prove filesystem/network/credential
      confinement.
- [x] Migrate standalone `#server` and `#rtc-probe` to existing crate2nix members:
      regular server/storage copies preserve default helper adjacency and exact
      cached bodies; the probe embeds the real fixture with complete scoped
      notices. Full six-node NixOS server suite passes using the actual standalone
      service (SQLite/Postgres/history/MLS/witness/stockvoice/import/restart), and
      the actual private RTC transport passes. No build manifests/lock/graph or
      runtime sandbox policy changed. Precise source/package evidence:
      docs/reviews/2026-11-12-server-rtc-packages.json.
- [ ] Make manual VM-driver scratch selection explicit: the pinned driver chooses
      inherited XDG_RUNTIME_DIR before TMPDIR. The complete server-suite proof
      passed but used 197,558,272 allocated bytes in six owned /run/user/1000
      tmpfs state directories; they were removed after shutdown. Set both variables
      to a short owned disk path before future runs, isolate generic node state
      names and report actual state paths instead of assuming TMPDIR controls them.
      tests/vm_driver_scratch.py now verifies all six actual driver state paths
      pre-boot on owned btrfs; wiring that invariant into future full-suite runners
      remains open. It does not relabel the functional run as disk-backed.
- [ ] Finish the remaining per-crate build migration: Windows desktop, Darwin
      desktop/verifier slices and the explicit `default-crane` fallback still use
      Crane. Native desktop/server/probe/web/witness and native/Windows updater
      engines now use crate2nix. Preserve Linux-hosted cross builds and platform
      dependencies; keep notice metadata/vendoring distinct from Rust compilation.
- [x] Keep source-dependent notice/runtime postInstall hooks out of Crane's
      buildDepsOnly cache key. Explicit artifact-only dependency passes exclude
      final installation hooks while final products retain notice installation.
      Actual Windows/macOS derivation evaluation proves a source-only edit keeps
      dependency paths and changes final products; a Windows target-flag edit
      still changes its dependency path (`tests/build_cache.py`,
      `docs/build-cache.md`). This is cache-key proof, not a new cross runtime test.
- [x] Build official cross updater engines with separate algorithm features.
      Windows uses separate crate2nix roots with scoped executable-suffix/static
      pthread handling and no fixture import shim; Darwin uses separate existing
      zigbuild roots for both architectures. All six outputs built; PE imports,
      Mach-O minimum OS/architecture and universal helper lipo passed. Exact new
      Windows helpers passed the full native hybrid/LPAC adversarial fixture
      (`tests/build_cache_cross.py`, `docs/build-cache.md`). No signer/fixture
      feature enters distributed engines; complete notice scopes stay unchanged.
- [x] Build and inspect complete Windows/macOS desktop archives at frozen source
      `c26529d3` (merged `0c1452be`, same tree as `39ea7132`): all nine programs,
      exact independent helper copies/pre-sign universal lipo, system imports,
      macOS 14.2/version/bundle metadata, source-bound notices and real release
      tar/ZIP/manifest preparation. Windows has 15 files, Mac 18, below 256.
      A public-only fixture shares the exact production dependency cache and
      eight worker/helper bytes; native LPAC plus actual signed GUI apply/restart
      passes all 16 hashes and absolute Unicode-profile/update-state continuity.
      See docs/reviews/2026-11-12-cross-distributions.json for precise limits.
- [ ] Revalidate the eventual tagged release source/CI artifacts after later
      code/dependency changes; frozen-source local proofs do not establish every
      later package or a successful remote all-system CI run/publication.
- [ ] Validate the universal app with Apple's native codesign/Gatekeeper tooling
      and exercise native macOS Seatbelt/audio/update behavior. Pinned rcodesign
      0.29's generic verifier rejects its own empty ad-hoc CMS wrapper and warns
      that it is buggy. Raw failures stay retained; independent bounded checks of
      actual code pages/Info.plist/CodeResources pass 11 real tampering tests,
      but do not establish Apple acceptance or publisher identity. No producer or
      publication policy was relaxed to work around the tool.
- [ ] Narrow the final cross Cargo source filter to real referenced inputs:
      unreferenced docs/reviews JSON can still change final products/notices even
      while dependency derivations stay cached. Preserve all actually included
      runtime JSON, test vectors, schemas/assets and notice authority inputs.
      Also audit non-workspace Cargo manifests retained as attribution evidence:
      Crane's recursive dummy-manifest search can treat them as dependency inputs;
      keep their license bytes available without needless Rust cache invalidation.
- [x] Prepare a required native Apple static-verification CI job after the Linux
      Mac cross-build, on officially verified `macos-26` ARM64. Same-run/source
      artifact metadata, safe extraction and notice policy precede absolute Apple
      tool calls for all nine programs/both architectures; verified output gates
      reusable workflow/release success. No keys, write permission, Nix rebuild or
      artifact execution; bounded diagnostics upload on failure. Local real-tar,
      context-guard and actionlint checks pass. This is prepared source only.
- [ ] Run that prepared job on the intended actual CI artifact and retain Apple's
      strict/deep/all-architecture codesign result. No Mac job/tool execution has
      happened locally; Gatekeeper publisher trust/Developer ID/notarisation and
      application/runtime behavior are separate from static ad-hoc integrity.
- [ ] Adapt the existing Unix hybrid runtime fixture for real macOS Seatbelt tests:
      its Python shebang may be denied by the literal-worker-only exec/read rules.
      The native fixture attribute exists, but a separate Linux-hosted Darwin
      fixture/fault-executable product and actual native canary/forgery/deadline/
      reaping/install tests are missing. Never distribute the signer/fixture or
      broaden production sandbox grants just to make a script pass.
- [ ] Add/review native macOS worker resource-containment parity: the update
      launcher has Seatbelt plus a six-second supervisor deadline/reaping, but no
      explicit address-space/CPU cap comparable to Linux limits/Windows Jobs.
      Validate limits on real macOS before claiming the same blast-radius bound.
- [ ] Windows installer (Start menu entry, uninstall); today the zip is portable and
      updates itself where it was unpacked.
- Deferred (user, 2026-11-11: **no Windows or macOS code signing for now**). The
  README and docs/install.md explain the first start without it.
  - [ ] macOS Developer ID signing and notarisation with `rcodesign` (needs an Apple
        Developer account); until then the app is only ad-hoc signed and the first
        start needs **Privacy & Security → Open Anyway**.
  - [ ] Windows Authenticode signing; until then SmartScreen asks on the first start
        (**More info → Run anyway**) and Smart App Control blocks Nuschl.
- [ ] Final third-party/source attribution review of the exact integrated native,
      cross and static-web archives after later dependency/vendor changes. Verify
      scope/roots/features, original notice/source hashes, updater file limits and
      both verifier binaries in every desktop distribution; run the release gates
      in `SECURITY.md`/docs/release-notices.md. Current metadata proofs do not imply
      an actual GitHub release/CI run, current-head Windows archive proof, macOS
      hybrid updater execution or hardware/accessibility proof.
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
  - [x] TUI media and color implementation (2026-11-11): local Linux PNG clipboard
        paste through the existing content worker (`Ctrl+V`, `/paste-image`),
        certificate-bound image drafts retained until send acknowledgment, and
        sandboxed formula rendering with clipped terminal pictures or a readable
        half-block raster fallback for KGX; source/dimensions remain in `--plain`.
        Server/channel/status accents and worker code-run syntax colors are kept.
        Clipboard providers require focus-free native data-control or a local
        X11/XWayland bridge; no popup-focus fallback is used. Focused TUI build and
        27 tests pass. Actual KGX under private GNOME's strict focus policy passed
        two distinct PNG pastes over an owned, initially cold XWayland bridge,
        exact decoded pixels/caption and send-ack draft clearing, no helper focus
        or visible windows, formula/image rendering, clipping/resize/scroll and
        plain fallback. Text-only clipboard rejected stale image reuse; OSC 52
        injection left the private clipboard unchanged. Pure Wayland without the
        required providers refuses paste.
  - [ ] TUI, left: identity and key transparency (warnings, verification),
        end-to-end direct conversations, file transfers (offers show as text),
        administration (ACLs, bans, moving people), whisper and push-to-talk (a
        terminal has no key-up), Settings beyond the command line, image alt text
        (the content worker keeps none), local clipboard image adapters for
        Windows/macOS and terminal-mediated image MIME over SSH. Pure Wayland KGX
        without data-control or XWayland currently refuses image paste explicitly.
  - [ ] **iOS — feasibility resumed after the supplied Xcode archive:** Swift +
        SwiftUI over UniFFI; staged milestones remain in "iOS feasibility and app
        plan" above. Archive integrity, installed macOS/Xcode and Simulator UI
        remain unproven. Review mobile processing/codec boundaries, CallKit,
        background audio and push extensions.
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
    - [x] Integrate the verified content-owner retirement proof/docs. Source
          `191a8d8d`/`732e31c2` is integrated; the final factory-release follow-up
          `82903f9f` and actual-WASM fixtures `6d427430` complete the scoped guard.
          One page/module owner shares global eight-job/16 MiB budgets across
          facades. Active cancel/dispose settles only its consumer while retaining
          original slot/bytes/deadline until exact actual ACK; healthy same-worker
          reuse and ordinary invalid-image recovery remain. Queued cancellation
          cannot reset startup or cancel another facade. Fatal/stuck retirement
          permanently consumes that owner, always settles cleanup despite thrown
          terminate, and exposes only detached frozen diagnostics.
          All 17 units and original actual-WASM code/image/canary checks pass.
          Real private-browser normal/throwing-terminate cases hold cancelled
          8 MiB active + queued 8 MiB through the original 8 s deadline; 20 new
          facades cannot spawn/reuse capacity. An actual late old-WASM error is
          ignored after failed termination. No synchronous physical-exit claim,
          no automatic replacement, no App/broker activation. See
          `docs/browser-content.md` and the source-bound handoff.
    - [x] Apply verified shared retirement/credit discipline to the active browser
          formula renderer (`87fae40c`/`585bb33f`, docs/browser-math.md). Math and
          content use one private scheduler implementation but distinct page
          owners, validators and budgets. All math facades share eight jobs /
          32 KiB source and one immutable 64-entry/2 MiB serialized cache. Active
          cancellation retains the exact original reservation until actual ACK;
          healthy cancellation/disposal/parser errors reuse the same worker.
          Two-second jobs may fall back while the original fifteen-second loader
          continues; later explicit retry uses that same loader. Fatal retirement
          cannot spawn replacements, and thrown termination still settles all
          consumers while retaining its consumed lease. Valid rendered/cached
          formulas survive; uncached permanent failure preserves source and offers
          Reload client instead of a misleading always-enabled Render formula.
          Packaged Node32/32, fourteen math lifecycle and seventeen content checks
          pass. Actual pinned Temml normal/throwing-stall proofs ignore the late
          old result, twenty new facades cannot bypass retirement, and original
          content WASM code/image/canary/retirement proofs pass after extraction.
          Real packaged App/private Murmur under production CSP proves cold
          2.5-second load → source/visible pointer retry on one owner, fatal source/
          reload at 320px, retained valid fraction DOM and unsent draft, no new
          posts for later uncached formulas, plus ordinary320/390/1280 geometry,
          source copy, code grammar and reflow. No physical thread-exit,
          Safari/mobile-device, OS-quota or content-broker activation claim.
    - [ ] Wire the real async browser content broker: preserve SDK-owned original
          authenticated source/mode; bind render tickets to server/connection,
          conversation/message/revision/digest; reject stale derivatives. The
          content WASM/Worker foundation exists but is currently unreferenced in
          app chunks. Connect real legacy HTML/image decode and validated RGBA to
          the UI, plus paste/attach encoding/send acknowledgement, without parsing
          in the TLS worker or giving the content worker keys/network/storage.
          Keep plaintext/newlines/code source and image/formula fallback accessible.
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
      Compatible-server implementation is ready for root integration (`69ed5405`,
      134 passing tests). Typed SDK/desktop/web controls and no-history MLS admission
      remain open; current E2EE admission fails closed. Native clients deliberately
      do not advertise `knock/1` until they consume its events and history floors.
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
