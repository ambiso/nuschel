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
| SQLite through rusqlite | Mature transactional history persistence | Storage worker; one database directory, parameterized operations |
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

Desktop confinement must restrict filesystem/network access, inherited environment and
handles, process creation, memory, CPU, and wall time. Content processing needs no
user files, network, device access, or secrets. Media needs explicitly scoped audio
service/device access. Storage needs only the selected database and its sidecars.

Process separation and OS sandboxing are different guarantees. The worker executables
can also be run directly for development; that does not establish restricted privileges.
Do not describe an adapter as sandboxed until its platform policy is implemented and
tested. Native Windows/macOS and mobile require their own capability review.

## Release gates

- Reject malformed/oversized frames before allocating the advertised body.
- Bound pending requests and terminate/reap stuck workers; never silently restart forever.
- Exercise content parsing with hostile inputs and test native codec interoperability.
- Verify workers cannot read an unrelated file or make a network connection.
- Test route/device churn and ensure recovery touches only Nuschel-owned resources.
- Run dependency license, source, vulnerability, and unsoundness checks.
- Preserve third-party notices and review dependency exceptions before distribution.
