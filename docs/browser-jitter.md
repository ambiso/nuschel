# Browser sequence-aware voice playout

Status: shared policy/native adapter checkpoint; browser activation is pending.
No microphone consent, session, server, broker or desktop UI changes belong to
this work. Generic VoiceDecoder stays available for independent codec proofs.

The old browser worker eagerly decoded arrival order and rejected reordered
packets. Its Worklet separately held 20 ms of PCM, with no sequence-clock FEC or
PLC. Native uses adaptive encoded buffering and real decoder FEC/concealment.

## Shared policy checkpoint

`nuschl-audio-jitter` contains the existing native policy without retuning. It
has no external dependencies, codec, device, network or platform-clock calls;
callers provide monotonic `Duration`. Native retains a thin `Instant` adapter.
Its existing tests remain in place; the shared crate also runs the policy tests.

A frozen-source comparison exercises 10,000 arrivals with reordering, duplicates,
loss, terminators, restarts and 10–120 ms observed durations. All 19,881 pull
choices and exact float timing bits match source `abe83021`. The trace payloads
are intentionally synthetic policy bytes, not a codec fidelity proof.
`tests/jitter_equivalence.py` compiles the same standalone trace against supplied
old/current module files without a Cargo target. Evidence is
`~/.cache/nuschl-output-quality/jitter-equivalence/proof.json`.

All 68 native tests and 7 shared policy tests pass. Actual private jitter playback
also passes the existing 3020 ms/5-drop fixture (one silent 20 ms window, longest
20 ms). Root/fuzz lock changes add only the first-party shared package; the
existing third-party versions and unsafe boundary are unchanged.

## Browser design

The isolated codec worker owns per-speaker Rust playout instances. Incoming
sequence/end/arrival metadata enters the shared encoded buffer; fixed 10 ms
pulls choose normal decode, following-packet FEC or PLC. The engine retains
finite raw stereo PCM and its bounded gain until Worklet wide accumulation.
The existing Mumble phase setting is preserved through reset/mode transitions.
Legal packet durations are 10–120 ms in 10 ms sequence ticks.

A direct transferred MessageChannel joins the Worklet and codec worker. Only
bounded audio and local authorization/clock tags cross it, never session keys,
network endpoints, storage or identity objects. The Worklet's rendered sample
clock grants one 480-frame block at a time; there is at most one in-flight
render and two blocks/20 ms of transport reserve. Responses have an absolute
playout-frame deadline and epoch. Late blocks are discarded or trimmed; a
whole-block scheduling miss triggers playback-only rebuffer instead of a catch-up
codec burst. No codec work runs in the UI or audio callback. PCM batches bypass
UI state updates; packet enqueue acknowledgments stay bounded.

Production Worklet per-speaker startup hold is removed. It must not add another
20 ms hold on top of adaptive encoded buffering. Targets and transport reserve
are distinct from codec packetization and the browser/device output latency:

| Component | Policy/bound |
| --- | --- |
| Encoded jitter target | Existing native 40–200 ms adaptive target; unchanged catch-up policy |
| Packet/decode cache | At most 120 ms of decoded stereo per speaker; consumed on sample-clock demand |
| Worklet transport reserve | At most 20 ms, no extra per-speaker startup hold |
| Stalled codec worker | One in-flight render, bounded late discard/rebuffer; no queued catch-up work |

The target is not a hard end-to-end latency ceiling. Packet duration, existing
native catch-up window, browser/device buffering and suspension also matter.
Deterministic and real-browser proofs must record actual startup/steady delay.
Background scheduling is not assumed reliable; stale media must not accumulate.

Existing caps remain 16 speakers, 64 encoded packets of at most 1024 bytes per
speaker, existing 200 ms PCM queue cap, and a bounded render batch of at most 16
480-frame stereo blocks. Reception limits must admit legitimate 16×100 packets/s;
the old global 1000/s limit does not. Compute/heap measurements and actual WASM
bounds must accompany implementation, not be inferred from queue counts alone.

## Authorization and continuity

Epoch/membership/listen revocation must clear encoded, decoded, in-flight and
Worklet audio immediately. Capture/permission gates remain independent and strict.
An initial direct-port handshake must tolerate the main-worker and Worklet
configure messages arriving in either order.

The current mic-only mute/bitrate `resetMedia → configure → free` path also clears
all receive decoders. The browser stage must either preserve authorized receive
continuity with a separate receive epoch keyed to server/connection/channel/listen
permission, or explicitly retain that interruption as a TODO. It must never keep
old RX when route membership, deafen, output context or listening is revoked.

## Required evidence

Keep native behavior/tests through the shared refactor. Independent reference
libopus fixtures must cover loss/FEC/PLC, reorder, variable durations, stereo,
gain, phase settings and end/restart; codec frame sizes must match the missing
audio duration. Then run actual WASM and clocked Worklet, late/stalled workers,
epoch/deafen/privacy tests and a real private-browser call under production CSP.
No public servers, host devices or hardware/perceptual equivalence claim.

Primary interfaces: [Web Audio message ports](https://www.w3.org/TR/2021/REC-webaudio-20210617/#dom-audioworkletprocessor-port),
[transferable MessagePorts](https://html.spec.whatwg.org/multipage/web-messaging.html#message-ports),
and [Opus decoder PLC/FEC contract](https://opus-codec.org/docs/opus_api-1.5/group__opus__decoder.html).
