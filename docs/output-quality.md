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
`5xzl4a7iwmpm3q3i0pyjpwrg05l4xd16-nuschl-web-audio-codec` artifact. Those artifacts represent the historical baseline; the later correction
checkpoints below describe changed receive behavior.

## Stereo/gain correction checkpoint

The first production change preserves two channels end to end. Native decoded
queues and playback rings hold LR frames; decoder sample capacity doubles while
queue duration remains 200 ms per speaker and the playback target stays measured
in frames. Browser decode replies explicitly carry two channels and their frame
count; the controller checks the exact interleaved length before forwarding.
Worklet storage is bounded at 200 ms per speaker after the packet-duration
correction below (two scalar samples per frame).
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

## Scalar Rust Speex candidate

The published `speexdsp-resampler` 0.1.0 source archive is 11,619 bytes, verified
against crates.io SHA-256
`b72d540d5c565dbe1f891d7e21ceb21d2649508306782f1066989fccb0b363d3`
and retained VCS revision `b543f416aea84140e237bb3747a9d3a01c31fc9c`.
It has no dependencies, no unsafe source, and includes its BSD-3-Clause terms.
However, the unmodified f32 path clones delay memory repeatedly (610 allocations
in the one-second 11,025 Hz probe). A separate original-source diagnostic also
reproduced incorrect channel-base access: left agreed at 146 dB but right was
-4.27 dB against the same reference. That diagnostic explicitly bypassed the
allocation assertion to observe PCM; it is not a passing acceptance run.

A cache-only review patch replaces the hot clone with `mem::take`/buffer reuse,
passes the correct per-channel memory slice to the unchanged kernel, and replaces
the magic-tail temporary with `copy_within`. Coefficients and filter arithmetic
are unchanged. The patched library compiles with `--forbid unsafe_code`.
`tests/resample_speex.rs` checks zero allocations throughout actual f32 processing,
equal LR source consumption and invariant callback chunks. The unchanged
reference comparison agrees at 135.8–156.2 dB, with maximum error 1.04e-7,
including both channels. Ten-second 11,025/22,050 Hz cases agree at 146 dB;
there is no rounded 10 ms block clock. Passband and alias/image results match
reference Speex quality 3; the largest measured alias-level difference is
0.014 dB around a -94 dB floor. The 48 kHz path bypasses resampling, matching
Mumble's rate-equal condition.

The reviewed patch is now committed under `vendor/speexdsp-resampler`; original
copyright, license, registry checksum and VCS metadata are preserved alongside
`NUSCHL-PATCH.md`. The production adapter is described next. No C dependency,
SIMD path or additional transitive crate is introduced. Evidence:
`resample-speex-patched-final/proof.json`, `speex-rust-review/metadata.json`,
`speex-rust-review/original-lr.json`, and `speex-patched-review/` in the owned cache.

## Production filtered converter

`apps/media-worker/src/playback.rs` replaces linear interpolation with the reviewed
scalar Speex quality-3 implementation when rates differ. Equal 48 kHz rates bypass
SRC, as in Mumble. Initialization allocates the state and fixed planar input/output
buffers before stream construction. Processing consumes the library's reported
source-frame count with its exact rational clock; 22,050 and 11,025 Hz are never
rounded to fixed 10 ms output counts. It produces only frames requested by the
current device callback, with no future output prefetch, allocation or lock.

The same stereo/mono/channel mapping applies after filtering. The fallback AEC
reference retains the consumed source LR and adds the library's reported digital
filter group delay to its playback anchor; this models SRC as part of the known
render path without changing microphone sequencing or Step/MAX_HOLD. Unit tests
compare declared delay to actual impulse peaks, retain channel/frame accounting,
and prove no allocation while underrunning and recovering from 8 to 384 kHz.
The existing callback allocator test also remains active.

The actual converter passes `tests/output_resample.py --require-reference`:
fixed-gain stereo agreement 135.8–156.2 dB against source-matched reference PCM,
maximum error 1.04e-7, invariant contiguous/irregular callbacks, and ten-second
fractional clocks. The gate requires at least 100 dB agreement and maximum error
2e-6 for this source-matched filter, not a fitted gain or a perceptual claim.
`tests/output_device_rates.py` additionally runs the actual live worker on private
Pulse sinks. It verifies client **and monitor** negotiate exactly 22,050, 44,100
and 96,000 Hz so server resampling cannot masquerade as the new converter.
Encoded stereo carrier errors are below 0.004 dB and LR separation exceeds 71 dB
in that fixture. Endpoint fractional delay is kept separate from amplitude.

