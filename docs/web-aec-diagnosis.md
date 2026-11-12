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
