# Web echo cancellation diagnosis (C049)

Read-only diagnosis of the report that echo cancellation "seems less reliable"
on the web client, later narrowed by the user to **iOS web** (WebKit on
iPhone/iPad; every iOS browser uses WebKit). No product code was changed. No
physical microphone, speaker, iOS device or user profile was used, so nothing
here is a measured acoustic result.

Base revision: `e04d4c57`. WebKit sources were read at WebKit `main`
`5ec99402cfcf9f95e86c8add5b9f092293fef3e1` (2026-11-12). Shipping iOS Safari
may differ from `main`, and the user's iOS version is unknown.

## Nuschl's web audio path (source facts)

- One `AudioContext({sampleRate: 48000, latencyHint: "interactive"})`
  (`web/app/src/audio.js:41`). Received voice runs from the playback
  AudioWorklet through a `GainNode` to `ctx.destination` (`audio.js:80-83`).
  There is no `<audio>` element, `MediaStreamDestination`, `setSinkId` or
  peer connection. Mono Opus is decoded with L=R
  (`crates/audio-wasm/src/lib.rs:98-99`).
- Capture: `getUserMedia` with `echoCancellation`, `noiseSuppression` and
  `autoGainControl` all `true` (`audio.js:137`). The capture worklet only
  clamps and frames; it applies no gain or noise processing, and its output
  is zero (`audio-worklet.js:89-101, 232-238`). There is no double processing
  in Nuschl.
- `getSettings()` is copied into `this.applied` (`audio.js:150-151`) and
  emitted as `state.processing` (`audio.js:19`). `App.svelte` never reads
  `processing`: line 131 only stores the state, and lines 49-51 and 447-452
  use only `phase`, `microphone`, `contextState` and `message`. A disabled or
  unknown canceller is neither shown nor acted on.
- Mute, deafen, a destination change, a transport stall and any
  non-`running` context state all stop the tracks
  (`audio.js:43-46, 110, 117, 129, 164-168, 289`). Each unmute calls
  `getUserMedia` again (`audio.js:57-63, 137`). This is correct for privacy,
  but each unmute starts a fresh browser canceller.
- While unmuted, every 20 ms frame is encoded and sent (`audio.js:237-245`).
  There is no VAD, gate or echo guard. The encoder is Opus
  `Application::Audio` at up to 128 kbit/s
  (`crates/audio-wasm/src/lib.rs:35-38`; `95a1a27f`), which reproduces any
  residual faithfully.
- Native comparison: desktop runs Sonora AEC3 with a recorded output
  reference (`docs/architecture.md:145-158`). It defaults to VoiceActivity
  (`crates/core/src/audio.rs:124`). Its gate demands clearer speech while
  residual echo persists during far-end speech
  (`apps/media-worker/src/dsp.rs:7, 20-21`; `adaptive.rs:135-138`). The web
  client can't see a canceller's residual-echo metrics or supply its own
  reference, so it can't match that pipeline.

## WebKit/iOS behaviour (primary sources)

1. **Playback reference path.** WebAudio output on iOS goes through
   `RemoteAudioDestinationManager` (GPU process,
   `Source/WebKit/GPUProcess/media/RemoteAudioDestinationManager.cpp:71,78`)
   to `AudioOutputUnitAdaptor`, which is a `kAudioUnitSubType_RemoteIO` unit
   (`Source/WebCore/platform/audio/ios/AudioOutputUnitAdaptorIOS.cpp:49`).
   With `echoCancellation`, capture uses a `kAudioUnitSubType_VoiceProcessingIO`
   unit (`Source/WebCore/platform/mediastream/cocoa/CoreAudioCaptureUnit.cpp:77`).
   MediaStreamTrack playback on the default device in the capturing process
   (for example `<audio>.srcObject`) is registered as that unit's speaker
   producer and rendered through the VPIO
   (`Source/WebKit/GPUProcess/webrtc/RemoteAudioMediaStreamTrackRendererInternalUnitManager.cpp:204,228-231,255-259`;
   `CoreAudioCaptureUnit.cpp:567`; `CoreAudioCaptureSource.cpp:192-195`).
   WebKit commit `4880a2fea1dd` (2021-12, bug 233316) introduced this. WebKit
   also enables VPIO "other audio" ducking (`CoreAudioCaptureUnit.cpp:359-369`).
   Apple describes this ducking as applying to "other non-voice audio"
   (AUVoiceIOOtherAudioDuckingConfiguration documentation). Apple documents
   VPIO bus 0 as the unit's output to hardware (kAudioUnitSubType_VoiceProcessingIO
   documentation). It does not document that other in-process output is used
   as an echo reference. **Inference:** Nuschl's playback is probably outside
   the iOS canceller's reference and is only ducked. This has not been
   verified acoustically.
