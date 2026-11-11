# The Nuschel protocol extension

Status: design (2026-11-11), revised after the security review of the same day;
implemented in `nuscheld`, `nuschel-session` and the desktop except files (§8) and
MLS; key transparency (`kt/1`) is implemented as listed in "Implementation notes". Where the implementation refined the design, the text says so
and "Implementation notes" at the end lists the refinements. It covers roadmap phase
2 (TODO.md, "Nuschel server"): negotiation, durable conversations, persistent
history, subscriptions. Identity, key transparency
and MLS (phases 3–4) are designed in `docs/e2ee.md` (in progress) and build on the
framing and identifiers defined here.

Goals, in priority order:

1. Stock Mumble clients keep working on Nuschel servers that allow them, and Nuschel
   keeps working on stock Mumble servers. Nothing here is sent to a peer that did not
   negotiate it.
2. One server implementation for both: the extension is Mumble's control protocol
   plus new messages, not a second protocol. A server can refuse legacy clients
   (end-to-end mode) without changing the protocol.
3. Decisions that are hard to change later (identifiers, ordering, what the server
   can see, who may read what) are made for the end-to-end encrypted future, not just
   for today's plaintext history.

## 1. Negotiation

**Decision: TLS ALPN, plus a capability hello in `Version`.**

- The Nuschel client offers ALPN `nuschel/1` and `mumble` in one handshake. Stock
  Murmur ignores ALPN; a Nuschel server selects `nuschel/1`. A server in end-to-end
  mode rejects a client that did not offer it with a `Reject` stock clients can show
  ("This server requires the Nuschel client") and closes without parsing or logging
  the stock client's `Authenticate`.
- After TLS both sides send `Version`. A Nuschel peer adds field 7200 holding a
  `NuschelHello`: protocol version, capabilities (`history/1`, `subscribe/1`,
  `typing/1`, `files/1`, `mls/1`, `kt/1`; at most 32, each ASCII
  `[a-z0-9-]+/[0-9]+` up to 64 bytes, unknown ones ignored), and the server's policy
  (mode, minimum and recommended client versions, retention).
- **The extension is on if and only if ALPN selected `nuschel/1` and both hellos are
  present.** Any mismatch (ALPN without a hello, a hello without ALPN) is a protocol
  error, never stock mode. A Nuschel client ignores field 7200 and extension fields in
  other messages unless ALPN selected `nuschel/1`; a server ignores field 7200 from a
  client that did not offer ALPN.
- The hello is accepted once, before `Authenticate`, and the capability set is then
  frozen for the connection. The effective set is the intersection; a capability is
  not a permission (§5).
- A client with a sticky record (below) waits for the server's `Version` before
  sending `Authenticate`, so credentials never go to a server that answers in a weaker
  mode.

**Downgrade protection.** The ALPN offer is covered by the TLS handshake transcript
(authenticated by the Finished messages), and `Version` travels inside the
authenticated channel, so a network attacker cannot strip `nuschel/1` or capabilities
without breaking the connection, provided the server is authenticated. Rules:

1. *No fallback.* The client never reconnects with fewer options after a failure.
2. *Sticky policy, never downgrade.* A server that negotiated `nuschel/1` (and every
   capability it negotiated, such as `mls/1`) is remembered by its address (later by its identity
   key), not by its leaf certificate: CA-issued certificates rotate every few months,
   and a rotation (or a misissued certificate) must not reset the record. If the
   server later answers as plain Mumble or without that capability, the connection
   fails with an error the user can act on; the client never continues in the weaker
   mode (failing is fine: an attacker can always cut the connection). Only the user
   can reset this, explicitly, per server. An end-to-end encrypted conversation is
   never sent plaintext, whatever the server announces: the client's own MLS state
   decides, and MLS groups declare required capabilities in their group context.
3. *First contact.* Trust on first use cannot stop a man in the middle on the very
   first connection. Invites and `mumble://` links carry the server's certificate or
   identity-key fingerprint and required policy (`?fp=sha256:…&policy=nuschel`),
   which closes first contact for invited users; CA-validated certificates,
   out-of-band fingerprints and the transparency log (phase 3) cover the rest.
4. *TLS settings.* No 0-RTT and no server session resumption. TLS 1.3 is preferred:
   in TLS 1.2 the client certificate, a stable cross-server identity, crosses the
   wire in cleartext.

The ALPN offer itself is visible to network observers (it reveals Nuschel); hiding
it would need Encrypted Client Hello and is not a goal now.

