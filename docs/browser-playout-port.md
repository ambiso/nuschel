# Direct browser playout port contract (version 1)

This freezes the interface between the codec-worker/audio-controller owner and
Worklet owner. It is an internal same-origin MessageChannel, with no session,
network, storage or identity capabilities. Existing capture messages are unchanged.
All frame positions are **48 kHz audio frames**, relative to the current RX
configuration. `frames` is always 480 (10 ms), stereo raw PCM remains unscaled,
and gain is finite 0..4. No codec executes in Worklet or UI callbacks.

## Ownership and setup

Main installs one transferred MessagePort at each end using
`{kind:"playout-port",port}` (worker.postMessage / playbackNode.port.postMessage).
Install the worker's message handler before awaiting WASM initialization; retain
at most one port and the latest configure while loading. Main retains separate
TX `epoch` and RX `receive_epoch`. TX-only changes do not configure/reset Worklet.

Main → codec-worker configure:
`{kind:"configure",epoch:TX,capture:boolean,bitrate:number,receive_epoch:RX,playback:boolean}`.
The existing TX capture gates/credits/epoch remain strict. RX authorization is
independent: only matching authorized RX may enqueue/render. Actual RX revocation
clears receivers; mic-only TX changes must not clear still-authorized RX.

Main → Worklet control:
`{kind:"configure",epoch:RX,enabled:boolean,clocked:true}`.
Same RX/enabled configuration is idempotent. A changed configuration flushes PCM,
changes the internal clock generation and waits for a direct-port ready barrier.
`{kind:"close"}` flushes/disables, closes the owned direct port and releases it.

Legacy compatibility: omitted `clocked` keeps the current PcmMixer startup hold
and PlaybackProcessor `playback`/`queued` test interface. Keep `.mixer` and exported
PcmMixer APIs and stereo/headroom/mono behavior for existing independent proofs.
Clocked production mode must not accept legacy main-port PCM and has **no extra
per-speaker 20 ms hold**. Worklet file remains a static self-contained module,
without relative imports that Vite's AudioWorklet asset URL would fail to bundle.

## Direct port messages

Worklet → worker:

- `{kind:"playout-configure",epoch:RX,enabled:boolean,clock_generation:G}`.
  G is a positive safe integer, strictly increasing across configurations/resync
  for this port lifetime, even when RX changes. Ignore stale G; duplicate exact
  configuration may repeat ready without changing state. Store only latest wanted
  configuration. When authorized matching RX exists, acknowledge it. If the two
  configure paths arrive in opposite order, wait rather than dropping the port.
- `{kind:"render",epoch:RX,clock_generation:G,request:J,start_frame:F,frames:480}`.
  J is a positive monotonically increasing safe integer. F is a nonnegative safe
  integer divisible by 480, with room for 480. Only one render may be unacknowledged.

Worker → Worklet:

- `{kind:"playout-ready",epoch:RX,enabled:boolean,clock_generation:G}`.
  Echo only the current wanted tuple after matching Main RX authorization exists.
  `enabled:false` flushes/disables rendering; it does not release an outstanding
  render merely to submit another. Main-authorized true plus wanted true can
  become ready later. New-generation ready is a FIFO barrier: the worker has
  handled earlier requests on this same direct port before acknowledging it.
- `{kind:"playout",epoch:RX,clock_generation:G,request:J,start_frame:F,frames:480,speakers:[...]}`.
  Echo the **exact** request tuple. Speakers has 0..16 unique u32 IDs, each
  `{speaker,samples:Float32Array(960),channels:2,frames:480,volume:number}`.
  Every PCM value is finite; gain is finite 0..4, preserved with its packet through
  reordering. FEC/PLC use the preceding known gain, not a later packet's control.
  Transfer sample buffers; no unbounded pending output or large copies per quantum.
  An empty batch is a valid acknowledged silent/buffering block. Reject stale
  requests before decode. There is at most one decode per speaker per render.

All strings are bounded ≤200 characters. Validate integers/typed arrays/unique IDs,
frame lengths and finite controls before mutating playout state. A matching but
malformed reply fails the audio lane; it cannot inject partial PCM. Unsolicited,
duplicate, wrong RX/G/J/F or old replies do not clear another outstanding request.

