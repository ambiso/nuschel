# Browser Worklet clock component proof

The direct port contract is specified in `browser-playout-port.md`. This note
covers the Worklet component only; codec-worker, generated WASM, private-browser
integration and physical device timing remain separate verification gates.

`PlayoutClock` counts frames actually handed to the playback output. Its first
request after a ready barrier starts at the next aligned frame plus 480 frames.
Later requests advance by 480 frames, with no more than one unacknowledged
request. A missed deadline continues to consume silence without granting another
request. An exact whole-late response clears buffered sound and increments the
clock generation; rendering resumes after the worker's new FIFO ready barrier.
Partially late responses retain their original timestamp and discard elapsed PCM.

The clock retains two 960-element f64 accumulation arrays (15,360 bytes total),
covering 960 stereo frames, or 20 ms at 48 kHz. Each accepted reply contains at
most 16 unique speakers and 480 stereo frames per speaker. PCM remains finite raw
f32, gain is finite 0..4, the sum uses f64, and clipping happens only at output.
Mono output folds the wide LR sum before clipping. The legacy `PcmMixer` remains
available for existing independent proofs, with its original hold and bounded
storage; its queues remain empty in production clocked mode. The processor's
legacy mixer scratch adds 153,600 preallocated bytes to the clock ring. These
figures exclude message metadata, transient transferred batches and browser
implementation storage. No PCM scratch array is created by clocked `process()`.

Clocked mode rejects main-port legacy PCM. RX configure changes flush the ring,
clear local pending output and require a new generation barrier. Repeating the
same RX/enabled tuple preserves state. Capture and codec operations remain
outside the clocked path. The Worklet checks its actual global sample rate and
fails at rates other than 48 kHz.

Verification at Worklet source checkpoint `3944d3b1`:

- `node --test tests/web_playout_clock.mjs tests/web_audio.unit.mjs` passed all
  27 tests: 13 new actual-processor clock tests and all 14 existing audio tests.
- The deterministic clock tests cover both setup orders, cadence, future/partial
  and whole-late PCM, 2,000 stalled callbacks, one 2.5-second stall report,
  duplicate and unsolicited tuples, RX revoke/reopen, disabled ready messages,
  port replacement/close, malformed batches, all 16 finite extreme contributors,
  stereo/mono gain behavior, rate rejection, variable callback sizes and a typed
  array constructor trap during callbacks.
- The unchanged `tests/web_headroom.mjs` passed with two owned synthetic stereo
  streams and an independently calculated wide-sum reference: 1,920 frames,
  maximum absolute error zero. Fixture and proof files were written under
  `~/.cache/nuschl-worklet-headroom-sed9jh7e/`; no device or codec was involved.

These tests prove component bounds and mixing behavior. They do not establish
realized end-to-end latency, legal packet duration handling in the codec worker,
FEC/PLC quality, mobile support or real hardware callback timing.
