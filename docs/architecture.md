# Nuschel architecture

Nuschel separates a portable application model from desktop rendering, platform
services, and processing of untrusted input. GPUI is a desktop choice, not the
application framework for every platform. Small typed interfaces keep renderer and
platform decisions reversible without duplicating the session model.

## Workspace ownership

| Package | Responsibility | Dependencies it must not acquire |
| --- | --- | --- |
| `nuschel-core` | Domain values, typed content, non-destructive message previews | UI, network runtime, database, native FFI |
| `nuschel-client` | Client preferences, state transitions, audio policy | GPUI, SQLite, codecs, platform device handles |
| `nuschel-protocol` | Mumble protobuf schemas and bounded control framing | UI, audio decoding, sockets |
| `nuschel-session` | Native TLS/control sessions, identities, bookmarks and worker supervision | GPUI, SQLite implementation, content parser, codecs |
| `nuschel-cli` | Diagnostic frontend using the real session implementation | GPUI, audio codecs |
| `nuschel-ipc` | Versioned worker messages, framing, and native supervision | Content parser, SQLite, codecs |
| `nuschel-content` | Literal text, fenced code parsing, syntax highlighting | Network access, database, native regex engines |
| `nuschel-storage` | SQLx/Tokio history operations for SQLite and PostgreSQL | UI, session credentials, arbitrary SQL from IPC |
| `nuschel` | GPUI desktop shell and native application supervision | Direct parser, database, or codec implementation |
| `nuscheld` | Server session/routing authority and native transports | GPUI, content parser, audio decoding |
| `nuschel-content-worker` | Parse and highlight untrusted content | Credentials, microphone, user files, network |
| `nuschel-media-worker` | Audio device adapters, DSP, Opus | Server credentials, history database, arbitrary files, network |
| `nuschel-storage-worker` | Own the explicitly selected history database | Server credentials, microphone, network |

Worker dependencies are restricted with cargo-deny's wrapper rules. The shell and
server exchange typed commands and results with workers rather than linking the
native processing implementations. The server routes compressed voice; it should
not decode or re-encode participants' audio.

The shared crates form a small portable layer, not a general plugin system or a
large collection of traits for hypothetical backends. Add interfaces around concrete
needs: commands, events, message values, route observations, and effect plans.

## Execution and trust

```mermaid
flowchart LR
    UI[GPUI shell] --> Client[Portable client state]
    Client --> Session[Native session adapter]
    Session --> Server[Mumble or Nuschel server]
    UI --> Content[Content worker]
    UI --> Storage[Storage worker]
    Session --> Media[Media worker]
    Media --> Devices[Platform audio services]
```

The native session adapter owns TLS identity, server authentication, control state,
TCP-tunneled Opus, and typed connection events. Encrypted UDP and automatic reconnection
remain future work. Workers receive only the data and capabilities they
need. In particular, the media worker receives Opus/PCM and route instructions, not
the credentials or encryption keys that authenticate the Mumble session.

Memory-safe implementations are preferred before introducing a new process exception.
Workers also contain panics, excessive CPU use, and native/SIMD implementation risks.
A process boundary alone is not an OS sandbox. The supervisor must limit inherited
handles, environment, filesystem/network access, outstanding work, memory, and lifetime.
Current enforcement and exceptions are recorded in [the safety policy](../SECURITY.md).

Control IPC is a versioned, length-prefixed JSON protocol with a 1 MiB limit checked
before allocating a frame body. Unknown versions, truncated frames, and oversized
results fail explicitly. Helpers are selected by trusted executable paths and
communicate over inherited pipes, not a publicly listening socket. Request IDs are
checked by the supervisor. Worker stderr must never become protocol stdout.

The media harness uses the same protocol for codec checks. Live control and small
compressed Opus frames currently use bounded JSON IPC with capture generation tags;
PCM stays inside the worker. A binary transport can replace this if profiling warrants it. Do not introduce
shared writable memory merely for performance without reviewing the new trust boundary.

The UI and network event loop must not wait synchronously for parsing, disk, or DSP.
The native audio callback must avoid blocking, logs and IPC. Bounded queues and
nonblocking locks connect callback I/O to media processing; callback allocation and
lock avoidance remain performance work. Drop stale
audio rather than building latency; apply a different durability policy to chat.

## Content and history

Store the original message independently of its presentation. Preserve newlines and
blank lines; interpret fenced code without interpreting raw HTML or loading resources.
The content worker returns text and style runs, not HTML, scripts, or image URLs to
execute. Renderer adapters remain responsible for selection, accessibility, focus,
layout, and a keyboard-accessible Show more action.

Initial previews collapse after 12 physical lines or 2,000 Unicode scalar values.
This handles both pasted documents and long single lines. The full message remains
available for expansion, copying, and history. A production renderer should also
use its measured layout height to handle narrow windows and text scaling.

Parsing and history each have explicit limits. The initial content limit is 64 KiB;
the adapter must report a rejected message rather than silently truncate it. Revisit
the storage limit separately when adding attachments or legacy HTML. Legacy Mumble HTML is converted to inert text in the content worker: entities and
line breaks are preserved, executable/resource elements are discarded. Images and
arbitrary rich formatting are not rendered.

