# cpal 0.18.2 local patch: application and stream names

Source: <https://github.com/RustAudio/cpal>, crates.io 0.18.2. The upstream
Apache-2.0 license remains in LICENSE. This is a third-party dependency,
excluded from Nuschel's workspace and first-party lint/format ownership.

Volume mixers (pavucontrol, GNOME Settings, Helvum) listed every stream as
`cpal-playback-<pid>`, `cpal-capture-<pid>` or `cpal-pulseaudio-<pid>`, so the
microphone, the voice playback and the echo canceller's recording of the
speakers could not be told apart, and the pid defeated volume restore.

The patch is a backport of the open upstream pull request
<https://github.com/RustAudio/cpal/pull/1409> ("feat(pipewire,pulseaudio): name
the application and its streams", head of 2026-10-21) to 0.18.2:

- `ApplicationInfo`, `StreamMetadata` and `StreamRole` (src/stream_metadata.rs).
- PipeWire and PulseAudio: `Host::with_application` names the client and every
  stream's application and icon; `Host::set_stream_metadata` names the streams
  built from the host's devices afterwards, and sets their `media.role`.
- `PulseAudioHost` is exported from `cpal::platform`.

One addition beyond the pull request: on PipeWire a stream name set through
`StreamMetadata` also becomes the node's `node.description`, which graph tools
show instead of `node.name` (the application's name on every node).

Remove the override once a cpal release contains this API (adapt the media
worker's `connect` and `label` if its final shape differs).


PulseAudio capture startup (2026-11-12): `Stream::play()` waits for the
server's successful uncork acknowledgement, then returns without awaiting
`RecordStream::started()` (the first PCM bytes). CPAL's contract is to start
the stream; data delivery is asynchronous. A silent/cold reference source must
not block setup and processing of a microphone that already delivers PCM.
Record callback/error delivery and playback behavior are unchanged; streams
remain initially corked and no microphone is retained while globally muted.
Private virtual-source timing plus independent libpulse and encoded-PCM tests
cover this change; it does not promise to eliminate hardware/server cold-start
latency. See <https://docs.rs/cpal/0.18.1/cpal/traits/trait.StreamTrait.html#tymethod.play>.
