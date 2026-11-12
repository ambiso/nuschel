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

Current Nuschl boundaries:

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
