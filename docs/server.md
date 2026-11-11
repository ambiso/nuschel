# The Nuschel server (`nuscheld`)

`nuscheld` is a Mumble-compatible server: protocol 1.5, with the legacy UDP voice
format for 1.4 and 1.3 clients. Stock Mumble clients and Nuschel use it like a
stock Murmur. It routes Opus voice without decoding it. This is phase 1 of the
server roadmap (TODO.md); the Nuschel extension (docs/extension.md) builds on it.

## Running it

```sh
nuscheld --config nuscheld.toml          # see apps/server/nuscheld.example.toml
printf %s 'secret' | nuscheld --config nuscheld.toml --set-superuser-password
nuscheld --config nuscheld.toml --check-config
```

Every configuration key is optional; names and defaults follow murmur.ini
(`users` is `max_users`, `bandwidth` is `max_bandwidth`, `messagelimit` is
`message_limit`, and so on). Command-line flags override the file:
`--listen-address`, `--port`, `--state-directory`, `--sqlite-path`,
`--database-url-file`, `--tls-certificate-file`/`--tls-key-file`, `--password-file`,
`--superuser-password-file`, and `--ephemeral` (state in memory only).

- **Security mode.** `security = "compatible"` admits stock clients and has no
  end-to-end encryption. `"e2ee"` will become the default once MLS exists; until
  then the server refuses to start with it, so nobody believes they have E2EE.
- **Secrets** come from files: `password_file`, `superuser_password_file`,
  `database_url_file`, `private_key`. The NixOS module passes them through
  systemd `LoadCredential` and refuses Nix store paths.
- **Certificate.** Without `certificate`/`private_key`, a self-signed ECDSA
  certificate is generated once in `state_directory` (key file 0600). Clients pin
  the server certificate, so keep that directory.
- **SuperUser** (user 0) has no password until one is set; without one it cannot
  log in.

The NixOS module (`services.nuschel`, nix/module.nix) generates the TOML file
from `settings` (never secrets), runs a database preflight, and hardens the unit
(DynamicUser, a private state directory, no capabilities, system call filter,
`SocketBindAllow` for the server port only).

## Design

```
TCP accept ──► connection task ──┐            ┌──► writer task ──► TLS
 (admission, TLS, framing,       │  Mutex     │    (byte-bounded queue)
  per-type limits)               ├─► State ───┤
UDP socket ──► udp task ─────────┘  (handlers,└──► UDP sockets
 (decrypt, bind, info pings)         routing)      (after the lock)
                                       │
                                       └──► storage task ──► nuschel-storage-worker
                                            (ordered writes)   (SQLite / PostgreSQL)
```

- **One state, one lock.** Channels, users, registrations, bans and permission
  caches live in `State` behind a mutex. Handlers run synchronously under it and
  never across an `.await`; Murmur serialises the same way (its main thread plus
  a voice read-lock). Authentication's slow parts (argon2id, loading a registered
  user's blobs) run outside the lock and are re-checked under it.
- **Output.** Each connection has a queue bounded in bytes (`max_queued_bytes`).
  Control frames are never dropped: a client that lets the limit fill is
  disconnected. Voice frames are dropped once 64 KiB are waiting. Broadcasts
  encode once and share the bytes.
