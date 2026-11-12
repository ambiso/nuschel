# Native capture conversion quality

At `89806ef2`, the actual native `CaptureConverter` still uses linear
interpolation/decimation, full-channel average and a unit input bound. The
untouched converter fixture (`20381267`) changes no production statement.
Independent C SpeexDSP1.2.1 quality3 is the source-matched SRC policy from pinned
Mumble `35dce9e4801fe52b2217f0d9f421d17b0aaf3bb0`,
`AudioInput.cpp:515–516,587`. The C reference duplicates mono into two identical
independent filter channels and verifies equality before taking one; it does not
compare full Mumble capture/DSP/encoding or add its PCM16 quantization.

`tests/capture_quality.py` runs actual ignored native converter fixtures with
1/2/6channel input, five device rates, low/high tones, an above-target-Nyquist
alias case and impulses. No delay/gain fitting occurs. Whole and fragmented
callbacks are byte-identical and one second remains48000output frames; this
count alone does not prove filtering or timestamp correspondence.

The frozen baseline command exits1:21of26 cases fail the unchanged100dB/2e-6
sample-agreement gate. All five48k bypass cases pass. Upper-band carriers lose
3.65dB at11.025/22.05/44.1k input. Their images measure−14.4dB versus C
−76.84…−94.96dB. At96k,36k aliases to12k at approximately0dB, versus C−96.56dB.
The tested96k impulse lies on a decimation phase the old converter discards;
its complete output is zero. No physical device, host audio, public server,
profile or native encoder is used, and no perceptual quality claim follows.

See [the preserved actual failure](reviews/2026-11-12-capture-baseline.json).
Raw float audio, scripts, plots, exact cached native fixture and logs remain
under `~/.cache/nuschl-capture-quality/`. The production correction and strict
channel/clock/overflow/reopen/unmute/AEC tests remain open until measured.
