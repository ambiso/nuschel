# Opus short-frame and FEC corrections

The independent 24-packet ordered receive failure was caused by valid one-byte
coded DTX frames, not established LBRR corruption. Packets 2 and 22 contain
`4b 41 38`, one coded `00` byte and 56 padding bytes. The original safe Rust
decoder passed that coded byte to its SILK range decoder. libopus treats any
parsed frame payload of zero or one byte as PLC/DTX, using the previous synthesis
mode. Subsequent frames also differed because the incorrect decode changed the
prediction history. Frozen packets, controls, original PCM and hashes remain at
`~/.cache/nuschl-output-quality/codec-lbrr-failure/`.

The correction recognizes short payloads before changing layer, internal rate or
channel state. It covers padded and mixed-frame packets through the same bounded
parser and uses a 1276-byte stack buffer to reuse the ordinary single-frame path
for nonshort frames in a mixed packet. There is no heap allocation in that path,
new dependency, unsafe code or backend change. PLC reports final entropy range
zero. The existing phase-inversion control and defaults are retained.

FEC now validates the complete future packet before any fallback or leading PLC.
The malformed `09 00` packet claims two equal-size frames but has an odd-size CBR
payload. Previously a CELT-mode fallback concealed before discovering that error;
it now returns `InvalidPacket`, preserving PCM history and final range. The
oracle also exposed a stale FEC range report: SILK FEC must report its own range
coder, including the no-LBRR case; hybrid FEC must report the CELT concealment RNG.
This reporting correction does not change FEC synthesis.

Accepted packet mode is tracked separately from the previous synthesis mode,
as in libopus. For example, a short CELT packet following SILK conceals in SILK,
but its accepted CELT TOC selects the fallback for the following FEC call. Reset
clears both modes. Malformed packets update neither.

Primary reference is the pinned libopus 1.6.1 source:
[src/opus_decoder.c](https://github.com/xiph/opus/blob/v1.6.1/src/opus_decoder.c)
(`opus_decode_frame` short-payload rule and range reporting;
`opus_decode_native` packet validation before FEC fallback), plus
[celt/celt_decoder.c](https://github.com/xiph/opus/blob/v1.6.1/celt/celt_decoder.c)
(`OPUS_GET_FINAL_RANGE`) and
[src/opus_private.h](https://github.com/xiph/opus/blob/v1.6.1/src/opus_private.h)
(test-only force-mode encoder control).

The device-free tests use libopus as a test oracle only. All application decoding
remains in safe Rust in its existing isolated process/worker. The oracle's CBR
encoder explicitly sets bitrate, channels, bandwidth, FEC, loss percentage and
mode; the 24-packet original uses its frozen original controls without force-mode.
No server, host audio, microphone, user profile or device is accessed.

Run using the cached Rust compiler and reference library (all artifacts on disk,
no Cargo/Nix graph rebuild):

```sh
mkdir -p "$HOME/.cache/nuschl-output-quality/silk-build"
CARGO_PKG_VERSION=0.2.3 rustc --edition=2024 -O --crate-type rlib \
  --crate-name opus_pure vendor/opus-pure/src/lib.rs \
  -o "$HOME/.cache/nuschl-output-quality/silk-build/libopus_pure.rlib"
rustc --edition=2024 -O tests/opus_silk_probe.rs \
  --extern "opus_pure=$HOME/.cache/nuschl-output-quality/silk-build/libopus_pure.rlib" \
  -o "$HOME/.cache/nuschl-output-quality/silk-build/probe"
python3 tests/opus_silk_fec.py \
  --opus-library /nix/store/ah26jmgs60s1dzphf0vi4f9pqmlpl9y6-libopus-1.6.1/lib/libopus.so \
  --probe "$HOME/.cache/nuschl-output-quality/silk-build/probe" \
  --frozen "$HOME/.cache/nuschl-output-quality/codec-lbrr-failure/packets.opus-frames" \
  --output "$HOME/.cache/nuschl-output-quality/silk-regressions" \
  --suite regressions --require-parity
```

The strict `regressions` suite covers the production receiver's 48 kHz decoder
API, ordered FEC on/off, stream/output mono/stereo, 10/20/40/60 ms SILK packets,
NB/MB/WB internal rates, real missing-frame FEC/PLC, SILK/hybrid/CELT short
payloads, padding and mixed frames, mode switches and malformed packet rejection
with exact same-decoder-state controls. It requires matching return values and
final entropy ranges plus at least 35 dB unity-gain PCM agreement, without delay
fitting. The original ordered fixture is bit-exact over all 46,080 samples and
all 24 final ranges, versus the preserved original 6.30 dB failure.

The separate full `--suite all --require-parity` audit adds all supported output
rates (8/12/16/24/48 kHz). Its gate remains strict and **fails**: pre-existing
SILK output resampling at the direct 8/12 kHz Opus API disagrees with libopus even
when entropy ranges match. This is not the native/browser receive path, which
decodes at 48 kHz and performs device conversion separately. Fixing that codec
resampler remains queued. These synthetic tests do not establish complete Opus
conformance, browser/native pipeline integration, physical-device quality or
perceptual parity with Mumble. See the preserved per-case `proof.json` artifacts
and [TODO](../TODO.md).

Final independent evidence (2026-11-12):

| Decoder API rate | Passing / total |
| --- | --- |
| 8 kHz | 0 / 60 |
| 12 kHz | 6 / 60 |
| 16 kHz | 60 / 60 |
| 24 kHz | 60 / 60 |
| 48 kHz, strict receive regressions | 241 / 241 |
| Full strict audit | 367 / 481 |

All 481 return-value and entropy-range comparisons match. The 114 remaining
failures are 8/12 kHz PCM comparisons (1.59–9.35 dB including FEC/PLC). The same
final test/probe source with the previous phase-control-only decoder passes
41/481: the fixes repair 326 cases without regressing any previously passing
case. Full baseline, fixed and strict receive records are respectively
`~/.cache/nuschl-output-quality/silk-published-final/proof.json`,
`silk-audit-final/proof.json`, and `silk-regressions-final/proof.json`.
Existing codec tests also pass: 154 units, 30 API, eight packet-loss, six
multiframe, two mode-transition and nine robustness tests.
