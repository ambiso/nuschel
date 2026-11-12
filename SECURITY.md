# Memory safety and isolation

Prefer a memory-safe implementation for every component that handles untrusted data.
If a suitable implementation is unavailable, isolate that processing in a worker with
the minimum required capabilities. Even memory-safe code needs input/resource limits:
panics, excessive allocations, infinite loops, and corrupted output remain possible.

Prefer RustCrypto crates for cryptographic primitives when they meet the protocol's
requirements and have a suitable security and maintenance history. Review the actual
implementation, enabled features and transitive dependencies: a RustCrypto API does
not imply that its implementation is free of unsafe code. Keep required native or
SIMD implementations behind the boundaries below. Retain and document an existing
provider when replacing it would weaken security, interoperability or support; do
not change cryptographic protocols merely to change dependencies.

All first-party crates inherit `unsafe_code = "forbid"`, except the Windows sandbox
adapter `nuschl-sandbox-windows` (below). This is an enforced rule for
Nuschl source, not a claim about the entire dependency graph. A safe API around a C
library does not make its implementation memory-safe. A Rust port with unchecked SIMD
or raw pointers also needs review. Dependency changes must include that assessment.

## Explicit exceptions and boundaries

| Component | Reason | Required boundary |
| --- | --- | --- |
| GPUI and graphics/font/window backends | Native desktop rendering and accessibility | Desktop shell only; never parse message HTML in the renderer |
| Terminal graphics and clipboard adapters | Terminal capabilities, OS clipboard services, rendering verified worker pixels | TUI only: image-format decoding and formula evaluation remain in workers. The reviewed ratatui-image patch uses scalar base64 and safe Rust palette/SIXEL encoding, without libchafa, base64-simd or icy_sixel/quantette. Existing image buffer operations, PNG/zlib checksums and Windows terminal calls retain unsafe internals under the rendering/system-service exception; this is not a memory-safe transitive graph. Only bounded, validated raw bitmaps reach these rendering APIs. Clipboard helpers receive fixed argv on explicit local paste, with byte/time bounds; SSH never reads the remote host clipboard |
| CPAL and OS audio bindings | Access to PipeWire/PulseAudio/CoreAudio/WASAPI | Media worker; device handles and audio only |
| Codec and DSP SIMD in Rust dependencies | Optimized processing still contains unsafe internals | Media worker; no session credentials, arbitrary files, or network |
| SQLx SQLite backend | Native SQLite provides transactional local history | Storage worker; one database directory, parameterized operations; extension loading and deserialization disabled. `nuschld --import-murmur` has the worker read an untrusted Murmur file confined by Landlock to that file: read-only and immutable, `query_only`, `trusted_schema` off, cell size checks, a heap limit, `quick_check`, no WAL files, ordinary tables only, every column capped, fixed statements |
| SQLx PostgreSQL backend | Server persistence on a managed database | Storage worker; database credentials and a scoped database connection |
| TLS crypto provider | Cryptographic implementation and CPU-specific acceleration | Session/server authority; bounded protocol framing and reviewed provider |
| OS sandbox/process APIs | Establish restricted worker execution | Small supervisor adapter; prefer safe Rust wrappers (`win32job` for Windows job objects) |
| `nuschl-sandbox-windows` (Win32 calls via `windows-sys`) | No maintained safe wrapper creates AppContainer processes with an explicit handle list and job limits (docs/windows-sandbox.md) | The workers' own launch stage only, before any input is read (`cargo-deny` allows only the six workers to depend on it). Each Win32 function is one trait method whose `# Safety` is Microsoft's documented contract (cross-checked with ReactOS); unsafe calls only in `os.rs`, one per block with a `SAFETY:` argument, behind guards that release what Windows allocates exactly once; pure logic in unsafe-free modules; no `as` casts. The launch steps run under Miri (Linux and Windows targets) against a mock of Win32 that fails every call in turn; an independent review found and fixed the issues listed in docs/windows-sandbox.md |
| `aes` (OCB2 voice crypto) | Mumble's UDP voice encryption; AES-NI intrinsics | Session authority; fixed-size blocks, keys from the TLS control channel |
| `rsa` (identity generation only) | Stock Mumble imports only RSA client certificates | Generates the local Mumble client key only; TLS signing uses ring, so the Marvin timing issue (RUSTSEC-2023-0071) in private-key operations never runs on untrusted input; `cargo-deny` confines the crate to `nuschl-session` |
| `image` decoders (PNG, JPEG, GIF) | Inline images and avatars from untrusted peers | Content worker only, with dimension and allocation limits; the UI receives raw RGBA |
| `akd` (key transparency) | Auditable key directory; no memory-safe alternative with VRF and audit proofs | Through `nuschl-kt` only, in the crypto worker and `nuschld`; akd_core turns on ed25519-dalek's `legacy_compatibility` for the whole binary, so `nuschl-kt` checks `s < ℓ` itself and `nuschl-identity` uses libcrux, not ed25519-dalek; akd's verifiers run under `catch_unwind` behind a strict decoder (docs/e2ee.md §5.2) |
| libcrux (SIMD intrinsics) and OpenMLS's `rayon` | Post-quantum MLS (X-Wing HPKE, ML-KEM) has no memory-safe alternative in Rust; openmls and tls_codec themselves have no `unsafe` | Through `nuschl-e2ee` only (cargo-deny), in the crypto worker and `nuschld`; every key package, tree and message is checked structurally before OpenMLS sees it (OpenMLS 0.9.2's provider is patched onto the fixed libcrux-kem 0.0.10 and libcrux-hmac-drbg 0.0.2, RUSTSEC-2026-0329/-0330/-0331, vendor/*/NUSCHEL-PATCH.md) and OpenMLS calls run under `catch_unwind` (docs/e2ee.md "Phase 4 prototype") |
| Global shortcuts (`ashpd` portal; `global-hotkey` on Windows/macOS) | System-wide push-to-talk | Desktop shell; press/release of one bound key only |
| Typst (`typst`, `typst-render`) | Typesetting LaTeX formulas; Typst is a scripting language with file, package, plugin and data access | Math worker only; formulas reach it as math markup from Nuschl's own converter, checked to contain no code; a world with one file and no packages, clock or system fonts; a library without `read`, `plugin`, `image`, the data loaders, `eval` and `std`; limits and a deadline per formula (docs/math.md) |
| `psm` (assembly, through `stacker`) | Typst's evaluator grows its stack on deep recursion | Only through `stacker` and `typst-eval`, in the math worker |

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

The math worker (docs/math.md) runs in the content worker's sandbox, but stays up
between formulas: it starts on the first formula and stops after a minute without
one. Each formula has a three-second deadline, after which the worker is killed and
the next formula starts a fresh one; its CPU limit (120 s) covers its life, its
address space is limited like the content worker's, and it answers with coverage
masks the desktop checks against their size and the limits before drawing them.

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
Nuschl code creates/changes only its own streams. The sandbox is not an audio-protocol
firewall or a claim that a compromised audio daemon cannot act outside it.

Against a hostile server the session keeps budgets: 4,096 channels, 16,384 users and
64 MiB of comments, descriptions and avatars per connection, 256 listened channels per
user; past them new items are dropped with one visible notice. (These counts are under
review: limits that drop data are an availability risk of their own; see TODO.md.)
Exported PKCS#12 identities are written owner-only (0600) and atomically. On every
platform the storage worker's writable folder is a dedicated `history/` folder (see
"Where secrets live"). The updater refuses a signed manifest whose release
time or version is older than the newest it has accepted; a client that never saw a
newer manifest can still be offered an older unexpired one, and withheld releases
are not discovered automatically. Signed v2 metadata expires within 30 days; the
local clock also gates acceptance. Two separately confined fixed-algorithm
Ed25519/ML-DSA-44 workers must both succeed. Trust is embedded only. On 2026-11-12 the dedicated production public pair
`dbc40ac478b51573eaebebb0a40532bec7cf2a87282a98ae08f1a25ee5b42535`
was enrolled; no adjacent writable key or minisign fallback is accepted. The
[updater boundary and proof limits](apps/update-worker/README.md#confinement-and-actual-proof)
include shared parent/OS/compiler/hash dependencies and the local signing authority.
The long-lived hybrid private pair stays local; hosted CI has no signing secret
or publication authority. [Local release tooling](docs/local-release.md) binds
exact tagged products and local ad-hoc bundle integrity before signing and
draft-first publication. Native Apple verification is optional; absent proof is
explicitly recorded as not natively verified, with no Apple publisher claim.
Hybrid verifier/LPAC, library-install and desktop update/restart proof exist on
recorded fixture sources in a private Windows 11 VM, including exact owned profile
and persisted update-state continuity. Current-head archive checks and macOS
Seatbelt runtime proof remain outstanding. See [Windows proof and limits](docs/windows-hybrid-update.md).

`nuschld admin` administers the server offline, through the storage worker, with
no network interface; an operator lock (a `flock` beside a SQLite file, an advisory
lock on PostgreSQL) keeps it and a running server from sharing a database.

`nuschld` is the network-facing process: it holds the TLS key (ring) and hashes
passwords with argon2id (RustCrypto), under a concurrency limit and a per-account
limit on failed guesses. Client certificates are accepted self-signed (Mumble's
identity is the certificate hash) but the TLS signature proving possession of the key
is always verified. Unauthenticated UDP pings are answered no larger than the request
by default (`allow_ping_amplification` permits up to 2×, rate-limited). Its state
lives in the storage worker, which then also holds the database credential; under
`nuschld` that worker runs without bwrap, confined by the systemd unit. Secrets
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
(`nuschl-policy.json`, per host and port) refuses any weaker answer from a server
that once negotiated the extension or a capability; only an explicit reset clears it.

The crypto worker owns the identity secrets (docs/e2ee.md, "Phase 3 as
implemented"): one per profile, sandboxed like the content worker, without network.
Its keystore and state are sealed under a store key kept in a 0600 file in its own
folder (`keys/` in the data folder), a stopgap until the OS keychain. It never logs secrets; the desktop and
the session see only public keys, statements and verification results. On the
server, key transparency needs `kt.signing_key_file` and `kt.vrf_secret_file`
(module: `services.nuschl.kt.*`, through `LoadCredential`, or generated once in the
state directory). Back both up: a lost VRF secret means a new log, which every client
that pinned the old one treats as misbehaviour until it is reset. Records behind
published bindings are never deleted.

`nuschl-witness` (docs/witness.md) holds the key clients trust to vouch for every
server's log: an ML-DSA-44 seed from `LoadCredential` or generated once (0600) in its
state directory; back it up. It parses submissions with nuschl-kt's strict decoder
(bounded bodies, per-origin and enrolment rate limits) and runs akd's verifiers as
the clients do; its only outbound connections are enrolment checks to the
submitted origin's `host:port`, never to private or loopback addresses unless
configured. `nuschld`'s submissions verify every cosignature against the configured
key before serving it.

The transfer worker (magic-wormhole file offers) is the one worker with network
access: rendezvous and transit relay, plus only the offered file read-only. The
peer's transit hints normally decide where it connects, and the rendezvous
address can normally come from an offer message. **Relay-only connections**
(`relay_only` in client settings) skips all direct socket setup, interface
enumeration, STUN and peer-address advertising. It uses only the locally
configured rendezvous and transit relay: peer-provided relay addresses are also
ignored, because an attacker could call their own address a relay. The narrow
pure-Rust dependency patch is documented in
`vendor/magic-wormhole/NUSCHEL-PATCH.md`. Failure of the configured relay never
falls back to a peer address. Enabling the setting revokes active direct-capable
workers; offers/downloads can be started again under the new policy. Configured
servers and relays still see the client's connection address. Legacy Mumble
servers cannot enforce address redaction for other clients.

The shared `nuschl_core::network::PeerPolicy` applies before gathering or dialing;
future RTC adapters must use it before ICE and advertise relay candidates only
in this mode. The standalone RTC probe uses two explicitly bound loopback sockets
and is not an application call path. Downloads stream back to the
desktop, which writes only the path the user chose (the offered name is a sanitised
suggestion) and never more than announced. magic-wormhole's own code denies
`unsafe` apart from a few marked constructors; its Noise crates are compiled in but
never negotiated by protocol 0.8.

When no talk server is unmuted, the client does not open a microphone stream. A mute
mid-sentence ends that server's transmission with a constant packet of Opus silence,
never captured audio. Deafness always implies mute; sparse server snapshots are
normalized, and stale self-state echoes cannot overwrite newer local privacy choices.
Capture epochs and per-connection routing revisions invalidate queued PCM/Opus across
mute/deafen changes, new speaking destinations and audio-worker restarts. The desktop
replaces the media process when capture or its destinations change: native stream
teardown can block, so dropping a stream alone is insufficient to close capture.
With another explicitly selected talk server still unmuted, global capture may
continue, but the deafened server receives and plays no voice.

An `e2ee/1` server is excluded from ordinary Mumble capture and playback until
encrypted voice/SFrame is implemented. It cannot receive shared microphone packets,
even while compatible servers keep global capture active. The SDK refuses voice
enablement and the desktop says voice is unavailable; an MLS chat lock is not a
claim that ordinary Opus is end-to-end encrypted. The session must also reject raw
voice commands for this mode, independently of the shared media supervisor.
The NixOS server's storage preflight inherits its hardened systemd unit.

### macOS (implemented, not yet validated on macOS)

macOS workers start under `sandbox-exec` with Seatbelt profiles (`crates/ipc/src/host.rs`).
Content, math and storage workers are deny-by-default on top of Apple's `system.sb`. Storage
may read and write only its history directory, and neither gets any network rule. The
media worker needs CoreAudio's undocumented Mach services, so for now it is confined by
a deny list: no network, no process execution, no writes outside temporary
directories, and no reads of the home directory except audio preferences. It must be
narrowed to deny-by-default once it can be traced on real hardware. Unit tests check
that the profiles are well formed. A macOS-only test checks that they load and block
writes to the home directory. Neither test has run on macOS yet.

### Windows (validated in a Windows 11 VM)

Windows workers run in AppContainers, each in a job object of its own
(docs/windows-sandbox.md). A worker executable is its own launch stage: started with
`--nuschl-sandbox` and a policy (`crates/ipc/src/host.rs`), it creates or reuses the
worker kind's AppContainer profile, grants it its directories, and starts itself
again inside it, suspended, then assigned to a job, then resumed; it waits and passes
on the exit code. It reads no untrusted input, and it refuses (exit 125) rather than
start a worker unconfined, as Linux refuses without bwrap.

Content, math, storage and crypto get no capabilities, so no network at all, and run as
less privileged AppContainers: unlike a full one, they cannot read what Windows grants
every AppContainer, such as the user's environment variables in the registry. Media gets
`microphone`; transfer gets `internetClient` and `privateNetworkClientServer`
(outgoing connections, and the LAN both ways for direct transit; nothing inbound from
the Internet, and no AppContainer reaches loopback). Beyond the container's own
folder and what every AppContainer may read (the Windows directory, Program Files),
storage may modify only what is in its history directory and crypto only what is in
its own directory (each directory itself only listed and added to; a junction there
is refused, not followed), each in a container of its own per directory; an offer's
file reaches the transfer worker as an inherited read-only handle, its permissions
unchanged. Only the standard handles
(and that file) are inherited, through an explicit handle list; there is no console.
The worker is created inside its job, which kills it when the launch stage exits or
is killed (the supervisor's kill-on-close job holds the launch stage), admits one process (the child-process
policy refuses process creation too), limits committed memory (512 MiB content and
math, 1 GiB the others) and content's and math's CPU time as on Linux, and blocks the clipboard, global atoms,
other processes' USER handles, desktop switching and system settings. Extension-point
DLLs, non-system fonts and remote or low-label images are refused.

In the VM, each worker kind failed to read a file in Documents, list or write the
user's profile, `C:\Users\Public` or the user's TEMP, connect over TCP (except
transfer), start a process, open the supervisor's process, commit past its limit,
read the clipboard or the autostart registry key, or create a registry key,
while the real workers decoded an image, wrote history, created an identity, opened
WASAPI capture and playback, and exchanged files through a magic-wormhole rendezvous
and relay on host loopback. The desktop ran against a test Murmur with images.
Remaining: workers share the user's desktop (they could open windows of their own; no
separate desktop, no win32k lockdown or Arbitrary Code Guard); media and transfer, full
AppContainers because COM and Winsock refuse less privileged ones, can read the
user's environment variables (`HKCU\Environment`), and what AppContainers may read in
general was not audited; media can use what the audio service offers, subject to
Windows' microphone privacy setting; no physical hardware has been tested.

## Where secrets live

Nuschl's folders (`crates/session/src/layout.rs`; on Linux the XDG Base Directory
ones, see docs/desktop.md) keep secrets out of the configuration folder, which people keep
in dotfiles repositories, and give each persistent worker one folder with nothing
else in it. Folders are created 0700, files 0600.

| What | Where (Linux) | Reached by |
| --- | --- | --- |
| Mumble certificate and private key, with lock and import backups | `$XDG_DATA_HOME/nuschl/identity/` | the main process only (TLS client authentication is in-process) |
| End-to-end keys: sealed keystore, state, store key | `$XDG_DATA_HOME/nuschl/keys/` | the crypto worker only; the main process starts it on that folder and sees public keys only |
| Local history (SQLite) | `$XDG_DATA_HOME/nuschl/history/` | the storage worker only |
| Servers' security records (`nuschl-policy.json`) | `$XDG_DATA_HOME/nuschl/` | the main process only |
| Settings, bookmarks with accepted certificate pins and access tokens, `managed.json` | `$XDG_CONFIG_HOME/nuschl/` | the main process only |
| Last channels, update check state | `$XDG_STATE_HOME/nuschl/` | the main process only |
| Channel trees and server icons (server-controlled, bounded when read) | `$XDG_CACHE_HOME/nuschl/` | the main process only |

The content, math, media and transfer workers get none of these folders. The storage
and crypto workers' folders are siblings, so neither reaches the other's, the
identity, the records or any configuration: bwrap binds the one folder at `/data`,
Seatbelt allows reads and writes below it only, and on Windows the folder's own
AppContainer is granted it. A profile whose XDG variables would put any other folder
inside a worker's is refused. Server passwords are never saved. Access tokens stay in
`servers.json` with their bookmarks: they are shared channel passwords, not
credentials of your identity, but leave them out of a public dotfiles repository.

The crypto worker acquires an exclusive OS lock on a persistent `keys/store.lock`
before reading or creating its store key, for file and supplied keys alike. It holds
the descriptor until exit; contention or an unavailable lock fails closed. The lock
file is never removed on release, so another worker cannot acquire a different inode
while the first still owns the store. Linux tests cover process-death release and
restart persistence; native Windows/macOS contention tests remain open.

Moving an earlier version's profile keeps these boundaries: the storage worker gets
the data folder as a whole only while an earlier version's history in it could not
be moved, and then nothing else moves into it and everything is used where it was;
once the folder holds more than that history, the storage worker gets a new
`history/` folder instead. On Windows, the rename migration retires the old
`nuschel.*` containers and removes their path grants, including the earlier broad
storage grant on the data folder, before moving the profile. A short-lived storage
worker performs this Win32 operation; failure aborts migration. Mock regression
tests cover retirement, but migration from a real earlier Windows installation
still needs a native VM test (TODO.md).

## Fuzzing

The libFuzzer targets in `fuzz/` (cargo-fuzz; a workspace of its own whose committed
lock is the main lock cut down) exercise the input paths below. Coverage gaps,
including update-signature parsing and storage requests, remain listed in TODO.md:

| Targets | Input |
| --- | --- |
| `html`, `legacy_html` | legacy HTML and plain messages into documents; text into the copy for stock clients and back |
| `images`, `favicon` | avatars, website icons, `data:` and pasted images through the content worker's decoders; a website's page through the icon lookup |
| `control_frames`, `server_messages`, `extension_messages`, `kt_messages` | frames and Mumble, extension and `kt/1` messages from several connections into nuschld's state; the crypto worker's IPC |
| `client_messages` | a hostile server's frames into the client session's state |
| `relay`, `crypt`, `voice`, `voice_playback` | voice: the server's relay, OCB2 decryption, the client's decoding, the media worker's jitter buffer and Opus decoder |
| `identity`, `kt` | identity and key-transparency encodings and verifiers |
| `kt_witness` | witnessing's formats: C2SP signed notes, checkpoints and verifier keys, `WitnessedHead` through the client's check, the auditor's `AddEpochs` through its check and its state files |
| `transfer_offer`, `mumble_url`, `policy_file` | file offers in messages, `mumble://` links and invites, `nuschl-policy.json` |
| `math`, `math_tokens` | LaTeX through the converter, its check, Typst and the rasteriser; message text through the formula grammar (tokens are the bytes typed, never from code, the text unchanged) |

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
- Test route/device churn and ensure recovery touches only Nuschl-owned resources.
- Run dependency license, source, vulnerability, and unsoundness checks.
- Preserve third-party notices and review dependency exceptions before distribution.
