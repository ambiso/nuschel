# Message interactions: replies, reactions, mentions and threads

Planning document, 2026-11-13, written against main `83de0df7` (line references are
at that revision). Revised the same day after the owner widened threads beyond E2EE.
Nothing here is implemented, and no source, manifest or graph file was edited for
this plan. It covers four user-requested issues as one design:

- [#191](https://github.com/ambiso/nuschl/issues/191) Reply to individual chat messages
  (absorbs the `reply_to` item of [#117](https://github.com/ambiso/nuschl/issues/117));
- [#195](https://github.com/ambiso/nuschl/issues/195) React to chat messages with emoji
  (not restricted to E2EE);
- [#189](https://github.com/ambiso/nuschl/issues/189) Mention users and roles in chat;
- [#192](https://github.com/ambiso/nuschl/issues/192) Threads. The issue title still
  says "exclusively E2EE"; the owner changed the scope on 2026-11-13 to compatible
  Nuschl servers and stock servers as well (comments on #192 and #191). The integrator
  updates the title.

Each issue says that protocol, storage and MLS changes need design review before
implementation. This document is that design input. It grants no approval.

**Owner guidance (2026-11-13, relayed by the coordinator), binding for this plan:**

> "For MLS/nuschl enabled servers all of this can ideally be more structured, and not
> recovered from plain text messages; the text message format only serves to be nice
> to look at for legacy mumble users. We can also use the PluginDataTransmission if
> it's cleaner - then presentation and parsing don't have to interfere."

What follows from it:

1. On Nuschl servers (stored conversations) and in MLS conversations, every
   interaction is **structured protocol data**: typed messages and fields with stable
   references. Nothing is parsed back out of display text.
2. On stock Mumble servers, the visible chat text is **presentation only** for legacy
   users: a quoted reply snippet, an `@name`, perhaps a reaction line. Nuschl clients
   never derive semantics from it.
3. Between Nuschl clients on a stock Mumble server, the structure travels separately in
   `PluginDataTransmission` (Mumble message 26) under a reserved `nuschl/` data-ID
   namespace. Generic bounded support for message 26 belongs to another lane (PD, §9).
   Section 2.5 defines correlation, partial delivery, ordering, limits and visibility.

**Owner guidance on threads (2026-11-13, on #192; "Threads no longer e2ee only" on
#191), binding for this plan:**

> "Given that we can send arbitrary plugin data now we can also implement threads for
> mumble or compatible servers. We just in general need to make sure that we don't
> also display the visible mumble message that legacy mumble clients get? We could
> e.g. generate the mumble message with a deterministic function - roughly
> `f(plugindata)` and then ignore that generated message once."

What follows from it:

4. Threads exist on all three paths, with one reference scheme (§2.2) and one set of
   rules (§6.2). Only the carrier differs: typed fields on Nuschl servers, the MLS
   ciphertext in E2EE, annotations on stock servers.
5. On stock servers the generated part of the visible text is `f(annotation)`: a
   thread line and a reply quote. Nuschl clients skip exactly that part, once, and show
   the body from the `TextMessage`. Section 2.5 explains why the body stays there.

Where plain text is still read at all, §2.9 lists it. No feature in this plan derives
meaning from it.

## 1. What exists today (at 83de0df7)

### 1.1 Message identity, edits, deletion and history

| What | Where | Notes |
|---|---|---|
| Server message identity | `docs/extension.md:136-172`; `crates/protocol/proto/nuschl.proto:184-215` | Per-conversation `seq` (u64 from 1), assigned under the state lock and never reused (void rows keep retried seqs). `ConversationId` is 16 random bytes and never reused. Identifiers are not capabilities |
| Author identity | `nuschl.proto:175-182` (`Author { account, user_id, name }`) | The immutable 128-bit account id. The registration number is a wire alias, and nuschld never reuses it (`crates/ipc/src/server.rs`, `ServerSnapshot.next_user_id`). `name` is a snapshot. Unregistered users have no account |
| `reply_to` on the wire | `ChatSend.reply_to = 6` (`nuschl.proto:218-227`), `Message.reply_to = 7` | "A `seq` in the same conversation, at most its head". The design says it is never a copied excerpt (`docs/extension.md:180-182`) |
| Server handling of `reply_to` | `apps/server/src/chat.rs:344-410` | Parent validation for structured replies is specified in §3.2. `body_hash` covers it (`:52-59`), `wire_message` echoes it (`:100`), tombstones clear it (`crates/storage/src/server/history.rs:329, 460, 487`; `chat.rs:878`; `apps/server/src/storage.rs:567`; `admin.rs:723`) |
| Client handling of `reply_to` | `crates/session/src/connection.rs:3152-3190` | The session always sends `reply_to: 0` (`:3177`). The SDK, timeline, local cache and every UI ignore it |
| Edits and deletions | `chat.rs:412-530`; `docs/extension.md:264-279, 541-566` | Edits by the author's account only, at most `Policy.edit_limit` times, while the author may still post, never in live-only conversations. They replace the content, and old versions are purged. Deletions are by the author at any time, or by a moderator (`Policy.moderators_delete` plus Write). Both are rows with their own `seq` and refer to `target`. `reply_to` and attachments cannot change |
| Revision | `Message.edits`, `edited_ms` (`nuschl.proto:184-215`); `timeline::Message.revision` (`crates/client/src/timeline.rs:96-128`) | The server counts edits. The SDK's `revision` is a local repaint counter, not a wire value |
| Server rows | `crates/storage/server-migrations/{sqlite,postgres}/0002_history.sql:5-28`; `StoredMessage` (`crates/ipc/src/server.rs:135-162`) | One row per `seq`: message, edit, delete or void. Writers: `history_append`/`_edit`/`_delete` (`crates/storage/src/server/history.rs:203, 243, 294`). Erase, forget, expire and purge are at `:457, 415, 478, 343`. `expirable` is at `ipc/src/server.rs:247` |
| Rate limits | `chat.rs:182` (`text_limited`); `apps/server/src/auth.rs:782`; `apps/server/src/config.rs:446-450, 547-549` | Sends and edits share Murmur's text bucket (1/s, burst 5). Plugin data has its own bucket (4/s, burst 15) |
| Authorisation | `apps/server/src/authz.rs:211-271` (`ReadHistory`, `Send`, `Edit`, `DeleteOwn`, `Moderate`, `Typing`, `MlsSend`); `can_read` `:930`; `can_write` `:984` | A capability is not a permission. "No such conversation" and "not allowed" are one `NOT_FOUND` |
| ACL groups (the basis for "roles") | `apps/server/src/authz/acl.rs:93-98` (`Group { inherit, inheritable, add, remove }`), `group_members` `:393-418`, meta groups `:296-301`; `authz.rs:726` (`in_group`, for whisper targets: "selects receivers; it is not a permission") | Named groups per channel with inheritance, membership by registration number. Meta groups (`all`, `auth`, `in`, `out`, `sub`, `~`, `$hash`, `#token`) depend on live state. Clients cannot see group membership without ACL access |
| Unread counts | `crates/client/src/client.rs:2199, 2664` | `head − read`; the exact count this plan needs is in §6.3 |
| Local history | `crates/storage/migrations/{sqlite,postgres}/0001_history.sql`, `0004_server_messages.sql`; `ChatMessage`/`HistoryEntry`/`ServerRecord` (`crates/core/src/lib.rs:29-66`) | `author` is a display name. Only `seq`, `edited_ms` and `deleted_ms` are stored, keyed by `(server, channel, seq)`. There is no account, reference, reaction or thread column |
| SDK timeline | `timeline.rs:17-36` (`IncomingMessage`), `:96-128` (`Message`), `MAX_TIMELINE = 1000` (`:14`) | `seq`, `author_id` (a registration number), `serial` (UI identity, never reused), `revision`. No account, reply, reaction, mention or thread fields |
| Server pages to the timeline | `crates/client/src/native.rs:96-104` | Only `kind == MESSAGE` rows that are not MLS are kept |
| SDK edit/delete API | `client.rs:1318-1380` | Named by `seq`, refused for MLS servers ("Encrypted edits and deletions are not implemented yet.") |
| Async binding | `crates/client/src/input.rs:23+` (inputs carry the connection `generation`); `crates/client/src/navigation.rs:34-69` (generation-tagged history tickets) | The precedent this plan reuses |

### 1.2 E2EE message format

| What | Where | Notes |
|---|---|---|
| Envelope | `crates/e2ee/src/envelope.rs:1-24` (format), `:33-76` (kinds); `docs/e2ee.md:908-947` | Kinds: Chat 1, **Edit 2, Delete 3, Reaction 4** (reserved, unimplemented), File 5, Typing 6, HistoryKey 7, HistoryGrant 8, KtHead 9, Generation 10. A per-device counter, a `prev` chain and `seen` let receivers detect withholding and forks. Body ≤ 64 KiB + 1 KiB |
| Chat body | `apps/crypto-worker/src/mls.rs:2024-2040` (send), `:2007-2020` (receive) | Raw UTF-8 text, with no structure. Any other durable kind returns `Received::Nothing` |
| Archive records | `mls.rs:2265-2279` (only for Chat), `:2194-2200` (an archived envelope must be Chat) | Late joiners read history only through these |
| Unknown kind | `mls.rs:1837-1840` | Decode fails, so the message is shown as `undecryptable("malformed envelope …")` and the sender's chain records a gap. **An older client therefore shows a false "withheld" notice for any new kind.** New kinds need an authenticated group gate (§2.6) |
| Unsigned metadata stripped | `crates/session/src/mls.rs:86` (`reconstructed_archive_chat`); test `crates/client/src/native.rs:3046-3077` | The server's `reply_to`, `target` and `kind` are dropped from MLS rows. **E2EE replies must live inside the ciphertext** |
| Group binding and capabilities | `crates/e2ee/src/binding.rs:1-30` (0xF4E0); `crates/e2ee/src/provider.rs:131-160` (leaf capabilities, `RequiredCapabilities`) | "Policy admins are not carried yet: nobody can change the policy of a group once it exists" (`binding.rs:24-26`) |
| Sender identity | `crates/ipc/src/crypto.rs:1202` (`MemberView`); `mls.rs:1822-1835` | The verified identity fingerprint, checked against the account the server names |
| Encrypt IPC | `crates/ipc/src/crypto.rs:495` (`MlsEncrypt`) | Text only |
| State of the work | `docs/e2ee.md:1811-1871` | Encrypted edits, deletes, typing, read markers and files are open (#133) |

### 1.3 Presentation, notifications and frontends

| What | Where | Notes |
|---|---|---|
| Legacy projection | `crates/legacy-html/src/lib.rs:10-46, 108` (`legacy_body`, `text_html`, `escape_html`) | Shared by the SDK and nuschld's stock copy, so nobody sees different text |
| Content model | `crates/core/src/lib.rs:199-241` (`TextRole::Quote`, `Span { style, link, math }`); `crates/ipc/src/lib.rs:51-71` (`ContentRequest::Render { text }`) | There is no mention span |
| Name heuristic | `crates/client/src/names.rs:110-121` (`mentions`: whole-word, case-insensitive match of the bookmark user name) | Used for desktop notifications over rendered text **including code runs** (`client.rs:1975-1990`), for the mention sound over the **raw body**, which for legacy HTML includes markup (`:2011`), and for `unread_mention` over text blocks without code (`:2033-2050`) |
| Sounds | `crates/client/src/sounds.rs:29-41` | Mention, message or nothing. The native driver keeps quiet while deafened |
| Settings | `crates/session/src/config.rs:276-290` (`notifications`, `sound_effects`, `friend_notifications`) | There is no per-conversation mute. `ClientSettings` is `deny_unknown_fields`, so new settings need a versioned sidecar |
| Desktop | `apps/desktop/src/menus.rs:808-878` (message menu: Copy link, Copy, Copy message, Edit, Delete); `view.rs:2800` (`message_row`), `:3103` (`composer`); `state.rs:324` (drafts per conversation); `emoji.rs` and `assets/emoji.txt` (Unicode 17 picker, 1,923 lines, no skin tones); `timeline_keys.rs`; `typing.rs` (a polite live region) | |
| TUI | `apps/tui/src/app.rs:817` (`submit`), `:890` (`start_edit`), `:1652-1665` (`/edit`, `/delete`); `view.rs:166` (`message_lines`); `plain.rs` | Message selection by serial already exists |
| Web | `web/app/src/App.svelte:96-99` (drafts: 30, 128 KiB); `crates/client-wasm/src/snapshot.rs:253-260` (`MessageView`) | No message actions yet |
| Stock plugin data | `crates/protocol/proto/Mumble.proto:611-621`; nuschld `apps/server/src/handlers.rs:31-32, 1390-1420`; `docs/extension.md:333-334`; `docs/reviews/2026-11-11-server-security.md:300-302, 413`; issue #93 | Murmur and nuschld forward to the **listed sessions, without channel authorisation**. Data ≤ 1,000 bytes, data id ≤ 100 bytes, own bucket 4/s burst 15. Excess is dropped silently, and the sender session is set by the server. The client session ignores message 26 (`connection.rs:4350`) |

### 1.4 Adjacent work this plan must align with

- **Polls** (`docs/polls.md` at `nyqrtlok`, not on main; #26) settled several shared
  questions the same way this plan needs them. MLS references are by **envelope hash**.
  The actor is the **verified identity**. On compatible servers, votes are **state
  without `seq`**, carried in pages and pushed. The polls plan also claims protocol
  space this plan must not reuse: envelope kinds 11–12, range 0x4E98–0x4E9F,
  `ChatSend` field 7 and `Message` fields 15–16. Several shared changes have no owner
  yet: `MlsEncrypt` with kind and body, archive records for new kinds, MLS deletion,
  the reader's account in `PageQuery`, and satellite cleanup in the storage writers.
- **Link previews** (lanes `lxpzzkuu`/`ntkzuksm`) add `ContentRequest::Preview` and
  change `Client::send_text`/`PendingSend`.
- **Browser content broker** (`zyvrlunw`/`uxotpsxn`, #172) adds
  `timeline::Message.source_format` and `merge_with_receipt`, and fences web actions
  with original entity references (`crates/client-wasm/src/actions.rs`,
  `web/app/src/action-context.js`).
- **Markdown quotes** (#190, `nuschl-md-quote`, no commits yet) and Nix highlighting
  (#185) change `crates/content`.
- **Lint branches** (`wontwxku` TUI fixes, `zmvkqywv` desktop Clippy) touch
  `apps/desktop/src/{menus,view}.rs`, `apps/tui/src/view.rs`, `crates/content` and
  `crates/core`.

## 2. Shared foundation

### 2.1 One rule per kind of server

| Path | Carrier of structure | Presentation for others | Text parsed for meaning |
|---|---|---|---|
| Nuschl server, compatible mode, conversation with `history/1` | Typed extension fields and messages (§3–§6), validated, stored and authorised by nuschld | The server makes the stock copy from the structured data with shared pure builders (`crates/legacy-html`) | Never |
| Nuschl server, `e2ee/1` | Inside the MLS ciphertext: a structured chat body (new kind, carrying reply, mentions and thread) and Reaction bodies (kind 4), behind authenticated group features (§2.6) | None. Stock clients cannot connect (`docs/extension.md:28-40`) | Never |
| Stock Murmur (or a Nuschl server without the capability, or a non-extension target) | `PluginDataTransmission` annotations between Nuschl clients (§2.5) | The visible `TextMessage`, written as presentation for legacy users | Never. Without a correlated annotation, a message is plain legacy text with no interaction semantics |

Features per path:

| Feature | Nuschl compatible | E2EE | Stock (annotations) |
|---|---|---|---|
| Replies (§3) | `replies/1`: validated `reply_to`, per-reader previews, server-made stock quote | Kind 13 `reply` (envelope hash) | `reply` and `quote`, plus a quote prefix |
| Reactions (§4) | `reactions/1`: state without `seq` | Kind 4 bodies | `nuschl/react/1`, seen on this device only |
| Mentions (§5) | `mentions/1`: users and roles | Users (roles later, D8) | Certificate hashes, users only |
| Threads (§6) | `threads/1`: validated `thread_root`, thread pages, server-made thread line | Kind 13 `thread`, group feature `threads/1` | `thread_root` and `thread_quote`, plus a thread line prefix |
| Stock-authored target | Stored, so it has a reference: everything applies | Not applicable | No reference: quote replies only (D3) |

### 2.2 Stable message references

A new portable type in `crates/core/src/interaction.rs`:

```rust
/// A reference another message, reaction or thread can carry. Never a display
/// name, timeline index, UI serial, local row id or Mumble session id.
pub enum MessageRef {
    /// A Nuschl server's stored message: never reused, scoped to the server's
    /// history identity (certificate pin, later its identity key).
    Server { conversation: [u8; 16], seq: u64 },
    /// An MLS message: the hash of its original durable envelope
    /// (`envelope::hash`, docs/e2ee.md §6.4), the same value polls use for votes.
    /// `seq_hint` is the row the referrer saw it in. It is unauthenticated and used
    /// only to fetch `around(seq_hint)`, after which the hash is checked.
    Envelope { conversation: [u8; 16], hash: [u8; 32], seq_hint: Option<u64> },
    /// A Nuschl client's message on a stock server: the author's Mumble certificate
    /// hash (SHA-1, 20 bytes, as `UserState.hash`) and the 16-byte message id the
    /// author chose in its annotation (§2.5). Resolvable only by clients that saw it.
    Stock { author: [u8; 20], msg_id: [u8; 16] },
}
```

A thread root is a `MessageRef` like a reply parent, always in the form of its
conversation's path. No path has a separate thread identifier.

Every client-side key also carries the **server history identity**
(`ServerState::history_identity`, as local history uses it), so a reference never
resolves on another server or after a certificate change.

**Revision binding.**

- **Server:** a reply records `reply_revision` = the parent's `edits` the replier saw.
  Readers show "edited since" when the parent's `edits` is higher. Reactions and
  mentions bind to the message, not a revision: reactions survive edits, as in common
  chat applications.
- **MLS:** the reference is the **original** envelope's hash, which is stable across
  encrypted edits (edits name the same hash). An optional `revision` counts the edits
  the replier had seen.
- **Stock:** no revisions. Edits do not exist on stock servers.
- **Threads:** the root binds to the message on every path. Editing a root never
  moves its thread.

**What has no reference.** A stock-authored message on a stock server (no annotation),
a stock message on a Nuschl server that the server did not store (over 1 MiB, or a
stock direct message without archiving), and local observations. Interactions with
them are limited to presentation (§3.7, §4.7): quote replies, no reactions, and no
threads (§6.4). They are never addressed by content digest, because identical texts
collide and receivers with different windows would attach to different messages
(decision D3).

### 2.3 Stable actor identities

| Path | Actor | Not used |
|---|---|---|
| Nuschl compatible | Account id (`Author.account`, 16 bytes) | Display names, session ids, registration numbers (only as an input alias, resolved by the server under its state lock) |
| E2EE | Verified identity fingerprint (`MemberView.identity`), which the crypto worker already checks against the attributed account. All devices of one identity act as one | The server's attribution alone, nickname labels |
| Stock (annotations) | Mumble certificate hash, as the receiving client sees it in `UserState.hash`, plus the presence `instance` (`crates/core/src/model.rs:78`) to correlate within one presence | Session ids alone, names |

Unregistered users on Nuschl servers have no durable identity. They may send replies
(a reply needs no actor identity), but they cannot react, cannot be mention targets,
and cannot receive mention notifications. Users without a certificate on stock servers
are likewise excluded from reactions and from structured mentions.

The SDK gains `author_account: Option<[u8; 16]>` and `author_identity: Option<String>`
on `timeline::Message`, and local history stores both (§2.10). The display name stays
a snapshot for presentation.

### 2.4 Structured payloads per path

- **Compatible server:** new fields on `ChatSend`, `ChatEdit`, `Message` and
  `HistoryRequest`, plus new message types in a fresh range (provisional
  **0x4EB0–0x4EBF**, pending Root's freeze of the central registry at
  `docs/extension.md:120-134`; 0x4E98–0x4E9F is proposed for polls and 0x4EA0–0x4EAF
  for screen-sharing signalling). Field numbers below
  are proposals around the polls lane's claims; Root freezes them.
- **E2EE:** a new durable kind, **`Message` (13)**, whose body is a protobuf
  `ChatBody { text, reply, mentions, thread, reply_quiet }`. Kind 4 (`Reaction`) gets a
  protobuf body. Edit (2) and Delete (3) bodies name an `EnvelopeRef`; the MLS deletion
  owner shared with polls defines them. Kind codes are allocated by the E2EE lane.
  Plain `Chat` (1) stays for groups without the feature.
- **Stock:** annotations in message 26, including the thread fields, §2.5.

### 2.5 The stock side channel: `PluginDataTransmission`

**Scope.** Only for targets without the extension: stock Murmur, Nuschl servers
without `history/1`, and non-extension targets there. Never on Nuschl servers in place
of typed messages; those are cleaner, stored and authorised. The PD lane (#93) owns
the generic session support: framing, the reserved `nuschl/` namespace, sending to an
exact receiver list, pacing below the server bucket, and handing received payloads to
the SDK with sender session and presence instance. It must do so equivalently in
`nuschl-session` and `crates/client-wasm/src/session.rs`. This plan owns the payload
schemas and the correlation logic in the portable SDK.

**Data ids** (proposal, to be allocated by the plugin-data lane): `nuschl/chat/1` (an annotation for one
`TextMessage`, including thread replies) and `nuschl/react/1` (one reaction change).

**Payloads** (protobuf, `nuschl.v1` interactions block; at most **960 bytes** encoded,
below the 1,000-byte cap that Murmur and nuschld enforce by silent drop):

```proto
message StockAnnotation {            // data id "nuschl/chat/1"
  uint32 version = 1;                // 1
  bytes  msg_id = 2;                 // 16 random bytes chosen by the sender
  bytes  text_digest = 3;            // SHA-256("nuschl/stock-text/1" || TextMessage.message)[..16]
  uint32 presentation_prefix = 4;    // length of f(annotation): the legacy-only leading bytes
  StockRef reply = 5;                // the parent, when it is an annotated Nuschl message
  StockQuote quote = 6;              // the reply quote's inputs, whenever the text carries one
  repeated bytes mentions = 7;       // ≤ 8 mentioned users' certificate hashes (20 bytes each)
  bool reply_quiet = 8;              // do not notify the parent's author
  StockRef thread_root = 9;          // the thread's root, an annotated Nuschl message (§6.4)
  StockQuote thread_quote = 10;      // the thread line's inputs: the root as the sender saw it
}
message StockRef   { bytes author = 1; bytes msg_id = 2; }        // 20 + 16 bytes
message StockQuote { string author = 1; string excerpt = 2; }     // ≤ 64 B; ≤ 160 B (quote) or ≤ 120 B (thread); inert
message StockReaction {              // data id "nuschl/react/1"
  uint32 version = 1; StockRef target = 2; string emoji = 3; bool add = 4;
  uint32 op = 5;                     // per-sender counter: drop replays and reorders
}
```

With every optional field at its cap, an annotation encodes to 726 bytes (792 with a
thread title, D26), so the 960-byte budget holds without truncation. The quote fields
are both the presentation inputs and what a receiver shows when it does not hold the
referenced message ("quoted by the sender, not verified").

**The generated prefix.** `f(annotation)` = `thread::line(thread_quote)` when set,
followed by `reply::quote(quote)` when set (§3.7, §6.5), built with the server's
`allow_html` from `ServerConfig`. The builders only escape and assemble: the sender
applies the excerpt cuts and budget degradation before filling the fields (author only
is an empty excerpt; a dropped part is an absent field), so `f` is a pure function of
the annotation that any receiver can recompute.

**Sending (Nuschl sender on a stock target).**

1. The SDK builds the visible text `T = f(annotation) + legacy_body(user_text)` and sets
   `presentation_prefix = len(f(annotation))`. It draws `msg_id` and computes
   `text_digest` over exactly the `TextMessage.message` string it will send.
2. Receivers are the sessions the text reaches as this client sees them: users in the
   target channel plus visible listeners, or the one peer of a direct message. Own
   session excluded, deduplicated, at most **256**. With more, only the text is sent
   and the composer says "Nuschl extras unavailable here (too many people)". The client
   never sends structure to fewer people than the text reaches, and never to others.
3. The session queues the annotation **immediately before** the `TextMessage`, in the
   same pacing slot on the same control connection. If the plugin pacer cannot send it
   within that slot, the text goes alone, and the sender's own copy is marked as sent
   without Nuschl information (§6.4 words it for threads). Text is never held back for
   its annotation.
4. **Rate limits.** An annotated message costs one token of the text bucket (Murmur
   default 1/s, burst 5) and one of the plugin bucket (4/s, burst 15). The text bucket
   binds first. The PD pacer serves `nuschl/chat/1` before reactions (sent ≤ 2/s) and
   cosmetic data, so annotations keep up with text. A server may configure smaller
   buckets and does not advertise them; drops then degrade to plain text as below.

**Receiving.**

1. An annotation from sender S (session plus presence instance) is decoded strictly
   (≤ 1,000 bytes; version 1; field bounds; unknown versions dropped) and buffered under
   `(connection generation, S.session, S.instance, text_digest)` for **5 s**. At most 8
   per sender and 64 per connection; the oldest is evicted.
2. A `TextMessage` from S is digested exactly as received. A buffered annotation
   correlates when the digest matches **and** the first `presentation_prefix` bytes of
   `T` equal `f(annotation)`, recomputed from the annotation's own validated quote
   fields with this server's `allow_html`. The message then gets the annotation's
   structure (its `MessageRef::Stock` id, reply, thread, mentions and quotes) and
   renders `T[presentation_prefix..]`. If the prefix check fails, the annotation does
   not correlate. Recomputing and comparing bytes the receiver built itself is not
   parsing: nothing is read out of the text.
3. **Only the text arrives** (the annotation was dropped, rate-limited, sent to fewer
   receivers, or the server rewrote the text so the digest differs): the text is shown
   at once as plain legacy text, including any visible quote or thread line, with no
   reply, thread, mention or reference semantics. A late annotation within **2 s**
   after the text (reorder tolerance) upgrades it in place, moving it into its thread
   where it has one, through the ordered change queue that edits already use
   (`timeline.rs:1-8`).
4. **Only the annotation arrives:** it expires silently. It never creates a message,
   a notification or a reaction target. A reference to an unknown `msg_id` resolves
   to "not on this device".
5. Duplicate `(author, msg_id)`: the first wins and later ones are ignored.

**Why the body stays in the `TextMessage`.** The owner suggested generating the
visible Mumble message deterministically from the plugin data, roughly
`f(plugindata)`, and ignoring that generated message once. The design does exactly
that for the part that is generated, and keeps the body in the text:

- Plugin data is capped at 1,000 bytes per message. Chat text may reach
  `message_length` (5,000 by default) and, with inline images, `image_message_length`.
  The body does not fit, and splitting it would need reassembly state and more of the
  plugin bucket.
- Murmur forwards plugin data to any listed session without channel authorisation
  (§1.3). The `TextMessage` is the only part the server authorises and delivers by its
  channel rules, so it must remain the message. An annotation alone never creates one.
- Sending the body twice would double traffic, and the two copies could diverge.

So `f(annotation)` generates only the presentation prefix (thread line and reply
quote). A Nuschl receiver shows the `TextMessage` once, without that prefix: it finds
the prefix by its declared length, inside a text bound by the digest, and confirms it
by recomputing `f`. Legacy users see `T` whole; Nuschl users see the body with its
structure; nobody sees the message twice, and a sender cannot hide other text from
Nuschl users behind a false length (D27). The message menu offers "Show as Mumble
users see it", which displays the whole `T`. Visible text still never carries meaning.

**Ordering.** Murmur processes one client's control messages in order and writes them
to each receiver's link in that order. So annotation-then-text is the normal case,
text-then-annotation is tolerated for 2 s, and anything else degrades to plain text.

**Trust.** The server sets `senderSession`. Murmur forwards message 26 without channel
authorisation, so annotations take effect only by **correlation with a `TextMessage`
the server delivered**, which applies its TextMessage permission and channel rules. A
reaction is accepted only if:

- its target is a locally known annotated message;
- the reactor is currently in or listening to that message's channel as this client
  sees it, or is the peer of the direct conversation;
- the reactor has a certificate hash;
- the sender stays within a local rate limit (§2.11).

A thread reply is placed under a root only by the §6.2 rules: the root is a locally
known annotated message in the same conversation and not itself a thread reply. An
unknown root yields a Threads list entry from `thread_quote` (§6.4) and never attaches
to anything else.

Annotations are therefore only as trustworthy as the server. It sets the sender, and it
can drop, rewrite or forge annotations exactly as it can text. Another client cannot
attach structure to someone else's text, because correlation needs the same session and
presence instance. The UI shows no verification mark for annotation-derived structure.

**What others see.** The server sees every annotation in clear, as it sees text. Listed
stock clients receive message 26 and hand unknown data ids to their plugin framework,
where no standard plugin consumes `nuschl/`. The payload holds message ids, digests,
emoji, quote inputs, and certificate hashes of mentioned users, all of which mirror
what the visible text already says. The receiver list reveals only the channel
membership the sender already sees. Nothing here is confidential, and the UI never
calls it private.

**Persistence.** Stock servers keep no history. Structure exists only on clients that
observed it. Local history stores `msg_id`, author certificate hash, the annotation
fields (thread root included) and observed reactions (§2.10). Late joiners and clients
that were offline do not learn them, and the UI says "seen on this device" where that
matters (reactions, threads whose first message is missing).

### 2.6 Capability gates (fail closed, no silent downgrade)

- **Server capabilities** (sticky like every capability, `docs/extension.md:58-78`):
  - `replies/1`: validated `reply_to`, previews, server-made stock quote;
  - `reactions/1`: compatible mode only;
  - `mentions/1`: structured mentions, notices and role targets in compatible mode;
    on `e2ee/1` servers only the `MentionTargets` query (own and mentionable roles);
  - `threads/1`: validated `thread_root`, thread views and `ThreadQuery` in compatible
    mode (needs `history/1` and `replies/1`; §6.3).

  Without a capability the feature is off for that server, or falls back to the stock
  presentation path where §3–§6 say so. A client never sends the typed fields to a
  server that did not negotiate them.
- **MLS group features**: Nuschl-private extension types advertised in leaf
  capabilities and listed in the group's `RequiredCapabilities`
  (`crates/e2ee/src/provider.rs:131-160`). Proposal: **0xF4E1** `interactions/1`
  (kind 13 bodies, Reaction bodies, archive records for both) and **0xF4E2** `threads/1`.
  A client sends kind 13 or 4 only when the authenticated group context requires
  0xF4E1, and offers threads only when it requires 0xF4E2. Groups created after rollout
  require the features when every initial leaf advertises them. Existing groups
  upgrade by a GroupContextExtensions commit that only adds features (decision D12,
  E2EE lane). Until then E2EE replies, mentions and threads are unavailable, and the
  composer says why. An E2EE conversation never uses the compatible or stock forms
  instead. Kind 13 and Reaction envelopes in a group without the feature are refused
  by receivers as malformed.
- **Stock:** annotations need the PD lane's session support. Without it, stock targets
  get the presentation text only.

### 2.7 Binding asynchronous work

AGENTS.md: asynchronous results are bound to their connection, conversation,
generation, source and request.

- **Action tickets.** Every UI action (reply, react, open reaction voters, open
  thread, insert mention) captures `ActionTicket { server history identity, connection
  generation, conversation (ConversationId or local storage key), MessageRef, revision
  seen }`. Before sending, the SDK checks that:
  - the server is still connected with that generation, or the action is refused with
    "The connection changed; try again";
  - the conversation is the same;
  - the referenced message is still present and resolvable in that conversation's
    timeline or local store.

  It never re-resolves by timeline index, UI serial or local row id. Server references
  stay valid across reconnects (seq and ConversationId are never reused); stock
  references do not survive a server identity change.
- **Composer contexts.** The reply or thread context is part of the draft, keyed by
  `(server history identity, conversation, thread root or none)`. It is dropped on a
  certificate or identity change. If the parent becomes unavailable before Send
  (deleted, access lost, retention), the send is held: "The message you replied to is
  no longer available. Send without the reply?" The reply is never silently stripped,
  and the same holds for a thread root.
- **Results.** Reply previews, reaction updates, voter pages, mention targets and
  thread pages carry the ticket or the navigation generation. Late results for an old
  generation, another conversation or a replaced thread view are dropped, including
  after switching back to the same conversation (`navigation.rs` precedent).
- **Ordering.** Reaction and preview updates for a message that has not arrived yet
  wait in the existing ordered change queue, bounded, as edits do (`timeline.rs:1-8`;
  `docs/extension.md:556-561`).

### 2.8 Older clients and unknown structure

- **Server fields** are proto3 additions: older clients ignore them and see plain
  messages, the same text, and no reactions. Thread replies appear flat in the
  conversation, without a thread line (D28): older clients request `ALL` pages and
  never receive the stock copy.
- **E2EE:** older clients would show false gaps for kinds 13 and 4, which is why §2.6
  gates them on a required group capability that older clients cannot satisfy.
  Separately, and shared with polls, unknown durable kinds should become a placeholder
  ("A message this version cannot show") that still advances the sender chain. The
  E2EE lane owns that.
- **Stock:** older Nuschl clients ignore message 26 and see presentation text,
  including quote and thread lines, as legacy users do.

### 2.9 Where plain text is still read

- **For interaction semantics: nowhere.** No reply, reaction, mention or thread is
  derived from visible text on any path. Stock text without a correlated annotation is
  plain legacy text.
- **Existing name heuristic** (`names::mentions`): today it notifies on a whole-word
  match of one's own user name. Recommendation (decision D9):
  - on Nuschl servers with `mentions/1` and in E2EE, notifications come only from
    structured mentions;
  - on stock servers the heuristic remains only as an opt-out local highlight and
    sound for legacy senders, who cannot send structure, computed over rendered
    non-code, non-quote text;
  - it never creates mention state, never counts for role mentions, and never applies
    to messages whose annotation already carries mentions.

  This changes visible behaviour, so it needs the owner's decision.
- **Presentation only:** rendering Markdown and legacy HTML in the content worker, and
  skipping a declared `presentation_prefix` once it equals the receiver's own
  recomputation of `f(annotation)` (§2.5).
- **Outside this plan:** the polls lane's stock path reads vote lines by user decision
  (`docs/polls.md` §11.4), and file offers are recognised in text
  (`crates/core/src/transfer.rs`). Neither is extended here.

### 2.10 Shared storage and model changes

- **Server migration** (number allocated by Root; polls proposes `0007_polls`), named
  `00NN_message_interactions.sql`, in SQLite and PostgreSQL twins:
  - `history_messages` gains `reply_revision INTEGER`, `mentions TEXT` (canonical,
    bounded encoding, ≤ 4 KiB) and `thread_root INTEGER`, with partial indexes
    `(conversation, thread_root, seq) WHERE thread_root IS NOT NULL` and
    `(conversation, seq) WHERE thread_root IS NULL` (§6.3);
  - `history_threads(conversation, root, replies, last_seq, last_ms,
    PRIMARY KEY (conversation, root))`, with an index on `(conversation, last_seq)`;
  - `history_reactions(conversation, target, account, emoji, created_ms,
    PRIMARY KEY (conversation, target, account, emoji))`, with an index on
    `(conversation, target)`;
  - `history_reaction_heads(conversation PRIMARY KEY, version)`;
  - `history_mentions(account, conversation, seq, PRIMARY KEY (account, conversation, seq))`.
- **One satellite hook** in `crates/storage/src/server/history.rs`, called by
  `history_delete`, `history_expire`, `history_erase`, `history_forget_account` and
  `history_purge`. It removes reactions and mention rows (and the polls lane's ballots)
  for affected messages and adjusts `history_threads` counts in the same transaction,
  so two lanes do not each edit five writer functions. `expirable` is unchanged:
  satellites follow their message.
- **Local migration** `crates/storage/migrations/{sqlite,postgres}/0005_message_interactions.sql`:
  - `messages` gains `author_account`, `author_identity`, `author_cert`, `ref_hash`
    (MLS envelope hash), `msg_id` (stock), `reply_seq`, `reply_hash`, `reply_msg_id`,
    `reply_revision`, `thread_seq`, `thread_hash`, `thread_msg_id` (the root, in the
    form of its path), `mentions_me`, `annotation` (bounded);
  - new tables `message_reactions(server, channel, target_key, actor, emoji, at_ms)` and
    `thread_state(server, channel, root_key, replies, last_ms, followed, read_seq)`,
    where `root_key` encodes the root's `MessageRef`.

  These are additive, so older rows read as having no interactions. `ServerRecord` and
  `HistoryEntry` (`crates/core/src/lib.rs:41-66`) gain optional fields with
  `#[serde(default)]`.
- **SDK timeline** (`timeline.rs`): `Message` gains `author_account`,
  `author_identity`, `reference: Option<MessageRef>`, `reply: Option<ReplyView>`,
  `reactions: ReactionsView` (≤ 20 entries), `mentions: Vec<MentionView>`,
  `mentions_me`, `thread: Option<ThreadSummary>` and `thread_root: Option<MessageRef>`.
  The broker lane adds `source_format` to the same struct (conflict note, §9).

### 2.11 Bounds (AGENTS.md: inputs and aggregate outputs)

| Item | Bound | Enforced by | Over the bound |
|---|---|---|---|
| Reply parent | 1 per message, same conversation, `kind = MESSAGE`, not a tombstone, readable by the sender | nuschld; MLS receivers; stock receivers | `NOT_FOUND` / reference shown as unavailable |
| Reply preview excerpt | First line, ≤ 256 bytes, cut at a character boundary, control and bidi-override characters removed, rendered as literal text (no Markdown) | nuschld builds it; SDK revalidates | Truncated with "…" |
| Previews per page | At most one per row (≤ 100), counted against the 256 KiB page cap | nuschld | The page ends earlier (never skips a message) |
| Stock quote presentation | Author ≤ 64 bytes; excerpt ≤ 120 UTF-16 units and ≤ 160 bytes; within Murmur's `message_length` together with the thread line | `legacy-html::reply` budget | Author only, then no quote. The user's text is never shortened |
| Thread line presentation | Author ≤ 64 bytes; the root's first line ≤ 60 UTF-16 units and ≤ 120 bytes; author only for `LEGACY_HTML` roots | `legacy-html::thread` budget | Author only, then the label-free `↪ thread`, then no line. The user's text is never shortened |
| Emoji | ≤ 32 bytes, ≤ 10 scalar values, on the RGI list (D14) | `nuschl_core::emoji`, used by nuschld, SDK, receivers | `MALFORMED` / ignored |
| Distinct emoji per message | 20 | nuschld; MLS and stock receivers deterministically in stream or arrival order | `LIMIT_REACHED` / ignored |
| Reactions per actor per message | 5 (D15) | same | same |
| Reaction summary in pages | ≤ 20 counts, each with ≤ 3 named reactors | nuschld | — |
| Reaction voters | ≤ 256 per page, cursor by account | nuschld | `more = true` |
| Reaction requests | Shared `MAX_OUTSTANDING = 16`; per-account bucket `history.reactions_per_minute` (default 60, burst 20); the SDK coalesces to the newest intent per (message, emoji) | nuschld; SDK | `RATE_LIMITED` with `retry_after_ms` |
| Reaction pushes | ≤ 1 `ReactionUpdate` per target per 250 ms per connection (coalesced) | nuschld | Coalesced, never queued without bound |
| Mentions per message | ≤ 20 users and ≤ 2 roles; label ≤ 128 bytes; ranges ascending, disjoint, on character boundaries, inside `content` | nuschld; SDK; content worker for styling | `MALFORMED` / styling skipped |
| Role recipients | ≤ 256 accounts per role mention (`mentions.max_role_recipients`) | nuschld | `LIMIT_REACHED`; the send is refused, notifications are never silently truncated |
| Role mention rate | ≤ 5 per account per hour (`mentions.role_mentions_per_hour`) | nuschld | `RATE_LIMITED` |
| Stored mention rows | ≤ 1,000 per account, oldest dropped; counts shown saturate at 999 as today | nuschld storage | Oldest dropped |
| Mention completion | ≤ 20 candidates shown; ≤ 64 roles per `MentionTargetList` | SDK; nuschld | — |
| E2EE receiver limits | Mention notifications ≤ 10 per sender per 10 min; events for unknown targets ≤ 256 per conversation (oldest evicted) | SDK | Ignored, with one notice per sender |
| Thread structure | Depth 1 on every path; the root is a live top-level `MESSAGE` row (or annotated or MLS message) in the same conversation; a reply inside a thread names the root or the same thread | nuschld inside the append transaction; E2EE and stock receivers | `NOT_FOUND`, seq voided / shown unplaced |
| Thread pages | `MAIN` and `THREAD` views keep the 100-row limit, 256 KiB cap and visible ranges; one stored summary or one last-reply lookup per root row; `ThreadQuery` ≤ 100 roots, or ≤ 50 entries with ≤ 200 candidates examined | nuschld | `more = true`; the page ends earlier, never skips |
| Threads (client) | ≤ 4,096 tracked roots per conversation, unresolved stock roots included; ≤ 256 followed per conversation; reply counts saturate at 9,999; thread window ≤ 1,000 messages in memory | SDK, local store | Oldest untracked; counts show "9,999+" |
| Drafts | ≤ 30 drafts and ≤ 128 KiB in total, now including reply and thread contexts (web precedent) | each frontend | Oldest dropped |
| Stock annotations | ≤ 960 bytes (worst case 726, or 792 with a thread title); data id ≤ 100 bytes; `presentation_prefix` ≤ 4 KiB and equal to `len(f(annotation))`; ≤ 256 receivers; buffered ≤ 8 per sender and ≤ 64 per connection, 5 s TTL, 2 s late-upgrade window | SDK, PD session layer | Not sent / evicted / plain text |
| Stock reactions | Sent ≤ 2/s per client (coalesced); accepted ≤ 10/s per sender; state per message within the limits above | SDK | Dropped |

## 3. Replies (#191)

### 3.1 Protocol

**Compatible server (`replies/1`, needs `history/1`):**

```proto
message ChatSend {                 // existing fields 1-6; 7 is the polls lane's
  uint32 reply_revision = 8;       // the parent's `edits` the sender saw
  bool   reply_quiet = 9;          // do not notify the parent's author
}
message ReplyPreview {
  enum State { AVAILABLE = 0; DELETED = 1; UNAVAILABLE = 2; }
  uint64 seq = 1; State state = 2;
  Author author = 3;               // empty unless AVAILABLE or DELETED
  string excerpt = 4;              // AVAILABLE only; ≤ 256 bytes, first line
  uint32 edits = 5;
}
message Message {                  // 15-16 are the polls lane's
  ReplyPreview reply = 17;         // per reader; only when `reply_to` is set
  uint32 reply_revision = 21;
}
```

`body_hash` (`chat.rs:52-59`) also covers `reply_revision` and `reply_quiet`, so an
idempotent retry is the same reply.

**E2EE:** `ChatBody.reply = EnvelopeRef { hash, seq_hint, revision }` inside kind 13.
No excerpt is carried. A reader without access to the parent must not learn it through
the reply, which matters for `since_join` (decision D18).

**Stock:** the annotation's `reply` (when the parent is annotated) and `quote`, plus
the presentation prefix (§3.7).

### 3.2 Server storage and validation (compatible mode)

`chat_send` under `replies/1`:

1. `reply_to` must lie in the sender's visible ranges: the server already computes
   them for pages (`since_access` intervals, `MAX_RANGES`).
2. `HistoryAppend` carries a `reply_check { target }`. Inside the append transaction,
   the storage worker checks that the target row has `kind = MESSAGE`, `deleted_ms IS
   NULL`, format `TEXT` or `LEGACY_HTML`, and `reply_revision ≤ edits`. It returns the
   parent's author and excerpt. On failure the seq becomes a void row and the client
   gets `NOT_FOUND`. A reply to a deleted parent is refused (decision D19).
3. The stock copy (`Delivery::Message.stock`) is built **after** the write from the
   returned parent snapshot (`crates/legacy-html/src/reply.rs`, §3.7). That moves its
   construction from `chat_send` into `stored()` (`chat.rs:533`).
4. Edits cannot change `reply_to` (`docs/extension.md:267`). Deleting the reply drops
   its reply metadata, as today.

Without `replies/1`, an older nuschld keeps the old behaviour, and newer clients send
no `reply_to` there. They fall back to the stock path only for non-extension targets;
on an older Nuschl server's extension conversation the Reply action is unavailable
(D20), because the content field must stay the user's text.

**Previews in pages and events.** For each row with `reply_to`, the page reader joins
the parent row and fills `ReplyPreview` for this reader:

- `UNAVAILABLE` when the parent is outside the reader's visible ranges, has expired,
  or was erased;
- `DELETED` for a tombstone;
- otherwise `AVAILABLE` with the excerpt.

Live `ChatEvent`s carry the preview computed for each receiving connection. Fan-out is
already per connection, so the encode happens once per distinct preview state.

### 3.3 Client model

- `ReplyView { target: MessageRef, state: Available { author, excerpt, edited_since }
  | Deleted | Unavailable | NotLoaded | QuotedBySender { author, excerpt } }`.
- Server path: from `Message.reply`. Updated when the parent is edited or deleted live:
  the SDK re-derives previews of loaded replies from its loaded parent; otherwise the
  next page refresh does.
- MLS: resolved locally by envelope hash from the decrypted store, else `NotLoaded`
  with a "Load" action that pages `around(seq_hint)` and verifies the hash.
- Stock: resolved locally by `(author, msg_id)`, else `QuotedBySender` from the
  annotation's quote, else `NotLoaded` ("not on this device").
- Composer: `Client::set_reply(ticket)`, `clear_reply()`; `send_text` takes the draft's
  reply context (§2.7).

### 3.4 History and reconnect

Server references are seqs, so they survive reconnects. The idempotent resend after a
reconnect (`PendingChat`, `connection.rs:122-131`) carries `reply_to` and
`reply_revision` unchanged. The local cache stores `reply_seq`/`reply_hash`/`reply_msg_id`
with the message. A preview is never stored as truth: it is recomputed from the stored
parent or re-fetched with pages.

### 3.5 UI

- **Desktop:**
  - The message menu gains *Reply*, and Alt+R on a focused message (`timeline_keys.rs`)
    does the same.
  - A reply bar above the composer (`composer`, `view.rs:3103`) shows "Replying to
    *Alice*" with the excerpt, and has an × button named "Cancel reply to Alice".
    Escape in an empty composer cancels the reply.
  - In the timeline, a compact line above the body reads "↩ Alice: excerpt". It is
    activatable: it jumps to the parent through the existing `around` path used by
    search results. Its accessible name is "In reply to Alice: excerpt. Activate to
    show the original."
  - States are worded plainly: "Original message deleted", "Original message
    unavailable", "Not loaded: load", "Quoted by the sender (not verified)", and
    "edited since" when the parent changed.
- **TUI:** `r` on a selected message, or `/reply [n]`. A `↩ Alice: excerpt` line sits
  above the composer, and `/noreply` removes it. Messages show `┃ ↩ Alice: excerpt`
  above the body. In `plain.rs` mode this is written out as "in reply to Alice:".
  `/original` jumps to the parent.
- **Web:** *Reply* in a message actions menu (a button and a long-press sheet on
  touch), a reply bar above the composer, and the same reply line. Actions go through
  the broker lane's original-entity fences (`action-context.js`).
- Literal and code content is preserved: excerpts are plain text and never Markdown.

### 3.6 Notifications

A reply to one's own message counts as a mention for notification and sound, unless
the replier turned it off with `reply_quiet` (a bell toggle in the reply bar, on by
default; decision D4).

- Compatible mode: the server adds the parent's author account to the message's
  mention recipients (§5.2), unless that account cannot read the conversation.
- MLS: the receiver sees that the parent is its own message by hash.
- Stock: the receiver resolves the parent locally and sees its own `msg_id`.

### 3.7 Legacy projection

- **Builder:** `nuschl_legacy_html::reply::quote(author, excerpt, html: bool) -> String`,
  with a budget function. With HTML it produces
  `<blockquote><b>{author}</b>: {excerpt}</blockquote>`, escaped, with no attributes,
  well-formed XML. Without HTML it produces `> {author}: {excerpt}\n`.
- **On a Nuschl server,** nuschld builds the stock copy as `quote(parent) +
  legacy_body(text)` from the **stored** parent, never from client-supplied text
  (with a thread line first for thread replies, §6.3). This follows the server-made
  stock HTML rule (`docs/extension.md:184-190`).
- **Visibility.** Stock receivers who may not read the parent under the history rules
  (outside their `since_access` ranges, or guests under `history.guests = none`) get
  the copy without the quote. nuschld builds at most two variants per message (§6.3).
- **On a stock server,** the sender's client builds the same text from the
  annotation's `quote` fields, so receivers can recompute the prefix and skip it
  (§2.5).
- **What stock users keep:** a quote shown to stock users stays in their logs after the
  parent is edited or deleted, like every stock copy. The reply bar warns before
  sending: "Stock Mumble users also see a quote of this message". The delete
  confirmation already says stock users keep the message; it adds "and quotes of it".
- **Stock-authored parents** have no reference. The reply sends the quote presentation
  and an annotation with `quote` only. Nuschl receivers show "Quoted by the sender (not
  verified)" without navigation (D3).

### 3.8 E2EE

Everything is inside the authenticated body: the reply reference, `revision` and
`reply_quiet`. The server sees one MLS row as for any message. Replies to messages
from before the reader's access show `NotLoaded` or `Unavailable`; no excerpt reveals
the parent (D18). The "Original message deleted" state needs MLS deletion (the
Delete envelope, shared with polls); until then encrypted parents cannot be deleted.

## 4. Reactions (#195)

### 4.1 Protocol

**Compatible server (`reactions/1`, needs `history/1`; reactions are state without
`seq`, D1):**

| Type (proposal) | Message | Reply |
|---|---|---|
| 0x4EB0 | `ReactionSet { request_id, conversation, target (seq), emoji, add }` | `ReactionUpdate` |
| 0x4EB1 | `ReactionUpdate { request_id (0 when pushed), conversation, target, summary: ReactionSummary { repeated ReactionCount { emoji, count, mine, repeated Author first (≤ 3) } (≤ 20), version }, applied }` | none (also pushed) |
| 0x4EB2 | `ReactionQuery { request_id, conversation, target, emoji, after (account), limit ≤ 256 }`. With an empty emoji it means "summaries for up to 100 `targets`" (the reconnect refresh) | `ReactionVoters` or `ReactionUpdate`s then `Done` |
| 0x4EB3 | `ReactionVoters { request_id, conversation, target, emoji, repeated Author authors, more }` | none |
| 0x4EB4 | `ReactionClear { request_id, conversation, target, emoji }`: moderators remove one emoji from a message | `ReactionUpdate` |

`Message.reactions = 20` carries the summary in pages, and `mine` is filled per reader.
`ConversationState.reactions_version = 6` (and the same on `Direct`) lets a
reconnecting client refresh loaded messages only when something changed. Pushes carry
counts. `mine` is set only on the actor's own connections, as in the polls design.

**E2EE:** kind 4 body `ReactionBody { EnvelopeRef target; string emoji; bool add }`.
The actor is the authenticated identity. Archive records are required (§4.8).

**Stock:** `nuschl/react/1` (§2.5).

### 4.2 Server storage, validation and permissions

- Validation:
  - `Action::React(id)`: a registered account and `can_write` (the TextMessage
    permission) in the conversation; not in live-only conversations;
  - the target is a `MESSAGE` row, not a tombstone, inside the actor's visible ranges;
  - the emoji is on the list;
  - limits: 20 distinct emoji and 5 per actor per message;
  - the per-account reaction bucket, not the text bucket, which is too tight for
    toggling and would starve chat.
- Add and remove are idempotent. Removing an absent reaction, or adding a present one,
  returns `applied = true` with the current summary.
- Only one's own account's rows can be removed. `ReactionClear` needs `Action::Moderate`
  (`moderators_delete` plus Write), is logged without content, and is shown as "removed
  by a moderator".
- Storage: `history_reactions` with the version bump in one transaction. The satellite
  hook (§2.10) removes reactions with their message on delete, expiry, erase, forget and
  purge, and removes a forgotten or erased account's reactions everywhere.
- The live audience gets pushes only for connections that may read the target, checked
  against the current `authz_epoch` as edit events are (`docs/extension.md:278`).

### 4.3 Client model

- `ReactionsView { entries: Vec<ReactionEntry { emoji, count, mine, first_names } > }`,
  plus a `pending` intent per (message, emoji) for optimistic display. The intent is
  rolled back on refusal and replaced by the server's summary.
- `Client::react(ticket, emoji, add)` and `Client::reaction_voters(ticket, emoji, cursor)`.
- MLS: the SDK aggregates decrypted Reaction events per `(target hash, identity,
  emoji)`, last writer by stream order. Events for unknown targets wait in the bounded
  orphan buffer. Local storage persists the aggregate per message. The crypto worker
  stays small and only authenticates and decrypts. (Polls counts in the worker store;
  this plan keeps reaction state with decrypted local history, D21.)
- Stock: an observed-only state per annotated message, persisted locally and labelled
  "Reactions seen on this device".

### 4.4 History and reconnect

- Pages carry current summaries, so history loads are consistent.
- After a reconnect, `Subscribed` carries `reactions_version`. When it changed, the SDK
  refreshes summaries of loaded targets in batches of 100.
- The SDK's pending intents are resent with the same `(target, emoji, add)`, which is
  idempotent.
- MLS: reactions are rows in the stream, so catch-up replays them, and archive records
  give late joiners the same state.
- Stock: nothing to resync, by design.

### 4.5 UI

- **Desktop:**
  - A row of chips under the message, each with the emoji and its count. A chip is
    pressed when one of the reactions is yours, and toggles yours on click or
    Enter/Space. The tooltip lists up to 3 names "and N others".
  - *React…* in the message menu and Alt+E on a focused message open the existing
    emoji picker (`emoji.rs`) in reaction mode.
  - *View reactions* opens a dialog with tabs per emoji listing the participants
    (paged).
  - The accessible name of a chip reads "Thumbs up, 3 reactions, including yours.
    Activate to remove yours."
  - The colour emoji font follows #7.
- **TUI:**
  - A line `[<emoji> 3*] [<emoji> 1]`, where `*` marks yours, with no reliance on colour. Plain
    mode writes "thumbs up 3, including you".
  - `+` on a selected message, or `/react NAME|EMOJI`, using emoji names from the
    shared table, so terminals without emoji input work.
  - `/unreact`, and `/reactions` for who reacted.
  - Display width uses the terminal's emoji width, and the emoji's name is printed when
    it cannot be shown.
- **Web:**
  - Chips are `<button aria-pressed>`.
  - A reaction picker is reachable from the actions menu and by long-press, with touch
    targets of at least 44 px.
  - Voters appear in a dialog.

### 4.6 Notifications

Reactions never notify, play no sound and change no unread count. They also never
mention anyone. Notifying the author about reactions to their message is a separate
opt-in for later (D22).

### 4.7 Legacy projection

- **Nuschl server:** stock users see nothing. No `TextMessage` is generated per
  reaction, and they keep receiving the messages themselves.
- **Stock Murmur:**
  - Reactions travel as annotations between online Nuschl clients, best effort, and
    persist only on observing clients.
  - React is offered only on annotated Nuschl-authored messages. On stock-authored
    messages the action is disabled with the explanation "Reactions need a Nuschl
    message or server" (D3).
  - A visible text line for legacy users ("Alice reacted thumbs up to Bob: …") is
    off. Decision D2 asks whether to offer it as an explicit per-reaction choice.

### 4.8 E2EE

- The reference, emoji and add/remove are all inside the ciphertext. The server sees an
  MLS row: its size, time and sender account.
- Messages are padded to a common size, as the polls plan also asks, so reactions do
  not stand out from short messages (D23).
- Reaction envelopes need archive records so `members`-policy joiners see the same
  state. Under `since_join`, reactions from before the join are not visible.
- Membership: reactions from devices whose identity is no longer a member remain, since
  they were authenticated when sent. A removed member's later reactions cannot be sent.
- Deleted targets (once MLS deletion exists) drop their reactions locally.

## 5. Mentions (#189)

### 5.1 Protocol

**Compatible server (`mentions/1`, needs `history/1`):**

```proto
message Mention {
  oneof target {
    bytes  account = 1;   // 16 bytes
    uint32 user_id = 2;   // resolved to the account by the server under its state lock
                          // (nuschld never reuses registration numbers)
    string role = 3;      // a named ACL group in the conversation's channel, ≤ 64 bytes
  }
  uint32 start = 4; uint32 end = 5;   // the label's bytes in `content` (presentation only)
}
message ChatSend { repeated Mention mentions = 10; }
message ChatEdit { repeated Mention mentions = 6; }
message Message {
  repeated Mention mentions = 18;   // stored form: account (+ user_id, name snapshot) or role
  bool mentions_me = 19;            // per reader
}
// ConversationState: unread_mentions = 4, last_mention = 5; Direct: unread_mentions = 7
```

| Type (proposal) | Message | Reply |
|---|---|---|
| 0x4EB5 | `MentionTargets { request_id, conversation }` | `MentionTargetList` |
| 0x4EB6 | `MentionTargetList { request_id, conversation, repeated string roles (mentionable by the requester, ≤ 64), repeated string own_roles (≤ 64) }` | none |
| 0x4EB7 | `MentionNotice { conversation, seq }`, pushed to connections of mentioned accounts | none |

The text keeps a readable `@Name` label as the user saw it in the composer. Semantics
come only from `mentions`. Ranges exist only to style the label (D6). An invalid range
just leaves the label unstyled, and notification never depends on it.

**E2EE:** `ChatBody.mentions` holds account targets and ranges. Role targets are
deferred (D8).

**Stock:** the annotation's `mentions` holds certificate hashes. The text holds `@Name`.

### 5.2 Server validation, recipients and permissions

- Shape: count and bounds (§2.11); ranges valid; account and user targets must be
  registered accounts; no role mentions in direct conversations.
- Users: a user target needs no permission beyond sending. Only targets that can read
  the conversation at send time are notified. The sender is not told who could not
  read, so ACLs cannot be probed.
- Roles:
  - **Who may mention a role:** a role mention needs `Action::MentionRole`. The default
    is Write on the channel (the moderators), configurable per server as
    `mentions.role_mentions = "moderators" | "members" | "none"` (D7). It is a server
    and channel property outside the Mumble permission bitmask, for the reason given
    in `docs/extension.md:212-221`.
  - **Who counts as a member:** the role must be a named, non-meta group defined
    in the channel or inherited (`acl.rs:393`, `group_members`). The meta groups `all`,
    `auth`, `in`, `out`, `sub`, `~`, `$…` and `#…` are refused. Members are resolved
    by the authorisation owner at send time, then filtered by `can_read`. More than
    `max_role_recipients` is refused with `LIMIT_REACHED`. Role mentions have their
    own rate bucket.
- Storage: `mentions` on the row, plus `history_mentions` rows for each notified
  account (explicit users, resolved role members, and the reply parent's author unless
  `reply_quiet`). The cap is per account.
- Edits: an edit may change mentions and labels. Rendering follows, and removed
  mentions delete their rows. **Edits never notify** newly added targets (D10): editing
  must not become a way to ping repeatedly. Deleting a message drops its mentions
  (`docs/extension.md:271-273`).
- Live: `ChatEvent` as usual, plus `MentionNotice` to the mentioned accounts'
  connections that may read. Offline accounts get `unread_mentions` and `last_mention`
  in `Subscribed` and `DirectList`, counted above their read marker.
- `MentionTargets` returns only roles the requester may mention, plus the requester's
  own roles in that channel, for highlighting and for E2EE receivers. It never returns
  member lists or counts.

### 5.3 Client model and completion

- `MentionView { target: User { account, current name } | Role { name }, range,
  me: bool }`.
- Completion on `@`: candidates come from users currently present who can be resolved
  to accounts, plus loaded message authors with `author_account`, plus the conversation's
  direct peer, plus roles from `MentionTargets` (cached per connection generation and
  conversation). Duplicate names are disambiguated in the list by registration state,
  channel and certificate fingerprint suffix. Names with spaces are no problem, because
  the inserted label's range is explicit.
- The composer keeps mention entries as structured ranges that move with edits. Typing
  inside a label, or deleting part of it, turns it back into plain text: the structure
  is dropped, not guessed. A label pasted from elsewhere is plain text.
- Rendering: the content worker receives the ranges with the source in a new additive
  request `ContentRequest::RenderMarked { text, marks }`, leaving `Render` untouched.
  It sets `Span.mention: Option<u8>` (an index into the message's mention list) only
  where a range maps entirely onto visible non-code text. A label inside code stays
  literal. Styling is presentation; the notification decision was already made from
  the structured list.

### 5.4 Notifications and mute

- **Who is notified:**
  - compatible mode: an account is notified exactly when the server says so
    (`MentionNotice`, `mentions_me`);
  - E2EE: when the receiver's own account is a target, or the reply parent is its own,
    within the per-sender limit;
  - stock: when the receiver's own certificate hash is in a correlated annotation.

  Quoted text, code and pasted labels never notify, because only structured entries do.
- **What it does:** desktop notification and the Mention sound go through the existing
  rules. They respect the global `notifications` and `sound_effects` settings. They
  stay silent while deafened, since the driver suppresses sounds then. In the
  conversation in front of the user only the sound plays (`sounds::message`).
  `unread_mention` comes from structured mentions, not the heuristic (D9).
- **Out of scope:** per-conversation notification levels (all / mentions / nothing) and
  a "suppress role mentions" preference would need a settings sidecar. They are a
  separate feature needing approval (D16).

### 5.5 History and reconnect

`mentions_me` is computed per reader in pages. Unread mention counts come from the
server's mention rows. In E2EE they are computed on the device after decryption: the
server cannot count them, and content-free push (#173) cannot say "mention". Stock:
local only.

### 5.6 UI

- **Desktop:**
  - A completion popup under the caret on `@`. Arrows and Tab/Enter choose, and Escape
    closes it. It is announced through the existing polite live region, for example
    "3 people match".
  - Mention labels are styled as chips in the composer and the timeline.
  - A message that mentions you gets an accent bar, and its accessible description
    adds "mentions you".
- **TUI:** Tab after `@` cycles candidates in the status line. Mentions render bold or
  reversed. A message mentioning you gets a `@` marker in the gutter, and plain mode
  writes out "mentions you".
- **Web:** an ARIA combobox (`role="combobox"`, a listbox, `aria-activedescendant`)
  with the same styling. On touch the candidate list sits above the keyboard.

### 5.7 Legacy projection

- **Nuschl server:** the stock copy is `legacy_body(content)`, so stock users read
  `@Name`. Their clients apply whatever highlighting they have on their own; Nuschl
  promises no notification to stock users. Role labels read `@admins`.
- **Stock Murmur:**
  - The text carries `@Name` and the annotation carries certificate hashes, for online
    users with a certificate only.
  - Roles are unavailable, because the client cannot inspect stock group membership and
    must not invent it. Completion lists users only.
- **Documentation:** a table in `docs/desktop.md` / `docs/clients.md` lists which
  capabilities each server type offers.

### 5.8 E2EE

- User mentions sit inside the ciphertext and the server learns nothing. Receivers
  decide locally whether they are mentioned.
- **Role mentions are deferred (D8):** members cannot verify who may mention a role
  (server ACLs), and policy admins are not yet carried in the binding (`binding.rs:24-26`).
  Until the binding carries policy admins and a role-mention policy, the composer
  offers no role mentions in E2EE. Received role entries render as plain text and never
  notify (fail closed).
- `MentionTargets.own_roles` is ready for that later step. It reveals to the server
  nothing it does not already hold.

## 6. Threads (#192)

Threads exist on all three paths (owner guidance above). One reference type (§2.2), one
set of rules (§6.2) and one client model (§6.8) serve all of them. Only the carrier and
the trust differ.

### 6.1 Availability

| Path | Offered when | Otherwise |
|---|---|---|
| Nuschl compatible | The server negotiated `threads/1` (with `history/1` and `replies/1`), and the conversation stores history. Live-only conversations store nothing that could anchor or page a thread | No thread actions there. On an older server, threads are unavailable in extension conversations, as replies are (D20) |
| E2EE | `e2ee/1`; the conversation's lock is green (authenticated MLS state from the crypto worker, never an advertisement; `docs/architecture.md:288-293`); the group context requires `threads/1` (0xF4E2) and `interactions/1` (0xF4E1) | "Threads are available once end-to-end encryption is set up". Never the compatible or stock form |
| Stock (Murmur, a Nuschl server without the capability, a non-extension target) | PD session support; the root is an annotated Nuschl message (it has a `StockRef`); the text reaches ≤ 256 receivers | Stock-authored root: *Reply in thread* is replaced by *Reply* with a quote (D3). Too many receivers or no PD support: unavailable, with the reason |

A thread reply never silently becomes something else. When the structure cannot be
carried, the thread composer says why and offers to send the draft as a quote reply
instead, which the user confirms.

### 6.2 One representation, three carriers

| Path | A thread reply carries | Root reference |
|---|---|---|
| Nuschl compatible | `ChatSend.thread_root`, echoed as `Message.thread_root` | `Server { conversation, seq }` |
| E2EE | Kind 13 `ChatBody.thread = EnvelopeRef(root)` | `Envelope { conversation, hash, seq_hint }` |
| Stock | `StockAnnotation.thread_root` and `thread_quote` | `Stock { author, msg_id }` |

Rules, identical on every path:

- A thread reply lives in the **same conversation** as its root: the same `seq` stream
  (in E2EE also the same MLS group), and on stock servers the same channel or the same
  direct peer.
- The root is a top-level message, not itself a thread reply (**depth 1**), and not
  deleted when the reply is sent.
- A thread reply's `reply` names the root or another reply of the same thread. A
  top-level message's `reply` names a top-level message.
- nuschld enforces these at send time (§6.3). E2EE and stock receivers check them once
  the root is known. A message that breaks them is shown as an ordinary message, marked
  "sent as a thread reply that this version cannot place", and is never attached
  elsewhere. A stock message in that state renders the whole `T`, as legacy users see
  it.
- **No separate MLS group per thread (D13).** That would show the server thread
  existence, membership and timing per thread, and would multiply commits and
  membership-sync failure modes for no gain.

### 6.3 Nuschl servers, compatible mode

**Protocol** (`threads/1`; field numbers are proposals next to those in §3.1 and §5.1,
frozen by Root):

```proto
message ChatSend { uint64 thread_root = 11; }   // a seq in the same conversation; 0 for none
message ThreadSummary {                         // per reader
  uint32 replies = 1;
  bool   count_known = 2;                       // false: the reader's ranges miss part of the thread
  uint64 last_seq = 3; int64 last_ms = 4;       // the last reply this reader may see
}
message Message {
  uint64 thread_root = 22;       // on thread replies, and on edits and deletions of them
  ThreadSummary thread = 23;     // on roots that have replies
}
message HistoryRequest {
  enum View { ALL = 0; MAIN = 1; THREAD = 2; }
  View view = 8;                 // ALL is today's behaviour, and what older clients get
  uint64 thread_root = 9;        // with THREAD
}
```

| Type (proposal) | Message | Reply |
|---|---|---|
| 0x4EB8 | `ThreadQuery { request_id, conversation, repeated uint64 roots (≤ 100), before (a last_seq cursor), limit (≤ 50) }`: summaries for the given roots (the refresh of followed threads after a reconnect), or, without roots, the conversation's threads by last activity | `ThreadList` |
| 0x4EB9 | `ThreadList { request_id, conversation, repeated ThreadEntry { root_seq, Message root (absent when this reader cannot see it), ThreadSummary summary }, more }` | none |

`body_hash` covers `thread_root`, so an idempotent retry is the same thread reply.
Edits cannot change it, as with `reply_to`.

**Validation** under `threads/1`, tighter than for `reply_to`:

1. `thread_root` lies in the sender's visible ranges.
2. `HistoryAppend` carries `thread_check { root }` beside `reply_check` (§3.2). Inside
   the append transaction the storage worker checks that the root row exists under this
   conversation's `(conversation, seq)` key, has `kind = MESSAGE`, `deleted_ms IS NULL`
   and format `TEXT` or `LEGACY_HTML` (so it is neither an MLS row nor a tombstone), and
   has `thread_root IS NULL` (depth 1). With `reply_to` set too, the parent must be the
   root or carry the same `thread_root`. Without `thread_root`, the parent must be
   top-level. The worker returns the root's author and excerpt.
3. Every failure voids the seq and returns `NOT_FOUND`, one code for all reasons, so
   the check cannot probe rows outside the sender's ranges. The client holds the draft
   (§2.7): "The thread's first message is no longer available."
4. Stock users' messages are stored (`LEGACY_HTML`) and can be roots. Stock users cannot
   send into a thread; their messages stay top-level.

**Storage** (§2.10). `history_messages.thread_root` is set on thread replies. Edit and
delete rows copy their target's value, so each view pages through one index.
`history_threads` keeps the count and last reply per root, updated in the append
transaction and by the satellite hook. A deleted root stays as a tombstone: its replies
remain readable in the thread, and new replies are refused (as D19). An expired or
erased root leaves its replies to their own retention, and the thread shows "First
message unavailable".

**Paging and thread views.**

- `MAIN` returns top-level rows (with their edit, delete and void rows) through the
  partial index on `thread_root IS NULL`, so thread replies never fill a page. `THREAD`
  returns the root row and the rows with that `thread_root`. Anchors (`latest`,
  `before`, `after`, `around`), the 100-row limit, the 256 KiB cap, `restricted` and
  the reader's visible ranges apply within the view exactly as today. `ALL` is
  unchanged, for older clients and full synchronisation.
- Summaries on root rows are per reader. `replies` is the stored count when the
  reader's ranges cover the thread from its root to the head, which always holds under
  `members` visibility. Otherwise `count_known = false`, and `last_seq` and `last_ms`
  come from one lookup of the last reply within the reader's ranges; the UI then shows
  "Replies" without a number. Counts never reveal rows outside the reader's window.
- `ThreadQuery` lists threads whose root or last reply lies within the reader's ranges.
  It examines at most 4 × `limit` candidates of the `(conversation, last_seq)` index per
  request and then sets `more`. Threads whose root the reader cannot see appear there as
  "Thread (first message unavailable)", never as orphans in the main timeline.
- Live `ChatEvent`s carry `thread_root`. Clients route them to the thread and update the
  root's summary locally.
- Unread: the main timeline's count must leave thread replies out (D11). That needs a
  server-side count (top-level `MESSAGE` rows above the marker, saturating at 999).
  Until it lands, the main count also includes thread replies.

**Stock copy.** After the write, from the returned root and parent snapshots, nuschld
builds `thread::line(root) + reply::quote(parent) + legacy_body(content)` with the
shared builders (§6.5), never from client-supplied text. Stock receivers who may not
read every referenced row (outside their `since_access` ranges, or guests under
`history.guests = none`) get the label-free `↪ thread` and no quote, so there are at
most two variants per message. Murmur's text rules apply to the result: the line
degrades to author only, then label-free, then none, and the user's text is never
shortened.

### 6.4 Stock servers: annotations

**Sending.** A thread reply is an annotated message (§2.5). `thread_root` is the root's
`StockRef`, and `thread_quote` holds the root's author and excerpt as the sender's
client shows them. These are the thread line's inputs, so `T =
thread::line(thread_quote) + reply::quote(quote) + legacy_body(text)`. Data id,
receivers, pacing, digest, correlation and expiry are those of §2.5, unchanged.

**Receiving.** A correlated annotation (digest and prefix check passed) places the
message in its root's thread under the §6.2 rules, rendered without the line.

| Case | Result |
|---|---|
| Root not seen: joined later, offline, or the root's own annotation was lost | The thread appears in the Threads list as "Alice: first line… (first message not on this device; quoted by the sender, not verified)", from `thread_quote`. It is never matched by text against a message on screen |
| Root authored by a stock client | No `StockRef`, so no thread: *Reply in thread* is unavailable, and *Reply* sends a quote reply (D3) |
| Annotation lost or rate-limited; digest or prefix mismatch | Plain legacy text in the main timeline, visible thread line included. A late annotation within 2 s moves it into the thread |
| Annotation alone | Expires silently (§2.5) |
| Rate limits | As §2.5: one token of the text bucket and one of the plugin bucket, `nuschl/chat/1` served first. A missed slot sends the text alone, and the sender's copy is marked "Sent without thread information: others may see it outside the thread" |
| More than 256 receivers | Text only. The thread composer says so before sending and offers a quote reply |
| Payload | Worst case 726 bytes, 792 with a title (D26), within the 960-byte budget |

### 6.5 What legacy users see

- **Builder:** `nuschl_legacy_html::thread::line(author, excerpt, html) -> String`,
  beside `reply::quote`, with a budget function. With HTML it produces
  `<i>↪ thread: <b>{author}</b>: {excerpt}</i><br>`, escaped, with no attributes,
  well-formed XML. Without HTML it produces `↪ thread: {author}: {excerpt}\n`. The
  author-only form is `↪ thread: {author}` and the label-free form `↪ thread` (D25).
- **Excerpt:** the root's first line, control and bidi characters removed, at most 60
  UTF-16 units and 120 bytes, ending in "…" when cut. `LEGACY_HTML` roots give the
  author-only form, because nuschld parses no HTML (§7).
- **The legacy view:** legacy users read the conversation flat, and each thread reply
  names its thread. Their own answers stay top-level. Stock copies keep the line after
  the root is edited or deleted, as quotes do (§3.7). The thread composer warns once:
  "Stock Mumble users see the start of the first message with each reply."
- **Older Nuschl clients** on a `threads/1` server see thread replies flat, without
  the line (§2.8, D28).

### 6.6 E2EE

Unchanged in substance; E2EE is now one of three carriers.

- **What the server sees:** exactly what it sees for any MLS message, one more row with
  its size (padded), time, `seq` and sender account. It cannot tell a thread reply from
  a conversation message, or which root a reply belongs to. Not adding a keyed thread
  tag for server-side thread pages is decision D13. This is documented in
  `docs/e2ee.md` and `docs/extension.md` §10 with the slice.
- **No other form:** E2EE servers refuse plaintext `ChatSend`, and E2EE conversations
  never send annotations, so a thread reply is never sent as plaintext or as a plain
  reply.
- **Membership and history:** threads inherit the conversation's membership and history
  policy. `members`-policy joiners read archived thread replies through the same
  history grants. Under `since_join` they see only replies after joining, and nothing
  older is implied. Removed devices receive nothing after removal (key rotation,
  `docs/e2ee.md` §6.6). Archive records are required for kind 13.
- **Thread views:** the server cannot filter what it cannot see, so E2EE views do not
  use `THREAD` pages. They load from the local decrypted store first. When the root
  precedes local coverage, they page the conversation `after(root seq)` through the
  crypto worker within the normal page budgets, showing "Loading earlier replies" or
  explicit gaps (bounded, never the whole archive at once).

### 6.7 Trust: who can forge structure, and who sees it

| Path | Can forge or move thread structure | Sees thread structure |
|---|---|---|
| Nuschl compatible | nuschld, as it can forge any compatible message. Other clients cannot: the sender comes from the session, and the root is validated | The server: roots, participants and timing. Labelled "not end-to-end encrypted" like other extras |
| E2EE | Nobody without the group's keys: the root reference is inside the authenticated body. The server can still withhold or reorder, which the sender chain detects | The server: row metadata only |
| Stock | The server, which sets the sender and can drop, rewrite or forge annotations as it can text; a sender, for its own messages only. Annotations are only as trustworthy as the server | The server and every listed receiver, in clear |

Thread structure never grants access: stock receivers attach a reply only to a root they
hold in the same conversation, and nuschld only to a root the sender may read. A false
`presentation_prefix` hides nothing (§2.5).

### 6.8 Client model and storage

- `crates/client/src/threads.rs` (portable), keyed by `MessageRef` and therefore the
  same on every path:
  - `ThreadIndex` per conversation: root, then reply count (the server's summary, or
    counted locally), last activity, participants (≤ 3 shown, from loaded replies),
    followed, read position;
  - `ThreadView`, a timeline restricted to one root with its own window and paging,
    fed by `THREAD` pages, the decrypted store or the local stock store;
  - drafts keyed `(server identity, conversation, root)`.
- Local storage: `thread_state` and the root columns on messages (§2.10).
- Following: the user follows threads they started, replied in or were mentioned in,
  and any they follow explicitly. Followed-thread replies notify like a direct message
  (respecting the same settings). Others only mark the thread unread. Follow and read
  state are per device on every path in v1 (D29). After a reconnect the SDK refreshes
  followed roots with `ThreadQuery { roots }` (compatible) or the stream catch-up
  (E2EE). Stock servers offer nothing to refresh.
- Unread: thread replies do not increase the parent conversation's unread count (D11).
  Threads with unread replies show a badge on the root and in the Threads list. E2EE
  unread counts are computed after decryption, and read positions are per device until
  MLS self-blob read markers exist (#133).
- Binding: thread view tickets carry `(server identity, generation, conversation, root,
  navigation generation)`. Results for a replaced view are dropped. Switching between
  the conversation and a thread keeps both drafts.

### 6.9 UI

- **Desktop:**
  - Under a root, a summary reads "N replies · last reply 5 min ago" ("Replies · …"
    when the count is unknown) with up to 3 participants. It is activatable, with the
    accessible name "Thread, 4 replies, 1 unread. Activate to open."
  - The thread pane sits on the right on wide windows. On narrow windows (#1, #53) it
    replaces the timeline, with a Back button and Escape. Its header repeats the
    conversation's protection label ("Not end-to-end encrypted" on compatible and stock
    paths).
  - The pane has its own composer and draft. A "Threads" list in the conversation
    header shows followed and recent threads with unread counts, including threads
    whose first message is unavailable or not on this device.
  - *Reply in thread* appears in the message menu, and Alt+T on a focused message does
    the same. On a stock-authored message on a stock server it is absent, and *Reply*
    explains "Threads need a message sent from Nuschl".
  - Messages whose prefix was skipped offer *Show as Mumble users see it* (§2.5).
- **TUI:** `t` on a selected root, or `/thread`, switches the message pane to the
  thread with a header line. `/back` returns. `/threads` lists threads with unread
  counts. The composer prompt shows `thread>`.
- **Web:** a thread drawer on wide screens and a full-screen sheet on mobile (below
  760 px), with history (`pushState`) so the system Back button returns. Focus moves to
  the thread heading on open and back to the root on close.

### 6.10 Fail-closed rules

- **E2EE:** if the group loses green state (blocked, pending commit), thread sending
  stops with the reason shown, and reading what is already decrypted continues. If a
  crypto-worker or capability check fails, the thread UI is hidden, not degraded.
- **Compatible:** without `threads/1` no thread UI is shown. A refused root holds the
  draft; it is never sent as a top-level message without the user's choice.
- **Stock:** without PD support, for a root without a `StockRef`, or above 256
  receivers, threads are unavailable and the composer offers a quote reply explicitly.
- **Typing:** compatible typing stays per conversation; typing in E2EE waits for
  encrypted typing (#133).
- **Edits and deletion:** compatible thread messages are edited and deleted like any
  message (the rows copy `thread_root`). E2EE ones wait for MLS edit and delete (shared
  with polls). Stock servers have no edits.

## 7. AGENTS.md contracts, checked

- **Worker boundaries.** No new worker or process.
  - Untrusted text stays in the content worker. `RenderMarked` adds bounded ranges and
    returns inert spans.
  - The crypto worker decodes and validates kind 13 and Reaction bodies before
    returning typed fields, with structural checks before allocation and counts
    bounded. Decrypted state goes to the storage worker like other decrypted history.
    Neither worker gains the other's authority.
  - nuschld gains no content parser: it validates byte ranges only, never Markdown,
    and builds quotes and thread lines from stored text without parsing HTML (§6.5).
- **Trusted selection and fixed shapes.** No new executable or argument shape. Typed IPC
  additions in `crates/ipc/src/{lib,server,crypto}.rs` stay versioned and use
  `deny_unknown_fields`.
- **Bounds** on inputs and aggregates: §2.11. Every list has a count cap enforced before
  and after decoding (extension framing rule, `docs/extension.md:105-110`), and every
  queue and cache is capped.
- **Validation before allocation and rendering:** pages, pushes, annotations and MLS
  bodies are length-checked before decoding. Names, excerpts and quote text are
  sanitised of control and bidi characters (#61) and rendered as literal text.
- **Binding:** §2.7. Obsolete results cannot repaint or mutate a new target.
- **Fail closed:** missing capabilities hide features (§2.6). E2EE never falls back to
  plaintext or to the compatible and stock thread forms. A role-resolution failure
  refuses the send. A reply or thread reply whose target vanished is held, not
  stripped, and a thread reply whose structure cannot be carried becomes a quote reply
  only by the user's choice (§6.10). Annotations without correlation, or whose prefix
  does not match its recomputation, do nothing.
- **No privacy downgrade:** E2EE metadata stays in ciphertext and is padded. Compatible
  mode interactions, threads included, are labelled "not end-to-end encrypted" like
  other extras (#133's presentation invariant). Stock annotations are described as
  visible to the server and only as trustworthy as it (§6.7).
- **Cancellation and process credit:** unchanged, since no worker is added.
- **Browser parity:**
  - client-wasm gets the same SDK logic, so validation and bounds are identical.
  - The content WASM Worker gets `RenderMarked` with the same budgets.
  - The web relay forwards message 26 as bytes.
  - A Web Worker is not an OS sandbox, as `docs/browser-content.md` already states.

## 8. Test plan

Private resources only: loopback nuschld and private Murmur, raw protocol clients,
synthetic text, disposable profiles. No public servers.

| Layer | Cases |
|---|---|
| Unit: core | `MessageRef` scoping; emoji list (RGI with skin tones, ZWJ, flags, keycaps accepted; lone ZWJ, text-presentation digits, over-long sequences refused); reply quote and thread line builders: escaping, XML well-formedness, budgets at Murmur 5000/131072, 1000/8000 and 0/0, degradation to author only, label-free and none; `f(annotation)` identical for sender and receiver in HTML and plain modes; annotation codec bounds (worst case 726/792 B within 960 B, unknown version); thread rules (depth 1, same conversation, replies inside a thread) as a pure function over each `MessageRef` form |
| Unit: SDK | Action tickets refused after reconnect, conversation switch, certificate change, and when the parent is gone; composer contexts per conversation and thread; mention ranges moving with edits, and broken labels dropped; reaction intent coalescing and rollback; stock correlation: annotation-then-text, text-then-annotation within 2 s and after, annotation only (no message), text only (plain), digest mismatch after server rewrite (plain), prefix not equal to its recomputation (plain), duplicate `msg_id`, session reuse with a new instance (no match), buffer eviction; reactions from people not present ignored; stock threads: root known, root not seen (Threads list entry from `thread_quote`), stock-authored root (no thread, quote reply offered), annotation lost (plain text with the visible line) and a late annotation moving it into the thread, root in another channel or itself a thread reply (unplaced), more than 256 receivers and a missed pacer slot (composer notice, marked sender copy); `ThreadIndex` keyed by each `MessageRef` form; E2EE orphan buffer bounds |
| Unit: server (`apps/server/tests/extension.rs` style) | Reply validation: edit, delete, void and MLS rows, tombstone, outside `since_access`, `reply_revision > edits`, idempotent retry with and without reply; previews per reader (available, deleted, unavailable); stock copy quote built from the stored parent; reactions: permissions, guests refused, live-only refused, limits, idempotency, moderator clear, cascades on delete, expiry, erase and forget, pushes only to readers, coalescing, version; mentions: user and role resolution, meta groups refused, role permission, recipient cap, rate, `can_read` filter, edits not notifying, delete dropping rows, `unread_mentions` in `Subscribed`; threads: a root that is an edit, delete, void or MLS row, a tombstone, a thread reply, expired, outside `since_access`, or in a live-only conversation (all `NOT_FOUND` with the seq voided), a reply inside a thread naming a top-level message and the reverse, idempotent retry covering `thread_root`, edits keeping it; `MAIN` and `THREAD` views with every anchor, the byte cap and visible ranges; summaries exact under `members` and `count_known = false` under partial `since_access`; `ThreadQuery` limits and candidate cap; the stock copy's thread line from the stored root, label-free for a receiver outside the root's range and for guests; a client without `threads/1` gets `ALL` pages |
| Storage | SQLite and PostgreSQL migrations (`just test-postgres`); satellite hook in one transaction, including `history_threads` counts through append, delete, expiry, erase, forget and purge; thread partial indexes used by `MAIN` and `THREAD` in both backends; page joins within byte caps |
| Fuzz | `interaction_messages` (server: new frames and fields into nuschld state, thread fields, `HistoryRequest` views and `ThreadQuery` included), `stock_annotation` (decoder, prefix recomputation and correlation), `e2ee_chat_body` (crypto-worker body validation); additions to `client_messages` |
| Worker protocol | `RenderMarked` through the real content worker: marks inside code ignored, out-of-range marks refused, budgets kept |
| Crypto worker | Kind 13 and 4 only under the required group feature; archive records for both; late joiner reconstructs reactions and threads; `since_join` sees none from before; forged reaction for another identity's removal ignored; padding sizes |
| Integration: nuschld (`tests/nuschl_conversations.py`, `nuschl_history.py`, extended or new `tests/message_interactions.py`) | Two Nuschl clients and a raw stock client: reply with stock quote seen by the raw client; thread replies seen by the raw client with the thread line, excluded from `MAIN` pages and paged by `THREAD`; a stock user's message rooting a thread; reactions converge across reconnect and history load; mention notifies only the target; role mention notifies members who can read, refused for non-moderators |
| Integration: stock Murmur (`tests/stock_annotations.py`, new) | Two Nuschl clients (TUI test build) and a raw stock receiver on a private Murmur: the raw receiver gets exactly the presentation text and one message 26 per text; Nuschl receivers attach structure; a thread reply whose exact thread line the raw receiver sees and Nuschl receivers place in the thread; a root sent before a third Nuschl client joined (Threads list entry); a stock-authored root (no thread offered); rate-limit drops (burst above 15) degrade replies and thread replies to plain text with their visible lines; more than 256 receivers means no annotation; a cross-channel annotation injection is ignored |
| Integration: E2EE (`tests/e2ee_direct.py` pattern, `nuschl-cli` devices) | Reply, reaction, mention and thread between three devices; a fourth device joins later (`members`, then `since_join`); an older-client simulation in a group without the feature shows no false gaps because nothing new was sent |
| Accessibility (`tests/accessibility.py`) | Reply bar × named; reply line activatable; reaction chips have pressed state and names; completion listbox announced; thread summary and pane navigation, Back; Threads list entries for unavailable first messages; TUI plain mode transcripts |
| VM / GUI | Stock Mumble GUI screenshots of a reply quote and a thread line, from nuschld and from a Nuschl client on a private Murmur (`tests/stock_client.py` pattern); desktop scenes (`tests/conversation_screenshots.py`) for replies, reactions, mentions and threads at 100% and 200% |

Results are reported as build, unit, private runtime, VM or physical. None implies
another.

## 9. Phased slices and ownership

### 9.1 Owner keys

| Key | Owner |
|---|---|
| **MI** | The message-interactions lane (to be assigned) |
| **Root** | Codex: manifests, lock, crate2nix graph, deny.toml, SECURITY.md, TODO.md, architecture.md, the `docs/extension.md` registry, proto field and range allocation, migration numbers, final integration |
| **POLLS** | The polls lane |
| **E2EE** | The MLS lane: envelope kinds, group features, archive records, MLS deletion owner, binding policy admins |
| **PD** | The plugin-data lane (#93) |
| **BROKER** | The browser content broker lane (#172) |
| **LP** | Link previews |
| **MD** | The Markdown and content lanes (#190, #185) |
| **LINT** | The TUI-fixes and desktop-Clippy branches |

### 9.2 Slices

Effort is focused agent-days including tests, excluding review queues and VM waiting.

| Slice | Contents | Depends on | Effort |
|---|---|---|---|
| **F0** Pure foundation | `core::interaction` (refs, actors, limits, view types), `core::emoji` with the generated RGI table, `legacy-html::reply` and `legacy-html::thread` builders, the shared thread rules, stock annotation codec with `f(annotation)` and correlation buffer (`crates/client/src/stock_side.rs`), SDK action tickets and composer contexts. No protocol change, testable offline | Root allocates files | M: 3–4 |
| **F1** Protocol freeze | Proto additions and reservations (fields, 0x4EB0–0x4EBF, capabilities including `threads/1`), MLS feature ids, `nuschl/` data ids, `docs/extension.md` registry and §10 text | F0; POLLS field claims; PD data-id namespace; E2EE kind and feature allocation; design review of this plan | S: 1–2 |
| **F2** Storage and model | Server migration (thread column, partial indexes and `history_threads` included), IPC (`StoredMessage`, `PageQuery` reader account shared with POLLS, `StoredPage` extras), satellite hook (with POLLS), local migration 0005, `ServerRecord`/`HistoryEntry` fields, timeline fields, native page conversion | F1 | M: 4–6 |
| **R1** Replies, Nuschl compatible | Server validation (strict reply parents, §3.2), previews, post-store stock quote, `replies/1`; session and SDK; desktop and TUI UI; notifications | F2 | M: 5–7 |
| **X1** Reactions, Nuschl compatible | `apps/server/src/interactions.rs`, storage, pushes, version refresh; SDK; desktop and TUI chips, picker, voters | F2 (parallel with R1) | M: 5–6 |
| **M1** Mentions, Nuschl compatible | Server recipients, roles, notices, counts; `MentionTargets`; `RenderMarked` in the content worker; composer completion on desktop and TUI; structured notification path and heuristic change (D9) | F2; MD for `crates/content`; LP for the `ContentRequest` enum | L: 6–8 |
| **S1** Stock annotations | Send and receive wiring of F0's correlation for replies, mentions, reactions and threads; presentation prefix and its recomputation; Threads list entries for roots not seen; local persistence | PD session support; R1/X1/M1/T1 SDK parts | M: 4–5 |
| **W1** Web | snapshot/browser/RPC fields; ReplyBar, Reactions, MentionPicker; through BROKER's action fences | R1, X1, M1 SDK; BROKER merged | M: 4–5 |
| **E1** E2EE interactions | Kind 13 `ChatBody`, Reaction body, feature 0xF4E1, archive records for both, receiver validation, padding; session and SDK paths; E2EE replies, reactions and user mentions | F1; E2EE (kinds, features, GCE upgrade); POLLS-shared `MlsEncrypt` with kind and body and archive generalisation, done once | L: 7–9 |
| **T1** Threads, Nuschl compatible | `threads/1`; `thread_check`; `MAIN` and `THREAD` views; `ThreadQuery`; per-reader summaries; the stock copy's thread line; `threads.rs` (index, views, drafts, per-device follow and unread); session and SDK | F2; R1 (shares `reply_check` and the post-store stock copy) | M: 5–7 |
| **T2** Thread UI | Desktop pane and list, TUI thread mode, web drawer and sheet, accessibility, for every path | T1; W1 for web | M: 5–6 |
| **T3** E2EE threads | Feature 0xF4E2, kind 13 `thread` validation by receivers, thread paging through the crypto worker, on T1's `threads.rs` | E1; T1 | M: 3–4 |
| **E2** Encrypted deleted and edited parents | "Original deleted" and "edited since" states and reaction cleanup in E2EE | The MLS edit/delete owner (shared with POLLS) | S: 2 |
| **E3** E2EE role mentions | After binding policy admins and a role-mention policy | E2EE binding v3; D8 | S–M: 3 |

The order the issues ask for is foundation, replies, reactions, mentions, threads.
After F2, R1/X1/M1 can run in parallel on disjoint server and UI files. They share
`chat.rs`, `client.rs` and `timeline.rs` hunks, which one MI owner serialises. T1
follows R1 on the same `chat.rs` and storage paths. S1 waits for PD, E1 waits for the
shared MLS pieces, and T3 follows E1 and T1. Total about 60–80 agent-days.

### 9.3 New files (MI; no conflict)

| Path | Contents | Slice |
|---|---|---|
| `crates/core/src/interaction.rs` | `MessageRef`, actor ids, limits, `ReplyView`/`ReactionsView`/`MentionView` value types | F0 |
| `crates/core/src/emoji.rs`, `crates/core/assets/emoji-rgi.txt`, `crates/core/assets/emoji_rgi.py` | Generated RGI list (pinned Unicode 17 `emoji-test.txt`, fully qualified, with modifiers) and lookup | F0 |
| `crates/legacy-html/src/reply.rs`, `crates/legacy-html/src/thread.rs` | Quote and thread line presentation and their budgets | F0 |
| `crates/client/src/interactions.rs` | Tickets, composer contexts, reaction intents, mention completion and ranges | F0 / R1 |
| `crates/client/src/stock_side.rs` | Annotation codec use, correlation buffer, stock references | F0 / S1 |
| `crates/client/src/threads.rs` | Thread index and views, keyed by `MessageRef` | T1 / T3 |
| `apps/server/src/interactions.rs` | Reactions, mention targets and notices, recipient resolution, `ThreadQuery` | X1 / M1 / T1 |
| `crates/storage/src/server/interactions.rs` | Reaction, mention and thread queries | F2 / T1 |
| `crates/storage/server-migrations/{sqlite,postgres}/00NN_message_interactions.sql` | §2.10 | F2 |
| `crates/storage/migrations/{sqlite,postgres}/0005_message_interactions.sql` | §2.10 | F2 |
| `apps/desktop/src/interactions.rs`, `apps/desktop/src/thread.rs` | Reply bar, reply line, chips, voters dialog, completion popup; thread pane and list | R1 / X1 / M1 / T2 |
| `apps/tui/src/interactions.rs` | Commands, keys and lines | R1 / X1 / M1 / T2 |
| `web/app/src/{ReplyBar,Reactions,MentionPicker,Thread}.svelte` | Web UI | W1 / T2 |
| `fuzz/fuzz_targets/{interaction_messages,stock_annotation,e2ee_chat_body}.rs` | Fuzz targets (manifest entries are Root's) | F1 / S1 / E1 |
| `tests/message_interactions.py`, `tests/stock_annotations.py` | Private integration fixtures | R1–T1 |

### 9.4 Hunks in shared files

| Path | Hunk | Owner | Conflict risk | Slice |
|---|---|---|---|---|
| `crates/protocol/proto/nuschl.proto` | Fields on `ChatSend`, `ChatEdit`, `Message`, `HistoryRequest`, `ConversationState`, `Direct`; interaction and thread messages; stock annotation messages | MI, sequenced by Root | **High:** POLLS adds fields 7/15/16 and 0x4E98–0x4E9F to the same messages. Freeze both together | F1 |
| `crates/protocol/src/nuschl/mod.rs` | Capabilities, `ALL` (array length), `kind` constants, `required_capability`, limits | MI, sequenced by Root | High: the same lines as POLLS's `capability::POLLS` | F1 |
| `docs/extension.md` | Registry rows, a new section, §10 | MI drafts, Root merges | Medium: POLLS adds a registry row | F1 |
| `apps/server/src/chat.rs` | `chat_send`/`chat_edit` validation (reply and thread checks), `body_hash`, post-store stock copy in `stored()`, `wire_message` fields, `page_read` per-reader fields and views, `extension_frame` dispatch | MI | **High:** POLLS edits `chat_send`, `body_hash`, `wire_message`, `extension_frame` | R1 / X1 / M1 / T1 |
| `apps/server/src/{extension,authz,history,config}.rs`, `authz/tests.rs` | Offering capabilities; `Action::{React, MentionRole}`; `Done` variants; settings | MI | Medium (POLLS adds `Action::Vote`, `Done::Poll`, `polls/1`) | X1 / M1 / T1 |
| `docs/server.md` | New settings | MI | Low | X1 / M1 |
| `crates/ipc/src/server.rs` | `StoredMessage` fields, `PageQuery` reader account, `StoredPage` extras, new requests | MI, with POLLS for the reader account | High: same structs as POLLS | F2 |
| `crates/storage/src/server/history.rs` | Satellite hook in five writers, thread counts included; `reply_check` and `thread_check` in append; page joins and views | MI with POLLS (one hook) | High | F2 / T1 |
| `apps/storage-worker/src/main.rs`, `apps/storage-worker/tests/postgres.rs` | Request dispatch, tests | MI | Medium (POLLS) | F2 |
| `crates/core/src/lib.rs` | `pub mod interaction; pub mod emoji;`; `ServerRecord`/`HistoryEntry` fields; `Span.mention` | MI | Medium: LINT touches the core crate; `Span` is shared with the content lanes | F0 / F2 / M1 |
| `crates/ipc/src/lib.rs` | `ContentRequest::RenderMarked` | MI, sequenced by Root | Medium: LP adds `Preview` to the same enum | M1 |
| `crates/content/src/{lib,markdown}.rs` | Applying marks to spans | MI after MD | **High:** #190 quotes and #185 highlighting are active in these files; land after them | M1 |
| `apps/content-worker/src/main.rs` | One match arm | MI | Low–medium (LP, FD) | M1 |
| `crates/client/src/timeline.rs` | `Message` fields; reply and reaction change handling in the ordered queue | MI | **High:** BROKER adds `source_format`/`merge_with_receipt` to the same struct and impl. Land after BROKER | F2 |
| `crates/client/src/client.rs` | `send_text` with the draft context; react, voters, set_reply; notification path (structured mentions, D9) | MI | **High:** hub file; LP changes `send_text`/`PendingSend`; BROKER changes 94 lines | R1 / X1 / M1 |
| `crates/client/src/{platform,input,native,conversation,names,sounds}.rs` | Request and Input variants; page conversion; unread mentions; heuristic restriction | MI | Medium (LP adds platform methods with default bodies) | R1 / X1 / M1 |
| `crates/session/src/connection.rs` | `ChatSend` fields; history views and `ThreadQuery`; new requests and events; pending reactions; annotation queueing next to text in the outbox | MI after PD | **High:** POLLS adds `PollVote`; PD adds message-26 framing; the outbox is shared | R1 / S1 / T1 |
| `crates/session/src/mls.rs`, `crates/ipc/src/crypto.rs`, `apps/crypto-worker/src/mls.rs`, `crates/e2ee/src/{envelope,provider,client,binding}.rs` | Kinds 13 and 4 bodies; `MlsEncrypt` kind and body; archive generalisation; features; `Received` variants | E2EE owns; MI supplies the body schemas | **High:** POLLS needs the same `MlsEncrypt`, archive and kind changes. Do them **once** under E2EE | E1 / T3 |
| `apps/desktop/src/{menus,view,state,emoji,timeline_keys}.rs` | Menu entries; `message_row` reply line and chips; composer bar; drafts with context; picker reaction mode; keys | MI after LINT | Medium: LINT touches `menus.rs`/`view.rs` | R1 / X1 / M1 / T2 |
| `apps/tui/src/{app,view,plain,main}.rs` | Commands, keys, lines | MI after LINT | Low–medium (LINT, POLLS `/poll`) | R1 / X1 / M1 / T2 |
| `crates/client-wasm/src/{snapshot,browser,session}.rs`, `web/app/src/{App,Message}.svelte`, `sdk-rpc.js`, `action-context.js` | Views, RPC methods, fences; message 26 in the browser session | MI after BROKER; PD for the session | **High:** BROKER rewrites these | W1 / S1 / T2 |
| `tests/accessibility.py`, `tests/conversation_screenshots.py` | New scenes | MI | Low–medium | each |
| `fuzz/Cargo.toml`, `SECURITY.md` fuzz table, `docs/architecture.md`, `TODO.md` | Entries and status | Root | Low | each |
| `Cargo.toml`, `Cargo.lock`, `nix/crate2nix/Cargo.nix` | **None expected.** `sha2` is already a dependency of `nuschl-core`; prost types are generated | Root | — | — |

## 10. Open decisions

Recommendations first. Owner-level decisions are marked (owner); integration matters
(integrator).

- **D1** Compatible-mode reactions as state without `seq`, in pages and pushes, like
  poll votes (recommended), versus seq rows (these inflate unread counts and push
  messages out of pages).
- **D2** (owner) Stock Murmur reactions: annotations between online Nuschl clients
  only, with no legacy text (recommended), versus an explicit per-reaction "also tell
  Mumble users" line, versus no reactions on stock servers at all.
- **D3** (owner) Interacting with **stock-authored** messages on stock servers: quote
  replies only, no structured reactions and no threads (recommended), versus
  content-digest references with ambiguity refusal. Digests can still bind the wrong
  one of two identical messages for a receiver with a different window, so they are
  rejected here.
- **D4** (owner) A reply notifies the parent's author by default, with a per-reply
  toggle (recommended).
- **D5** (owner) Stock quote: author plus first line, ≤ 120 UTF-16 units and ≤ 160
  bytes (so the annotation can carry the quote's inputs, §2.5), with the composer
  warning that stock users keep it (recommended), versus author only.
- **D6** Mention labels in text with presentation-only ranges (recommended), versus
  placeholder tokens substituted by the server (breaks search, local cache and edits
  of the typed text).
- **D7** (owner) Role mentions need Write on the channel by default, configurable per
  server (`moderators`/`members`/`none`); named groups only; no meta groups and no
  `@here`/`@channel` in v1 (recommended).
- **D8** (owner) E2EE role mentions wait for policy admins in the binding
  (recommended), versus each receiver trusting server-published moderator lists.
- **D9** (owner) Restrict the name heuristic: no notifications from it on Nuschl
  servers with `mentions/1` or in E2EE; on stock servers it stays as an opt-out local
  highlight and sound over non-code, non-quote rendered text (recommended). This is a
  visible behaviour change.
- **D10** Edits never notify newly added mentions (recommended).
- **D11** (owner) Thread replies do not count in the parent's unread count; followed
  threads notify (recommended). On compatible servers an exact main count needs the
  server-side count in §6.3.
- **D12** (E2EE lane) Upgrading existing groups to `interactions/1`/`threads/1`: an
  add-only GroupContextExtensions commit by any member once every leaf advertises the
  feature (recommended), versus only for groups created after rollout.
- **D13** In E2EE, no thread metadata for the server and no group per thread
  (recommended), versus a keyed thread tag enabling server-side thread pages (reveals
  which rows belong together). Compatible and stock threads are visible to the server
  by design (§6.7).
- **D14** Emoji: the RGI list from a pinned Unicode 17 `emoji-test.txt`, with skin
  tones, in `nuschl-core` (recommended); custom server emoji stay out of scope.
- **D15** (owner) Limits: 20 distinct emoji and 5 per actor per message; 20 user and 2
  role mentions; 256 role recipients; 5 role mentions per hour (recommended starting
  values, generous in the spirit of `docs/extension.md:473-476`).
- **D16** (owner) Per-conversation notification levels and a "suppress role mentions"
  setting: a separate feature needing approval (not part of these issues).
- **D17** Stock annotation receivers: everyone the text reaches, capped at 256
  (recommended), versus discovering Nuschl peers first (more traffic, more state).
- **D18** No parent excerpt inside E2EE reply bodies (recommended): it would leak the
  parent to `since_join` readers.
- **D19** Replies to deleted parents, and thread replies to deleted roots, are refused
  (recommended).
- **D20** On an older Nuschl server without `replies/1` (or `threads/1`), Reply (or
  Reply in thread) is unavailable in extension conversations (recommended), versus
  writing a Markdown quote or thread line into the stored content (mixes presentation
  into data).
- **D21** (integrator) E2EE reaction state is aggregated by the SDK and storage
  (recommended), whereas polls counts in the crypto worker store. Either is bounded;
  pick one convention for future kinds.
- **D22** (owner) Later: an opt-in notification for reactions to one's own messages.
- **D23** (E2EE lane) Pad MLS application messages to a common bucket size (for
  example multiples of 256 bytes) for reactions, votes and short messages alike.
- **D24** (integrator) Allocations, all provisional until Root's registry freeze: range
  0x4EB0–0x4EBF (threads use 0x4EB8–0x4EB9);
  the field numbers above around POLLS's 7/15/16, including `ChatSend` 11, `Message`
  22–23 and `HistoryRequest` 8–9; capability `threads/1`; migration numbers; envelope
  kind 13; features 0xF4E1/0xF4E2; data ids `nuschl/chat/1` and `nuschl/react/1`.
  Also one owner for the shared MLS changes (`MlsEncrypt` kind and body, archive
  records for new kinds, MLS deletion) and for the storage satellite hook, used by
  POLLS and MI alike.
- **D25** (owner) The thread line for legacy users: `↪ thread: Alice: first line…` on
  its own line before the body (recommended), versus an inline `[thread: Alice: …]`
  prefix, versus author only (`↪ thread: Alice`).
- **D26** (owner) Explicit thread titles: none in v1, and the label is the root's
  author and first line (recommended). If wanted later, one optional `title` (≤ 64
  bytes) goes on the root in all three forms at once (`ChatSend`, `ChatBody`,
  `StockAnnotation`); the annotation budget already allows it.
- **D27** Stock prefix check: receivers skip the declared prefix only when it equals
  `f(annotation)` as they recompute it (recommended), versus trusting the declared
  length inside the digest-bound text, which is simpler but lets a sender hide up to
  4 KiB of its own text from Nuschl users.
- **D28** Older Nuschl clients on a `threads/1` server see thread replies flat, without
  a thread line (recommended), versus the server giving such connections stock-style
  content (breaks "content is what the user typed" and "a Nuschl connection never gets
  both").
- **D29** (owner) Thread follow and read state per device on every path in v1
  (recommended; E2EE has no other option until #133), versus server-synced per account
  in compatible mode (new tables and messages, but consistent across devices).
