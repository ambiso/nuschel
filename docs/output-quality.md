# Receive and playback quality

The goal is stereo-preserving playback at least as good as reference Mumble, with
unit gain unless the user requests another volume. This is an open verification
and implementation task. Codec bitrate is a separate transmit setting; 128 kb/s
does not repair downmixing, clipping, resampling or packet-loss behavior. No
absolute perceptual-equivalence claim follows from a synthetic-tone comparison.

## Independent baseline, 2026-11-12

`tests/output_quality.py` creates reference libopus packets, decodes those same
packets independently, and sends them to the real muted native media worker. A
private PulseAudio daemon loads only two null sinks; its stereo monitor records
receive → jitter → mixing → device conversion → output. It never loads hardware
detection, uses no host server/socket and requests no microphone. The reference
C codec is confined to the test process, not a production dependency.

Alignment estimates one integer delay from a quiet-onset marker plus waveform
correlation and uses the same delay for both channels. Gain is fixed at one;
reported least-squares gain is diagnostic and is never applied to samples. Only
fixed initial-silence/tail regions are excluded. The metric tests independently
prove half gain remains a 6.02 dB error and mono collapse remains visible.

On the frozen integrated Linux `12d6695c` package:

| Case | Fixed-gain reference agreement | Diagnostic gain | LR separation |
| --- | --- | --- | --- |
| Mono, unity, private 48 kHz output | 77.4 dB | 0.99990 | Mono source |
| Stereo, distinct LR carriers | 3.01 dB | 0.49991 | 0 dB; identical outputs |

These are observations for one bounded synthetic fixture, not arbitrary music,
packet loss, physical devices or non-48 kHz output. Packets were 320 bytes each,
well inside the current 1024-byte receive limit. The stereo case fails the
explicit parity gate. `--require-parity` makes that known failure an exit status;
the default audit writes failures honestly without pretending parity passed.

`tests/web_output_quality.mjs` executes actual worklet code and optionally the
compiled WASM decoder with the independent packets. The compiled decoder emitted
one channel for stereo input and matched reference *mono* decoding to 138 dB,
confirming the downmix. The actual processor duplicates that mono output. A
separate deterministic overshoot test observed PCM 2.0 × volume 0.25 become 0.25
instead of 0.5: the worklet clips before attenuation. Safe in-range unity PCM was
exact. The WASM decoder also clamps before user volume, so both boundaries need
review. The raw overshoot test is a boundary regression, not a claim that ordinary
encoded speech always exceeds full scale.

Proofs and exact tool paths are in `~/.cache/nuschl-output-quality/`:
`native-final-baseline/proof.json`, `web-baseline.json`, `measurement-tests.log`.
The worker was `/nix/store/ra83a3r8jjaari1yd1fvqa77y2vfs3j8-nuschl-0.1.0/libexec/nuschl/nuschl-media-worker`;
reference libopus was 1.6.1. Browser WASM was the existing
`5xzl4a7iwmpm3q3i0pyjpwrg05l4xd16-nuschl-web-audio-codec` artifact. Current source
retains the same receive behavior. No new production fix is represented by this
baseline checkpoint.

## Stereo/gain correction checkpoint

The first production change preserves two channels end to end. Native decoded
queues and playback rings hold LR frames; decoder sample capacity doubles while
queue duration remains 200 ms per speaker and the playback target stays measured
in frames. Browser decode replies explicitly carry two channels and their frame
count; the controller checks the exact interleaved length before forwarding.
Worklet storage remains 120 ms per speaker (now two scalar samples per frame).
Mono input Opus is duplicated by the stereo decoder. Mono output deliberately
averages LR; native outputs with at least two channels map front-left/right and
silence additional channels because portable CPAL does not supply a speaker mask.
The browser requests two Worklet channels and lets Web Audio map its destination.

Mono UI sounds/probes enter both native channels. The fallback echo reference
retains actual LR (or duplicated folded mono for a mono device) and frame-based
timestamps. System output recording still supplies its original reference path.
AEC receives full interleaved reference audio; only far-end *activity detection*
uses the louder channel so antiphase LR cannot look silent. Capture/startup,
consent, Step/MAX_HOLD and microphone timestamp sequencing are unchanged. The
non-live codec IPC retains its existing mono `Samples` semantics.