2. **Session and route.** While capturing, WebKit sets `PlayAndRecord` with
   mode `VideoChat` (`Source/WebCore/platform/audio/cocoa/MediaSessionManagerCocoa.mm:182-184`)
   and `DefaultToSpeaker`, unless a receiver speaker was selected
   (`Source/WebCore/platform/audio/ios/AudioSessionIOS.mm:236-241, 256-261`).
   So the loudspeaker sits next to the microphone. Apple's chat-mode
   documentation says the system applies echo cancellation only when VPIO or
   voice-processing AVAudioEngine is used (AVAudioSession.Mode videoChat and
   voiceChat documentation).
3. **Mute and unmute lifecycle.** Without a VPIO-rendered track, stopping the
   last track stops and cleans up the unit (`BaseAudioCaptureUnit.cpp:265-276`;
   `.h:148`). WebKit stores the AudioUnit for 3 s only
   (`CoreAudioCaptureUnit.cpp:290`), so adaptation restarts on each unmute.
   WebAudio counts as "playing" only while its destination is non-silent
   (`Source/WebCore/Modules/webaudio/DefaultAudioDestinationNode.cpp:295,319`).
   Muting in a quiet room can therefore drop the session from
   PlayAndRecord/VideoChat to Ambient (`MediaSessionManagerCocoa.mm:182-194`),
   and the next unmute switches it back. With MediaStreamTrack playback
   through the VPIO, WebKit keeps the unit running after capture stops and
   mutes the uplink (`kAUVoiceIOProperty_MuteOutput`, `CoreAudioCaptureUnit.cpp:748-777`).
4. **Exposed processing.** WebKit supports `echoCancellation` (boolean only;
   `Source/WebCore/Modules/mediastream/MediaTrackConstraints.idl:53-54` FIXME)
   and `sampleRate` (`CoreAudioCaptureSource.cpp:309-311`). `noiseSuppression`
   and `autoGainControl` are not supported constraints
   (`MediaTrackSupportedConstraints.idl`), so Nuschl records them as
   `"unknown"`. Any VPIO noise suppression or AGC is internal to Apple's unit.
5. **Interruptions.** WebKit exposes an `"interrupted"` AudioContext state
   (`AudioContextState.idl`), entered on calls, Siri or backgrounding.
   Nuschl treats it like suspension: it stops capture and clears consent
   (`audio.js:43-46`). That is safe, but each return needs a gesture and
   starts a new capture and a new canceller.
6. **Specification.** The Media Capture and Streams Editor's Draft (fetched
   2026-11-12) defines `echoCancellation: true` as at least `"remote-only"`,
   which covers peer-connection tracks; `"all"` covers all system playback.
   Nuschl's playback is not a peer connection, so `true` guarantees nothing
   for it. WebKit can't accept `"all"`.
7. **Open WebKit reports** (Bugzilla, status NEW): 326286, crackle on iOS
   26.2-27 while an echo-cancelled mic is open, affecting both WebAudio and
   `<audio>` (2026-10); 311451, output degraded while the mic is active
   (2026-04). These are relevant to the same path but are not proven causes.

## Ranked causes (iOS)

| # | Cause | Evidence | Explains |
|---|---|---|---|
| 1 | Playback through the WebAudio RemoteIO unit, probably not in the VPIO echo reference | WebKit items 1 and 2 | Far end hears its own voice back from an iPhone on its speaker; worse than desktop or other browsers |
| 2 | No gate or echo guard on web; every residual frame is sent, with high-fidelity Opus | `audio.js:237-245`, native `dsp.rs` and `adaptive.rs` | Residual echo audible on every browser, compared with desktop |
| 3 | Loudspeaker default in VideoChat mode | `AudioSessionIOS.mm:236-241` | Strong acoustic coupling on phones |
| 4 | Canceller and session restart on every unmute or interruption; Ambient fallback while muted | WebKit item 3, `audio.js:43-46, 137, 164-168` | Echo bursts right after unmute or returning to the app |
| 5 | Granted processing is invisible and unchecked | `audio.js:150-151`; App never reads `processing` | Can't tell the user when the canceller is off |
| 6 | Possible WebKit output regressions | Bugs 326286 and 311451 | Crackle or nonlinearity, if the user's iOS version is affected |