The source suite passes 57 tests, with two pre-existing ignored tests plus the
explicit ignored PCM-extraction fixture. Existing private 48 kHz DSP regressions pass
with the filtered source, including echo routing/probe, double talk, clipping and
sound isolation. The jitter run had one silent 20 ms window over 3020 ms with
five dropped packets, within its existing limits. A new integrated distribution
and full reference-Mumble/device/listening proof remain pending.

Final playback leaf: `xq4r52yz85acnipv6w1i7xmbzqv1r5bj-rust_nuschl-media-worker-0.1.0-linked`.
The follow-up read-chunk implementation and its private runtime proofs use this
same artifact.
Evidence: `resample-adapter-gated/proof.json`, `private-device-rates/proof.json`
and `filtered-dsp-regressions.log`. The source graph is based on integrated updater
checkpoint `d1480f2c`; lock/generated edits add only the reviewed SRC package.

Review found that the first adapter filled an input stage with speculative zeros
and retained its unconsumed tail after an empty callback. A tiny high-rate
callback could then hide newly arrived real audio behind those future zeros.
The correction peeks a bounded rtrb read chunk and commits only the source frames
Speex reports consumed. An empty call offers fixed silence only for that call;
its unconsumed tail is discarded. A 384 kHz single-frame underrun/marker regression
proves immediate real queue consumption and an audible marker within the declared
filter delay, with zero allocations. The full reference/chunk/clock matrix and
private negotiated-rate proofs passed again: `resample-peek-gated/` and
`private-peek-device-rates/`.

A stricter newly added 44.1 kHz-render/48 kHz-capture echo check exposed an
active reliability follow-up alongside this bounded SRC quality checkpoint. Separate new-source attempts
removed only 2.32/0.09 dB against the unchanged 15 dB requirement; one frozen
pre-SRC run removed 19 dB. A paired comparison on one unchanged private daemon,
loopback, seed and timing then measured:

| Order | Frozen pre-SRC | Filtered source | Pass at ≥15 dB and ≤25% transmitted |
| --- | --- | --- | --- |
| 1 | 12.88 dB, 30.1% transmitted | — | No |
| 2 | — | 19.20 dB, 9.6% | Yes |
| 3 | — | 32.39 dB, 0% | Yes |
| 4 | 37.20 dB, 0% | — | Yes |

This establishes substantial pre-existing variation and does not establish a
consistent SRC regression. All these runs explicitly reported the system-recorded
monitor, never fallback. Only the fallback anchor receives playback filter delay;
monitor timestamps were untouched in that playback-only checkpoint. Keep the
stricter assertion. At that checkpoint the monitor `ReferenceConverter` still
linearly converted 44.1→48 kHz; its signal fidelity and timestamp relationship
were candidates, not diagnosed causes. Its new exact
frame-count/chunk test passes at 11,025/22,050/44,100/96,000 Hz; that rules out those
specific counter/boundary hypotheses but does not certify reconstruction quality. Do not substitute passing output
spectra or 48 kHz echo tests for stable non-48 AEC. Evidence is retained in
`private-device-rates-echo/`, `old-src-echo-baseline/` and `paired-src-echo/`.

The repeated ABBA test after the peek fix is also retained unchanged:
old/new/new/old reduction was 45.33/0.55/43.72/47.81 dB. The failed new trial
transmitted 34.8%; it is not discarded because the next trial passed. Combined
with the earlier baseline failure, these observations leave reliability and a
possible timing/reconstruction interaction open. They do not justify claiming
stable AEC. `paired-peek-src-echo/proof.json` records every trial and the 15 dB
assertion remains unchanged.

## Recorded-reference reconstruction investigation

The separate standalone `tests/resample_reference.rs` initially included the
unchanged `ReferenceConverter`; `tests/reference_resample.py` compares its signal
to test-only Speex quality 3 without devices. Counts remain exactly 48,000 frames
per second and whole/irregular callbacks are bit-identical, but reconstruction is
poor: 44.1→48 kHz loses 3.65 dB at 15.435 kHz and produces a 19.335 kHz image at
-14.41 dB, versus reference carrier near 0 dB/image -94.96 dB. The 22.05→48 kHz
probe likewise loses 3.65 dB at 7.7175 kHz and images at -14.41 dB.
This is a measured reference-signal defect, not proof that it causes the runtime
AEC variation. Evidence: `monitor-reference-spectral/proof.json`.
The follow-up now uses the same scalar Speex quality-3 primitive, with fixed
planar buffers and exact rational source/output frame counts. It preserves
channel mapping, callback bounds and the 48 kHz bypass. Both source partial
batches and complete batches are processed within the current callback; no
future silence is reserved. Microphone capture and Step/MAX_HOLD are unchanged.