History is on by default, including with stock servers. Server and channel identities
scope queries; persist author display snapshots and local observation timestamps.
Connection session IDs are not durable author identities. Mark disconnected intervals
in the timeline. Do not deduplicate repeated text or fabricate missed messages.

The storage worker uses SQLx with Tokio: SQLite WAL for local history and PostgreSQL
for server persistence. Separate embedded migrations account for dialect differences
and track checksums through SQLx. Statements bind parameters. SQLite needs only the
chosen database directory, including WAL/SHM sidecars; PostgreSQL needs a connection to
its configured database and that database's credential. No worker request can select
another database or execute SQL. Keep database credentials outside command lines and
Nix store values; the server module uses systemd LoadCredential.

## Audio configuration and recovery

Audio policy operates on observations of the selected capture and playback path.
For AEC, noise suppression, and gain control separately, evidence is active,
inactive, or unknown. Missing metadata is not proof that processing is absent.

Reuse confirmed upstream effects. An upstream AEC also needs a reference that covers
Nuschel's actual speaker output: move Nuschel's own playback connection to the matching
reference when possible. Do not reconfigure an unrelated processor, change global
defaults, move another application's stream, or change hardware gain to make a local
optimization. If the route is opaque, retain the uncertainty and expose a device-level
override rather than stacking a speculative second canceller.

The preferred in-application DSP candidates are Rust: Sonora for WebRTC-derived AEC,
nnnoiseless for RNNoise-derived suppression, and opus-pure for the codec. Rust ports
still need interoperability, audio-quality, performance, and robustness evaluation;
being written in Rust is not proof of memory safety or correct Opus output. Codec and
DSP SIMD remains confined to the media worker. Native PipeWire or platform processing
can be reused when it is already working well on the active route.

On Linux, inspect PipeWire nodes/links and PulseAudio module/stream metadata. On
Windows, inspect active stream effects and effect-change notifications where the OS
supports them. On macOS, native voice processing is an option, but its gain control
and other-audio ducking must be evaluated against Nuschel's ownership rule.

While audio is active, observe residual echo, clipping, noise, underruns, latency, and
near-end speech preservation. Use far-end-only and double-talk conditions correctly;
silencing the microphone does not count as better cancellation. Apply hysteresis and
allow AEC to converge. Do not claim ERLE when the required unprocessed reference is
unavailable. Monitoring is local and does not save microphone audio by default.

Device changes and audio-server restarts invalidate device handles, link IDs, reference
timing, and DSP state. Increment the route generation; stop old callbacks; discard old
queued PCM; re-enumerate; recreate Nuschel-owned resources; re-evaluate effects; resume.
Keep network/chat state alive. Mute/deafen transitions replace the media worker so
blocked native stream destructors cannot retain microphone access. Automatic recovery must not repeatedly undo an explicit
user routing change.

## Protocol and server compatibility

Support stock clients against the new server and the new client against stock servers.
Share the vendored Mumble schemas, but test behavioral compatibility separately: TLS
client certificates, authentication, permissions, channel state, whisper targets,
mute/deafen, UDP reachability, nonce resync, and TCP tunneling. Mumble's historical OCB
packet format must not be substituted with a generic modern OCB implementation.
Handle both legacy UDP encoding and the protobuf format introduced with Mumble 1.5.

The implementation of a future WebRTC transport remains open. Do not use `str0m`:
it is excluded based on the maintainer's prior experience with it. Prefer memory-safe
implementations and evaluate interoperability, robustness, recovery and sustained-call
behavior before selecting one. Native-code exceptions still require the isolation and
dependency review described in the safety policy.

Persistent chat, server message IDs, history pagination, and independent text-channel
subscriptions require a capability-negotiated extension. Stock clients continue to
receive ordinary live TextMessage messages; stock servers provide live chat plus local
history in Nuschel. Authentication and channel ACLs must govern history as well as live
messages. Persistently address a conversation without relying on transient session IDs.

Keep history writes and pagination off the voice-routing critical path. Apply rate
limits and bounded queues per connection. Do not let a slow reader or a large history
query block the server's control state machine or voice forwarding.

## Desktop navigation and complete history

The desktop has a narrow server rail, a channel pane (208 logical pixels initially),
and the conversation. Channels remain a hierarchical tree, including channels that
have both children and a conversation. A single click selects text history; it must
never move a voice connection. A double-click joins voice, as does the explicit Join voice button. Branch toggles
consume pointer events so expanding a branch cannot join voice. Only root branches
start expanded; explicit expansion choices survive updates during the session.
Remember the selected conversation for each server. Tree branches, server selection,
channel selection, and message expansion support pointer and keyboard activation.

The portable navigation model issues a generation-tagged history ticket whenever a
conversation is selected. Ignore late responses after switching away, including
switching back to the same channel. The desktop executes local history requests asynchronously through the storage worker.
History is scoped to the endpoint and observed server certificate; cached channel names
allow offline browsing. Direct messages use peer certificate hashes when available,
otherwise a connection-specific identity avoids merging reused session IDs.