**Minimum versions** (Signal-like): the hello carries `minimum_client` and
`recommended_client`; below `recommended` the client shows an update notice, below
`minimum` the server rejects after the announced grace date. These are self-reported
and advisory, not a security control. A server must never use them to lock out stock
clients in compatible mode.

## 2. Framing, message types, limits

Extension messages use the Mumble control framing (2-byte type, 4-byte length,
protobuf body) with types from a reserved block, **0x4E00–0x4EFF**, defined in a
separate protobuf package (`nuschel.v1`) in `crates/protocol`.

- Every message type has a wire-size cap and element-count caps, enforced before and
  after decoding (protobuf repeated fields expand several-fold in memory); the caps
  also depend on connection state (much lower before authentication).
- A frame from the reserved block before capabilities are frozen, or for a capability
  that was not negotiated, means disconnect (before authentication) or
  `NuschelError { unsupported }` (after); it is never processed.
- Requests carry a client-chosen `request_id`; at most 16 may be outstanding per
  connection, and a duplicate in-flight id is rejected. Responses echo it.
- Errors are one type, `NuschelError { request_id, code, detail }`, with stable codes
  and fixed detail strings (never SQL, paths or user text); clients show them as
  plain text. "No such conversation" and "not allowed" are the same error, so ids
  cannot be probed.
- Voice, channel and user state stay Mumble messages. Video and screen sharing are
  not carried on this connection (see docs/architecture.md, WebRTC).

## 3. Identifiers

- **Server.** Its TLS certificate pin (trust on first use) and, from phase 3, a server
  signing key that is also the key-transparency log key.
- **Accounts.** Registered users get an immutable 128-bit account id; Mumble's
  registration number (which Murmur reuses, lowest free first) is only a wire alias.
  Deleting an account never lets a new registrant inherit its direct messages, edit
  rights or ACL entries, and per-user ACL entries are removed on unregistration.
  Unregistered users can read and write in channels as permitted, but have no durable
  identity: no cross-reconnect idempotency and no stored direct messages.
- **Conversations.** A 128-bit random `ConversationId` assigned by the server, stored
  with the channel and never reused (Mumble channel ids are). Direct conversations get
  one per pair of accounts. Temporary channels keep no history by default (it is
  deleted with the channel); a deleted permanent channel's history becomes
  administrator-only, since no ACL remains to authorise it.
