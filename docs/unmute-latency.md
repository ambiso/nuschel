# Native unmute latency

The native audio worker starts PulseAudio capture asynchronously after the server
acknowledges uncorking it. It no longer waits for the first PCM bytes before
starting playback and the echo-reference stream. Those streams start together;
a delayed reference cannot hold otherwise available microphone processing inside
`Stream::play()`. AEC reference ordering and its bounded capture hold remain in
force. The supervisor also skips its 30 ms coalescing delay for a first explicitly
authorized talk destination; moves between already active talk destinations retain
that delay and revision guards.

In the private virtual PulseAudio fixture, warm first processed samples moved
from about 103 ms to 16–22 ms after worker launch. First emitted Opus moved from
roughly 87–105 ms to about 30–35 ms for continuous/PTT.
A later measurement decoded every packet independently: the first non-silent
packet (RMS above 0.001, not an intelligibility score) arrived around 55 ms in
the warmed standalone Pulse fixture. These are worker-command timings, not
GUI-click or physical microphone measurements. The 20 ms codec frame, model and
codec startup, device delivery time and scheduling still apply.

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
exclusions, closed-gate silence and a second closed-to-open edge while the old
tail still drains. That edge retains the next onset without enlarging the queue
or flooding packets. The optional marker fixture is exported by the real
queue/encoder unit test using `NUSCHL_PREFIX_PROOF_OUTPUT`; ordinary unit
tests write no file. Numeric opt-in direct-worker stage diagnostics use
`NUSCHL_AUDIO_TIMING=1`; device callbacks never log or allocate for diagnostics. The current desktop
sandbox does not forward that flag and its launcher discards worker stderr;
these are fixture/direct-worker diagnostics, not an advertised desktop log switch.

`tests/unmute_pipewire.py` reuses the private PipeWire/WirePlumber recovery
fixture with hardware monitors disabled. Its virtual graph injects only synthetic
PCM, requests a 480-sample quantum, and tests native PipeWire separately from
pipewire-pulse. The real PipeWire WebRTC echo-cancel variant also verifies that
Nuschl reports upstream AEC and routes playback to its sink. In one comparison,
native PipeWire first packets were roughly 66/63/79 ms for continuous/PTT/VAD;
with upstream WebRTC AEC the final source emitted around 50–53 ms and first
non-silent decoded packets around 53–74 ms. These numbers do not establish that
the standalone Pulse wait explains the user's native-PipeWire delay.

Remaining limits are explicit:

- A cold virtual Pulse graph took about 1.3 s before first worker PCM. Independent
  libpulse `parec` measured about 1.42 s with producer latency 20 ms/process time
  10 ms and consumer latency/process time 10 ms. This remains a shared virtual
  graph observation, not proof of an unavoidable hardware limit or of the cause
  on the user's machine. Early "Audio ready" does not mean samples have arrived.
- The strict fresh-noise assertion in `tests/unmute_noise.py` is preserved. Live
  runs depend on source/callback context: some old and changed runs emitted
  deterministic white noise; other controlled runs with byte-identical captured
  input stayed silent on both. The zero-packet result is not universal.
  The explicitly ignored, opt-in owned-PCM diagnostic replays 200 identical frames
  through two instances of this build's DSP and compares their probabilities,
  processed PCM and admission decisions with/without prefix storage. It is not
  a general old/new implementation regression proof. One zero-reference replay
  admitted 0 frames with AEC off and 39 with AEC on (maximum voice probabilities
  about 0.564/0.653). Source/AEC/classifier context still needs investigation; no
  warm-up suppression or threshold change conceals it.
- Current native VAD requires agreement from Sonora/WebRTC and RNNoise at the
  pre-local-suppression, pre-gain tap; RNNoise remains the suppressor. Earlier
  RNNoise timings above are historical controls. The new model's default opening threshold is calibrated
  to 0.4: reusing RNNoise's 0.6 threshold delayed some quiet inputs beyond the
  existing 80 ms prefix. The detector is causal and persistent, but classification
  still needs input evidence; the prefix does not make it instantaneous. Quiet
  speech, real acoustic double talk and physical cold-start latency retain their
  separate verification requirements. A matched public capture-queue audit
  found five additional 10 ms frames omitted across 29 recorded-speech controls
  at an energy cutoff 30 dB below each clip's peak, and none at 20 dB below.
  Those quieter frames may include consonants; energy is not a phoneme label.
  This change does not claim perfect first-syllable retention for every voice.
- The combined native media process is still replaced on microphone mute/unmute
  or a changed speaking destination. This interrupts incoming playback while it
  restarts. Separating playback lifetime/capabilities is a distinct follow-up;
  keeping the microphone open while muted is not an acceptable workaround.
- Physical Pulse/PipeWire, WASAPI/CoreAudio, actual speech onset and GUI-to-remote
  playback latency need their own measurements. The private worker fixture is
  synthetic and has no host microphone/speaker access.
