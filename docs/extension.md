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

**Minimum versions** (Signal-like): the client's hello carries its own version
(`client_version`, semver); the server's policy carries `minimum_client`,
`recommended_client` and the grace date `minimum_client_from`. Below `recommended`
the client shows an update notice the user can dismiss; below `minimum` it shows a
warning with the date, which cannot be dismissed, and from that date on the server
rejects it at `Authenticate` with a `Reject` (`WrongVersion`, "This server requires
Nuschel X or newer"). Without a date the minimum applies at once. The client shows
the refusal as an error naming the version to update to and never retries it, and
never falls back to plain Mumble (rules 1 and 2 above). Versions compare by semver precedence; a
missing or unparsable client version is below every minimum. These are self-reported
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
  default is forever (user decision: "we don't want people to lose data because of
  such a setting being too low by default"), and shorter retention is an explicit
  setting. Expired messages become tombstones without content, so `seq` ranges stay
  explicit.
  - The server default is `history.retention` (`"forever"`, `"none"` or days); a
    channel may set its own with `ChannelSettings`, which needs Write on the channel
    like the visibility setting. It is a channel property outside the Mumble
    permission bitmask (no new ACL bit, which a stock ACL editor would silently drop,
    §5). `history.max_retention_days` caps every conversation, so an operator with a
    legal maximum is never overridden by a channel. Direct conversations follow the
    server default.
  - The setting belongs to the conversation, so a deleted channel's archive keeps
    its channel's retention; a temporary channel's goes with its history.
  - **Shortening applies to existing messages**: the new retention is what the
    server keeps, and older messages become tombstones within the hour (expiry runs
    hourly) and are not served from the moment of the change. So that nobody does
    this by accident, a change that keeps less than before is refused with
    `CONFIRMATION_REQUIRED` unless it carries `confirm_expiry`; clients show what will
    be removed ("messages older than N days", or all of them for "none") and ask
    before sending it. Keeping more needs no confirmation, but cannot bring back
    what already expired.
  - **"None"** means not archived: live delivery only. Messages are acknowledged
    and delivered with a `seq` as usual, but nothing is stored except the
    conversation's highest `seq` (so sequence numbers are never reused after a
    restart); there are no history pages (an empty page), no idempotency across
    reconnects, and no edits. Switching a channel to "none" expires what it stored
    before.
  - Retention is visible per channel in `ChannelInfo` (days or live-only, and
    whether the channel sets its own), and the server default in the hello's policy.
    Stock clients are told in the archive notice (§9).
  - Expiry never removes records the server must keep: one predicate decides what
    may expire (`nuschel_ipc::server::expirable` and its SQL twin), where MLS
    commits within their catch-up window (docs/e2ee.md §6.3) will be exempted.
    Expired text is overwritten in SQLite (`secure_delete`); PostgreSQL keeps old row
    versions until VACUUM, as for edits.
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

On a Nuschel server, files are not wormhole codes in text: `files/1` either keeps a
file in the server's blob store or sends it live from the sender's device.

**The design is [docs/files.md](files.md)** (2026-11-11), which supersedes the details
below where they differ. What changes: two classes, *kept* (stored, for images and small
files, as below) and *live* (large files stay on the sender's disk and go magic-wormhole
style: a direct connection between the two clients first, the server as relay when none
works, with a bounded, short-lived piece cache on the relay; nothing is stored), sharing
one encrypted format so a live file can be kept later; `upload_id` becomes a
server-assigned `object_id` with the same ownership rule; the server pulls pieces from
the sender on a second TLS connection (ALPN `nuschel-bulk/1`) and verifies each against a
Merkle root that also commits to the key, instead of verifying a hash at completion; the
relay-only mode below becomes the live class, with per-account budgets that slow
transfers rather than fail them; the global quota is `files.global_quota`, off by default
behind a free-disk reserve. What stays: downloads authorised by a referencing message, no
cross-user deduplication or convergent encryption, quota reserved at start, reference
counting with deletion and retention, no trusted MIME types.

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
  that text is retained and for how long; retention is visible per channel. The
  welcome text gives the server default; entering a channel that keeps something else
  (another number of days, or not archived at all) says so, and so does a change of
  the channel's retention to stock users in it.
- Direct messages from stock senders are not archived by default
  (`history.archive_stock_direct_messages` stores those between two accounts, when
  the sender may write to that direct conversation as a Nuschel client would:
  TextMessage in the channel of every connection of the other account, not only
  the one the stock client named; otherwise that connection gets it live only).
- Operators get tools to erase a user's messages (and must, for GDPR requests):
  `nuscheld admin history erase-user NAME --yes` (or the older `nuscheld
  --erase-user NAME`) turns them into tombstones. Copies already on clients
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
  `ChannelState` carries a channel's conversation id, visibility and retention
  (`retention_days`, 0 for forever; `live_only`; `own_retention`), to Nuschel
  connections only. `ChannelSettings` changes only what it carries: an optional
  visibility and an optional `Retention` (server default, forever, 1–36,500 days or
  live-only), with `confirm_expiry` for a shortening.
- **Minimum versions**: `Hello.client_version` (field 5, at most 64 bytes, only
  from clients); `Policy.minimum_client` and `recommended_client` in the 1.5 version
  format (`major << 48 | minor << 32 | patch << 16`, 0 for none) and
  `minimum_client_from` (field 9, Unix seconds, 0 for "now"). A pre-release client
  is below its release; build metadata is ignored. The server's settings are
  `history.minimum_client`, `recommended_client` and `minimum_client_from` (a date,
  docs/server.md); the session's `Error::Outdated` is the refusal: a `WrongVersion`
  reject while below the hello's minimum, or any failure before login once the
  hello's date has passed by the client's clock (the server may close before its
  `Reject` is read).
- **Capabilities**: `history/1` covers sending, history, edits, deletions, read
  markers, direct conversations and channel settings; `subscribe/1` and `typing/1` are
  separate. The client's sticky record keeps `nuschel/1` and every negotiated
  capability.
- **Rows** are `message`, `edit`, `delete` or `void`; deletion reasons are 0 author,
  1 moderator, 2 expired, 3 erased by the operator.
- **Errors**: `unsupported`, `malformed`, `not_found` (also "not allowed"),
  `too_large`, `rate_limited`, `idempotency_conflict`, `busy`, `duplicate_request`,
  `too_many_requests`, `limit_reached`, `confirmation_required` (12; 11 is reserved
  for `mls/1`), each with one fixed detail string.
- **Limits**: `ChatSend`/`ChatEdit` text ≤ 64 KiB (and the stock copy within the
  server's text limit), stored stock HTML ≤ 1 MiB, ≤ 64 conversations per
  `Subscribe`, ≤ 256 subscriptions per connection and 1,024 per account, ≤ 1,024
  direct conversations per account, pages ≤ 100 rows and 256 KiB of content, pages and
  bytes per minute per connection (`history.pages_per_minute`,
  `page_bytes_per_minute`) and per network: an IPv4 /24 or IPv6 /56 for unregistered
  users, the account for registered ones, so a busy NAT never locks out its
  registered users (`network_pages_per_minute`, 600 with a burst of 200, and
  `network_page_bytes_per_minute`, 64 MiB; `unlimited_networks` exempts networks).
  Following the user's view that arbitrary limits themselves cost availability,
  the defaults are generous and exceeding them fails only that request, with
  `rate_limited` and a `retry_after_ms` for the client to honour; nothing is
  disconnected and there is no server-wide cap. Search stays client-side (§6); a
  server-side search would draw from the same budgets.
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

## Desktop client (2026-11-11)

How the desktop uses the extension, and the decisions taken while the user was away
(each can be revisited):

- **Edits and deletions** are offered only on Nuschel servers with `history/1`, while
  connected, in conversations the server named. Your own messages (sent from this
  device, or by your registration number) get *Edit message* and *Delete message*;
  unregistered users get neither, since the server refuses them. Moderators get
  *Delete message* on others' messages only where the server says so: the hello's
  `Policy.moderators_delete` (field 7, added for this) and Write on the channel, the
  server's own rule. Up in an empty composer edits your last message; Enter saves,
  Escape restores what the composer held; emptying an edit asks whether to delete.
  Deletion asks first and says that stock Mumble users keep the message. The server
  sends edits and deletions to others only, so the client applies its own on
  `ChatAck`, rendering the new text in the content worker like any message. Edits and
  deletions go through the same ordered queue as new messages (content worker, then
  local history, then the timeline), so one never overtakes its message; one for a
  message on a history page still on its way waits for it (a minute at most), and a
  page that was rendered before a change does not bring the old text back, neither
  on screen nor in local history. An older edit never replaces a newer one.
  Messages are named by their `seq`, never by a local row id: on a server-history
  timeline a message without a seq (sent the stock way) has no id at all. The server
  records a deletion by someone holding Write as a moderator's, also of their own
  message, and the client shows it the same way. An edit typed while the connection
  is down waits in the composer (Enter says to reconnect; Escape discards it).
- **Tombstones** are one faint line ("Message deleted", "… by a moderator") in the
  message's place; they keep the author group they are in but never start one, so the
  next message shows its author again. An edited message shows "(edited)" with the
  edit time in its tooltip; screen readers hear ", edited" and "message deleted".
- **Typing**: a `Typing` at most every 3 s (the server's limit) while the composer
  changes and is not empty; nothing while editing, nothing once sent or emptied, and
  only in conversations the server named where we may write (Text message
  permission). The protocol has no "stopped typing": an indicator ends with that
  person's next message or 6 s after their last `Typing`. The indicator replaces the
  composer's "Message #channel" footer, a line that is always there, so nothing moves;
  it is a polite live region (AccessKit `live`, set by a small custom element because
  GPUI's elements cannot). Settings → Privacy → *Show when you are typing* (on by
  default) stops sending; others' indicators still show.
- **Direct messages** between two registered accounts on a Nuschel server use the
  pair's direct conversation (`OpenDirect`, `TextTarget::DirectConversation` in the
  session): sends are paced, idempotent and resent after a reconnect like channel
  messages, the timeline pages the server's history, and read markers and unread counts
  work as for channels. `DirectList` at login gives unread counts; a conversation with
  unread messages is listed even while the other account is offline (one without stays
  out of the list until opened: the DM home is a later item). A message in an unknown
  conversation is a new direct one (the server delivers those to its two accounts only)
  and the session asks for the list again. Stock servers, unregistered people on either
  side, and a refused `OpenDirect` keep the stock path (a `TextMessage` to the session);
  those timelines load local history and say "Local history". A registered peer on a
  stock client still gets the server's stock copy of our messages; their replies come
  over the stock path into a separate local conversation and are stored by the server
  only with `history.archive_stock_direct_messages`; stock direct messages from
  someone with a durable conversation (images, or from a stock client) show in that
  conversation, marked "not saved". Images and messages over 64 KiB, which the
  extension does not carry, go to the other account's session the stock way while it
  is online. Registration number 0 (SuperUser, and what the server reports for a
  deleted account) never maps to a direct conversation.
- **Local cache**: server pages, live messages, edits and deletions are written to the
  local history by `(server identity, storage key, seq)`: a message seen live and again
  in a page is kept once, an edit overwrites the text and search index, a deletion
  empties both and keeps a tombstone that a later copy cannot bring back, and our own
  message gets its seq when acknowledged. Local pages run in time order (ties by row
  id) because pages fetched later hold older messages than rows stored before them.
  Direct conversations are stored under `conversation:<id>`, channels under their
  channel id, which Mumble reuses (see TODO.md). Search runs over this local copy; a
  result in a conversation shown from server history opens by its seq, with the
  server's page around it (`around`) when it is not loaded, and newer pages
  (`after`) as the reader scrolls down; a stock message there that the server never
  stored cannot be opened.