## Synthetic fixture: what it showed

Scratch harness: `~/.cache/claude-nuschl/web-aec/fixture/`. It reuses the
confined Chromium wrapper from `tests/web_audio.py` (no `/dev/snd`, no host
Pulse/PipeWire, no profile). Chromium 156.0.8078.79 uses the fake mic set to
an owned periodic speech-like WAV. The page plays the same signal through a
Worklet→Gain→destination graph shaped like Nuschl's, with Playwright's
`--mute-audio` removed. Results are in `~/.cache/claude-nuschl/web-aec/runs/20261112-213341`, `-213447` and `-213556`
(`aec-probe-results.json`, sha256 `7f8814e0…`, `04c0192e…`, `0aaa5a6e…`). The
first run, `-213223`, had a broken phase locator and is void.

- Requested `true/true/true` was granted and reported by `getSettings()`.
  Phase alignment of mic and playback was verified (envelope plus fine
  correlation).
- Attenuation was about 0 dB (−0.3 to +0.6) in every condition: WebAudio
  destination, `MediaStreamDestination`→`<audio>`, EC off, uncorrelated, 400 ms
  lead, **and the WebRTC peer-connection loopback positive control**.
- Conclusion: **inconclusive.** In this device-less confinement, no playback
  path demonstrably reaches Chromium's canceller, so the fixture can't
  discriminate routing. It says nothing about iOS.
- The existing `tests/web_audio.fixture.mjs:4-7` uses `sinkId: {type: "none"}`
  and Playwright's default `--mute-audio`. It therefore can't test AEC either.

## Narrow fix plan (for root review; no edits made)

1. **Diagnostics only (App.svelte owner):** show `audioState.processing`, and
   on WebKit also `getCapabilities().echoCancellation`. When
   `echoCancellation !== true`, warn and suggest headphones. This makes no
   change to audio processing.
2. **WebKit playback route experiment (audio.js, after the bitrate lane):** in
   `initialize()`/`activateGesture()`, connect `this.output` to
   `ctx.createMediaStreamDestination()`. Play it through a hidden `playsinline`
   `<audio>` element, calling `play()` synchronously in the gesture, instead of
   `ctx.destination`, gated to WebKit. Keep the epochs, the deafen gain-0 flush
   and dispose, and stop the element on `fail()`/`dispose()`. **Privacy
   decision needed:** per WebKit source, the VPIO then keeps running while
   Nuschl is muted. The page's track is stopped, but the OS-level input path
   remains with its uplink muted, and the iOS microphone indicator behaviour
   is unverified. If that is unacceptable, keep the current route and rely on
   items 3 and 4.
3. **Web transmit gate with far-end awareness:** a VAD or level gate in the
   capture path whose threshold rises while the playout clock is rendering
   remote speakers. Hysteresis and hangover should match native defaults.
   This is a gate, not a second canceller.
4. **Opus profile:** the bitrate lane should review Audio vs VoIP for web
   capture.
5. Do **not** add a second canceller in WASM. The browser exposes no aligned
   output reference on iOS, and a canceller without one can't converge.

**Tests.**
- Unit tests in `tests/web_audio.unit.mjs`: route graph, element lifetime,
  deafen silence, dispose.
- The confined Chromium fixture must still pass with the element sink muted.
- Acoustic proof needs a physical iPhone with explicit device scope: an owned
  loopback Murmur, a desktop far end playing a synthetic speech file, and an
  A/B between `ctx.destination` and the element route on speaker vs earpiece
  vs headphones, recording the far end's returned echo. Repeat for
  unmute/convergence timing and interruptions.

## Questions for the user

- iOS version and device model. Safari, another iOS browser, or a home-screen
  web app?
- Speaker, earpiece, wired headphones or AirPods? Is it the same room or
  devices as the desktop comparison?
- Who hears the echo: the far end (their own voice coming back from the
  iPhone) or the iPhone user?
- Is the echo strongest right after unmute or after returning to the app, and
  does it fade within seconds? Or is it constant?
- Any crackling while the mic is on (WebKit bug 326286)?

## Follow-up (C055)

Root's decisions: Opus stays on `Application::Audio`, and there is no default
MediaStream output route that could keep the VPIO or input hardware alive
while muted. The applied-processing display was approved and is implemented
in `ppxmvvop` (`web/app/src/voice-processing.js` and a Settings section).
Unknown values are shown as unknown. It does not change any audio behaviour.

