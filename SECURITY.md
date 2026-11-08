# Memory safety and isolation

Prefer a memory-safe implementation for every component that handles untrusted data.
If a suitable implementation is unavailable, isolate that processing in a worker with
the minimum required capabilities. Even memory-safe code needs input/resource limits:
panics, excessive allocations, infinite loops, and corrupted output remain possible.

All first-party crates inherit `unsafe_code = "forbid"`. This is an enforced rule for
Nuschel source, not a claim about the entire dependency graph. A safe API around a C
library does not make its implementation memory-safe. A Rust port with unchecked SIMD
or raw pointers also needs review. Dependency changes must include that assessment.

## Explicit exceptions and boundaries

| Component | Reason | Required boundary |
| --- | --- | --- |
| GPUI and graphics/font/window backends | Native desktop rendering and accessibility | Desktop shell only; never parse message HTML in the renderer |
| CPAL and OS audio bindings | Access to PipeWire/PulseAudio/CoreAudio/WASAPI | Media worker; device handles and audio only |
| Codec and DSP SIMD in Rust dependencies | Optimized processing still contains unsafe internals | Media worker; no session credentials, arbitrary files, or network |
| SQLx SQLite backend | Native SQLite provides transactional local history | Storage worker; one database directory, parameterized operations; extension loading and deserialization disabled |
| SQLx PostgreSQL backend | Server persistence on a managed database | Storage worker; database credentials and a scoped database connection |
| TLS crypto provider | Cryptographic implementation and CPU-specific acceleration | Session/server authority; bounded protocol framing and reviewed provider |
| OS sandbox/process APIs | Establish restricted worker execution | Small supervisor adapter; prefer safe Rust wrappers |

The `cargo-deny` wrapper rules enforce ownership for the named top-level dependencies.
They do not automatically audit every transitive crate, dynamic system library, or new
dependency that has not been classified. Review a new dependency before adding an
allowance; keep exceptions narrow and explain the unavailable safer alternative.

## Worker contract

Workers are separate executables with versioned, size-bounded messages over inherited
pipes. They receive no server credentials. Content responses contain only inert text
and style data. A worker cannot name an executable, choose a database, request a URL
fetch, or send an arbitrary SQL statement through the protocol.

The Linux content supervisor uses bubblewrap namespaces, read-only runtime libraries,
an empty environment, private temporary storage, a 512 MiB address-space limit, a
five-second CPU limit, and a five-second wall deadline. It kills and reaps failed
one-shot helpers. It does not expose the user's home, audio devices, or network.

Desktop confinement must restrict filesystem/network access, inherited environment and
handles, process creation, memory, CPU, and wall time. Content processing needs no
user files, network, device access, or secrets. Media needs explicitly scoped audio
service/device access. Storage needs only the selected database and its sidecars.

Process separation and OS sandboxing are different guarantees. The worker executables
can also be run directly for development; that does not establish restricted privileges.
Do not describe an adapter as sandboxed until its platform policy is implemented and
tested. Native Windows/macOS and mobile require their own capability review. The
Linux desktop storage runs with only its writable history directory, runtime libraries,
private temporary storage and inherited IPC. The media profile grants audio sockets
and ALSA devices, runtime libraries and private temporary storage; it grants no host
home, D-Bus socket, session credentials, or network namespace. Both long-lived profiles
have an address-space limit. Media restarts remount recreated audio sockets.

An authorized audio service socket is a capability to that service's API; filesystem
and network namespaces do not restrict every operation an audio daemon exposes. Trusted
Nuschel code creates/changes only its own streams. The sandbox is not an audio-protocol
firewall or a claim that a compromised audio daemon cannot act outside it.

Muted clients do not open microphone streams. Capture generations invalidate queued
PCM and encoded packets across mute/deafen changes and audio-worker restarts. The
desktop replaces the media process on mute/deafen transitions: native stream teardown
can block, so dropping a stream alone is insufficient to guarantee capture closes.
Windows/macOS media/storage profiles remain unimplemented. The NixOS server's storage
preflight inherits its hardened systemd unit.

## Release gates

- Reject malformed/oversized frames before allocating the advertised body.
- Bound pending requests and terminate/reap stuck workers; never silently restart forever.
- Exercise content parsing with hostile inputs and test native codec interoperability.
- Verify workers cannot read an unrelated file or make a network connection.
- Test route/device churn and ensure recovery touches only Nuschel-owned resources.
- Run dependency license, source, vulnerability, and unsoundness checks.
- Preserve third-party notices and review dependency exceptions before distribution.
