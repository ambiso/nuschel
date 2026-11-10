# Evaluation of the Rust audio stack

Nuschel's media worker uses memory-safe ports instead of the C/C++ originals:
[opus-pure](https://crates.io/crates/opus-pure) for Opus,
[Sonora](https://crates.io/crates/sonora) for WebRTC's high-pass filter, AEC3 and AGC2, and
[nnnoiseless](https://crates.io/crates/nnnoiseless) for RNNoise. This page records
how they were evaluated and what the measurements show. All numbers are from
2026-11-10 on an AMD Ryzen 9 5900X (release builds, one core).

## How to reproduce

| What | Command |
| --- | --- |
| Opus against libopus | `python3 tests/opus_eval.py <bin> --opus-library <libopus.so>` (also in the VM suite) |
| AEC3 in a reverberant room, CPU cost | `cargo test --release -p nuschel-media-worker -- --ignored --nocapture evaluation` |
| JSON IPC cost per voice packet | `cargo test --release -p nuschel-ipc -- --ignored --nocapture voice_ipc_cost` |
| Device callbacks never allocate | `cargo test -p nuschel-media-worker device_callbacks_never_allocate` |
| Live controller, jitter buffer, echo routing | `tests/media_live.py` (VM suite) |

libopus is loaded only by the test process, through ctypes, as a reference.

## Opus (opus-pure)

**Decoder conformance.** The same packets, decoded by libopus 1.6.1 and by
opus-pure, agree to within float rounding in every mode:

| Packets | Mode | Agreement (signal to difference) |
| --- | --- | --- |
| libopus, 10 kb/s | SILK | 155.5 dB |
| libopus, 20 kb/s | Hybrid | 155.7 dB |
| libopus, 32 kb/s | Hybrid | 155.5 dB |
| libopus, 64 kb/s | CELT | 142.4 dB (largest sample difference 1.5e-7) |
| opus-pure, 64 kb/s | CELT | 141.0 dB (largest sample difference 1.2e-7) |

**Robustness.** 5,000 bit-flipped, truncated, re-headed and random packets:
no crash, and opus-pure makes the same accept/reject decision with the same
output length as libopus for 100% of them. Like libopus, it can decode garbage
far outside full scale (up to ~2.6 million). The live mixer therefore clamps each
speaker to full scale before mixing.

**Encoder.** At a 64 kb/s target, both encoders chose CELT for every
packet. libopus used 42.0 kb/s and opus-pure 43.9 kb/s. Each stream decodes in
the other implementation, and stock Mumble interop runs in the VM suite.
Waveform SNR against the input was 13.3 dB for libopus and 29.6 dB for
opus-pure. This is **not** a perceptual verdict. libopus deliberately trades
waveform fidelity for perceptual tools (pitch pre-filter, spreading,
energy-preserving band folding). opus-pure's simpler encoder keeps the waveform
closer at a similar bitrate, which may or may not sound better. A listening test
or a perceptual metric (ViSQOL/POLQA) on real speech recordings is still
needed before claiming parity.

**Cost.** Encoding at 40 kb/s with in-band FEC takes 0.94% of one core per
talker. Decoding takes 0.10% per speaker.

## Echo cancellation (Sonora AEC3)

The test room is synthetic but reverberant: a 40 ms first reflection plus 64
sparse taps decaying over 250 ms (about 0.4 s RT60). The far-end and near-end
talkers are synthetic voices with pitch glides, formants, syllables and pauses.

| Measurement | Result |
| --- | --- |
| Echo reduction, far end only (seconds 3–6) | **32.8 dB** |
| Near-end level during double talk vs near end alone | −4.4 dB |
| AEC3's own ERLE estimate | 8.2 dB (a conservative internal estimate) |
| Residual-echo likelihood, per second | 0.05–0.33 |

So AEC3 converges within about a second and removes echo well. It ducks the
near-end talker by about 4 dB while both people talk, which is typical of the
WebRTC suppressor. The residual-echo likelihood stays below the adaptive
controller's 0.5 threshold in this room, so the echo guard stays off, as it
should. The unit test `echo_canceller_attenuates_a_delayed_copy_of_the_far_end`
guards against regressions (more than 15 dB required).

## Noise suppression and voice activity (nnnoiseless)

The unit tests check that RNNoise lowers steady noise and that its speech
probability opens the gate for voiced speech while staying closed for steady
noise. The whole capture chain (HPF, AEC3, RNNoise, AGC2 and limiter) costs
**1.31% of one core**.

What is not yet evaluated: recorded real-world noise (keyboards, fans, street)
and real speech. The synthetic signals show that the ports work and are
stable, not how good they sound. That needs recorded material and a
perceptual metric. No such corpus ships with the repository.

## Real-time safety

- The device callbacks only convert samples and move them through lock-free
  SPSC rings (rtrb). Underruns are counted with one relaxed atomic.
  `device_callbacks_never_allocate` runs both converters under a counting
  allocator, including the underrun path, and requires zero allocations. A
  control measurement proves the counter is active.
- Decoding, mixing, DSP and encoding run on the worker thread, never in a
  device callback.

## Binary IPC for audio: not warranted

JSON framing of a voice packet (about 100 bytes of Opus) produces a 479-byte
frame. Encoding and decoding one packet each way costs 7.4 µs, which is
**0.15% of one core** with four people talking at once. Over a local pipe that
is about 24 kB/s per stream. A binary format would save little and add a second
parser to the attack surface, so audio stays on the same bounded, versioned
JSON framing as everything else. Revisit this if profiling on low-end
hardware says otherwise.
