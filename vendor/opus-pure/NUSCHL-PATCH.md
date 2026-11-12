# Receive phase-inversion decoder control

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