For a Nuschel server, show all retained history the authenticated member is authorized
to read, including messages from before their current connection. Fetch older pages
as the user scrolls; complete access does not mean an unbounded response or allocating
the whole archive. Stock servers fall back to local observations with explicit gaps.
Check current channel ACLs on every page; a capability announcement is not permission.
Use durable conversation identities, an authenticated server identity, and stable
server message IDs. Persist before acknowledging a new message; retries need an
idempotency key rather than deduplication by text or timestamp. Membership changes,
retention, moderation, edits, and deletion/tombstone policy must also apply to history.

The storage worker implements exclusive-ID paging for SQLite and PostgreSQL. It
returns chronological pages capped at 100 messages and 96 KiB of unescaped text,
with a cursor when older records remain. Byte limits must not silently skip a message.
Concurrent inserts do not shift older pages. IDs are scoped to a database and query;
the future session adapter must not treat an arbitrary remote cursor as authorization.
The existing plaintext message table supports local and non-E2EE server history.

## MLS and encrypted history

MLS is a separate, strict server security policy, not an optional decoration on a
legacy session. A legacy-compatible server continues to support stock Mumble clients
and transport encryption, but the server can read message and audio content. An
MLS-required server rejects clients without the negotiated MLS protocol and must not
silently downgrade, bridge plaintext to a legacy participant, or expose decrypted
history through legacy APIs. Operators can run separate server instances for the
two policies. No MLS wire protocol or cryptographic session is implemented yet.

The client shows a red open lock for transport-only sessions and a green closed lock
only after the authenticated MLS session is established. Unknown or negotiating state
has a neutral or pending indicator. A server advertisement or configuration flag
cannot produce a green lock. Labels explain "no end-to-end encryption" so that users
do not confuse transport encryption with E2EE. The current client displays only transport-only sessions.

Use an existing Rust MLS implementation after dependency/security review, in a
dedicated crypto worker that owns credentials, ratchet state, key deletion and
transactional state persistence. Bind member credentials to verified device identities,
channel authorization, and the correct server/group identity. Plan one MLS group per
conversation; membership/ACL changes must be reflected in committed group membership.
MLS application messages protect chat. Voice needs an independently specified media
encryption protocol with exported keys, replay protection, sender identity and epoch
handling. Chat-only encryption must not turn the whole server's voice/chat badge green.

MLS forward secrecy depends on deleting old epoch/message secrets; adding a member
does not automatically grant access to all earlier messages. See
[RFC 9420, sections 12.4.3.1 and 16.6](https://www.rfc-editor.org/rfc/rfc9420.html#section-16.6).
Full persistent history therefore needs a separately specified encrypted archive,
client-held archive keys, and an explicit policy for granting historical access to
new members and devices. Do not retain all MLS epoch secrets just to decrypt an archive.
Historical-access policy and archive-key rotation/recovery must describe their own
compromise and revocation limits; access to retained history changes the protection
available after a device compromise. Offline delivery, expired membership and key loss
need visible gaps instead of promises that every stored ciphertext can be decrypted.

An MLS server stores opaque authenticated ciphertext and delivery metadata in a
separate storage representation, never decrypted `ChatMessage` bodies or group secrets.
Its role is delivery and archive authorization, not a hidden member with decryption
keys. Local decrypted history stays on the device under the user's history policy;
at-rest protection and device key recovery need OS credential-store integration.
The crypto worker must not get the server database credential or unrestricted network
access. The storage worker must not get live MLS private keys.

## Mobile and browser adapters

Mobile and web are future targets with open renderer choices. Core and client state
must compile for WebAssembly without GPUI, Tokio's native sockets, SQLite, or CPAL.
Native mobile adapters must handle microphone permissions, audio-session changes,
interruptions, background execution, and platform call integration.

iOS does not offer the desktop helper-process model to an ordinary application.
Do not assume the worker topology can be copied unchanged. Prefer memory-safe
in-process parsing/processing on mobile, use OS-native services where appropriate,
and review each remaining native exception against the mobile application's sandbox.
GPUI mobile/web experiments are not a commitment to that distribution path.

A browser cannot directly speak Mumble's TLS TCP and encrypted UDP protocol. A web
client needs an explicit server-side gateway, with WebSocket/HTTP control and a
browser-compatible media transport. Shared Rust state can be reused without pretending
that the native network adapter or background-audio behavior transfers to a browser.

## Builds and integration tests

Nix pins the toolchain, native dependencies, checks, and advisory snapshot. Native
Linux and Darwin derivations are the initial build surface. Linux-to-Darwin can follow
`../opendeck-akp05`: pinned SDK, cargo-zigbuild, two architecture slices, and llvm-lipo.
GPUI shader generation and native dependencies must pass a real cross-build before
adding a release target. Do not label an untested cross-build a supported release.

NixOS VM tests should cover all three new client/server combinations, with stock/stock
as the baseline. Real stock Mumble applications are necessary; a protobuf fixture or
TCP connect probe alone is not interoperability. The [test plan](../tests/README.md)
defines progression from packaging to control, voice, recovery, and history behavior.
Linux VM tests complement physical Windows/macOS audio and accessibility testing.
