# Local speech synthesis and transcription

Requested on 2026-11-11: optional Kokoro TTS and Voxtral Mini 4B Realtime
transcription, with models downloaded only when needed. This is a design and
implementation queue, not a shipped feature. Speech inference must never become a
prerequisite for joining a server or using ordinary text and voice.

## Runtime choices and existing reference

The local `~/Projects/whisper-cpp` project is the first implementation reference.
Its `flake.nix` enables `pkgs.llama-cpp.override { rocmSupport = true; }` on
`x86_64-linux`. Its Python adapter selects
`ggml-org/Voxtral-Mini-4B-Realtime-2602-GGUF:Q8_0`, uses `llama-mtmd-cli`, and lets
llama.cpp download on first use. It resamples recordings to mono 16 kHz PCM and
cuts them near silence into approximately four-minute chunks. This establishes a
concrete model/runtime integration to reuse; it does not establish live-caption
latency, Vulkan compatibility, or Nuschl's isolation guarantees. Do not copy its
URL downloader, mutable model selector, stdout scraping, or temporary-file pipeline
into the live audio path.

Upstream llama.cpp merged the stateful `mtmd_audio_stream` API and Voxtral Realtime
support in [PR 30629](https://github.com/ggml-org/llama.cpp/pull/30629). The installed
reference project's llama.cpp 0.8.0 development headers expose push, flush and step
operations. Evaluate this API in the inference helper against a pinned revision;
the original merge explicitly left CLI/server integration to follow-up work.
Use actual streaming state rather than retranscribing growing recordings.

Native backend targets:

| Platform | Initial backend | Additional backend | Fallback |
| --- | --- | --- | --- |
| Linux | Vulkan for broad GPU coverage | ROCm on compatible AMD hardware, explicitly available through Nix | CPU |
| Windows | Vulkan | Other accelerators only after separate review | CPU |
| macOS | Metal on Apple silicon | Vulkan is not a prerequisite | CPU |

Provide optional Nix package variants for Vulkan and ROCm, building the same pinned
runtime/model adapter with different backend options. ROCm should use the nixpkgs
`rocmSupport` override, following the local reference, without pulling its large
closure into every desktop installation. Keep the default compatible with systems
without ROCm. Runtime selection must report the actual selected device/backend and
allow an override. Do not assume that enabling a backend proves that every Voxtral
operation runs there or that a given GPU is supported. Test CPU fallback, unavailable
devices, out-of-memory failures, and safe recovery without repeatedly retrying a
broken GPU. Record real-time factor, first partial/final latency, VRAM/RAM and the
effect on call quality before choosing defaults.

Kokoro is a separate TTS backend decision. Its
[upstream model](https://huggingface.co/hexgrad/Kokoro-82M) and
[ONNX implementation](https://github.com/thewh1teagle/kokoro-onnx) provide concrete
references; no llama.cpp Kokoro support has been established by this review.
Prefer a suitable memory-safe implementation after verifying model, voice and
grapheme-to-phoneme compatibility. Otherwise evaluate ONNX/native inference only
inside the same restricted helper boundary. CPU TTS is a valid baseline; do not
promise Vulkan or ROCm support for Kokoro merely because Voxtral uses llama.cpp.
Review model, conversion, voice and phonemizer licenses separately.

## Isolation and audio routing

Use a dedicated inference process, separate from the media worker. The user's
llama.cpp request permits a C/C++ inference runtime at this boundary; it does not
change the exclusion of C++/Go WebRTC implementations or permit linking inference
into the desktop, server, SDK or media worker. Review the exact runtime dependency
graph and record the narrow exception in the safety policy before implementation.

The helper gets inherited IPC, the selected verified model files read-only, a bounded
scratch area, and only the device/driver access its backend needs. It gets no
network, microphone/audio-service handles, identity keys, history database, or home
directory. GPU access expands its attack surface and must be reviewed and tested on
each platform; a subprocess alone does not prove confinement. Start on demand,
unload after inactivity, bound memory/work/outputs and reap stuck processes. Call
audio must remain responsive if inference crashes, stalls, or exhausts resources.

Send bounded, timestamped PCM chunks from an asynchronous media-worker tap. Never
run inference or block IPC in an audio callback. Transcribe authorized, locally
audible per-speaker streams before mixing, preserving participant identity without
model-based diarization. Optional own-microphone captions must not open capture by
themselves and must honor capture epochs and mute/privacy transitions. Changing
server, channel, participant instance, or authorization cancels the relevant work;
generation-tagged results cannot appear in a later conversation. Cap active streams
and discard stale work with a visible caption gap instead of accumulating latency.

Show partial captions and final text with stable segment IDs and timestamps. Default
to local ephemeral captions: no raw audio files, transcript history, remote upload,
or publication as chat messages. Persistence, sharing and transmission are explicit
user choices, with retention and deletion behavior defined before they are added.
MLS sessions can supply only media the client is already authorized to decrypt;
inference receives no MLS keys and cannot enable currently unsupported MLS voice.

Kokoro initially reads explicitly selected chat or composer text to local playback.
Synthesized playback must enter the owned mixer and echo-reference path. Never feed
TTS back into caption input. Sending synthesized speech to a channel requires an
explicit transmit action and the ordinary speak/mute/PTT/authorization checks; TTS
must not silently unmute or create a new speaking destination.

## Downloads and packaging

Keep model weights out of the default client package and ordinary Nix builds. On
first enabling a feature, show its model/voice selection, total download size and
disk requirement, then offer download with progress, cancellation and retry. Reuse
verified cached files offline and offer removal. Optional Nix model packages or
explicit import can support offline installations without mandatory runtime fetches.

A trusted manifest pins revision, filename, size, SHA-256 and license for every
required file, including Voxtral's audio encoder/projector, tokenizer and Kokoro
voices/phonemizer assets. The downloader belongs outside the networkless inference
helper. Verify bounded streaming downloads before atomic installation into a
dedicated disk-backed cache; handle interrupted downloads, disk-full and concurrent
requests. Never load incomplete/unverified files, execute downloaded model code, or
unpickle runtime downloads. Reuse existing local caches only after verification,
without mutating another application's files. Do not use RAM-backed `/tmp` for
models or recordings. Pin runtime sources in the flake and generate dependency
metadata through the existing build tooling.

## Implementation order and evidence

1. Pin a runtime/model fixture and prove incremental Voxtral output from a recorded
   synthetic/test fixture on CPU, then Linux Vulkan and Nix ROCm. Establish the
   Kokoro runtime/phonemizer separately. Do not acquire large weights as a side
   effect of dependency evaluation or normal tests.
2. Implement verified model lifecycle and the inference process/sandbox contract,
   including denied network/filesystem access and bounded failure recovery.
3. Add generation-safe audio taps and local caption/TTS commands/events to the SDK,
   then desktop controls, accessibility, cancellation and download UI.
4. Test long-running latency/backpressure, inference failure during calls, mute and
   channel changes, model/cache failures and GPU contention using private fixtures.
   Validate Windows Vulkan and macOS Metal separately. Browser/mobile backends need
   separate lifecycle and isolation decisions; do not assume native helpers transfer.
