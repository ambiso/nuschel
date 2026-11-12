# Receive decoder corrections

Base: the exact published `opus-pure` 0.2.3 archive, retrieved from
https://static.crates.io/crates/opus-pure/opus-pure-0.2.3.crate (547373 bytes).
SHA-256: `5afacd236411dddcc284983066ef66b27e306e53ce4de85cd745b3da9be8a0be`.
Published `.cargo_vcs_info.json` identifies upstream revision
`3f6077b8bdc0f6c090c3b585de9294065a50e2a4` in
https://github.com/stephenberry/opus-pure.
The original `LICENSE`, `Cargo.toml.orig` and VCS metadata are unchanged.
`LICENSE` SHA-256: `c9fa68646ff1dda4747667ca2ab58c73b199f4876bb31c48172810a2d8a9234b`.

This patch adds a default-false intensity-stereo decoder setting and public
`OpusDecoder::set_phase_inversion_disabled` / `phase_inversion_disabled` controls.
It threads that setting to the already existing `quant_all_bands` argument,
which was hardcoded false. The existing band decoder still consumes the inversion
bit before overriding the synthesis sign: entropy state is unchanged. No encoder,
bitstream parser, unsafe/SIMD path, allocation or default behavior is changed.

`CeltDecoder::reset` resets histories in place, preserving this setting alongside
its existing downsample configuration. All `OpusDecoder` reset, mode/redundancy
transition and channel-switch paths use that same instance; none replaces it.
The added API test covers reset/defaults at every API rate and channel count.
Independent native/WASM fixtures and final-range comparisons are in
`tests/opus_phase.py` and `docs/output-quality.md`.

Nuschl's native and browser voice receivers opt in separately to match stock
Mumble's `AudioOutputSpeech.cpp` setting. Generic and nonlive decoder users retain
the original default. This is a deliberate mono-downmix/stereo tradeoff, not a
claim that inversion-disabled rendering is universally better stereo.

Additional bounded receive fixes match libopus 1.6.1 `opus_decoder.c`:

- Validate all FEC packet framing before concealment or state changes, including
  CELT/short-request fallbacks and leading PLC. Invalid odd-sized CBR payloads
  return an error without consuming decoder history.
- Treat parsed zero- and one-byte payloads as PLC/DTX before changing layer,
  internal sample rate or channel state, including padded/mixed-frame packets.
  A bounded stack buffer reuses the ordinary decoder for mixed nonshort frames.
  PLC reports zero final entropy range and clears consumed redundancy state.
  A private accepted-packet mode remains distinct from the synthesis mode, so
  short-frame mode switches select subsequent FEC fallbacks as libopus does.
- Report the actual SILK FEC range coder or hybrid CELT concealment RNG instead
  of retaining the previous packet's public final range. The CELT accessor is
  crate-private; no public codec API is added.

A separate admission follow-up exposes `packet::validate(&[u8]) -> Result<()>`.
It reuses the decoder's complete ordinary-packet framing parser without
allocation or state mutation. Header-only duration helpers retain their existing
semantics; callers can validate before changing jitter/sequence/gain state, then
read the duration. All valid Opus durations and zero/one-byte DTX remain accepted;
this adds no Mumble duration policy or compressed-entropy validity claim. Three
unit regressions cover valid DTX/padding/all packing codes, malformed framing and
the difference from TOC-only duration inspection. An independent libopus 1.6.1
`opus_packet_parse` audit matched all 74,019 cases (all zero/one/two-byte packets,
seeded longer packets, frame-size limits and explicit padded/multiframe cases);
evidence is at `~/.cache/nuschl-output-quality/framing-api/proof.json`.

The phase setting is unchanged. There are no new dependencies, unsafe code,
encoder changes or backend replacements. Original provenance bytes above remain
unchanged. Independent synthetic checks, evidence and known direct 8/12 kHz API
resampling limitations are documented in `docs/opus-silk.md`; the strict broad
audit remains failing until those separate limitations are corrected. These
patches do not claim general bitstream or perceptual parity.