Native and browser per-speaker gain precedes defensive full-scale protection;
WASM decoding preserves finite codec headroom instead of clipping before user
attenuation. Malformed/nonfinite audio and final mix clipping remain bounded.
The independent high-level fixture uses legal full-scale square-wave input whose
reference decode peaks at 1.6275; at volume 0.25 no output clipping is necessary.

After the change the actual private native monitor passes all three gates:
mono 77.4 dB, stereo 77.4 dB with 75.6 dB LR separation, and attenuated decoder
overshoot 82.9 dB with 69.8 dB separation, all at fixed intended gain. The compiled
browser stereo WASM matches reference stereo by 138.5 dB on the quiet fixture
and 132.0 dB on the separate overshoot fixture. Actual Worklet code preserves LR,
folds mono deliberately, and returns 0.5 for 2.0 × 0.25. These values describe the
specific test paths and are not perceptual scores.

The media-worker suite passed 52 tests with two pre-existing ignored cases,
including no-allocation device callbacks and stereo fallback-reference mapping.
Three native WASM-codec tests, twelve browser audio unit tests and the real codec's changing-bitrate checks
passed. The existing complete private Pulse suite passed jitter/loss/reordering,
upstream AEC routing, echo probe, double talk, input clipping and sound isolation:
zero silent 20 ms jitter windows, 45 dB echo reduction, 0.9 dB double-talk level
change, and effects six dB quieter at half volume. The actual confined Chromium
fixture also passed real Opus/Worklet sound to a null sink, mute/deafen flush,
late permission cancellation, rapid controls and reconnect consent revocation.
It received 26 packets and independently retained the synthetic 997 Hz carrier.
This browser call regression is mono transmit; end-to-end browser stereo device
rendering remains a separate fixture beyond the Node Worklet/WASM LR proof.

Final native leaf: `p353gcinf8cn44asqk92vim94bgqlyp1-rust_nuschl-media-worker-0.1.0-linked`.
Browser codec: `b22x40is12ysqrhi4h29dlm0v7vxdpzk-nuschl-web-audio-codec`.
Evidence: `native-final-gain/`, `web-stereo.json`, `web-overshoot-stereo.json`,
`native-dsp-regressions.log`, `browser-stereo-privacy/browser-proof.json` and
`browser-stereo-privacy/reference-proof.json` in the same owned disk cache.
Filtered non-48 kHz conversion, browser loss/PLC and matched full reference-Mumble
playback remain open; the stereo checkpoint does not certify those paths.

## Legal packet durations

The pinned Mumble source explicitly supports packets up to 120 ms and rejects
packets whose decoded duration is not a multiple of its 10 ms clock tick. Its
single Opus *frames* may be shorter, but aggregating frames into one packet is a
separate duration bound. Nuschl now accepts 10–120 ms packets on that tick and
rejects sub-tick durations rather than silently miscounting them.

A second audited bug was a native decoded queue described as 200 ms but sized
`10 * FRAME`, actually 100 ms. A legal 120 ms packet lost its first 20 ms.
The queue now has a true 200 ms bound. Jitter advances from the just-pulled
packet's origin once decoding reveals its actual duration, correcting the
previous duration guess rather than inserting phantom PLC for variable packets.
A native unit explicitly covers changing 120/20 ms packets and terminators.
The browser FIFO also needs room for the 20 ms startup hold plus a complete
120 ms packet, so its storage cap is now 200 ms without increasing that hold.
A Node regression checks the previous packet's complete tail at the next arrival.

The independent libopus repacketizer builds legal stereo 120 ms packets at a
64 kb/s target: 956 bytes each, within the existing wire limit. The actual native
private monitor matches complete decoded reference PCM by 77.5 dB with 65.0 dB
LR separation at unity gain. The actual compiled browser decoder also preserves
all 5760 stereo frames per packet. Evidence: `native-long-duration/proof.json`
and `web-120ms.json`. Native tests pass 53 with two pre-existing ignores; browser
audio units pass 13. This does not close browser sequence-clock PLC or prove
identical buffering latency to Mumble.

