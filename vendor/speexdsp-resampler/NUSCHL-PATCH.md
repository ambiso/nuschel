# Scalar SpeexDSP playback resampling

Base: the published `speexdsp-resampler` 0.1.0 source archive from
https://static.crates.io/crates/speexdsp-resampler/speexdsp-resampler-0.1.0.crate
(11,619 bytes), SHA-256
`b72d540d5c565dbe1f891d7e21ceb21d2649508306782f1066989fccb0b363d3`.
Its original `.cargo_vcs_info.json` identifies upstream revision
`b543f416aea84140e237bb3747a9d3a01c31fc9c` at
https://github.com/rust-av/speexdsp-rs . Original BSD-3-Clause copyright/license,
README and manifest provenance are retained. No new dependency or SIMD path is
introduced. The scalar library now also explicitly forbids unsafe Rust.

The published f32 process path clones its entire delay buffer repeatedly. In an
independent one-second 11,025 Hz test, that performs 610 allocations. Its kernel
also receives the beginning of the whole buffer instead of the chosen channel's
base; an independently phased stereo fixture reproduces incorrect right audio.
Current upstream `7c67def48fc5af526c5350915a4a1531fc1a0cfe` still contains these
paths, so upgrading the version number or disabling features does not fix them.

The patch is deliberately restricted to buffer ownership and indexing:

1. Temporarily take and reuse the existing delay `Vec` around the kernel call;
   restore it after moving the preserved tail with `copy_within`.
2. Pass `memory[mem_idx..]` to the kernel so each channel reads its own history.
3. Use `copy_within` for the magic-sample tail instead of allocating a temporary.

Filter coefficients, rate arithmetic, convolution and public APIs are unchanged.
Rate/quality initialization remains outside device callbacks. The unused integer
conversion and filter-reconfiguration paths retain upstream behavior; only the
f32 steady-state path is used by Nuschl playback.

`tests/resample_speex.rs` compares this actual implementation against test-only
SpeexDSP 1.2.1 at the quality-3 setting used by reference Mumble. Fixed-gain stereo
PCM agrees by at least 135.8 dB in the recorded matrix; callback chunks are exact,
10-second 11,025/22,050 Hz clocks remain aligned, and processing allocates zero
bytes. Source compiled with `--forbid unsafe_code`. See `docs/output-quality.md`
for provenance and the limits of these synthetic measurements. These are tests
of the resampler, not a claim of complete device/perceptual playback parity.
