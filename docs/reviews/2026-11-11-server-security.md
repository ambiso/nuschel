# Nuschel server security review (2026-11-11)

Read-only review. Inputs: `README.md`, `SECURITY.md`, `docs/architecture.md`,
`docs/extension.md`, `TODO.md` ("Nuschel server") and `nix/module.nix` on `main`
(55f6157); the in-progress `server@` change in `~/Projects/nuschel-server`
(`noxnyusu`, on `dbafe3a2`): `crates/protocol/src/{crypt.rs,lib.rs,relay.rs}` and
`apps/server/src/main.rs`; stock Murmur at `~/Projects/mumble2` (83503fe1b) for
comparison. Nothing was built or run. Line numbers below refer to those trees.

Severity: **Critical** = exploitable by any network peer to break authentication or
confidentiality; **High** = authorisation/confidentiality failure or cheap remote DoS
that is expensive to fix later; **Medium** = real gap with a clear fix; **Low** =
hardening or clarity.

---

## 0. Summary

| # | Sev | Area | Finding |
|---|-----|------|---------|
| R1 | Critical (latent) | TLS | A rustls client-certificate verifier that "accepts self-signed" must still verify CertificateVerify signatures, or anyone replaying a victim's (public, in TLS 1.2 cleartext) certificate logs in as them |
| D1 | High | Extension | "Read permission" is undefined; Mumble has no read bit, and a new bit is silently erased when a stock client edits that ACL entry |
| D2 | High | Extension | With Mumble's default ACL, unregistered guests could read every public channel's full archive |
| D3 | High | Extension | Revocation races: subscriptions, in-flight history pages, listener proxies and whisper caches |
| D4 | High | Extension | `legacy_html` lets a sender show stock users different content from Nuschel users and the archive; second HTML-injection path |
| D5 | High | Extension | Murmur reuses the lowest free user id; extension data keyed by it passes to the next registrant |
| C4 | High (before first handler) | Code | One 8 MiB frame limit for all states; protobuf decode amplifies repeated fields 4–40x before any permission check |
| C3 | Medium | Code | `answer_info_ping` answers a 3-byte request with ~17 bytes (~6x reflection) |
| C2 | Medium | Code | `CryptState::new` argument order differs by role; swapped nonces mean both directions reuse one OCB2 nonce stream |
| C1 | Medium | Code | prost-derived `Debug` prints `Authenticate.password`, tokens and `CryptSetup.key` |
| D6–D13 | Medium | Extension | Edits/deletes unspecified; idempotency details; negotiation edge cases; files; consent of stock users; "since joining" semantics; conversation lifecycle; extension rate limits |

Verified correct in the WIP code: sender session comes from the server, never the
packet; failed decrypts leave state untouched (safe for trial decryption); both
OCB2 XEX\* countermeasures are in shared code, so the server inherits them; oversized
frames are rejected from the header, unlike stock Murmur.

---

## 1. Protocol extension design (`docs/extension.md`)

### D1 — High: what "can read" means is undefined, and new ACL bits do not survive stock ACL editors

§5/§6 say "the server checks the read permission", but Mumble's permission set
(`src/ACL.h`: Write, Traverse, Enter, Speak, MuteDeafen, Move, MakeChannel,
LinkChannel, Whisper, TextMessage, MakeTempChannel, Listen, plus root-only Kick, Ban,
Register, SelfRegister, ResetUserContent) has no read bit. Live text in Mumble reaches
whoever is *in* the channel or *listening* to it, and that does not need Enter
(users can be moved in).

Adding a bit (say `ReadHistory = 0x1000`) is unsafe on mixed servers. The stock ACL
editor rebuilds `pAllow`/`pDeny` from the checkboxes it knows whenever an entry is
touched (`src/mumble/ACLEditor.cpp` ~990–1003: `allowed |= p` over known permissions
only), so unknown bits disappear. If the bit is a deny, editing that entry with a stock
client **fails open**.

Read also depends on more than ACL edits. `in`, `out` and `sub,…` groups depend on the
user's current channel; `#token` groups depend on access tokens; `auth` on
registration; `strong` on CA verification. Mumble 1.5's
`UserState.temporary_access_tokens` grant tokens for a single message.

**Fix**
- Define it in the doc: `can_read(u, c) = effective(u, c) ∋ Enter ∨ u.channel == c ∨ u listens to c`,
  evaluated by the Murmur-compatible engine with the user's **persistent** tokens
  only. Temporary access tokens never authorise a subscription or history page.
- Keep extension-only policy (history mode, guest access, moderator deletion,
  retention) as channel/server properties outside the Mumble grant/deny bitmask,
  changed only by extension messages and requiring Write on the channel. If a real
  permission is ever needed, keep it in a separate extension ACL table that stock
  editors never round-trip.
- List the re-evaluation triggers (see D3).

### D2 — High: guests can read every public archive by default

Murmur's default permissions (`ACL.cpp`: `def = Traverse|Enter|Speak|Whisper|TextMessage|Listen`)
give Enter to `all`. Under D1's natural definition and the "history for new members:
yes" decision, any anonymous TLS client on a public server can page through every
public channel's history, kept forever by default. Today Mumble text is ephemeral:
you see only what is said while you are present.