- **Messages.** A per-conversation sequence number (`seq`, u64 from 1) assigned by the
  single owner of authorisation state (the server's state lock) and acknowledged once
  the message is durably stored; storage, fan-out and access intervals therefore agree
  on one order. A global counter would let members infer activity in
  conversations they cannot read. `seq` reveals how many messages came before a
  reader's window; that is accepted and documented.
- **Identifiers are not capabilities.** Knowing a `ConversationId`, a `seq` or a
  cursor grants nothing; every use is authorised (§5).
- **End-to-end ordering.** A server-assigned `seq` lets a malicious server reorder,
  withhold or fork history per member. MLS payloads therefore carry their own
  per-sender counter and a hash of the sender's previous message inside the
  ciphertext (docs/e2ee.md), so clients can detect it.

**Idempotency.** Every send carries a client-generated 128-bit `idempotency_key`
(random). It is scoped to `(account, conversation, key)` and stored on the message row
under a unique constraint, checked and inserted in one transaction by the single
history writer (two devices retrying at once cannot both insert). A retry with the
same key and the same body (a hash of conversation, format, reply and content)
returns the original `seq`; with a different body it returns `idempotency_conflict`
(a buggy client must not lose messages silently). The `seq` the server had assigned to
the retry is stored as a `void` row, so seq ranges stay explicit. Keys are never
forwarded to other users or returned in pages.

## 4. Sending and live delivery

- `ChatSend { request_id, conversation, idempotency_key, format, content, reply_to?,
  attachments? }` → `ChatAck { request_id, conversation, seq, server_time }` once the
  message is durably stored, or an error (permission, size, rate limit with
  `retry_after`). The client shows the message as pending until the ack.
- `content` is what the user typed, with a `format` tag (Nuschel Markdown with fenced
  code today); every stored message keeps its format. `reply_to` is a `seq` in the
  same conversation (at most the head), never a copied excerpt. Attachments are
  references to the sender's own uploads (§8).
- **Stock clients' copy is made by the server** from `content`, with the same pure
  functions the client uses on stock servers (escaping, links, the code-fence
  newline; today `legacy_body`/`text_html`, to move into a shared crate), and images
  from attachments as pre-sized inline images. A sender therefore cannot show stock
  users something different from what Nuschel users, moderators and the archive see,
  and the server has no second HTML path to police. Murmur's text rules
  (`isTextAllowed`, length, image limits) apply to that output.
- In end-to-end conversations `content` is an MLS ciphertext and nothing is sent to
  stock clients (they are not members).
- Messages from stock clients are stored too (on servers that keep history) with a
  `legacy_html` format, so Nuschel clients see one history. A stock `TextMessage`
  with several channel or tree targets is deduplicated before traversal, capped in
  target count, and stored in each reached channel the sender may write to; direct
  messages to unregistered sessions are not stored.
- **Rate limits.** `ChatSend` and edits share Murmur's text bucket with `TextMessage`
  (`messagelimit`/`messageburst`, default 1/s with a burst of 5), so Nuschel clients
  get no second allowance.
- Live delivery is `ChatEvent { conversation, message }`, after the message is
  stored, to each subscribed connection that may read the conversation at that moment
  (§5), and to Nuschel connections in its live audience as for stock text (in the
  channel or listening, guests included; both accounts of a direct conversation).
  Stock clients get the `TextMessage` copy; a Nuschel connection never gets both. A
  stock sender's channel message reaches stock recipients at once and Nuschel readers
  once stored (live as `TextMessage` if storing fails, or if it exceeds 1 MiB, which
  is not kept).

## 5. Who may read, and subscriptions

Mumble has no "read" permission, and a new ACL bit would not survive: the stock ACL
editor rebuilds grant/deny from the checkboxes it knows, so editing an entry with a
stock client would silently erase (or, for a deny, open) it.

- **`can_read(user, conversation)` = Enter on the channel ∨ the user is in the
  channel ∨ listening to it**, evaluated with persistent credentials only (registered
  account, certificate, persistent access tokens). Extension-only policy (history
  visibility, guest access, moderation) lives in channel and server properties
  outside the Mumble permission bitmask.
- **Guests.** Unregistered users read no history by default (`history.guests = none`;
  they see live messages, as on any Mumble server). Murmur's defaults give Enter to
  everyone, and an archive kept forever should not be readable anonymously.
- **History visibility** per channel: `members` (default, user decision: members read
  the retained history) or `since_access`, which means the set of intervals in which
  the user could read the channel, not first join (a user removed and later readmitted
  does not see what was said while they were out). All anchors, including
  `around(seq)`, are clipped to those intervals; outside them is indistinguishable from
  nonexistent, and pages carry a `restricted` flag at the window's edge. Intervals are
  kept in seq space per account: one opens (at head + 1) when the server sees the
  account able to read (at login, or on any change while it is online) and closes (at
  head) when it no longer can, which is checked on every global change even while the
  account is offline (by its registration and certificate, without tokens). An
  account granted access while offline gets its interval at its next login, so
  `since_access` errs towards showing less. Switching a channel to `since_access` keeps
  what its current readers could see (an interval from seq 1); the setting is
  `ChannelSettings` and needs Write on the channel. At most 32 intervals are kept per
  account and channel; the oldest are dropped, never merged.
- **One owner of authorisation state.** A single task owns channels, ACLs, groups,
  memberships and tokens, with a version counter (`authz_epoch`). Every change that
  can affect reading re-evaluates subscriptions: ACL edits, user moves, joining or
  leaving a listening channel, token changes, registration and unregistration,
  channel reparenting (inheritance), deletion. Fan-out of new messages, history pages
  in flight and listeners all check against the current epoch; a history page is
  re-checked just before it is sent.
- `Subscribe { conversations }` / `Unsubscribe`, bounded per connection and per
  account. On losing `can_read`, the server unsubscribes and notifies rather than
  letting events leak. Subscribers are invisible to others (unlike Mumble's channel
  members and listeners); see §9.

## 6. History

- `HistoryRequest { request_id, conversation, anchor, limit }` with
  `anchor = latest | before(seq) | after(seq) | around(seq)`; `limit ≤ 100`, a page is
  capped at 256 KiB, and requests have their own rate limits (pages and bytes per
  minute). Pages are chronological, say whether more exists in each direction, end
  early at the byte cap but never skip a message, and return a message larger than the
  cap alone. Pages run off the control path with bounded concurrency per connection.
- `can_read` and the visibility policy are checked on every page against current
  state (§5).
- **Edits:** only the original author (same account; unregistered authors cannot
  edit), only while they may still post in that conversation, in the same
  conversation, at most N times per message, counting against the text rate limit.
  `reply_to` and attachments cannot change. An edit replaces the stored content;
  earlier versions are not served and are purged (people edit to remove pasted
  secrets).
- **Deletions:** by the author at any time, even after losing access, or by
  moderators (a server property per §5). A tombstone keeps `seq`, the deletion time
  and, for moderator deletions, a reason code; it drops content, attachments,
  `reply_to` previews and mentions. Attachments are released by reference count (§8).
- Edits and deletions are events with their own `seq` that refer to the target;
  pages return each message's current state plus tombstones, so clients converge
  without replaying everything. Event rows in pages carry no content (the target row
  has it); an event about a message outside the reader's visible ranges is a `void`.
  A live edit event carries the new content, only to readers who may see the target. Stock clients see neither: a "removed" message stays
  on their screens and in their logs, and the UI says so to moderators.
- **Retention:** per server and per channel (keep forever, N days, or none); the
  default is forever (user decision). Expired messages become tombstones without
  content, so `seq` ranges stay explicit. Implemented: a server-wide number of days
  (checked hourly); per-channel retention and "none" are not.
- **Search** stays client-side (over pages the client has fetched and stored),
  consistent for plaintext and encrypted conversations; a server-side search for
  plaintext servers can be added later as a separate capability.

## 7. Read state, typing, presence

- `ReadMarker { conversation, seq }`: per account, accepted only for readable
  conversations, monotonic, `seq ≤ head`, bounded per account; synced across the
  user's devices by default (pushed to the account's other connections and returned
  with `Subscribed`; Mumble allows one connection per account at a time, so in practice
  the next device to log in gets it) (user decision) and never shown to others unless read
  receipts are enabled (off by default). In end-to-end conversations markers sync as
  an encrypted per-user blob or an MLS message to oneself, so the server does not
  learn reading times.
