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
| SQLx SQLite backend | Native SQLite provides transactional local history | Storage worker; one database directory, parameterized operations; extension loading and deserialization disabled. `nuscheld --import-murmur` has the worker read an untrusted Murmur file confined by Landlock to that file: read-only and immutable, `query_only`, `trusted_schema` off, cell size checks, a heap limit, `quick_check`, no WAL files, ordinary tables only, every column capped, fixed statements |
| SQLx PostgreSQL backend | Server persistence on a managed database | Storage worker; database credentials and a scoped database connection |
| TLS crypto provider | Cryptographic implementation and CPU-specific acceleration | Session/server authority; bounded protocol framing and reviewed provider |
| OS sandbox/process APIs | Establish restricted worker execution | Small supervisor adapter; prefer safe Rust wrappers (`win32job` for Windows job objects) |
| `aes` (OCB2 voice crypto) | Mumble's UDP voice encryption; AES-NI intrinsics | Session authority; fixed-size blocks, keys from the TLS control channel |
| `rsa` (identity generation only) | Stock Mumble imports only RSA client certificates | Generates the local Mumble client key only; TLS signing uses ring, so the Marvin timing issue (RUSTSEC-2023-0071) in private-key operations never runs on untrusted input; `cargo-deny` confines the crate to `nuschel-session` |
| `image` decoders (PNG, JPEG, GIF) | Inline images and avatars from untrusted peers | Content worker only, with dimension and allocation limits; the UI receives raw RGBA |
| `akd` (key transparency) | Auditable key directory; no memory-safe alternative with VRF and audit proofs | Through `nuschel-kt` only, in the crypto worker and `nuscheld`; akd_core turns on ed25519-dalek's `legacy_compatibility` for the whole binary, so `nuschel-kt` checks `s < ℓ` itself and `nuschel-identity` uses libcrux, not ed25519-dalek; akd's verifiers run under `catch_unwind` behind a strict decoder (docs/e2ee.md §5.2) |
| Global shortcuts (`ashpd` portal; `global-hotkey` on Windows/macOS) | System-wide push-to-talk | Desktop shell; press/release of one bound key only |

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

Against a hostile server the session keeps budgets: 4,096 channels, 16,384 users and
64 MiB of comments, descriptions and avatars per connection, 256 listened channels per
user; past them new items are dropped with one visible notice. (These counts are under
review: limits that drop data are an availability risk of their own; see TODO.md.)
Exported PKCS#12 identities are written owner-only (0600) and atomically. On macOS,
where data and config share a folder, the storage worker's writable folder is a
dedicated `history/` subfolder. The updater refuses a signed manifest whose release
time or version is older than the newest it has accepted; a client that never saw a
newer manifest can still be offered an older one, and withheld releases go unnoticed.

`nuscheld admin` administers the server offline, through the storage worker, with
no network interface; an operator lock (a `flock` beside a SQLite file, an advisory
lock on PostgreSQL) keeps it and a running server from sharing a database.

`nuscheld` is the network-facing process: it holds the TLS key (ring) and hashes
passwords with argon2id (RustCrypto), under a concurrency limit and a per-account
limit on failed guesses. Client certificates are accepted self-signed (Mumble's
identity is the certificate hash) but the TLS signature proving possession of the key
is always verified. Unauthenticated UDP pings are answered no larger than the request
by default (`allow_ping_amplification` permits up to 2×, rate-limited). Its state
lives in the storage worker, which then also holds the database credential; under
`nuscheld` that worker runs without bwrap, confined by the systemd unit. Secrets
reach the service only through `LoadCredential`; the module refuses store paths.

The extension (docs/extension.md): logs never contain message text or idempotency
keys. An edited, deleted, expired or erased message's old text is overwritten in
SQLite (`secure_delete`); in PostgreSQL old row versions remain until VACUUM.
Retention is forever unless the server or a channel says otherwise; a shorter
retention needs an explicit confirmation and then applies to existing messages,
and live-only ("none") conversations store nothing but their highest `seq`. Erasing a user removes their
messages on the server, not copies clients already hold. Stock clients are told
that channels are archived, and their direct messages are not stored by default.
Unregistered users read no history by default. The client's sticky record
(`nuschel-policy.json`, per host and port) refuses any weaker answer from a server
that once negotiated the extension or a capability; only an explicit reset clears it.

