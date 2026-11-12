# Flatpak packaging

This directory is under active implementation. It does not yet contain a tested
installable Flatpak. The native Nix package cannot simply be copied into `/app`:
its ELF interpreter, RPATHs, dynamically loaded libraries and worker launch paths
need an explicit Flatpak mapping.

`runtime-lock.json` selects an exact official Freedesktop runtime commit for
reproducible build and ABI inspection. Deployment can receive compatible runtime
security updates through Flatpak. Initial architecture is x86_64; other architectures
need their own pinned runtime and actual artifact proof. No host Flatpak
installation/configuration or public repository is changed by the packaging tests.

## Worker boundary

Standard Flatpak restricts nested namespace creation. The existing native
bubblewrap adapter is not an unrestricted fallback inside Flatpak. The planned
adapter uses the Flatpak spawn portal, with an independent `SpawnExited` monitor;
exiting or killing a launcher proxy is not evidence that its worker has exited.
Unknown exit state retains resource credit and blocks replacement.

Every role starts with `--sandbox --clear-env --watch-bus`, fixed executable and
arguments, and only the inherited IPC descriptors. Limits must apply **inside**
the portal child: limits on the caller do not propagate across a D-Bus launch.
No role uses `--host`, grants the whole profile, or receives the parent D-Bus socket.

| Role | Additional authority |
| --- | --- |
| Content, math, update verifier | No network, user files, display, audio or devices |
| Storage | No network; selected history directory only |
| Crypto | No network; selected keys directory only |
| Media | No network; explicit sound capability, including ALSA devices if present |
| Transfer | Network for the configured transfer policy; one offered file read-only |

The media prototype targets PulseAudio's Flatpak sound capability
(`--sandbox-flag=share-sound`). Native PipeWire graph/default/processing detection
requires separate explicit socket and runtime proof; sound access is not evidence
of that parity. Existing microphone consent, epochs and relay-only policy remain
mandatory. Linux builds already disable in-app binary replacement; Flatpak owns
package updates.

The portal sound flag can expose PulseAudio and `/dev/snd`; it is not a
PulseAudio-only grant. No other role receives it. `--watch-bus` sends a signal
on owner loss and does not guarantee whole-tree exit. Keep actual worker/tree
exit proof and quarantine unknown outcomes rather than releasing credit when a
proxy or portal disappears.

Read-only data authority must use a read-only mount at the portal's validated
canonical path. An inherited `O_RDONLY` file descriptor can be reopened writable
through `/proc/self/fd`; do not pass it as a data FD. Setup descriptors must not
reach worker code: role workers receive only stdin, stdout and stderr.

## Acceptance checks

The builder must include the desktop plus its eight fixed helpers, unchanged
source-scoped license bytes, icons, desktop/URI registration and AppStream metadata.
It must reject unresolved ELF dependencies and incompatible symbol versions, not
merely remove the old RPATH. `org.nuschl.Nuschl.metainfo.xml` is shared descriptive
metadata; native distribution packagers may reuse it without a competing copy.

Before claiming support, install the actual offline-produced bundle in an owned
non-Nix Linux VM. Check launch, a private stock-Mumble connection, content/math,
SQLite and crypto helpers, synthetic audio, selected-file transfer, and per-role
file/network/audio/FD/environment canaries. Test failed confinement, cancellation,
worker termination and bounded replacement. No host profiles, clipboard or physical
sound devices are part of that fixture. A successful Nix build alone is insufficient.

Primary references: [Flatpak spawn portal](https://github.com/flatpak/flatpak/blob/1.18.4/data/org.freedesktop.portal.Flatpak.xml),
[pinned spawn helper](https://github.com/flatpak/flatpak-xdg-utils/blob/1.0.6/src/flatpak-spawn.c),
[Flatpak permissions](https://docs.flatpak.org/en/latest/sandbox-permissions.html).