The recorded-reference anchor subtracts the reconstruction filter delay, plus
its exact source/output clock phase: reconstructed samples represent an earlier
physical time. This is opposite to the played fallback's playback-delay addition.
An impulse through the actual converter and Timeline maps back to its physical
capture time within one 48 kHz sample at 8/11.025/22.05/44.1/48/96/192/384 kHz.
That sign test ran before actual echo tests. The standalone full-stereo fixed-gain
reference gate passes at at least 134.5 dB; whole/irregular callbacks agree
exactly. All 58 native units pass, including zero-allocation callback checks
(three explicit diagnostic tests remain ignored).

A new same-graph ABBA comparison at 44.1 kHz render/48 kHz capture measured old
linear-reference/new-filtered/new-filtered/old-linear reductions of
0.0/45.26/44.12/4.44 dB, with transmitted fractions 40.8/0/0/18.6%. The original
15 dB and 25% assertions remain unchanged. A separate fresh private session
passed the original best-of-two-on/one-off check at 27 dB and 9% transmitted,
versus 100% without cancellation; this does not assert both on attempts passed.
The original 48 kHz DSP suite also passes: echo removal 46 dB, double-talk level
change 1.1 dB, clipping detection, sound isolation and half-volume 6 dB behavior.
Evidence: `monitor-reference-filtered-gated/proof.json`,
`paired-reference-filter-echo/proof.json`, `reference-filter-cold-echo/proof.json`
and `reference-filter-dsp-regressions.log` under the owned output-quality cache.
These scoped results support the reconstruction fix; they do not erase the
prior failed trials or establish general AEC reliability across devices/load.

## Per-speaker headroom correction

The former native `Speaker::extend` and Worklet enqueue clipped each gain-adjusted
speaker before summing. Opposing samples 1.5 and -1.25 consequently became zero
instead of 0.25. Stock Mumble sums gain-adjusted contributors before its final
output clip. Both paths now retain raw finite PCM plus bounded per-frame gain,
multiply and accumulate in f64, then clip the final mix. Native local effects,
loopback and probe audio also enter that wide sum before the 48 kHz mix boundary;
the device converter separately guards filter overshoot. Native decoded
nonfinite values become silence; the Worklet rejects nonfinite chunks. Finite
f32 extremes times gain≤4 and the existing speaker caps cannot overflow f64.
No arbitrary amplitude cutoff is imposed on finite decoded headroom.

`tests/output_headroom.py` generates two independent legal ±0.99 stereo sources
with reference libopus, then compares the actual native decoder/jitter/mixer
against an independently summed and final-clipped reference. At gain 3/2.5,
decoded peaks reach 1.6275/1.6267 but the mixed native PCM agrees at 119.21 dB
fixed gain. The computed old per-speaker clipping algorithm scores only
0.00073 dB. The real `PlaybackProcessor` running in Node produces exactly the
independent sum from those reference-decoded streams. Units also cover
opposing constants, local-effect cancellation, maximum finite f32 ×4,
nonfinite rejection, frame-frozen gain and zero native mix allocations.
All 60 native tests pass (four explicit/old diagnostics ignored); 14 Node audio
units pass. Evidence: `headroom-independent/proof.json` and the associated logs.

Queue duration remains 200 ms: native raw/gain frames use 12 instead of 8 bytes,
adding at most 4,915,200 bytes across 128 fully occupied queues. Browser PCM,
gains and persistent wide scratch retain 1,996,800 bytes at 16 speakers, versus
1,228,800 bytes for the old PCM arrays. The render loop allocates no PCM scratch
and iterates the speaker map once per block, never once per sample. Mix-only
paired microbenchmarks on this machine measured roughly 5.3–5.7 → 6.9–7.7 µs per
native 480-frame block with 16 speakers. Node measured 8.2–8.6 → 9.3–9.9 µs per 128-frame/16-speaker block. Those trials
are recorded separately in `headroom-cpu-node.json`; native trials and exact
source hashes are in `headroom-cpu-native.json`. These exclude decode/enqueue
and are component cost measurements, not browser/device deadline guarantees.
The unchanged original private DSP suite also passes after the wide mix: echo
46 dB/0% sent, double-talk 0.7 dB/100% gate, clipping detection and effects
isolation/6 dB volume (`headroom-dsp.log`). This does not close full live
multispeaker, loss/jitter or perceptual parity.