## Design only: iOS capture-scoped MediaStream playback route

Status: **not implemented, default off.** It needs root review and the
physical proof below before any default enable. The Chromium fixture above
remains **inconclusive**, and nothing here is based on a measured iOS result.

### Invariants

- **R1.** The stream route exists only while `captureRunning === true`.
  Reaching that state already requires the explicit Unmute gesture, the
  server's acknowledgment, a consent bound to this destination, a live track
  and a running context (`audio.js:22-26, 207`).
- **R2.** `this.output` (the playback GainNode) has exactly one sink:
  `ctx.destination` or the route's `MediaStreamAudioDestinationNode`. It is
  never connected to both.
- **R3.** Leaving is synchronous within the calling JS task. It pauses the
  `<audio>` element, sets `srcObject = null`, stops every track of
  `dest.stream`, disconnects `output` from `dest` and reconnects
  `ctx.destination`. Leaving **precedes** the capture `track.stop()`, so the
  WebKit renderer releases the VPIO speaker bus before capture ends.
- **R4.** A generation counter binds every asynchronous `play()` result. A
  late resolution after leaving is ignored. A rejection while still in the
  route leaves the route and reports it; it never retries without capture.
- **R5.** Epochs and consent are unchanged. Deafen still sets gain 0 and
  revokes the RX epoch before the route is left (`audio.js:185-193`). The
  route holds no decoded PCM of its own beyond the element renderer's buffer,
  which `pause()` and `srcObject = null` discard.
- **R6.** Opt-in only: a `mediaStreamRoute` constructor option, default
  `false`, set from an explicit experimental Settings toggle. With the option
  off, behaviour is byte-for-byte today's.

### State machine (BrowserAudio, `web/app/src/audio.js`)

`route ∈ {context, stream}`, plus `routeGeneration`, `routeDest` and
`routeElement`.

| From | Event (existing call site) | Guard | Action | To |
|---|---|---|---|---|
| context | `applyPolicy()` turns capture on (`audio.js:207-214`) | option on ∧ `playbackEnabled()` | `enterStreamRoute()`: create a fresh dest and `new Audio()`, set `srcObject`, disconnect `output`→destination, connect `output`→dest, `generation++`, `play()` | stream |
| stream | `stopCapture()` (`audio.js:164`): mute and deafen via `setLocalPolicy` (129), `syncVoice` destination, mute or deafen (110, 117), transport stall (289, 295), `track.onended` (146), context not running or interrupted (45), `fail()` (306), `dispose()` (314) | — | `leaveStreamRoute()` as the **first** statement, then the existing body | context |
| stream | `resetMedia()` sets `captureRunning = false` (`audio.js:198`; `track.onmute` 147, `syncVoice` changes 120, worker `ready` 257) | — | `leaveStreamRoute()` | context |
| stream | `applyPolicy()` turns capture off (`audio.js:208-213`) | — | `leaveStreamRoute()` before `configureWorker` | context |
| stream | `play()` rejects, with the generation unchanged | — | `leaveStreamRoute()`, `status("capturing")` with route `"context"` | context |
| any | `play()` settles with a stale generation | — | ignore | unchanged |

`status()` (`audio.js:17-21`) adds `route` so Settings can show
"Playback route: Web Audio / voice-processing stream". App.svelte owns only
the opt-in toggle and that display. Starting the element without a fresh
gesture is permitted by WebKit, because a MediaStream-source element may play
while the document is capturing (`Source/WebCore/html/MediaElementSession.cpp:507-511`).

### WebKit evidence that leaving releases the VPIO (WebKit `5ec99402`)

1. `pause()`, an ended or disabled track, or a null `srcObject` stops the
   renderer: `AudioTrackPrivateMediaStream::updateRenderer` →
   `stopRenderer` → `renderer->stop()`
   (`Source/WebCore/platform/mediastream/AudioTrackPrivateMediaStream.cpp:160-166, 183-192`).
2. The renderer removes its source
   (`cocoa/AudioMediaStreamTrackRendererCocoa.cpp:80-85`). The unit stops when
   no source remains (`cocoa/AudioMediaStreamTrackRendererUnit.cpp:189-208, 243-248`).