**Fix:** add a server setting `history.guests = none | since_connect | full`,
defaulting to `none` (live only, as stock Mumble behaves). Registered users get the
decided default. Rate-limit history pages per IP prefix as well as per connection.
Announce the policy in the hello.

### D3 — High: revocation races

§5 re-checks "on every ACL change that affects the subscriber". Gaps:
1. **Fan-out vs revocation.** If fan-out runs on a snapshot taken before an ACL commit,
   a revoked subscriber can still receive events enqueued after the revocation.
2. **In-flight history.** §6 runs history "off the control path". A page queried
   before the revocation and finished after it is still sent.
3. **Triggers beyond ACL edits:** group membership; user moves (`in`/`out`/`sub`);
   token changes (a second `Authenticate` in the authenticated state, as Murmur
   handles); register/unregister (`auth`); channel reparenting and `inherit_acl`
   changes; channel deletion; kick/ban; Traverse denied higher up the tree.
4. **The same race in voice.** Listener proxies (Listen revoked) and whisper target
   caches. Murmur clears whisper caches on ACL changes and erases a departing session
   from every target (`Server.cpp` ~1736).

**Fix:** one task owns authorisation state, applies every change and bumps a
monotonic `authz_epoch`. Fan-out to per-connection queues happens in that task, so an
`Unsubscribed` push is ordered relative to `ChatEvent`s in the same queue. A history
page carries the epoch it was authorised at; before sending, the connection re-checks
`can_read` if the epoch moved, and drops the page if it fails. The voice router's
whisper and listener caches are keyed by epoch. Add the trigger list to §5.

### D4 — High: `legacy_html` (equivocation, archive gap, HTML policy)

- **Equivocation.** `content` and `legacy_html` come from the same sender
  independently, and the server cannot check that they match. A hostile Nuschel
  client can show Nuschel users and moderators a harmless message while stock users
  get a phishing link or the opposite statement. History keeps only `content`, so the
  version stock users saw cannot be reviewed or reported.
- **Injection into stock clients.** This is not a new capability: any stock client
  already sends arbitrary HTML through Murmur. Current stock clients load only
  `data:` images (`Log.cpp` `LogDocument::loadResource`) and filter link schemes
  (`Log::validHtml`; `mumble://`, http(s), ftp and others stay clickable). Older
  clients had rich-text resource bugs (fixed around 1.2.6). Stock clients print
  `Name: ` and then the HTML, so a message can fake further lines such as
  `<br>[12:01] Bob: …` and impersonate other users. The server must apply exactly
  Murmur's `isTextAllowed` policy to this path: `allowhtml=false` turns HTML into
  text; `textmessagelength` and `imagemessagelength` are measured in QString
  (UTF-16) units, with `<img src>` stripped for the text-length check. It must also
  share Murmur's rate limit.
- **Format confusion.** History mixes stock HTML (from stock senders) with Nuschel
  text. Without a stored `format` tag a client could interpret one as the other.

**Fix (recommended):** remove `legacy_html` from `ChatSend`. The server builds the
stock rendering from `content` with the existing pure functions `legacy_body` and
`text_html` (`crates/session/src/connection.rs` 2210–2300: escape, `<br/>`, linkify
http(s)/mumble/www), moved into a shared crate. That is not Markdown rendering and
needs no parser. For inline images the client supplies an attachment already sized
under `imagemessagelength`; the server checks magic bytes and size and wraps it as
`<img src="data:…">` without decoding it. Add `format: nuschel_text | mumble_html`
to every stored message. If `legacy_html` stays, store it with the message, show both
versions to moderators, and run it through the policy above.

### D5 — High: registration ids are reused

The extension keys DM ConversationIds ("per pair of registered users"), authorship,
read markers, idempotency and edit/delete rights on "Mumble's registration id".
Murmur assigns the **lowest unoccupied** id (`database/UserTable.cpp`
`getFreeUserID` → `getLowestUnoccupiedIDStatement`), so after an account is deleted
the next registrant gets its id. That user would inherit the old DMs and history, the
right to edit the old messages, and any `ChanACL.user_id` grants that were not
cascaded.

**Fix:** allocate ids monotonically and never reuse them, or key all extension data
on a 128-bit immutable account id with the u32 used only on the Mumble wire.
Unregistering cascades per-user ACL entries and group memberships, freezes the
account's DM conversations, and renders its history as "deleted user".

### D6 — Medium: edits and deletions have no authorisation rules

Proposed text:
- **Edit:** only the original author (same account id; unregistered authors cannot
  edit), only while they can still post (TextMessage) in that conversation, and the
  same conversation only (`reply_to` and attachments cannot change). Edits count
  against the text rate limit (otherwise an edit storm bypasses it) and have a
  per-message cap.
- **Delete:** by the author at any time, even after losing access (a privacy right),
  or by moderators with **Write** on the channel (a server property, per D1).
- **Purge:** an edit replaces the stored content. Earlier versions are not served
  and are purged per policy, because people edit to remove pasted secrets. A tombstone
  keeps `seq`, deletion time and (for moderators) a reason code; it drops content,
  attachments, `reply_to` previews and mentions. Attachments are released by
  reference count (D9).