## Explicit reference decoder setting

Stock Mumble sets `OPUS_SET_PHASE_INVERSION_DISABLED(1)` after decoder creation.
Published opus-pure 0.2.3's CELT decoder passed `disable_inv=false` and exposed
no public control. The earlier reference-libopus fixtures use defaults, so
matching those does not establish matching this stock setting.

The independent `tests/opus_phase.py` fixtures now explicitly compare both
libopus CTL4046 settings on identical packets. Published Rust decode agrees with
default libopus at 127–138 dB, but with the stock Mumble setting at only
11.04/7.70/3.56/−2.76 dB for 16/24/32/48 kbit intensity-stereo cases. At 48 kbit,
stock mono-fold RMS is 4.14× the default; the 128 kbit low-band control is unchanged.
These are real synthesis differences, not entropy parsing: every final-range
value remains identical. The low-rate streams include Hybrid→CELT transitions.
Evidence: `phase-published/proof.json` and `phase-reference-baseline/proof.json`.

The pinned opus-pure 0.2.3 vendor patch exposes the existing CELT `disable_inv`
argument as an explicit decoder setting, default false. It preserves histories,
reset/channel/rate behavior, entropy-bit consumption and encoder/generic defaults.
Only three upstream files change; original license/VCS/manifests are retained
byte-for-byte, with archive identity and patch detail in
`vendor/opus-pure/NUSCHL-PATCH.md`. All 30 upstream API tests pass, including the
new default/reset test at every API rate/channel count. A device-free patched
Rust decoder matches both explicit reference modes at ≥127 dB on the original
intensity/control cases, including an actual midstream reset.

The native Speaker and browser VoiceDecoder now explicitly opt in; generic and
nonlive decoder defaults remain false. The native decoder/jitter/mixer matches
independent final-clipped reference stereo at 127–138 dB on the 16/24/32/48 kbit
and high-rate control fixtures. Actual compiled WASM VoiceDecoder plus real
Worklet stereo/mono rendering matches at 126–138 dB for stereo and 127–133 dB
for mono. A deliberate stereo Hybrid/CELT → mono SILK → stereo stream also passes:
native 93.1 dB (postmix mono fold 101.5 dB), WASM/Worklet 76.7 dB stereo and 76.8 dB
mono. These are fixed-gain comparisons, with no fitted amplitude or timing.
All final entropy ranges still match, and an actual midstream reset retains the
configured synthesis behavior. The 35 dB gate was not weakened. Combined native
units pass 67, audio-WASM native units 4, Node audio units 14, and the upstream API
suite 30. The compiled codec is
`/nix/store/dq866hjy2i1gnfqmk47yfmb3y6xqfx8v-nuschl-web-audio-codec`;
`phase-receivers-final/proof.json` records all exact per-case results. A fresh
actual private 48 kHz sink run with explicit stock reference control also passes
mono source 77.41 dB, stereo 77.41 dB, attenuated overshoot 82.89 dB and 120 ms
stereo 77.51 dB, including preserved LR. Evidence: `~/.cache/noqphase/proof.json`.
No host device or microphone was used.