The crypto worker owns the identity secrets (docs/e2ee.md, "Phase 3 as
implemented"): one per profile, sandboxed like the content worker, without network.
Its keystore and state are sealed under a store key kept in a 0600 file in its own
directory, a stopgap until the OS keychain. It never logs secrets; the desktop and
the session see only public keys, statements and verification results. On the
server, key transparency needs `kt.signing_key_file` and `kt.vrf_secret_file`
(module: `services.nuschel.kt.*`, through `LoadCredential`, or generated once in the
state directory). Back both up: a lost VRF secret means a new log, which every client
that pinned the old one treats as misbehaviour until it is reset. Records behind
published bindings are never deleted.

The transfer worker (magic-wormhole file offers) is the one worker with network
access: rendezvous and transit relay, plus only the offered file read-only. The
peer's transit hints decide where it connects (inherent to magic-wormhole), and the
rendezvous address can come from an offer message. Downloads stream back to the
desktop, which writes only the path the user chose (the offered name is a sanitised
suggestion) and never more than announced. magic-wormhole's own code denies
`unsafe` apart from a few marked constructors; its Noise crates are compiled in but
never negotiated by protocol 0.8.

Muted clients do not open microphone streams. A mute mid-sentence ends the
transmission with a constant packet of Opus silence, never captured audio. Capture generations invalidate queued
PCM and encoded packets across mute/deafen changes and audio-worker restarts. The
desktop replaces the media process on mute/deafen transitions: native stream teardown
can block, so dropping a stream alone is insufficient to guarantee capture closes.
The NixOS server's storage preflight inherits its hardened systemd unit.

### macOS and Windows (implemented, not yet validated on those systems)

macOS workers start under `sandbox-exec` with Seatbelt profiles (`crates/ipc/src/host.rs`).
Content and storage workers are deny-by-default on top of Apple's `system.sb`. Storage
may read and write only its history directory, and neither gets any network rule. The
media worker needs CoreAudio's undocumented Mach services, so for now it is confined by
a deny list: no network, no process execution, no writes outside temporary
directories, and no reads of the home directory except audio preferences. It must be
narrowed to deny-by-default once it can be traced on real hardware. Unit tests check
that the profiles are well formed. A macOS-only test checks that they load and block
writes to the home directory. Neither test has run on macOS yet.

Windows workers run with an empty environment (except `SystemRoot`, `WINDIR`, `TEMP`,
`TMP`), without a console window, inside a kill-on-close job object. That guarantees
they never outlive Nuschel, but **it is not a sandbox**: a Windows worker can read
the user's files and use the network. AppContainer or restricted-token process creation
needs unsafe Win32 calls and Windows testing. Until then, Windows offers process
separation only.

## Fuzzing

The libFuzzer targets in `fuzz/` (cargo-fuzz; a workspace of its own whose committed
lock is the main lock cut down) give hostile input to every parser that takes it:

| Targets | Input |
| --- | --- |
| `html`, `legacy_html` | legacy HTML and plain messages into documents; text into the copy for stock clients and back |
| `images`, `favicon` | avatars, website icons, `data:` and pasted images through the content worker's decoders; a website's page through the icon lookup |
| `control_frames`, `server_messages`, `extension_messages`, `kt_messages` | frames and Mumble, extension and `kt/1` messages from several connections into nuscheld's state; the crypto worker's IPC |
| `client_messages` | a hostile server's frames into the client session's state |
| `relay`, `crypt`, `voice`, `voice_playback` | voice: the server's relay, OCB2 decryption, the client's decoding, the media worker's jitter buffer and Opus decoder |
| `identity`, `kt` | identity and key-transparency encodings and verifiers |
| `transfer_offer`, `mumble_url`, `policy_file` | file offers in messages, `mumble://` links and invites, `nuschel-policy.json` |

`just fuzz <target> [seconds] [libFuzzer flags]` runs one in the flake's fuzz shell
(`nix develop .#fuzz`: the newest nightly in the pinned rust-overlay, and cargo-fuzz),
from the small committed corpus in `fuzz/seeds/<target>` into `fuzz/corpus/<target>`,
with debug assertions (overflow checks), a 10-second limit per input and 2 GiB of
memory; findings land in `fuzz/artifacts/<target>`. Neither directory is committed.
Targets build without a sanitizer, which safe Rust does not need and which costs
three quarters of the speed; `FUZZ_SANITIZER=address` checks the unsafe code in
dependencies (html5ever, the image decoders), for example over a corpus with
`just fuzz <target> 0 -runs=0`. To rerun a finding: `nix develop .#fuzz -c cargo fuzz
run --fuzz-dir fuzz -s none -a <target> fuzz/artifacts/<target>/<file>` (with
`cargo fuzz tmin` to minimise it). A finding is fixed at its root and gets a
regression test with the minimised input next to the code. Coverage does not reveal
super-linear time in small inputs, so the paths where it was found (link detection,
the icon page scan, HTML nesting) have tests timing a hostile input at the largest
size accepted.

## Release gates

- Reject malformed/oversized frames before allocating the advertised body.
- Bound pending requests and terminate/reap stuck workers; never silently restart forever.
- Exercise content parsing with hostile inputs and test native codec interoperability.
- Verify workers cannot read an unrelated file or make a network connection.
- Test route/device churn and ensure recovery touches only Nuschel-owned resources.
- Run dependency license, source, vulnerability, and unsoundness checks.
- Preserve third-party notices and review dependency exceptions before distribution.
