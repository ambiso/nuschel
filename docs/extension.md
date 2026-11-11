# The Nuschel protocol extension

Status: design for review (2026-11-11), not implemented. It covers roadmap phase 2
(TODO.md, "Nuschel server"): negotiation, durable conversations, persistent history,
subscriptions. Identity, key transparency and MLS (phases 3–4) are designed in
`docs/e2ee.md` and build on the framing and identifiers defined here.

Goals, in priority order:

1. Stock Mumble clients keep working on Nuschel servers that allow them, and Nuschel
   keeps working on stock Mumble servers. Nothing here is sent to a peer that did not
   negotiate it.
2. One server implementation for both: the extension is Mumble's control protocol
   plus new messages, not a second protocol. A server can refuse legacy clients
   (strict mode) without changing the protocol.
3. Decisions that are hard to change later (identifiers, ordering, what the server
   can see) are made for the end-to-end encrypted future, not just for today's
   plaintext history.

## 1. Negotiation

**Decision: TLS ALPN, plus a capability list in `Version`.**

- The Nuschel client offers ALPN protocols `nuschel/1` and then `mumble`. Stock
  Murmur ignores ALPN; a Nuschel server selects `nuschel/1`. Both sides know before
  the first Mumble message whether the extension is available, and a strict server
  can reject a client that did not offer it with a `Reject` that stock clients can
  display ("This server requires the Nuschel client").
- After TLS, both sides send `Version` as usual. A Nuschel peer adds a field with a
  high, unregistered field number (`nuschel = 7200`) holding a `NuschelHello`:
  protocol version, capability strings with versions (`history/1`, `subscribe/1`,
  `typing/1`, `files/1`, `mls/1`, `kt/1`), and the server's policy (strict or mixed,
  minimum and recommended client versions, retention). Protobuf decoders skip unknown
  fields, so stock peers are unaffected even if a server mistakenly sent it.
- The effective capability set is the intersection. Each feature is used only if
  both sides announced it; a capability is not a permission (ACLs still apply).

Alternatives considered: a capability message right after `Version` (works, but a
strict server would learn too late to reject cleanly); a separate port or protocol
(two implementations, splits the ecosystem); encoding capabilities in `release` or
`os` strings (fragile, user-visible).

**Downgrade protection.** The ALPN offer is covered by the TLS handshake transcript
(authenticated by the Finished messages in TLS 1.2 and 1.3), and `Version` travels
inside the authenticated channel, so a network attacker cannot strip `nuschel/1` or
capabilities without breaking the connection, provided the server is authenticated
(CA-validated certificate, pin, or later the key-transparency log). Three rules
close the remaining gaps:

1. *No fallback.* The client offers `nuschel/1` and `mumble` together in one
   handshake and lets the server choose. It never reconnects with fewer options after
   a failure (that is how POODLE-style downgrades worked).
2. *Sticky policy.* A server that negotiated `nuschel/1` (or a capability such as
   `mls/1`) is remembered with its pin, like HSTS. If it later answers as plain
   Mumble or drops a capability, the client warns loudly and does not silently
   continue in the weaker mode. An end-to-end encrypted conversation is never sent
   plaintext, whatever the server announces: the client's own MLS state decides,
   not the server's advertisement, and MLS groups declare required capabilities in
   their group context.
3. *First contact.* Trust on first use cannot stop a man in the middle on the very
   first connection (he can impersonate the whole server, not just downgrade it).
   CA-validated server certificates, out-of-band fingerprints and the server key in
   the transparency log (phase 3) address that.

The ALPN offer is visible to network observers (it reveals that the client is
Nuschel); hiding it would need Encrypted Client Hello and is not a goal now.

**Minimum versions** (user request, Signal-like): the hello carries
`minimum_client` and `recommended_client`. A client below `recommended` shows an
update notice; below `minimum` the server rejects after the announced grace date.
Servers must never use this to lock out stock clients on a mixed server.

## 2. Framing and message types

Extension messages use the Mumble control framing (2-byte type, 4-byte length,
protobuf body) with types from a reserved block, **0x4E00–0x4EFF** ("N"), far from
Mumble's own range (0–26 today). They are defined in a separate protobuf package
(`nuschel.v1`) in `crates/protocol`. Limits per message type are explicit (sizes,
counts) and enforced on both sides; oversized input is answered with an error, never
silently truncated.

Every request carries a client-chosen `request_id` (u32, per connection), and every
response or error echoes it. Server pushes have no `request_id`. Errors are one
message type (`NuschelError { request_id, code, detail }`) with stable codes.

Voice, channel and user state stay Mumble messages. Video and screen sharing are not
carried on this connection (see the WebRTC plan in docs/architecture.md).

## 3. Identifiers

These are the decisions that are hardest to change.

- **Server.** Its TLS certificate pin (trust on first use, as today) and, from phase
  3, a server signing key that is also the key-transparency log key.
- **Users.** Registered users have a persistent server-side user id (u32, Mumble's
  registration id). Unregistered users can read and write in channels but get no
  persistent identity and no server history of their own direct messages. The
  certificate hash stays the cross-server Mumble identity; the phase-3 identity key
  replaces it for anything end-to-end.