This control deliberately trades some stereo rendering quality for mono-fold
behavior, matching Mumble; it is not universally superior stereo or a
perceptual-equivalence guarantee. Native component mono metrics above fold its
final 48 kHz stereo mix, whereas the Worklet folds the wide LR sum before its
final clip. Native physical-mono clipping/mapping order under overshoot needs
its own reference/device fixture; it is not certified by these phase tests.

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
   callback chunk boundaries, underruns and reference timing. The selected Rust
   [speexdsp-resampler source](https://github.com/rust-av/speexdsp-rs/tree/7c67def48fc5af526c5350915a4a1531fc1a0cfe/resampler)
   offers a scalar path and separately enabled SIMD features; the audited vendor
   patch and its allocation/channel fixes are described above. Rubato is another option,
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


## Native mono-device order correction

Stock Mumble 35dce9e sets a mono device's LR weights to 0.5/0.5
(`AudioOutput.cpp430`), maps un-clipped stereo while summing contributors
(line 827), then clips the device output (line 856). Nuschl's frozen phase-control
worker instead clips the 48 kHz stereo mix and only later averages LR in its
mono-device converter. These operations differ under valid decoded overshoot.

`tests/output_mono.py` runs the actual worker against an independently negotiated
private `s16le 1ch 48000Hz` sink and same-rate mono monitor, never the host audio
server. Reference libopus explicitly uses Mumble's phase-inversion setting.
An identical quiet LR marker aligns time before the loud case; gain is never
fitted. Legal source peak 0.99 decodes to 1.2804. At unity, output agrees with
stock mono order by only 14.13 dB, while agreeing with the old clipping order at
75.86 dB. At gain 4 the old path becomes exactly silent in the steady measurement
window, losing a reference RMS 0.46895 signal. The quiet control passes 66.10 dB.
`~/.cache/nmono-base/proof.json` preserves every case; the earlier exploratory
no-marker run is retained separately at `~/.cache/nmono-old/proof.json`.

The scoped correction now folds the full f64 mix for actual one-channel devices
before the existing 48 kHz mix clip, duplicating that mono into the existing LR
ring. The negotiated output channel count is fixed with the output stream; there
is no callback/device re-query. Mono local effects join the wide folded voices
before protection. Stereo and SRC clip placement stay unchanged. The fallback
reference contains the same folded/clipped mono handed to playback, with the
existing frame clocks and filter-delay compensation.

On an actual negotiated `s16le 1ch 48000Hz` client/sink and same-rate mono monitor,
the unchanged 35 dB gate now passes: quiet control 66.10 dB, decoded overshoot 76.51 dB,
and gain 4 opposing LR 88.53 dB. There is no gain reduction, normalization or fitted
amplitude. `~/.cache/nmono-fixed/proof.json` records exact source/decoded peaks,
negotiated formats, delays and fixed-gain metrics. Stereo regressions also pass
mono-source 77.41 dB, stereo 77.41 dB, attenuated overshoot 82.89 dB and 120 ms
stereo 77.51 dB (`~/.cache/nmono-stereo/proof.json`).

All 68 native units pass (five explicit/old diagnostics ignored), including a
zero-allocation wide fold→ring→actual device converter→fallback-reference check
at 1/2/6 channels, local effects and maximum-finite opposing headroom. Existing
exact-rate/chunk/delay/underrun tests remain green. The original private DSP
suite also passes: echo 46 dB/0% transmitted, double-talk 0.8 dB/100% gate, clipping
detection and effects isolation/6 dB volume (`mono-dsp-regression.log`).
This does not establish the broader non-48 kHz SRC/final-clip ordering or full stock-device equivalence:
Mumble filters before its device-output clip; Nuschl retains its existing 48 kHz
mix clip before SRC and a separate post-filter device guard. Quantifying that
staging under overload needs its own reference/fallback design and fixtures.

## Native encoded-packet gain baseline (2026-11-12)

At source `63b5c2de`, a device-free probe compiles the actual native Mixer and
Instant adapter with the same reviewed decoder and uses a virtual receive clock.
Independent libopus packet/FEC/PLC operations provide fixed-gain PCM at specified
ticks; neither alignment nor gain is fitted. All source signals are synthetic.
The constant-gain control agrees at 323.78 dB, while changing packet controls
produce 4.63 dB (ordered), 3.74 dB (reorder/FEC/PLC), −4.66/−5.25 dB (120 ms then
10/20 ms loss), and −0.33 dB when a late duplicate's unrelated gain is injected.
The constant-gain 10–120 ms control remains 140.91 dB. The unchanged quality gate
is 35 dB. This confirms actual mixed output corruption, not just a source
suspicion. Preserve `~/.cache/nuschl-native-gain/baseline/` and its frozen probe.

The IPC volume is currently one product: remote packet adjustment × local
per-user gain × local server gain, sampled when the native client forwards a
packet. Locally muted users are filtered upstream; no per-user worker flush or
live gain command exists. Deafen/disconnect separately use EndStream/global
state to revoke a stream. Correct packet gain ownership must not be described
as instantaneous local mute/volume control: queued encoded/decoded/device audio
needs its own explicit semantics and control lane. The following correction is
limited to carrying each forwarded packet's already-sampled bounded gain through
the native jitter queue, matching the browser's typed-payload precedent.