- `reply_to` is a reference within the same conversation, never a copied excerpt,
  and is checked to be ≤ head.
- Stock clients never see edits or deletes, so a "removed" message stays on their
  screens and in their local logs. Say so in the UI.

### D7 — Medium: idempotency keys

- **Scope** is `(account, conversation, key)`. Unregistered users get no
  cross-reconnect idempotency (no durable principal); say so.
- **Conflicts:** store a hash of `(conversation, content, reply_to, attachments)`
  with the key. A retry with the same key and a different body returns
  `idempotency_conflict`, not the old `seq` (otherwise a buggy client loses messages
  silently).
- **Atomicity:** two devices of one user retrying at once must not both insert. Put
  the key on the message row with `UNIQUE(conversation, author, idempotency_key)` and
  `INSERT … ON CONFLICT DO NOTHING RETURNING seq`, not in a check-then-insert side
  table.
- **Growth:** storing the key on the row makes it proportional to messages (already
  rate-limited), and tombstones keep it. If a separate table is kept, cap it per
  account (for example the last 10,000 keys or 7 days, whichever is smaller).
- Never forward keys to other users. UUIDv7 contains the client's clock.

### D8 — Medium: negotiation edge cases

1. **Key the sticky policy by server address**, and later by the server identity
   key, **not by the leaf certificate**. CA-validated servers rotate every 60–90
   days; if rotation resets stickiness, a rotation (or a misissued certificate) is a
   downgrade. A pin change keeps the "requires nuschel/1 (+caps)" record.
2. **Mismatch fails closed:** extension iff ALPN = `nuschel/1` **and** both hellos
   are present. ALPN `nuschel/1` without a hello, or a hello without ALPN, is a
   protocol error, never stock mode. A Nuschel client ignores field 7200 and any
   ChannelState extension field unless ALPN selected `nuschel/1`; a server ignores
   field 7200 from a client that did not offer ALPN.
3. **Order of credentials.** Stock clients send `Authenticate` (password, tokens)
   straight after `Version`. A Nuschel client with a sticky record checks ALPN
   (available right after the handshake) and, when sticky capabilities exist, waits
   for the server's `Version` **before** sending `Authenticate`. A strict server sends
   `Reject` and closes without parsing or logging the stock client's `Authenticate`.
4. **Frozen capabilities:** the hello is accepted once, before `Authenticate`; the
   capability set is fixed for the connection. Later `Version` messages cannot change
   it.
5. **Hello bounds:** at most 32 capabilities, 64 bytes each, ASCII
   `[a-z0-9-]+/[0-9]+`; unknown capabilities ignored.
6. **Reserved range before negotiation:** a frame in 0x4E00–0x4EFF before the
   capabilities are fixed, or for a capability not negotiated, means disconnect
   (before authentication) or `NuschelError{unsupported}` (after), and is never
   processed.
7. **First contact:** let invites and `mumble://` links carry the certificate or
   identity-key fingerprint and the required policy
   (`?fp=sha256:…&policy=nuschel`). That closes first contact for invited users
   without waiting for key transparency.
8. **Minimum versions** are self-reported and advisory. They are not a security
   control (a client can omit ALPN or lie).
9. **TLS knobs** that affect the downgrade argument: never enable 0-RTT; disable
   server session resumption (see R-TLS). Prefer TLS 1.3: in 1.2 the client
   certificate, a stable cross-server identity, crosses the wire in cleartext.

### D9 — Medium: files (`files/1`)

- **A content hash is not a capability.** If blobs are global and referenced by
  hash, anyone who learns a hash, or holds a known file, can reference it in a channel
  they can read and download it. Authorise downloads by "can read a message that
  references this upload", and let `ChatSend` reference only uploads made by the
  sender (`upload_id` owned by the account).
- **No cross-user deduplication signals:** "upload finished instantly" is a
  confirmation-of-file oracle. Deduplicate per uploader at most. Never use convergent
  encryption for E2EE blobs.
- Reserve quota at upload start (declared size, per account, per conversation and
  global). Validate chunk offsets, verify the hash at completion, expire and garbage-
  collect partial uploads, and reference-count so deletion and retention remove the
  blob.
- Clients never trust a server-supplied MIME type for rendering. Relay-only transfer
  needs per-user bandwidth caps and is allowed only between members of the
  conversation.

### D10 — Medium: consent and privacy on mixed servers

Stock users cannot know their channel messages and DMs (to registered Nuschel users)
are now archived forever and readable by people who join later, or that **invisible
subscribers** receive them. Mumble shows who is in a channel and who listens. That is
a privacy regression and, with "forever" as the default, a GDPR exposure for EU
operators.

**Fix:** on mixed servers tell stock clients (welcome text, a server message on
entering an archived channel, or the channel description) that text is retained and
for how long. Make retention visible per channel. Consider not archiving DMs from
stock senders by default. Provide operator tools to erase a user's messages.

### D11 — Medium: "only since joining" and paging metadata

