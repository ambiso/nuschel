# pulseaudio 0.3.2 local compatibility patch

Source: <https://github.com/colinmarc/pulseaudio-rs>, crates.io 0.3.2. The upstream
MIT license remains in LICENSE.md. This is a third-party dependency, excluded from
Nuschel's workspace and first-party lint/format ownership.

- Send a 256-byte zero authentication blob when no cookie is present. PulseAudio
  requires this wire size even when it authorizes a Unix peer by credentials or
  anonymous local test policy. A zero-length blob caused a protocol disconnect.
- Respect an explicit PULSE_COOKIE without first requiring a home directory. A
  confined audio process intentionally has no host home directory.

Remove the override after these fixes are available upstream. The real PulseAudio
integration test exercises the no-home/no-cookie sandbox case. No native C audio
implementation or new privilege is introduced by this patch.