## Audio clock, bounds and failure behavior

Use actual rendered frame count, not a JS timer, to grant 480-frame blocks. Keep
at most two blocks/960 frames (20 ms) of transport reserve and one outstanding
request. Initial request starts at the next aligned frame **plus one block**;
subsequent requests start 480 frames apart. Request when its start is no farther
than 480 frames ahead. A late scheduler must not produce catch-up decode bursts.

A convenient implementation is a fixed 960-frame stereo f64 accumulation ring:
validated batches mix unscaled finite PCM×gain into their absolute future slots,
then final clip occurs at rendering (mono folds wide LR before clipping). Clearing
consumed slots prevents stale wraparound. Other bounded implementations are fine.
No per-process-call PCM allocation or per-sample Map iterator creation. Existing
PcmMixer fallback APIs can stay separate from this clocked path.

For an acknowledged batch whose block partially elapsed, trim elapsed frames;
never shift it later. If the entire block expired, discard it and perform a
playback-only rebuffer handshake: increment G, flush PCM, send configure again,
wait for ready. **Deadline expiry alone never clears inFlight or submits another
render.** Until its exact reply arrives (or worker replacement), output silence.
A changed configuration may cancel locally but must wait for the new FIFO-ready
barrier before issuing fresh work. This bounds worker inbox growth across rapid
revoke/reopen and stalls. Unknown/duplicate replies never act as acknowledgments.

After 2500 ms of rendered audio without acknowledgment, send Main exactly one
`{kind:"playout-stalled",epoch:RX,clock_generation:G,request:J,start_frame:F}`;
keep the request outstanding. Main validates current RX, fails/replaces the worker
and closes capture according to its existing policy. Fatal rate/configuration or
matching malformed replies send Main one bounded
`{kind:"playout-error",epoch:RX,message:string}`. No per-packet UI/timeline events.

Clocked Worklet must assert actual global sampleRate 48000. Main already requests
and verifies AudioContext 48000; neither side may silently play pitched audio at
another rate. A codec-worker wall-clock demand gap can independently clear stale
receiver caches before resuming; capture/permission state is not changed by that
playback-only rebuffer. Browser/device/codec latency is additional to this 20 ms
transport ceiling; measure realized delay, including single 120 ms/end/restart.

## Required boundary tests

Both configure-order permutations; stale/duplicate/unsolicited tuples; partial
late batches; complete deadline expiry without repeated requests; long suspended
worker with packet ACK cap; revoke/reopen and new-generation barrier; incorrect
rate; finite extreme opposing headroom; mono fold; variable quantum lengths;
bounded memory and rendering cost. Existing PcmMixer and PlaybackProcessor proofs
must remain green. Actual WASM/worker/private-browser/CSP tests are a separate gate.

## Admission component checkpoint

Before browser activation, native Mixer and WASM VoicePlayout validate full Opus
packing before creating a speaker or changing arrival/sequence/gain state. The
public validator is allocation-free; it does not authenticate entropy content.
The separate packet-duration gate retains Mumble's 10–120 ms integer tick range.
The worker can use VoicePlayout.validate_packet before allocating a decoder.

Six independent, fixed-gain/unfitted-delay libopus action fixtures pass in both
the actual WASM artifact and native component: ordered FEC, reorder/loss/FEC/PLC/
restart, all 10–120 ms durations, 120 ms followed by 10/20 ms holes, and malformed
packing before another queued valid packet. The 20 ms loss now reaches 81.59 dB
(after the separate codec fix), while malformed admission reaches 71.26 dB
instead of 6.34 dB. The unchanged quality floor is 35 dB. Frozen failures remain
under `~/.cache/nuschl-output-quality/`; final artifacts are
`admission-native-final/proof.json` and `admission-wasm-final/proof.json`.

Focused Nix tests pass 70 native worker tests (5 diagnostic tests ignored) and
7 audio component tests. Actual WASM codec artifact:
`/nix/store/0ad60z2nas0l24pqw0q51z3lzx2vdppw-nuschl-web-audio-codec`.
This is component evidence; direct-port browser activation is still pending.