## Source audit

The local clean Mumble checkout was pinned at
[`35dce9e4801fe52b2217f0d9f421d17b0aaf3bb0`](https://github.com/mumble-voip/mumble/tree/35dce9e4801fe52b2217f0d9f421d17b0aaf3bb0).
Its [AudioOutputSpeech.cpp](https://github.com/mumble-voip/mumble/blob/35dce9e4801fe52b2217f0d9f421d17b0aaf3bb0/src/mumble/AudioOutputSpeech.cpp)
constructs a stereo Opus decoder, represents returned frame counts separately
from interleaved sample counts, invokes decoder PLC for missing packets and uses
Speex resampling quality 3. Its
[AudioOutput.cpp](https://github.com/mumble-voip/mumble/blob/35dce9e4801fe52b2217f0d9f421d17b0aaf3bb0/src/mumble/AudioOutput.cpp)
preserves stereo into stereo output; downmixing occurs for mono destinations or
specific recording/positional cases. This is a source baseline, not a completed
live reference-Mumble output capture.

Baseline Nuschl boundaries (the correction above addresses stereo and gain):

- Native `Mixer` always constructs a mono decoder and returns mono frames.
  `PlaybackConverter` duplicates them to every device channel. Per-speaker volume
  precedes its defensive clamp; final summed output is also clamped.
- Browser `VoiceDecoder` always constructs mono and clamps decoded samples.
  `PcmMixer` clamps again before applying volume, sums, then clamps the final mix.
  The Worklet copies the first output to every channel.
- Native device-rate conversion is linear interpolation, with no antialias filter.
  At 48 kHz the fixture mainly exercises a fixed delay. Other rates require a
  separate actual converter/monitor test; Pulse's own resampling must not be
  mistaken for testing Nuschl's converter.
- Native jitter adapts a 40–200 ms target, reorders and invokes FEC/PLC. Browser
  sequence checks reject duplicates/stale packets, but decoded PCM currently
  enters a 20 ms FIFO hold without sequence-clock gap filling or PLC. Its packet
  loss behavior is therefore not equivalent to Mumble's jitter/decoder path.
- The bounded 1024-byte wire/receive ceiling is distinct from libopus's maximum
  packet size. Long/high-rate stereo packets need explicit interoperability
  fixtures before any limit changes; raising it blindly could break framing.

Existing `tests/opus_eval.py` directly compares decoded packets without fitting
sample gain, but its *encoder-to-original* waveform metric fits gain and delay.
That latter metric must not be reused to certify output gain. Codec-mode IPC does
not include the live mixer, device converter, Worklet or final output path.
`tests/media_live.py` proves bounded jitter gaps and routing in private Pulse,
but does not compare complete fixed-gain stereo PCM against reference playback.

## Non-48 kHz converter audit and rejected candidate

`tests/output_resample.py` invokes the actual native converter through an explicit
ignored test-only PCM dumper (`live::output_fixture::dump`). It constructs no
devices or audio sessions. Contiguous and irregular callbacks must produce
bit-identical output. The test-side reference is SpeexDSP 1.2.1, quality 3,
matching the pinned Mumble constructor; it is not a new production C dependency.
Impulse delay is measured separately from carrier amplitude; gain is never fitted.

| Rate | Probe | Existing linear converter | Speex quality 3 |
| --- | --- | --- | --- |
| 11,025 Hz | 3,858.75 Hz passband | -0.187 dB | -0.0003 dB |
| 11,025 Hz | 8,268.75 → 2,756.25 Hz alias | -0.857 dB | -95.82 dB |
| 22,050 Hz | 7,717.5 Hz passband | -0.745 dB | -0.0010 dB |
| 44,100 Hz | 15,435 Hz passband | -3.062 dB | -0.0007 dB |
| 44,100 Hz | 23,000 → 21,100 Hz alias | -7.142 dB | -85.14 dB |
| 96,000 Hz | 16,800 Hz passband | -2.769 dB | +0.00003 dB |
| 96,000 Hz | 16,800 → 31,200 Hz image | -11.277 dB | -93.96 dB |

The passband probes stay below the Opus music band (20 kHz). The 23 kHz input is
broader synthetic PCM alias stress, not a claim that ordinary Opus carries 23 kHz.
All five rate cases (including 48 kHz) were exactly invariant to callback chunk
boundaries in this fixture. The current converter's impulse delay is about two
48 kHz samples; reference group delay depends on rate/filter. These one-second
fixtures do not yet establish long-duration fractional-rate drift bounds.

The already-locked Sonora 0.3 `SincResampler` was tested independently before any
production integration, using `tests/resample_sonora.rs` against its actual cached
Rust library. Its source revision was `1632d47c1e77d0857f9145bd175134b348541e11`.
The fixed 32-tap Blackman kernel improves on linear interpolation but fails the
reference quality goal: at 11,025 Hz it loses 2.126 dB in the same passband and
aliases at -46.72 dB; at 44,100 Hz the 23 kHz alias is -42.87 dB. Therefore it is
**rejected for this playback conversion**, without changing its existing capture
DSP use. No new Sonora dependency edge or production converter change was made.

Evidence: `resample-linear-baseline/proof.json`,
`resample-sonora-candidate/proof.json` and `sonora-compile.json` in the owned cache.
The reference library is the explicit x86-64
`/nix/store/9mzqdjywmmlb10c06jiql8mgwlk5lw1s-speexdsp-1.2.1/lib/libspeexdsp.so`.
The next candidate is the reviewed scalar Rust SpeexDSP port, tested against the
same unchanged reference before choosing a production dependency. Do not weaken
alias/passband requirements or invent a new filter to make a candidate pass.

## Implementation sequence

1. Preserve two Opus channels through native per-speaker queues/mixer and browser
   decoder/worker/Worklet. Use explicit frame and channel counts; keep existing
   time/byte/speaker budgets rather than accidentally doubling queued duration.
   Mono Opus is duplicated by the stereo decoder. Stereo devices retain LR;
   mono devices deliberately average LR. Mono UI sounds/probes are added to both
   channels. Do not change microphone/transmit channel policy.
2. Apply requested per-speaker gain before final full-scale protection, preserving
   legitimate decoded headroom when attenuation makes it safe. Keep nonfinite and
   malformed-stream protection bounded. Test multiple speakers and cancellation,
   not only one quiet tone. Match the actual stereo far-end reference/timestamps
   to what playback hands the device; do not silently average antiphase reference
   into silence or change microphone privacy/startup ordering.
3. Replace linear playback conversion with a reviewed filtered Rust resampler,
   initialized outside the callback, with preallocated buffers and bounded work.
   Test 44.1/48/96 kHz, passband amplitude/phase, transition/alias rejection,
   callback chunk boundaries, underruns and reference timing. No new dependency
   has been selected. The Rust
   [speexdsp-resampler source](https://github.com/rust-av/speexdsp-rs/tree/7c67def48fc5af526c5350915a4a1531fc1a0cfe/resampler)
   offers a scalar path and separately enabled SIMD features; audit the published
   version and allocation behavior before adoption. Rubato is another option,
   but [its scalar 0.16.2 path also uses unsafe unchecked indexing](https://github.com/HEnquist/rubato/blob/v0.16.2/src/sinc_interpolator/mod.rs),
   so disabling SIMD alone does not make that implementation memory safe.
4. Build a deterministic sequence-clock browser jitter/PLC path and independent
   loss/reordering/FEC reference tests. Compare native/browser latency, discontinuity,
   recovery and music/stereo behavior without pretending distinct adaptive jitter
   policies can produce identical PCM for every loss schedule.
5. Repeat actual reference-Mumble and Nuschl output captures under matching gain,
   rate and speaker layout, including high-bitrate/stereo/multispeaker/loss cases.
   Report numeric bounds and limitations. Physical hardware/listening tests are
   additional evidence, not replaceable by an absolute guarantee from SNR.

Startup/unmute latency has another owner. Playback-only changes must preserve its
timing/consent gates; no latency shortcut may keep a muted microphone recording.
