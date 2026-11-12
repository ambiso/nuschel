# Opus transmit quality audit

This is a transmit-side source audit, not a perceptual-equivalence result. The
user's reported quality difference needs measured encoding and capture evidence.
Receive/output measurements have a separate owner in `docs/output-quality.md`.
No host microphone, personal Mumble configuration or public server is sampled.

## Exact source identity

- Nuschl audited source: `caff0785`, with published `opus-pure` 0.2.3 plus the
  documented receive-only patches in `vendor/opus-pure/NUSCHL-PATCH.md`. The
  published archive SHA256 is
  `5afacd236411dddcc284983066ef66b27e306e53ce4de85cd745b3da9be8a0be`;
  retained upstream VCS revision is
  `3f6077b8bdc0f6c090c3b585de9294065a50e2a4`. Those receive patches do not alter
  the encoder.
- Pinned stock comparison: [Mumble v1.5.939 AudioInput.cpp](https://github.com/mumble-voip/mumble/blob/dfbac8d3d9c967003e74b4ffc5610f7d45e8f666/src/mumble/AudioInput.cpp),
  local tag `v1.5.939` = `dfbac8d3d9c967003e74b4ffc5610f7d45e8f666`.
  [Settings.h at the same revision](https://github.com/mumble-voip/mumble/blob/dfbac8d3d9c967003e74b4ffc5610f7d45e8f666/src/mumble/Settings.h)
  supplies defaults, not a claim about the user's saved settings.
- Current upstream is a distinct comparison:
  [master at 8c538851b25a8349e03807df33c159ad4fe1c63a](https://github.com/mumble-voip/mumble/blob/8c538851b25a8349e03807df33c159ad4fe1c63a/src/mumble/AudioInput.cpp),
  the remote HEAD observed on 2026-11-12. Do not label the local checkout
  `35dce9e4801fe52b2217f0d9f421d17b0aaf3bb0` as today's remote HEAD.
- Independent test-side codec is libopus 1.6.1. Its actual reported version,
  library hash and controls must accompany measurements. It is not claimed to
  be the exact dynamically linked codec of every Mumble distribution.

## Encoder settings actually requested

| Setting | Native live TX | Browser WASM TX | Mumble 1.5.939 |
|---|---|---|---|
| API sample rate/channels | 48,000 Hz / mono | 48,000 Hz / mono | 48,000 Hz / mono |
| Application | `Voip` | `Voip` | `RESTRICTED_LOWDELAY` if low-delay allowed and initial adjusted bitrate ≥64k; otherwise `AUDIO` at ≥32k; otherwise `VOIP` |
| Signal hint | automatic (`None`) | automatic (`None`) | automatic, no explicit `OPUS_SET_SIGNAL` |
| Bandwidth | automatic, max fullband | automatic, max fullband | automatic, no explicit bandwidth override |
| Complexity | 9, inherited crate default | explicitly 5 | inherited libopus default; 9 in test-side libopus 1.6.1 |
| Rate control | constrained VBR | constrained VBR | explicit CBR (`OPUS_SET_VBR(0)`) |
| DTX | off | off | codec default off; Mumble's external voice gate stops packets |
| In-band FEC/loss hint | enabled; 5% initially, configured loss clamped 1–30% | off / 0% | codec defaults off / 0%; no explicit setters in AudioInput |
| Packet duration | 20 ms, two 10 ms DSP frames | 20 ms, 960 capture samples | default 20 ms; adjusts packet count under tight server budgets |
| Requested bitrate | 128,000 bit/s | 128,000 bit/s | default 40,000 bit/s; user-configurable |
| Budget handling | current authenticated server budget wins | current authenticated server budget wins | adjusted codec bitrate + framing accounting; may enlarge frames |
| API PCM precision | float encode, default 24-bit analysis depth | float encode, default 24-bit analysis depth | signed 16-bit `opus_encode` |

Native controls are in `apps/media-worker/src/live.rs` (`run`, `Configure`,
`Transmit::encode`); WASM controls are in `crates/audio-wasm/src/lib.rs`
(`VoiceEncoder`). Both live paths cap each 20 ms encoded payload to
`bitrate_bps / 400` bytes, which is the target's 20 ms byte allowance. Browser
also imposes a 960-byte absolute ceiling. The native `--live` path must be
distinguished from the nonlive codec test IPC in `apps/media-worker/src/main.rs`,
which uses different FEC and output-buffer defaults.

The initial conservative server allowance is 72,000 bit/s. After reserving
24,000 bit/s for 50 packets/s of framing/transport, that gives a **48,000 bit/s
codec target**, not 128k. Reaching 128k requires sufficient advertised server
budget (at least 152k under this accounting). If fewer than 6k remain, TX is
disabled instead of exceeding the server budget. This audit does not change
bandwidth accounting, session authority or controls.

Mumble's application choice occurs when its encoder is constructed. Later
`setMaxBandwidth` updates bitrate/frame state without recreating that encoder;
do not infer an application switch from every live bitrate change. Its source
default is low-delay allowed, 40k and two 10 ms frames, so a fresh default encoder
uses `AUDIO`. At 128k with low-delay allowed a fresh Mumble encoder instead uses
`RESTRICTED_LOWDELAY`. Current upstream retains this application ladder and CBR
selection; its preprocessing implementation/version is separately identified.

## VoIP does not mean narrowband-only

The actual vendored TX encoder contains `CeltEncoder`, `SilkEncoder`, bitrate/
signal/complexity-driven mode selection, hybrid coding and fullband CELT. This is
verified from `vendor/opus-pure/src/encoder.rs`, not inferred from decoder support
or the `Application::Audio` name. `RestrictedLowDelay` forces CELT; `Voip` and
`Audio` select SILK/CELT from equivalent rate and voice/music analysis, then may
construct hybrid coding from the chosen bandwidth. A `Voip` encoder can emit
fullband CELT. Opus fullband audio reaches approximately 20 kHz; a 48 kHz PCM API
does not promise preservation through Nyquist.

The application remains a meaningful policy difference. Voice bias without a
classifier is 115 for VoIP versus 48 for Audio in this implementation. Tonality/
music/bandwidth analysis runs only at complexity ≥7: browser complexity 5 skips
it, while native default 9 includes it. Rate control and the loss/FEC estimate
also enter rate/mode decisions. Inspect actual packet TOC mode/bandwidth and
decoded spectra before assigning a reported sound difference to any one setting.
Switching only the enum is not a demonstrated quality fix.

## Capture defaults are another independent variable

Native capture follows the chosen device's rate/channel configuration, averages
its channels to mono and converts to 48 kHz. `CaptureConverter` is still a
linear interpolator; its separate non-48 kHz passband/aliasing audit remains open.
The output/reference filtered SRC fixes do not imply capture is filtered.

The default `AudioSettings` enable voice activity, sensitivity 0.5, noise
suppression, automatic gain and automatic echo handling. In the media worker:

1. Sonora's high-pass stage runs on 10 ms mono frames. Its AEC is enabled when
   the route needs Nuschl cancellation; recognized upstream AEC is reused instead.
2. `nnnoiseless` RNNoise always estimates voice probability. Its processed PCM
   replaces capture when noise suppression is enabled; recognized upstream
   suppression bypasses this replacement to avoid doubling it.
3. Sonora AGC2 adaptive digital gain defaults on, with max gain 30 dB and limiter.
   Echo-dominant frames cannot raise gain beyond the remembered near-end ceiling.
   With AGC disabled, a soft limiter above 0.85 remains. The final native frame
   sanitizes nonfinite values and clamps to full scale.
4. The VAD gate uses RNNoise speech probability and signal level. Default open
   threshold is 0.6 and close threshold 0.3, with hold; PTT and continuous modes
   have distinct admission. Twenty-millisecond encoding joins processed frames,
   preserving current consent/onset queue rules. This audit does not relax mute
   privacy, reopen capture, or change startup sequencing.

Browser capture asks `getUserMedia` for mono/48k, echo cancellation, noise
suppression and automatic gain, then truthfully reports the browser's applied
settings. These requests are not equivalent to a specified RNNoise/AGC algorithm;
the browser/OS may choose or ignore processing. The AudioContext is explicitly
checked for 48 kHz, with a mono Worklet feeding 20 ms float PCM to isolated Rust
WASM. The browser has no separate native RNNoise/VAD processing in that encoder.
Permission and microphone activation remain explicit user actions.

Pinned Mumble defaults instead use Speex suppression at −30 dB, AGC and dereverb
through the Speex preprocessor, with AGC target 30000 in its 16-bit domain and
max gain derived from `iMinLoudness=1000`. Default echo option is Speex mixed;
actual backend/reference availability governs activation. ReNameNoise is a
separate selectable/build-dependent option in 1.5.939, not its default. Mumble
uses Speex resampling quality 3 for device conversion. Current upstream has moved
to a preprocessor wrapper and RNNoise 0.2, so its capture implementation must not
be substituted for the pinned 1.5.939 baseline.

## Measurement plan and open gates

No production encoder setting is changed by this source checkpoint.

- Compare identical, owned synthetic formant/fricative speech, harmonic and
  percussive music, impulses/transients, stepped high-frequency tones and
  multiband signals. Silence and 997 Hz alone cannot establish fidelity.
- Encode native/browser settings and Mumble's fresh-encoder application/CBR
  policy at 24/40/48/64/128k. Keep a separate matched Rust/C-settings comparison
  so application policy is not conflated with codec implementation error.
- Decode every transmitted packet with independent C libopus. Record actual
  TOC modes/bandwidths, payload-rate distribution, packet durations, lookahead,
  encode timing and all failures. Include reset and speech/music transitions.
- Use fixed gain and only declared/measured integer delay alignment. Report
  gain deviation separately; never fit it away. Spectrograms and band-envelope
  errors complement waveform residuals: a lossy codec may preserve spectral
  energy without waveform phase. Neither metric proves perceptual equivalence.
- Measure preprocessing separately from the encoder and then actual native
  private-device and browser synthetic-capture→TX→independent-decode paths. Keep
  native-vs-browser differences explicit. No host device or personal profile.
- Preserve failed baselines before proposing a bounded settings correction.
  Capture SRC, stereo/music presets, browser preprocessing controls and broad
  codec defects need separate allocation/approval; receive parity is separate.

Existing `tests/opus_eval.py` is insufficient as the final TX gate: it exercises
the nonlive IPC and a C VoIP comparator, has an outdated 64k assumption, and its
quality score fits a least-squares gain after delay search. Keep that historical
evidence but do not present it as current live Mumble-policy or fixed-gain parity.