- **Conversations.** A 128-bit random `ConversationId` assigned by the server and
  stored with the channel. Mumble channel ids are reused (temporary channels, a
  deleted and recreated channel), so they must not key history. Direct
  conversations get their own `ConversationId` per pair of registered users. The
  `ChannelState` of a Nuschel server carries the conversation id in an extension
  field.
- **Messages.** A per-conversation sequence number (`seq`, u64 from 1), assigned by
  the server when the message is durably stored. Not a global counter: a global
  counter would let any member infer activity in conversations they cannot read
  (gaps). Ordering within a conversation is total; across conversations there is no
  order, only server timestamps.
- **Idempotency.** Every send carries a client-generated 128-bit `idempotency_key`
  (UUIDv7, random part from the OS RNG). The server stores `(user, key) → seq` for a
  bounded period (e.g. 7 days) and answers a retry with the original `seq`. This is
  how a client resends after a dropped connection without duplicates.

## 4. Sending and live delivery

- `ChatSend { request_id, conversation, idempotency_key, content, reply_to? }`
  → `ChatAck { request_id, conversation, seq, server_time }` after the message is
  durably stored, or an error (permission, size, rate limit with `retry_after`).
  The client shows the message as pending until the ack; that state already exists
  for stock servers.
- `content` is the user's text as typed (Nuschel's Markdown with fenced code) plus
  attachments by reference. For stock clients in the same channel on a mixed
  server, the sending client also supplies `legacy_html` (what it sends to stock
  servers today: escaped, links as `<a>`, images inline). The server forwards that
  to stock clients as an ordinary `TextMessage` and never renders Markdown itself.
  In an end-to-end encrypted conversation `content` is an MLS ciphertext and there
  is no `legacy_html` (stock clients are not members).
- Messages from stock clients get a `seq` too and are stored (on servers that keep
  history), so Nuschel clients see one history.
- Live delivery is `ChatEvent { conversation, message }` to every connection
  subscribed to that conversation that may read it now.

## 5. Subscriptions

Text conversations are independent of voice: `Subscribe { conversations }` /
`Unsubscribe`. The server checks the read permission at subscription time and on
every ACL change that affects the subscriber (it unsubscribes and notifies, rather
than letting events leak after a permission is removed). The client subscribes to
the conversations shown in its sidebar, bounded per connection.

## 6. History

- `HistoryRequest { request_id, conversation, anchor, limit }` with
  `anchor = latest | before(seq) | after(seq) | around(seq)`; `limit ≤ 100`; the
  server also caps a page at 256 KiB. Pages are chronological and say whether more
  exists in each direction; a byte cap ends a page early but never skips a message,
  and a single message larger than the cap is returned alone.
- The read permission is checked on every page, against current ACLs. Being able to
  read a channel now does not imply having been able to read it then; the default
  policy is "members can read the retained history of conversations they can read",
  and servers may choose "only since joining" per channel (a per-channel setting;
  in MLS conversations this follows from the cryptography anyway).
- Edits and deletions are events with their own `seq` that refer to the target
  (`edits`, `deletes`). Pages return the current state of each message plus a
  tombstone for deleted ones, so clients converge without replaying everything.
  Stock clients see neither (they never see history).
- Retention: a per-server and per-channel policy (keep forever, N days, or none).
  Expired messages become tombstones without content so that `seq` ranges stay
  explicit. Moderators' deletions are tombstones with a reason code.
- History queries run off the control path with bounded concurrency per
  connection; a slow reader or a huge query never delays voice or control messages.
- Search stays client-side (over pages the client has fetched and stored). This is
  consistent for plaintext and encrypted conversations and keeps the server from
  becoming a content index. A server-side search for plaintext servers can be added
  later as a separate capability.

## 7. Read state, typing, presence

- `ReadMarker { conversation, seq }`: per user, synced across the user's devices,
  never shown to others unless the user enables read receipts (off by default).
- `Typing { conversation }`: ephemeral, rate-limited (one per 3 s), forwarded only to
  subscribers, never stored. In end-to-end conversations typing indicators are sent
  as MLS application messages instead (the server would otherwise learn who types
  where).

## 8. Files

On a Nuschel server, files are not wormhole codes in text: `files/1` is a
chunked, resumable upload to the server's blob store, referenced from a message by
content hash, with a per-server quota. In end-to-end conversations the blob is
encrypted client-side with a per-file key carried inside the MLS message. Peer-to-peer
transfer through the server as a relay (no storage) is a server policy option. The
details belong to the files design once phase 4 exists; the wormhole transfer stays
the Mumble-compatible path.

## 9. What the server can see

On a plaintext (non-MLS) Nuschel server: everything, as on any Mumble server, plus
the history it stores. In MLS conversations: membership, sizes, timing, `seq`,
sender user id, and read markers if synced; not content, not typing, not file
contents. The lock indicator rules in docs/architecture.md apply unchanged.

## Open questions for review

1. **Default history policy for channels.** The design defaults to "members read
   the retained history". The alternative is "only since joining". Which should be
   the default for plaintext channels?
2. **Read markers synced server-side.** On by default for multi-device convenience,
   off for privacy?
3. **Strict servers.** Should strict servers reject stock clients at the TLS/ALPN
   stage (cleanest) or accept them read-only into non-E2EE channels?
4. **Retention defaults** for a fresh server: keep forever, or 90 days?