3. The GPU-process unit's `stop()` unregisters the VPIO speaker producer
   (`Source/WebKit/GPUProcess/webrtc/RemoteAudioMediaStreamTrackRendererInternalUnitManager.cpp:288-293`).
   That calls `setIsRenderingAudio(false)` (`CoreAudioCaptureUnit.cpp:909-920`).
4. Once capture has also stopped, `shouldContinueRunning()` is false
   (`BaseAudioCaptureUnit.h:148`), so `stopRunning()` calls `stopInternal()`
   and `cleanupAudioUnit()` (`BaseAudioCaptureUnit.cpp:265-295`;
   `CoreAudioCaptureUnit.cpp:862-885`). The AudioUnit object may be kept
   uninitialized for 3 s for reuse (`CoreAudioCaptureUnit.cpp:286-300`).
5. The failure signature: if a renderer were still registered after capture
   stopped, WebKit would keep the unit running and mute the uplink instead,
   calling `kAUVoiceIOProperty_MuteOutput` and
   `AVAudioApplication.setInputMuted` after 500 ms. It would log
   `CoreAudioCaptureUnit::updateMutedState muteUplinkOutput=1`
   (`CoreAudioCaptureUnit.cpp:748-777`; `CoreAudioCaptureUnit.mm:245-254`).

Source reasoning alone is not proof. All of these steps cross IPC
asynchronously, and shipping iOS may differ from `main`.

### Physical observations required on mute

- The iOS microphone indicator (orange dot) and the Control Center privacy
  entry go from active to "recently" within the same time as the current
  `ctx.destination` route, in 20 of 20 mute cycles.
- Safari's page capture indicator clears.
- In device logs (macOS Console with the iPhone attached, WebKit GPU process,
  category WebRTC): `AudioMediaStreamTrackRendererUnit::stop` precedes the
  capture unit stop, and **no** `updateMutedState muteUplinkOutput=1` appears
  after a mute.
- Playback of the other speakers continues through Web Audio, and deafen
  still gives exact silence.

### Physical iPhone proof plan (required before any default enable)

This needs explicit device scope from the user and synthetic speech only.

- **Setup:** an owned private Murmur and relay. The far end is a desktop
  Nuschl on headphones whose transmit source is a synthetic speech file; it
  records what it receives from the iPhone. Use a quiet room and fixed iPhone
  volumes (50% and 100%).
- **Matrix:**
  - devices: at least two iPhone models;
  - iOS: the current version, the previous major, and one release in the
    26.2-27 range (WebKit bug 326286);
  - browsers: Safari, one third-party iOS browser, and a home-screen web app;
  - output: loudspeaker, earpiece (if selectable), wired headphones and
    AirPods;
  - route: option off vs on, in randomized order, with at least 5 trials
    each.
- **Measures:**
  - echo level returned to the far end during far-end-only speech, and the
    relative reduction on vs off;
  - near-end speech preservation under double talk, using a second speaker
    playing a near-end speech file;
  - residual in 0-2 s vs 5-10 s after unmute (convergence);
  - glitches or pitch changes at route entry, since a VPIO reconfiguration
    on producer registration is expected;
  - crackle relative to baseline.
- **Lifecycle:**
  - 20 mute/unmute cycles with the indicator observations above;
  - deafen and undeafen;
  - an incoming phone call, Siri, background and foreground, screen lock;
  - AirPods connect and disconnect mid-call;
  - a destination or channel change while unmuted.
- **Pass:**
  - every mute releases the indicator exactly like baseline;
  - echo returned on the loudspeaker improves materially (target ≥ 10 dB
    median), with no regression on headphones;
  - no new audible artefacts;
  - all existing epoch and consent tests still pass.

  Any indicator or log evidence that the input stays alive while muted is an
  automatic fail.

### Synthetic checks possible before the physical run

- `tests/web_audio.unit.mjs`, with a fake context, destination and `Audio`:
  - R1-R4 under mute, deafen, interruption, stall, destination change and
    four rapid deafen toggles;
  - a stale `play()` after leaving;
  - a rejected `play()`.
- `tests/web_audio.browser.mjs` (confined Chromium, fake media): with the
  option on, entry on unmute and exit on mute (element paused, `srcObject`
  null, dest track `ended`), no doubled output, and unchanged behaviour with
  the option off. These are functional checks only: they prove nothing about
  echo cancellation or iOS.

### Proposed sentence for `docs/browser-audio.md` (shared; not edited)

"Settings shows the echo cancellation, noise suppression and automatic gain
values the browser reported for the most recent microphone use, as on, off or
unknown; these are reports, not measured echo removal."