- Define visibility as the **set of intervals** in which the user had `can_read`, not
  "first join". With first-join semantics, a user removed for misconduct reads
  everything said while they were removed once they come back.
- Clip every anchor, including `around(seq)`, to visible intervals.
  `has_more_before` is false at the window edge, plus a `restricted` flag;
  everything outside the window is indistinguishable from nonexistent.
- Accept and document that `seq` reveals how many messages came before the window.

### D12 — Medium: conversation lifecycle and stock fan-out shapes

- Temporary channels: no persistent history by default, or history deleted with the
  channel. A deleted permanent channel's history becomes admin-only, because ACLs no
  longer exist to authorise it. Reparenting changes inheritance (D3).
- Stock `TextMessage` with many `channel_id`/`tree_id` entries: deduplicate before
  traversal (Murmur walks the tree once per listed id, so repeated `tree_id` costs
  O(ids × channels)) and cap the ids per message. Define which conversations a tree
  message is stored in (each reached channel the sender may message) and that DMs to
  unregistered sessions are not stored.

### D13 — Medium: limits for extension messages

- `ChatSend` and edits share Murmur's text bucket with `TextMessage`
  (`messagelimit`/`messageburst`, default 1/s burst 5); otherwise Nuschel clients get
  a second allowance.
- Separate buckets: `Subscribe`/`Unsubscribe`, `HistoryRequest` (pages and bytes per
  minute), `Typing` (1 per 3 s per conversation), `ReadMarker`, upload chunks.
- At most 16 outstanding `request_id`s per connection; reject a duplicate in-flight
  id.
- Uniform errors: "no such conversation" and "not allowed" return the same code
  (no existence oracle for DM ids). `NuschelError.detail` is a fixed string, never
  SQL or path text, and clients show it as plain text.
- A wire size cap and an element-count cap per message type, enforced before decoding
  (C4).

### Lower-severity notes

- **Read markers (Low):** accept only for readable conversations, with
  `seq ≤ head`, monotonic; bounded per account. In E2EE conversations sync them as an
  encrypted per-user blob or MLS self-message, so the server stops learning reading
  times.
- **Typing (Low):** subscribers are invisible, so lurkers see who types. The
  TODO.md plan to use `PluginDataTransmission` gets no channel authorisation: Murmur
  forwards plugin data to any session listed (`Messages.cpp` ~2546). Use it only for
  cosmetic hints.
- **Identifiers (Low):** state that `ConversationId`, `seq` and cursors are
  identifiers, never capabilities. `docs/e2ee.md` is referenced but exists in no
  workspace.
- **E2EE ordering (forward-looking, decide now):** a server-assigned `seq` lets a
  malicious server reorder, withhold or fork history per member. Reserve a per-sender
  counter or previous-hash field **inside** the ciphertext now (an identifier
  decision §3 calls hard to change).
- **UDP crypto (forward-looking):** OCB2 with a 24-bit tag is kept only for stock
  compatibility. Add a `udp-aead/1` capability for Nuschel↔Nuschel voice (an AEAD
  with a 128-bit tag and an explicit counter) if Mumble-transport voice outlives the
  WebRTC plan.

---

## 2. Threat model and requirements for the phase-1 server

**Assets:** account credentials (passwords, certificate-hash bindings, tokens),
channel passwords (tokens), voice and text confidentiality, the history archive, IP
addresses, server availability, the server's TLS key.

**Adversaries:** an anonymous Internet peer (TCP, UDP, spoofed UDP); an authenticated
guest; a registered user; a moderator abusing rights; a co-tenant behind the same NAT
or CGNAT; an on-path attacker; a malicious stock or Nuschel client.

Priorities: **P0** before any listener binds beyond loopback; **P1** before the
phase-1 release; **P2** hardening.

### P0