- **Voice.** The UDP task decrypts (OCB2-AES128, `crates/protocol/src/crypt.rs`),
  parses without copying (`relay.rs`), routes, and re-encodes per receiver
  (legacy or protobuf, sender, context, volume, positional data) into reused
  buffers; encrypted datagrams are sent after the lock is released. Receivers
  follow Murmur's `processMsg`: channel, listeners (with volume), linked channels
  where the speaker may speak, whisper and shout targets (links, children,
  groups) cached per routing generation, loopback (target 31). Mute, suppress
  and self-mute stop routing; Speak in the user's channel becomes `suppress`.
  The per-user bandwidth limit (Murmur's 360-packet window) applies to UDP and
  the tunnel alike.
- **UDP binding.** A datagram from an unknown address is trial-decrypted only
  against sessions whose TLS connection came from the same IP (at most eight,
  with a per-prefix budget); success binds or rebinds the address. As in Murmur,
  voice goes over UDP until the client tunnels something over TCP, and again
  after it sends voice over UDP. After five seconds without a good packet the
  server asks for the client's nonce (at most every five seconds); a flood of
  undecryptable packets pauses decryption for that address.
- **Persistence.** `nuscheld` links no database code (deny.toml keeps SQLx in
  `nuschel-storage` and that crate in the storage worker). It starts
  `nuschel-storage-worker --server` and writes every change through an ordered
  queue (`crates/ipc/src/server.rs` defines the messages); the worker owns the
  database and its credential. Startup loads channels, ACLs, links, bans and
  registered users (in pages); comments and avatars are loaded when a registered
  user connects. Graceful shutdown (SIGTERM) disconnects everyone and flushes the
  queue. The schema (`crates/storage/server-migrations`) works on SQLite and
  PostgreSQL and is tracked in its own migrations table, so it can share a
  database with the history schema.

## Compatibility with Murmur

Handlers follow `Messages.cpp` and `Server.cpp`: the same permission checks in the
same order, the same `PermissionDenied` types and `Reject` reasons, the same
initial state on login (CryptSetup, CodecVersion, the channel tree and links, user
states, ServerSync, ServerConfig, SuggestConfig), the ACL engine of `ACL.cpp` and
`Group.cpp` (ordered evaluation, Traverse/Write abort, root-only permissions,
Write's implications, meta groups `all auth strong in out none sub,a,b,c`, the
`~ ! # $` prefixes), Murmur's default root ACLs, temporary channels counted down
from `i32::MAX`, text limits in UTF-16 units with the `<img>` exception, the
leaky bucket for `messagelimit`/`messageburst`, and blobs announced by hash from
128 bytes on. The client test suites run unchanged against stock Murmur and
`nuscheld` (see Tests).

Deliberate differences:

- **Ghost takeover.** An unregistered name in use is only taken over by the same
  certificate (or, both without certificates, the same address). Murmur lets any
  connection from the same address take it.
- **Certificates of registered users.** A password login from a new certificate
  does not replace the stored one (Murmur's behaviour lets a leaked password take
  the account over for good); an account without a certificate gets the first one.
- **Registration ids are never reused**, even after an account is deleted.
- **Session ids** count up and wrap only after 2³¹ sessions.
- **Server-browser pings** are answered with at most the request's size by
  default (`allow_ping_amplification = false`), so the server cannot amplify
  spoofed traffic; stock server lists then show no user count. With the option on,
  answers are at most twice the request (stock's 12 → 24 bytes), rate-limited per
  address and globally; protobuf requests under 12 bytes are never answered.
- **Access tokens** are capped (32 of up to 128 bytes) and kept only as SHA-256
  digests, compared in constant time.
- **Limits Murmur does not have**: per-type frame limits from the header (before
  authentication only `Version` ≤ 4 KiB, `Authenticate` ≤ 16 KiB and `Ping`
  ≤ 256 B; anything else closes the connection), element caps after decoding
  (ACL entries, groups, ban entries, voice targets, blob requests), a flood bucket
  for all control messages and a query bucket for permission and blob queries,
  per-address and pending-connection caps, and a bounded argon2id queue (a busy
  server answers `AuthenticatorFail`, "try again later").
- Not implemented: Ice/gRPC, external authenticators, `strong` certificates (no
  CA store; the group never matches), email login, persisted channel listeners,
  rolling statistics, behaviour specific to clients older than 1.2.2, recording
  notices for clients older than 1.2.3, the pre-1.4 Opus/CELT codec switching.

## Security measures

Passwords are argon2id (19 MiB, two passes by default) on the blocking pool behind
a semaphore of two with at most 32 queued; the server password and tokens are
compared as digests in constant time. The TLS verifier accepts any client
certificate but rustls still checks the handshake signature, so presenting
someone else's certificate fails (tested). TLS session resumption is off. Debug
output of protocol messages redacts passwords, tokens, keys, comments and
message bodies; logs contain user names, session ids and addresses, never
contents. The database file is created 0600 in a 0700 directory. Handshake (TLS
within `handshake_timeout`, authentication within 1.5×) and idle timeouts,
autoban (Murmur's 10 attempts in 120 s → 300 s) and IP and certificate bans are
checked before a connection gets anywhere. The accept loop backs off on
descriptor exhaustion.

## Extension points (phase 2)

- `tls.rs` offers ALPN `nuschel/1` (and `mumble`); whether a connection
  negotiated it is recorded per user (`User::nuschel`).
- `ClientInfo::apply` is the only place a `Version` message is interpreted; the
  `NuschelHello` field (7200) is decoded there.
- `State::handle` is the single dispatch point; frames in 0x4E00–0x4EFF are
  counted and ignored until the extension's handlers exist.
- Every text message goes through `State::text_message`, where persistence with
  per-conversation sequence numbers and idempotency keys slots in.
- Channels carry a random 128-bit `conversation_id`, assigned once and never
  reused, and a per-channel `history_visibility` (default: members read earlier
  history). Nothing is pruned (retention defaults to forever).
- `State::broadcast` is where a second encoding with extension fields would be
  chosen per connection; stock clients never receive extension fields.

## Tests

| Command | What it covers |
| --- | --- |
| `cargo test -p nuscheld` | ACL engine (Murmur's cases), leaky bucket and bandwidth record, text limits, config, time format, argon2id; a fuzzing harness over the state; loopback integration tests: login and initial state, every reject reason, a certificate without its key, oversized pre-authentication frames, channels, text, ACL denial, kick, temporary channels, voice over the tunnel and encrypted UDP to protobuf and legacy clients, whisper isolation, self-mute, restart persistence, hostile frames |
| `cargo test -p nuschel-protocol` | OCB2 role constructors, relay parse/encode, info-ping limits, per-type frame limits, redacted `Debug` |
| `cargo test -p nuschel-storage server` | the server schema on SQLite, reopening, cascades, the registration high mark |
| `python3 tests/with_local_nuscheld.py <bins> tests/<suite>.py <bins> --port PORT …` | `mumble_client.py`, `udp_voice.py`, `protocol_features.py` against nuscheld; `with_local_murmur.py` runs the same scripts against stock Murmur |
| `nix build .#checks.x86_64-linux.vm-server --cores 2` | the module with SQLite and PostgreSQL, restarts, the client suites (with loss and blocked UDP), and the stock Mumble GUI (login with a Nuschel-exported certificate, voice both ways) |
| `cargo +nightly fuzz run server_messages` (in `fuzz/`) | also `control_frames`, `relay`, `crypt` |