- `Typing { conversation }`: ephemeral, rate-limited (one per 3 s per conversation),
  forwarded only to subscribers, never stored; invisible subscribers also see it. In
  end-to-end conversations typing travels as MLS application messages. Mumble's
  `PluginDataTransmission` is forwarded by Murmur to any listed session without channel
  authorisation, so on stock servers it is only usable for cosmetic hints.

## 8. Files

On a Nuschel server, files are not wormhole codes in text: `files/1` is a chunked,
resumable upload to the server's blob store.

- An upload has an `upload_id` owned by the uploading account; `ChatSend` may only
  reference the sender's own uploads. A content hash is never a capability: downloads
  are authorised by "can read a message that references this upload".
- No cross-user deduplication (an instantly finished upload would confirm that someone
  else holds the file); deduplicate per uploader at most, and never use convergent
  encryption for end-to-end blobs.
- Quota is reserved at upload start (declared size; per account, per conversation and
  global). Chunk offsets are validated, the hash verified at completion, partial
  uploads expire and are collected, and blobs are reference-counted so deletion and
  retention remove them.
- Clients never trust a server-supplied MIME type for rendering. A relay-only mode
  (no storage) has per-user bandwidth caps and works only between members of the
  conversation.
- In end-to-end conversations the blob is encrypted client-side with a per-file key
  carried inside the MLS message. The wormhole transfer stays the Mumble-compatible
  path.

## 9. Mixed servers and stock users

Stock users cannot tell that their channel messages are archived and readable by
people who join later, or that invisible subscribers receive them; Mumble shows who is
in a channel and who listens. On a server in compatible mode:

- Stock clients are told, in the welcome text and when entering an archived channel,
  that text is retained and for how long; retention is visible per channel.
- Direct messages from stock senders are not archived by default
  (`history.archive_stock_direct_messages` stores those between two accounts).
