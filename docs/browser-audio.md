# Browser voice

Browser voice uses the existing Mumble connection, with Opus packets carried in
TLS `UDPTunnel` messages through the configured WebSocket relay. It does not
establish a peer-to-peer connection or expose the browser's address to other
participants. The configured relay and Mumble server remain trusted network
endpoints. This transport is separate from the newer RTC transport work.

## Consent and routing

Joining voice enables playback following an explicit user gesture and starts
muted. Unmute requests microphone permission explicitly. The browser requests
its native echo cancellation, noise suppression and automatic gain control;
the status reports the settings the browser actually exposes, with missing
settings recorded as unknown. These settings do not prove measured echo
cancellation quality or detect processing elsewhere in an operating system's
audio graph.

Only one explicitly selected server/channel receives microphone packets. Text
navigation does not move that route. Microphone consent is bound to the server,
connection and channel. Disconnecting, changing that destination, or stopping
audio releases capture and invalidates old results. A late permission result
is stopped after a newer mute. Unmute also waits for the server's matching
self-state acknowledgment; local intent alone does not permit transmission.

Mute closes capture immediately. Deafen closes capture and flushes playback.
An explicit playback gesture can reopen the local playback gate, while the
SDK's effective deafen state continues to gate output pending acknowledgment.
Undeafening does not restore microphone consent. Suspended audio contexts need
another user gesture. UI controls distinguish the SDK's local mute/deafen
intent from effective restrictions while a server acknowledgment is pending.

## Media boundary and bounds

`BrowserAudio` in `web/app/src/audio.js` handles permission, Web Audio nodes and
routing gates. It exchanges only bounded PCM/Opus messages with a dedicated
module Worker. TLS, certificate identity and session state stay in the SDK
Worker; they are not passed to the codec Worker. A browser Worker is a separate
JavaScript/WASM realm, not a guarantee of a separate operating-system process.

`nuschl-audio-wasm` uses the memory-safe `opus-pure` implementation and forbids
first-party unsafe Rust. Its WASM imports contain no network, storage or clock
capability. The fixed same-origin JavaScript loader fetches the codec module;
the media protocol provides no caller-selected URL. AudioWorklet modules are
also fixed same-origin resources. The browser's audio and permission services
are system-service boundaries.

Capture is mono 48 kHz in 20 ms frames. Encoded output is capped at 960 bytes.
Decode validates packet duration before allocation, accepting at most 120 ms
per packet. Speaker state is capped at 16, each playback queue at 120 ms, and
both decoder jobs and worklet transfers at 16. Capture and acknowledged SDK
sends have two credits each. Encode/decode results older than 120 ms are
ignored. SDK send acknowledgment has a 250 ms deadline; a stalled route closes
capture. Old epoch completions cannot release new epoch credits. Worker
heartbeat failure tears down media and requires explicit retry.

These are application bounds rather than a hard CPU or memory quota enforced
by the browser. A malicious packet remains subject to the browser's own worker
and WASM runtime isolation. No claim of native process sandbox equivalence is
made.

## Browser support and deployment

The baseline uses Rust WASM Opus rather than requiring WebCodecs AudioEncoder.
WebCodecs codec support is optional and older Safari/iOS releases cannot be
assumed to provide Opus encoding. Voice still requires a secure context,
AudioWorklet, a 48 kHz AudioContext, and user permission. AudioContext resume
and permission acquisition begin synchronously with the activating gesture to
support autoplay policies; there is no physical Safari/iOS or Android audio
hardware proof yet.

The modules operate with same-origin scripts/workers/connections and
`script-src 'self' 'wasm-unsafe-eval'`, without inline scripts, blob workers or
JavaScript `unsafe-eval`. Deployment must permit microphone access for the
application origin; camera access is not needed for voice.

## Verification scope

`node --test tests/web_audio.unit.mjs` exercises the production worklet queue,
mixing, policy epochs, stale results and acknowledged-send credit behavior.
The native codec tests check actual tone encode/decode with spectral validation
and reject malformed/non-finite/unbounded inputs without poisoning subsequent
codec use.

`tests/web_audio.py` runs the actual production audio modules, generated Rust
WASM codec and SDK Worker against an owned loopback Murmur and relay under
Chromium. Its wrapper exposes only the Nix store and owned scratch, constructs
a fresh device tree without sound/camera devices, and hides host session
sockets. Input is an owned 997 Hz WAV supplied through Chromium's fake capture
device. Output uses a null AudioContext sink, with an ALSA null fallback and
unavailable private Pulse/PipeWire sockets. Worklet PCM is measured in the
browser; an independent libopus decoder verifies the wire data's 997 Hz
carrier. No physical microphone or speaker is part of this fixture.

The controller fixture and the separate production App fixture prove different
layers. Controller success alone does not establish UI control correctness,
mobile audio compatibility, real-device echo cancellation, packet-loss quality,
or long-running audio performance. Exact run evidence and any incomplete
assertions are recorded in the integration handoff rather than inferred from a
green unit suite.

The extended Chromium 156 controller run passed against stock Murmur 1.5.939
with its default control rate limits: actual server Opus interchange and worklet
PCM, startup mute, explicit synthetic capture, deafen silence, playback-only
undeafen, transmission stopping on mute, disposal of a late permission result,
four rapid deafen toggles and muted reconnect with a new connection generation.
An independent libopus 1.6.1 decoder verified 27 wire packets (25,920 finite
samples) against the owned 997 Hz input. No CSP violations, foreign-origin
requests or browser errors were observed. This is a controller integration
result; production App control tests are recorded separately.
