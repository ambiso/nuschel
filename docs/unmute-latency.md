# Native unmute latency

The native audio worker starts PulseAudio capture asynchronously after the server
acknowledges uncorking it. It no longer waits for the first PCM bytes before
starting playback and the echo-reference stream. Those streams start together;
a delayed reference cannot hold otherwise available microphone processing inside
`Stream::play()`. AEC reference ordering and its bounded capture hold remain in
force. Playback/mixing behavior is outside this change.

In the private virtual PulseAudio fixture, warm first processed samples moved
from about 103 ms to 16–22 ms after worker launch; actual independently decoded
Opus moved from roughly 87–105 ms to 27–33 ms for continuous/PTT. These are worker
command timings, not GUI-click or physical microphone measurements. The 20 ms
codec frame, device delivery time and scheduling still apply.

VAD keeps up to 80 ms of **processed** PCM from its current unmuted epoch. When
the existing classifier opens the gate, that prefix preserves the initial
syllables; it never opens the gate or waits for an 80 ms buffer to fill. A bounded
100 ms queue emits on a 20 ms clock, without a prefix packet flood or an unbounded
catch-up queue. Keeping an onset can leave up to 80 ms of queued audio while
speaking; overloaded/stalled processing drops old whole frames. PTT and
continuous mode never replay a VAD prefix. Mute, deafen, a new capture epoch,
settings changes, local preview and device failure clear queued capture;
activating the echo guard revokes unsent prefix frames. Only PCM processed while
unmuted can enter this queue. No microphone stays open when all destinations are
muted, and a mute ends transmission with constant Opus silence. No noise/AEC
processing or classifier threshold was disabled to obtain the result.

`tests/unmute_latency.py` runs a private Pulse daemon with virtual/null devices,
forces the worker to that explicit server, and decodes emitted Opus with independent
libopus. It measures first real packets, validates a first-packet 997 Hz prefix
marker followed by a 3000 Hz source marker, and checks mute termination, worker
exit, absence of a Nuschl microphone stream after mute and playback-only startup.
Rust queue tests cover boundedness, onset order, pacing, resets, echo-guard/PTT
exclusions and closed-gate silence. The optional marker fixture is exported by
the real queue/encoder unit test using `NUSCHL_PREFIX_PROOF_OUTPUT`; ordinary unit
tests write no file. Numeric opt-in worker stage diagnostics are enabled with
`NUSCHL_AUDIO_TIMING=1`; device callbacks never log or allocate for diagnostics.

Remaining limits are explicit:

- A cold virtual Pulse source still took about 1.3 s to deliver its first PCM.
  Independent libpulse `parec` took 1.413–1.415 s on fresh copies of the same
  fixture. Early "Audio ready" does not mean samples have arrived; this change
  cannot manufacture missing hardware/server input or promise instant cold start.
- RNNoise VAD still needs evidence from the input. The measured synthetic onset
  needed roughly one to five 10 ms frames. Prefix retention preserves the onset
  without lowering the confidence threshold; it does not make classification
  instantaneous.
- Physical Pulse/PipeWire, WASAPI/CoreAudio, actual speech onset and GUI-to-remote
  playback latency need their own measurements. The private worker fixture is
  synthetic and has no host microphone/speaker access.