- Operators get tools to erase a user's messages (and must, for GDPR requests):
  `nuscheld --erase-user NAME` turns them into tombstones. Copies already on clients
  (Nuschel caches, stock clients' logs) remain.

## 10. What the server can see

In compatible mode: everything, as on any Mumble server, plus the history it stores.
In MLS conversations: membership, sizes, timing, `seq`, sender account; not content,
typing, read markers or file contents. The lock indicator rules in
docs/architecture.md apply unchanged.

## 11. Later

- `udp-aead/1`: Mumble's OCB2 with a 24-bit tag is kept only for stock compatibility.
  If Mumble-transport voice outlives the WebRTC plan, Nuschel↔Nuschel voice should
  negotiate an AEAD with a 128-bit tag and an explicit counter.

## Decisions (user, 2026-11-11)

1. **History for new members:** yes by default (`members`); a per-channel setting
   restricts it (`since_access`). In end-to-end channels this needs a history-sharing
   mechanism on top of MLS (docs/e2ee.md).
2. **Read markers:** synced across the user's devices by default.
3. **End-to-end mode:** the default for new Nuschel servers; it rejects stock clients
   at the connection stage. Compatible mode, which admits them and has no end-to-end
   encryption, is an explicit choice.
4. **Retention:** keep forever by default; shorter retention is an explicit server or
   channel setting.

Adopted from the security review (2026-11-11): server-made stock HTML instead of
client-supplied `legacy_html`; `can_read` from existing Mumble permissions; guests
read no history by default; account ids never reused; access intervals for
`since_access`; edit and deletion rules with purging; scoped, atomic idempotency;
fail-closed negotiation with frozen capabilities; upload ids instead of hash
capabilities; notices for stock users on mixed servers; per-sender ordering inside
end-to-end payloads.

## Implementation notes (2026-11-11)

What the implementation settled or refined (`crates/protocol/proto/nuschel.proto` is
the wire definition):

- **Messages** `0x4E00`–`0x4E13`: `Error`, `ChatSend`, `ChatAck`, `ChatEvent`,
  `HistoryRequest`, `HistoryPage`, `Subscribe`, `Subscribed`, `Unsubscribe`,
  `Unsubscribed`, `ChatEdit`, `ChatDelete`, `ReadMarker`, `Typing`, `OpenDirect`,
  `DirectOpened`, `ListDirect`, `DirectList`, `ChannelSettings`, `Done`. Field 7200 of
  `ChannelState` carries a channel's conversation id, visibility and retention, to
  Nuschel connections only.
- **Capabilities**: `history/1` covers sending, history, edits, deletions, read
  markers, direct conversations and channel settings; `subscribe/1` and `typing/1` are
  separate. The client's sticky record keeps `nuschel/1` and every negotiated
  capability.
- **Rows** are `message`, `edit`, `delete` or `void`; deletion reasons are 0 author,
  1 moderator, 2 expired, 3 erased by the operator.
- **Errors**: `unsupported`, `malformed`, `not_found` (also "not allowed"),
  `too_large`, `rate_limited`, `idempotency_conflict`, `busy`, `duplicate_request`,
  `too_many_requests`, `limit_reached`, each with one fixed detail string.
- **Limits**: `ChatSend`/`ChatEdit` text ≤ 64 KiB (and the stock copy within the
  server's text limit), stored stock HTML ≤ 1 MiB, ≤ 64 conversations per
  `Subscribe`, ≤ 256 subscriptions per connection and 1,024 per account, ≤ 1,024
  direct conversations per account, pages ≤ 100 rows and 256 KiB of content, pages and
  bytes per minute per connection (`history.pages_per_minute`,
  `page_bytes_per_minute`). History pages are not yet limited per address prefix;
  `connections_per_address` bounds them.
- **Guests**: `history.guests = none | full` (no `since_connect`).
- **Direct conversations** are opened by registration number, created on first use,
  and need Murmur's TextMessage permission in the online peer's channel; a deleted
  account's conversations are frozen and its messages show no name.
- **Storage**: three storage-worker processes on one database (server state,
  history writes in order, history reads), so a page never waits behind a write.
- **Key transparency** (`kt/1`, docs/e2ee.md §5): sticky like the other
  capabilities; the server's hello carries its log descriptor (`kt_log`, at most 512
  bytes), which the client's crypto worker pins, and `kt/1` without a descriptor fails
  the connection. Messages `0x4E14`–`0x4E1F`: `KtStatus`/`KtStatusReply` (the
  requester's account id and the newest head), `KtPublish`/`KtPublished` (bind one's
  own account: the root, a `KtRecord` with the device-signing certificate, at most 16
  device certificates and 32 revocations, and the identity's account statement for
  this account and log; registered users only), `KtLookup`/`KtLookupReply` (by
  registration number or account id, any registered account; the proof and the record
  the binding commits to), `KtHistory`/`KtHistoryReply` (the owner and contacts only),
  `KtAudit`/`KtAuditReply` (at most 4096 epochs; `too_large` above 6 MiB, and the
  client audits shorter spans) and `KtHead`/`KtHeadReply` (an earlier epoch's head).
  Proofs, heads and certificates are nuschel-kt's and nuschel-identity's own strict
  encodings in `bytes` fields. Limits: `KtPublish` ≤ 256 KiB from clients, other
  requests 64 bytes; lookup replies ≤ 512 KiB, histories and audits ≤ 6 MiB; per
  connection and minute `kt.lookups_per_minute` (120), `kt.histories_per_minute` (60,
  also audits and heads) and `kt.publishes_per_minute` (6). A missing account, one
  without a binding, and a history the requester may not see are the same
  `not_found`.