1. **R1 — client certificates (rustls).** Implement `ClientCertVerifier` with
   `offer_client_auth = true` and `client_auth_mandatory = false` (Mumble allows no
   certificate). `verify_client_cert` accepts any well-formed chain, but
   `verify_tls12_signature`/`verify_tls13_signature` **must** delegate to
   `rustls::crypto::verify_tls1{2,3}_signature` with the provider's algorithms.
   Returning `HandshakeSignatureValid::assertion()` unconditionally lets anyone who
   holds a victim's certificate (seen in cleartext in TLS 1.2) become them. Identity
   = SHA-1 of the leaf DER (Mumble's `qsHash`), read only after the handshake. Test:
   a client presenting another user's certificate without the key must fail.
2. **Connection state machine:**
   `TlsHandshake → AwaitVersion → AwaitAuthenticate → Authenticating → Authenticated → Closing`,
   with a dispatch table of (state, type) that rejects by default. Handlers for
   authenticated messages take an `&Authenticated` principal type, so a pre-auth path
   cannot reach them (Murmur's `MSG_SETUP` pattern, enforced by types).
3. **Frame limits from the 6-byte header, before buffering or decoding:**
   - before authentication: `Version` ≤ 4 KiB, `Authenticate` ≤ 16 KiB, `Ping` ≤ 256 B;
     any other type disconnects
   - after authentication, per type: `TextMessage` ≈ `imagemessagelength` + 4 KiB,
     `UserState` ≈ texture + comment limits, `ACL` ≤ 512 KiB, `VoiceTarget` ≤ 8 KiB,
     `PluginDataTransmission` ≤ 2 KiB, `UDPTunnel` ≤ 1 KiB
   - after decoding: caps on element counts (tokens, sessions, channel ids, ACL
     entries, targets)
   - never emit a frame larger than 0x7fffff (stock clients reject anything bigger)
4. **Bounded output:** per-connection queues bounded in **bytes**, with a control
   lane and a bulk lane (blobs, history). If the control lane overflows (slow reader),
   disconnect; if voice overflows, drop. Broadcasts share encoded `Bytes`. A
   `RequestBlob` for every session followed by never reading must not grow memory.
5. **Before authentication:**
   - a global semaphore on concurrent TLS handshakes; per-IP (/32, /64) connection
     caps applied before TLS
   - handshake timeout of about 10 s; `Authenticate` within about 15 s; idle timeout
     30 s (Murmur `timeout`)
   - a global `users` cap; Murmur-style autoban (10 attempts in 120 s → 300 s)
   - the accept loop backs off on `EMFILE`/`ENFILE` instead of spinning
6. **Secrets never logged:** no passwords, tokens, crypt keys, message bodies or
   comments in logs or `Debug`. Redact the prost types (C1). Logged IPs can be
   obfuscated (Murmur `obfuscate`).
7. **Passwords:**
   - argon2id with OWASP parameters (m = 19 MiB, t = 2, p = 1) on the blocking pool,
     behind a semaphore of 1–2 with a bounded wait queue; reject when full (100
     concurrent hashes = 1.9 GiB)
   - constant-time verification (the `password-hash` verify); server password and
     tokens compared as SHA-256 digests with `subtle`
   - Murmur import: verify PBKDF2 or legacy unsalted SHA-1, rehash to argon2id on
     success, flag SHA-1 accounts
8. **The authorisation engine is Murmur-exact**, including what the task brief
   called "deny-overrides" and Murmur does not implement:
   - **ordered evaluation**: root to leaf, entries in order, `granted |= allow;
     granted &= ~deny`, so a later allow overrides an earlier deny
   - `inherit_acl` resets to the defaults; Traverse/Write abort rule; root-only Kick,
     Ban, Register, SelfRegister and ResetUserContent from `apply_here` at root;
     Write implies the listed permissions
   - SuperUser = All except Speak and Whisper, and cannot be targeted by others
   - groups: `all auth strong in out none sub,a,b,c`, prefixes `~ ! # $`,
     inherit/inheritable, add/remove members
   - tokens match case-insensitively
   - a cache invalidated by `authz_epoch` (D3)
   - a pure crate with differential tests against `src/ACL.cpp` and `src/Group.cpp`
     cases
9. **Every message type checked** (Murmur's rules, plus caps):

| Message | Required |
|---|---|
| ChannelState create | MakeChannel/MakeTempChannel on parent; `channelcountlimit`, `channelnestinglimit`, name regex, unique sibling name, no children under temporary channels; creator gets Write (registered only) |
| ChannelState edit/move | Write on channel (not root for name); move: Write on channel + Make\* on new parent, **no cycles** (new parent not a descendant), nesting rechecked; description through `isTextAllowed` |
| Links | LinkChannel on the channel and on each added target; removal needs only the channel |
| ChannelRemove | Write, not root; users moved to parent; history per D12 |
| UserState (self) | text bucket; listener limits (`listenersperchannel`, `listenersperuser`); comment and texture limits; plugin context/identity size caps |
| UserState (other) | names never changeable; move: Move on source **and** (Move or target's Enter) on destination, `usersperchannel`; mute/deaf/priority: MuteDeafen in target's channel (first non-temporary parent for temporary channels), `suppress` never client-settable; reset comment/texture: ResetUserContent on root, clearing only; register: Register/SelfRegister on root and the target has a certificate; self-only fields ignored for others; SuperUser untouchable |
| UserRemove | Kick/Ban on root; ban entries with IP/mask/hash/duration |
| TextMessage | text bucket; TextMessage on each channel, each tree node (deduplicated, capped) and each target user's channel; `isTextAllowed` |
| VoiceTarget | id 1–30; deduplicated; ≤ 512 sessions/channels per target; own bucket; Whisper checked per resolved channel when the cache is built; cache invalidated by epoch and on disconnect |
| RequestBlob | ≤ 64 ids, own bucket, counted against the output budget |
| QueryUsers | Write on some channel (Murmur), ≤ 256 ids/names, bucket |
| UserList | Register on root (Murmur) |
| BanList | Ban on root |
| ACL query/set | Write on channel; replies contain `#token` secrets |
| UserStats | address and certificates only to self or root Ban; local stats to the same channel or Enter |
| PermissionQuery | bucket (each one evaluates the ACL) |
| Authenticate (re-sent) | token update: ≤ 32 tokens of ≤ 128 bytes, own bucket. One message with thousands of tokens is otherwise a channel-password oracle via `can_enter`/PermissionQuery |
| ContextAction | ignored unless the server registered that action |
| PluginDataTransmission | 4/s burst 15 (Murmur), data ≤ 1,000 bytes, data id length cap, receivers deduplicated and capped; sender set by server |
| CryptSetup | client nonce sets decrypt IV; empty → reply with encrypt IV; bucket |
| Unknown types | ignored (Mumble); reserved 0x4E00–0x4EFF per D8.6 |

10. **UDP voice:**
    - drop datagrams over 1,024 bytes or under 5 bytes before any work
    - key and nonces from the OS CSPRNG, fresh per connection, never reused
    - bind (canonical IP, port) → session; canonicalise IPv4-mapped IPv6 so TCP and
      UDP addresses compare equal
    - unknown sources: trial-decrypt only against authenticated sessions **whose TCP
      peer IP equals the source IP** (Murmur's `qhHostUsers`); pre-filter on the IV
      byte window, at most 8 trials per packet, a per-source budget for unknown-peer
      packets
    - rebind only from the TCP IP
    - resync requests at most every 5 s (`tLastGood`/`tLastRequest` as in Murmur);
      count decrypt failures and rate-limit them per session (24-bit tag: one forgery
      per ~2²⁴ tries with spoofing)
    - the client-supplied sender session, context and volume are always ignored
    - session ids are **never reused** (monotonic u32). Murmur recycles a FIFO pool of
      2 × `users` and has to erase stale ids from every whisper target on disconnect;
      in-flight kicks and whispers also hit the wrong person
11. **Unencrypted ping:** reply ≤ 2× request (C3); per-prefix and global token
    buckets; `allow_ping` and an option to hide user counts.
12. **Persistence:**
    - SQLx bound parameters only, no string-built SQL
    - database file 0600 and state directory 0700 (CVE-2012-0863: Murmur's database
      was world-readable)
    - secrets only through `LoadCredential`; module assertions reject paths under
      `/nix/store`
    - any future server, SuperUser or registration password options are `*File`
      options only

### P1

13. **Authentication semantics:**
    - registered names are reserved
    - a password for a registered name without a password fails; SuperUser
      authenticates by password only
    - **do not** overwrite the stored certificate hash when a password logs in from a
      new certificate (Murmur does, `Server.cpp` ~2838–2842, so a leaked password takes the
      account over permanently): keep several certificates per account, or require
      confirmation, and notify
    - no email-based login with CA-verified certificates by default (Murmur's
      `findRegisteredUserByEmail` trusts any CA email certificate)
    - failed logins rate-limited per IP prefix and per account
    - network SuperUser login off by default (admin CLI instead)
    - the Reject type already reveals whether a name is registered, so timing
      equalisation buys nothing
14. **Names and impersonation:**
    - NFC plus case folding for uniqueness; a UTS #39 confusable skeleton across
      online users and registered names; no control or bidi characters; length caps;
      Murmur's regex as an extra filter
    - **ghost kicking:** Murmur lets an unregistered connection with the same name
      kick an existing session whenever the IP matches, whatever the certificate
      (`Messages.cpp` ~268–276), which is impersonation across CGNAT and
      universities. Require the same certificate hash (or both without certificate and
      the same IP)
15. **Channel and resource limits:** count, nesting, users per channel, ≤ 3
    temporary channels per user with a creation bucket, listeners, comment and texture
    sizes (`textmessagelength`, `imagemessagelength`), blob cache by use.
16. **Voice:**
    - per-user bandwidth limit (`bandwidth`) on UDP and the tunnel before fan-out
    - mute, suppress and self-mute stop routing; Speak in the own channel turns into
      `suppress`, recomputed on moves and ACL changes; linked channels need Speak
    - positional data only to receivers with the same plugin context
    - receivers deduplicated with Murmur's context precedence
    - a global egress budget for shout fan-out
17. **TLS:**
    - rustls with the ring provider, TLS 1.2 and 1.3 (configurable minimum 1.3)
    - session tickets and the stateful cache disabled (`send_tls13_tickets = 0`,
      `NoServerSessionStorage`); 0-RTT never
    - generated server key: ECDSA P-256 via rcgen (or RSA-2048), written once,
      atomically, 0600; **never regenerated silently** (fail to start instead);
      reload on SIGHUP for ACME
    - `strong` is false unless a CA store is configured
18. **Logging and privacy:**
    - connect logs keep session id, (obfuscated) IP and certificate-hash prefix
    - no contents, comments or channel passwords in logs
    - journald retention documented; ban-list IPs expire
    - backups contain password hashes, history and IPs: encrypt them; retention and
      erasure must reach backups (document)
19. **Hostile input:**
    - prost recursion limit (default 100); after decoding, every repeated field
      length-checked
    - `clippy::unwrap_used`, `expect_used`, `indexing_slicing` and
      `arithmetic_side_effects` in server crates
    - libFuzzer targets for framing, every `decode`, `relay::parse/encode` and crypt
    - one bad message closes only its own connection, never shared state (no locks
      held across `.await` that a panic can poison)

### P2

20. **NixOS module** (`nix/module.nix`):
    - add `SystemCallFilter = [ "@system-service" "~@privileged" "~@resources" ]` and
      `SystemCallArchitectures = "native"`
    - add `ProtectClock`, `ProtectHostname`, `ProtectKernelLogs`, `RestrictRealtime`
      and `PrivateIPC`
    - add `SocketBindAllow = [ "tcp:${port}" "udp:${port}" ]` with
      `SocketBindDeny = "any"`
    - add `LimitNOFILE = 65536`, `MemoryMax`/`MemoryHigh` and `TasksMax`
    - `AmbientCapabilities = CAP_NET_BIND_SERVICE` only when the port is below 1024
      (64738 needs none, and `CapabilityBoundingSet = ""` is right)
    - default `listenAddress` to `::` (dual-stack) or a list (today IPv4 only)
    - assertions that `tls.keyFile` and `database.urlFile` are not under `/nix/store`
    - outbound traffic is unrestricted (a compromised server can connect out); an
      nftables cgroup egress rule allowing only the database is optional
    - credentials copied by `LoadCredential` need a restart on ACME renewal
21. Separate storage priority lanes: authentication and registration lookups must
    not queue behind history pages. Keep channels, ACLs and groups in memory, as
    Murmur does.
22. Admin CLI over a local socket only (no network RPC by default; Ice/gRPC were
    Murmur attack surface).

---

## 3. In-progress implementation (`server@`, `noxnyusu`)

The change adds `relay.rs` (zero-copy voice parse and per-recipient encode, info
pings), `encrypt_into`/`decrypt_into` in `crypt.rs`, message-type constants and
`encode_control`/`encode_tunnel` in `lib.rs`. `apps/server/src/main.rs` still
refuses to listen. There is no connection, state or routing code yet, so the
structural review concerns what comes next.

### Verified correct
- `relay::encode` takes the sender from `Delivery`, i.e. the server session.
  `parse_protobuf_audio` skips client fields 2 (context), 3 (sender_session) and
  7 (volume) as unknown, so session spoofing is impossible by construction.
- `decrypt_into` restores the IV on every failure and updates stats only on success,
  so trial decryption against several candidate sessions does not corrupt state.
- Both XEX\* countermeasures (the encrypt-side bit flip and the decrypt-side forged
  final block check) match stock `CryptStateOCB2.cpp`. This matters on the server: by
  relaying, the server becomes a chosen-plaintext encryption oracle under every
  recipient's key, which is the precondition the encrypt-side flip defeats.
- `decode_control` rejects an oversized header without waiting for the body. Stock
  `Connection.cpp` (128–158) checks `> 0x7fffff` only after the full advertised body
  has arrived and sets no read-buffer limit.
- Legacy CELT and Speex are rejected; the Opus size is capped; output is capped at
  1,020 bytes; the hostile-input test covers parse and encode.

### C1 — Medium: secrets in `Debug` (fix before the first handler logs anything)
`crates/protocol/build.rs` uses the default `prost_build::Config`, so `Authenticate`
(`password`, `tokens`), `CryptSetup` (`key`, nonces), `UserState`
(`temporary_access_tokens`) and `ACL` (group names `#token`) derive `Debug`. One
`tracing::debug!(?msg)` writes passwords and voice keys to the journal.

**Fix:** `.skip_debug([...])` for these types with hand-written redacting `Debug`
impls, plus a test asserting the redaction.

### C2 — Medium: `CryptState::new` depends on argument order by role
The diff documents that "a server passes them the other way round". Swapped
arguments make the server encrypt with the client's nonce, so both directions use the
same (key, nonce) sequence (OCB2 nonce reuse) until someone notices voice failing.

**Fix:** use `CryptState::client(key, client_nonce, server_nonce)` and
`CryptState::server(key, client_nonce, server_nonce)` (same argument order, role in
the name), and add `CryptState::generate_server(rng) -> (CryptState, CryptSetupParts)`
so the triple sent and the state kept cannot diverge. Make `new` private.

### C3 — Medium: `answer_info_ping` reflection (fix before merge)
A protobuf request of three bytes (`01 10 01`: type, then
`request_extended_information = true`) gets about 17 bytes back (type, version
varint, users, max users, bandwidth), roughly 6x. The legacy request is 12 → 24 (2x).
Stock Murmur behaves the same, but nothing forces Nuschel to.

**Fix:** do not answer protobuf pings shorter than 12 bytes. Stock 1.5 sends a
timestamp XORed with `rand64 << 32` (`ConnectDialog.cpp` ~1797), so its requests are
almost always 12–14 bytes; the check drops about 1 in 16,000 stock pings, and the
browser retries. The function or its caller must also apply per-prefix and global
token buckets; take a limiter argument so it cannot be forgotten. Add `allow_ping`.

### C4 — High before the first handler: frame and decode limits
`decode_control` has one 8 MiB limit (0x800000, one byte above the stock 0x7fffff)
for every state and type. Protobuf decoding amplifies:
- an empty repeated `ACL.ChanACL` is 2 wire bytes and about 64 bytes decoded; an
  8 MiB ACL frame becomes about 4M entries and several hundred MB with `Vec` growth
- packed `u32` lists grow 4x; `VoiceTarget.Target` about 40x

Decoding before the state and permission check lets an unauthenticated peer trigger
this.

**Fix:** `decode_control(buffer, max_body)` with `max_body(state, kind)` from P0-3;
decode only types allowed in the current state; check counts after decoding.
`encode_control` should check `encoded_len() ≤ 0x7fffff` instead of
`length as u32`.

### C5 — Low
- Tag and forged-block comparisons exit early. That is not observable remotely today
  (failed packets get no reply), but `subtle::ConstantTimeEq` on 3 + 15 bytes is free.
- `Delivery.positional` must document that the caller checks equality of plugin
  context; positional data reveals in-game location.
- `encode` returning `None` (a legacy → protobuf conversion over 1,020 bytes) drops
  silently for that receiver; count it.
- Fuzz targets: only `fuzz_targets/html.rs` exists. Add framing plus prost decode,
  `relay::parse`/`encode` round trip, and `CryptState::decrypt_into`.

### Structural decisions to make before more server code lands
1. **State and authorisation:** one task owns channels, users, ACLs, groups,
   subscriptions and `authz_epoch` (D3). Connection tasks do TLS, framing, per-type
   caps, buckets and decoding, then send typed commands. Every permission check lives
   in one `authorize(actor, action, target)` module over the pure ACL engine, never
   inline in handlers.
2. **Voice:** a router task owns the UDP sockets, the address → session map, crypt
   states and an **immutable routing snapshot** (membership, mute/suppress, listeners,
   links with Speak, resolved whisper targets) published by the state task through
   `ArcSwap` with the epoch. That way the hot path takes no locks and cannot use
   authorisation older than one snapshot. `relay.rs` fits this well.
3. **Backpressure:** per-connection byte-bounded queues (P0-4); history and blobs in
   the bulk lane; the storage worker's IPC multiplexed with a global concurrency limit
   and an authentication priority lane.
4. **UDP binding and session ids** as in P0-10; monotonic session ids from the start
   (changing this later breaks client assumptions).
5. **Capabilities per connection:** broadcasts need two encodings (stock, and
   Nuschel with extension fields). Design `encode_control` callers for that instead of
   one shared frame.
6. **TLS verifier** (R1) with a negative test, before any authentication code.
7. `main.rs`: when no certificate is passed, generate and persist one (P1-17);
   `--check-config` should validate credential locations and permissions, not only
   the port.

### Must change before this change merges
- C3 (reflection guard on `answer_info_ping`).
- C2 (role-explicit crypt constructors; the new doc comment shows the trap is known).
- C4's API part: a caller-supplied limit on `decode_control`, and a checked length in
  the newly added `encode_control`.

Before the first network handler: C1, R1, the P0 frame limits and fuzz targets.

---

## 4. Proposed changes to `docs/extension.md`

1. **§1:**
   - sticky policy keyed by server address (and later identity key), surviving
     certificate rotation
   - fail closed on ALPN/hello mismatch; ignore extension fields without ALPN
   - client waits for the server `Version` before `Authenticate` when sticky
     capabilities exist; a strict server does not parse the stock `Authenticate`
   - hello accepted once, then frozen; hello bounds
   - invites carry fingerprint and policy
   - minimum version is advisory
   - no 0-RTT, no resumption, TLS 1.3 preferred (the client certificate is cleartext
     in TLS 1.2)
2. **§2:** reserved types before negotiation → disconnect; unnegotiated capability
   → error, not processed; ≤ 16 in-flight `request_id`s; fixed error strings; one
   error for missing and forbidden.
3. **§3:**
   - account ids never reused (or a 128-bit account id)
   - "ConversationId, seq and cursors are identifiers, not capabilities"
   - lifecycle of temporary and deleted channels' conversations
   - reserve a per-sender counter/prev-hash inside E2EE payloads
4. **§3 idempotency:** scope `(account, conversation, key)`; conflict on a different
   body; a unique constraint on the message row; unregistered users excluded; keys
   never forwarded.
5. **§4:**
   - replace client `legacy_html` with server rendering through the shared
     `legacy_body`/`text_html`, with a pre-sized image attachment for stock clients
   - a `format` field on stored messages
   - ChatSend and edits share Murmur's text bucket; Murmur's `isTextAllowed` applies to
     stock output
6. **§5:**
   - define `can_read` (Enter ∨ in channel ∨ listening; persistent tokens only)
   - list the re-evaluation triggers
   - single-writer `authz_epoch`; re-check pages at send time
   - subscriptions bounded per connection and per account
7. **§6:**
   - guest history setting (default none)
   - "since joining" as visibility intervals; clip all anchors; `restricted` flag
   - edit and delete rules (D6); purge semantics; `reply_to` same conversation
   - rate limits on pages and bytes
8. **§7:** validation and bounds for read markers; encrypted marker sync in E2EE;
   typing visible to invisible subscribers; plugin data is never authorisation.
9. **§8:** upload ids instead of hash capabilities; no cross-user deduplication;
   quota reservation; reference counting; MIME not trusted.
10. **New "Mixed servers and stock users" section:** stock users are told about
    retention; DMs from stock senders (not archived by default); erasure tools.
11. Fix the dangling `docs/e2ee.md` reference.
