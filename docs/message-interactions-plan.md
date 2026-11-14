# Message interactions: replies, reactions, mentions and threads

Planning document, 2026-11-13, against main `83de0df7`. Nothing here is
implemented. It is the design input that #191 (replies, including #117's
`reply_to`), #195 (reactions), #189 (mentions of users and roles) and #192
(threads) ask for before protocol, storage or MLS changes. It grants no
approval.

**Owner guidance (2026-11-13):**

> "For MLS/nuschl enabled servers all of this can ideally be more structured,
> and not recovered from plain text messages; the text message format only
> serves to be nice to look at for legacy mumble users. We can also use the
> PluginDataTransmission if it's cleaner - then presentation and parsing don't
> have to interfere."

> "Given that we can send arbitrary plugin data now we can also implement
> threads for mumble or compatible servers. We just in general need to make sure
> that we don't also display the visible mumble message that legacy mumble
> clients get? We could e.g. generate the mumble message with a deterministic
> function - roughly `f(plugindata)` and then ignore that generated message
> once."

So every interaction is structured data with a stable reference, on every path,
and nothing is parsed back out of display text. On stock Mumble the visible text
is presentation for legacy users, and the structure travels between Nuschl
clients in `PluginDataTransmission` (message 26). Threads exist on all three
paths, not only in E2EE.

## 1. Facts that shape the design

- **MLS rows drop the server's unsigned `reply_to`, `target` and `kind`,** so
  E2EE replies must live inside the ciphertext.
- **An unknown envelope kind shows a false gap:** an older client fails to
  decode it and records a "withheld" gap in the sender's chain. New kinds need
  an authenticated group gate (§2.5).
- **Message 26 is ≤ 1,000 bytes and is forwarded to the listed sessions without
  channel authorisation.** The server sets the sender session; the bucket is 4/s
  with a burst of 15, and excess is dropped silently.
- **`names::mentions` is a heuristic:** a whole-word match of one's user name,
  over rendered text including code, or the raw body including markup.
- The server's `seq` is per conversation and never reused. `reply_to` is on the
  wire but unused. Sends and edits share Murmur's text bucket (1/s, burst 5).
- The polls plan (#26) settled shared questions the same way: MLS references by
  envelope hash, the verified identity as actor, compatible-mode votes as state
  without `seq`. It claims envelope kinds 11–12, range 0x4E98–0x4E9F, `ChatSend`
  field 7 and `Message` fields 15–16.

## 2. Shared foundation

### 2.1 One rule per kind of server

| | Nuschl compatible (`history/1`) | E2EE (`e2ee/1`) | Stock Murmur, or no capability |
| --- | --- | --- | --- |
| Carrier | Typed extension fields, validated, stored and authorised by nuschld | MLS ciphertext, behind group features (§2.5) | Message 26 annotations (§2.4) |
| Legacy presentation | The server builds the stock copy | None | The visible `TextMessage` |
| Replies | `replies/1`, per-reader previews | Kind 13 `reply` | `reply` and `quote` |
| Reactions | `reactions/1`, state without `seq` | Kind 4 bodies | `nuschl/react/1`, seen on this device only |
| Mentions | `mentions/1`, users and roles | Users | Certificate hashes, users only |
| Threads | `threads/1`, thread pages | Kind 13 `thread` | `thread_root` and `thread_quote` |
| Stock-authored target | Stored, so everything applies | — | Quote replies only (D3) |

### 2.2 References and actors

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
    /// hash (SHA-1, as `UserState.hash`) and the 16-byte message id the author
    /// chose in its annotation (§2.4). Resolvable only by clients that saw it.
    Stock { author: [u8; 20], msg_id: [u8; 16] },
}
```

- A thread root is a `MessageRef` like a reply parent. Client-side keys also
  carry the server history identity, so a reference never resolves on another
  server.
- A server reply records `reply_revision`, the parent's `edits` the replier saw,
  so readers can show "edited since". An MLS reference is the original
  envelope's hash, which edits keep. Reactions, mentions and thread roots bind
  to the message, not a revision.
- Stock-authored messages on stock servers, and messages a Nuschl server didn't
  store, have no reference: quote replies only. Content digests are not used as
  references, because identical texts collide (D3).

| Path | Actor | Never |
| --- | --- | --- |
| Nuschl compatible | Account id (16 bytes) | Names, session ids; registration numbers only as an alias the server resolves |
| E2EE | Verified identity fingerprint; all devices of one identity act as one | The server's attribution alone |
| Stock | Certificate hash from `UserState.hash`, plus the presence instance | Session ids alone, names |

Unregistered users, and stock users without a certificate, may reply but can't
react or be mentioned.

### 2.3 Where structure goes

- **Compatible:** new fields on `ChatSend`, `ChatEdit`, `Message` and
  `HistoryRequest`, and new types in a provisional range **0x4EB0–0x4EBF**
  (D24).
- **E2EE:** a new durable kind **13** with a protobuf `ChatBody { text, reply,
  mentions, thread, reply_quiet }`, and a protobuf body for kind 4 (`Reaction`).
  Plain `Chat` (1) stays for groups without the feature. The crypto worker
  validates these bodies against §2.9 before returning typed fields.
- **Stock:** annotations in message 26.

### 2.4 The stock side channel

Used on stock Murmur and Nuschl servers without `history/1`, never in place of
typed messages. Generic message-26 support (framing, the reserved `nuschl/`
namespace, exact receivers, pacing, delivery with sender and presence instance)
belongs to #93, in both `nuschl-session` and `crates/client-wasm`. This plan
owns the payloads and the correlation in the portable SDK.

Two data ids (proposals), payloads ≤ **960 bytes** encoded:

```proto
message StockAnnotation {            // data id "nuschl/chat/1": one per TextMessage
  uint32 version = 1;                // 1
  bytes  msg_id = 2;                 // 16 random bytes chosen by the sender
  bytes  text_digest = 3;            // SHA-256("nuschl/stock-text/1" || TextMessage.message)[..16]
  uint32 presentation_prefix = 4;    // length of f(annotation): the legacy-only leading bytes
  StockRef reply = 5;                // the parent, when it is an annotated Nuschl message
  StockQuote quote = 6;              // the reply quote's inputs, whenever the text carries one
  repeated bytes mentions = 7;       // ≤ 8 mentioned users' certificate hashes (20 bytes each)
  bool reply_quiet = 8;              // do not notify the parent's author
  StockRef thread_root = 9;          // the thread's root, an annotated Nuschl message (§6)
  StockQuote thread_quote = 10;      // the thread line's inputs: the root as the sender saw it
}
message StockRef   { bytes author = 1; bytes msg_id = 2; }        // 20 + 16 bytes
message StockQuote { string author = 1; string excerpt = 2; }     // ≤ 64 B; ≤ 160 B (quote) or ≤ 120 B (thread); inert
message StockReaction {              // data id "nuschl/react/1"
  uint32 version = 1; StockRef target = 2; string emoji = 3; bool add = 4;
  uint32 op = 5;                     // per-sender counter: drop replays and reorders
}
```

The worst case is 726 bytes, or 792 with a thread title (D26). The quote fields
are what a receiver shows when it doesn't hold the referenced message ("quoted
by the sender, not verified").

**The generated prefix.** `f(annotation)` is `thread::line(thread_quote)` then
`reply::quote(quote)`, each when set, built with the server's `allow_html`. The
sender makes all cuts before filling the fields, so `f` is a pure function any
receiver can recompute.

**Sending:** build `T = f(annotation) + legacy_body(text)` and digest exactly
the string to send. Send the annotation to the sessions the text reaches as this
client sees them (channel plus visible listeners, or the direct peer), at most
256; above that, text only, and the composer says "Nuschl extras unavailable
here (too many people)". Queue it **immediately before** the `TextMessage`, in
the same pacing slot. If the pacer misses the slot, the text goes alone and the
sender's copy is marked as sent without Nuschl information. The pacer serves
`nuschl/chat/1` before reactions.

**Receiving:**
1. Decode strictly and buffer under `(connection generation, sender session,
   presence instance, text_digest)` for 5 s.
2. A `TextMessage` from the same sender correlates when its digest matches
   **and** its first `presentation_prefix` bytes equal the receiver's own
   `f(annotation)`. It then gets the annotation's structure and renders
   `T[presentation_prefix..]`. Comparing against bytes the receiver built itself
   is not parsing.
3. Text without an annotation is shown at once as plain legacy text, with no
   semantics. A late annotation within 2 s upgrades it in place through the
   ordered change queue.
4. An annotation without text expires silently; it never creates a message,
   notification or reaction target. A duplicate `(author, msg_id)` is ignored.

**Why the body stays in the `TextMessage`** rather than being generated from
plugin data: plugin data is capped at 1,000 bytes while chat text may reach
5,000; the `TextMessage` is the only part the server authorises by channel
rules; and two copies of the body could diverge. So `f` generates only the
prefix. Legacy users see `T` whole, Nuschl users see the body once with its
structure, and a false length can't hide text (D27). The message menu offers
"Show as Mumble users see it".

**Trust.** Correlation needs the same session and presence instance, so nobody
can attach structure to someone else's text. A reaction counts only if its
target is a known annotated message, the reactor is in or listening to that
channel (or is the direct peer) and has a certificate, and the sender is within
the rate limit. Annotations are as trustworthy as the server, which can drop,
rewrite or forge them as it can text, so the UI shows no verification mark. The
server and listed stock clients see them in clear. Stock servers keep no
history: structure exists only on clients that saw it, and the UI says "seen on
this device" where that matters.

### 2.5 Capability gates

All fail closed and never downgrade silently.

- **Server capabilities** (sticky): `replies/1`, `reactions/1`, `mentions/1` (on
  `e2ee/1` servers only the `MentionTargets` query), `threads/1` (needs
  `history/1` and `replies/1`). Typed fields go only to servers that negotiated
  them.
- **MLS group features** in leaf capabilities and `RequiredCapabilities`:
  **0xF4E1** `interactions/1` (kind 13 and Reaction bodies, with archive
  records) and **0xF4E2** `threads/1`. Clients send these kinds only when the
  authenticated group context requires the feature, and receivers refuse them
  otherwise, so older clients never see false gaps. Existing groups upgrade by
  an add-only GroupContextExtensions commit (D12). An E2EE conversation never
  falls back to the compatible or stock forms.
- **Stock:** without message-26 support, presentation text only.

### 2.6 Binding asynchronous work

- Every action captures a ticket `{server history identity, connection
  generation, conversation, MessageRef, revision seen}`. The SDK sends only if
  the generation and conversation are unchanged and the message still resolves,
  and never re-resolves by timeline index or UI serial.
- A reply or thread context belongs to the draft, keyed by `(server history
  identity, conversation, thread root or none)`. If its target becomes
  unavailable before Send, the send is held: "The message you replied to is no
  longer available. Send without the reply?"
- Previews, reaction updates, voter pages, mention targets and thread pages
  carry the ticket or navigation generation and are dropped when stale.
- Updates for a message that hasn't arrived wait, bounded, in the ordered change
  queue, as edits do.

### 2.7 Older clients and plain text

- Older clients on Nuschl servers ignore the new fields: same text, no
  reactions, thread replies flat without a thread line (D28). Older Nuschl
  clients on stock servers ignore message 26 and see the presentation text.
- Unknown durable MLS kinds should become a placeholder that still advances the
  sender chain (E2EE lane, shared with polls).
- **The name heuristic (D9):** on Nuschl servers with `mentions/1` and in E2EE,
  only structured mentions notify. On stock servers it remains an opt-out local
  highlight and sound for legacy senders, over rendered non-code, non-quote
  text, and never creates mention state.

### 2.8 Storage

- **Server migration** `00NN_message_interactions.sql` (SQLite and PostgreSQL):
  `history_messages` gains `reply_revision`, `mentions` (≤ 4 KiB) and
  `thread_root`, with partial indexes for thread and top-level rows; new tables
  `history_threads` (count and last reply per root), `history_reactions` with a
  per-conversation version, and `history_mentions`.
- **One satellite hook,** called by delete, expire, erase, forget-account and
  purge, removes reactions, mention rows (and poll ballots) and adjusts thread
  counts **in the same transaction**.
- **Local migration** `0005_message_interactions.sql`: author account, identity
  and certificate, the message's own reference, reply and thread-root
  references, `mentions_me`, the bounded annotation, and tables for reactions
  and thread state. Older rows have no interactions.
- **SDK timeline:** `Message` gains the author account and identity,
  `reference`, `reply`, `reactions`, `mentions`, `mentions_me`, `thread` and
  `thread_root`.

### 2.9 Bounds

Stated only here.

| Item | Bound | Over the bound |
| --- | --- | --- |
| Reply parent | 1 per message, same conversation, a live `MESSAGE` row readable by the sender | `NOT_FOUND` / shown unavailable |
| Reply preview | First line, ≤ 256 bytes, control and bidi characters removed, literal text; ≤ 1 per page row within the 256 KiB page cap | Cut with "…" / page ends earlier |
| Stock quote | Author ≤ 64 bytes; excerpt ≤ 120 UTF-16 units and ≤ 160 bytes | Author only, then none; the user's text is never shortened |
| Thread line | Author ≤ 64 bytes; root's first line ≤ 60 UTF-16 units and ≤ 120 bytes | Author only, then `↪ thread`, then none |
| Emoji | ≤ 32 bytes, ≤ 10 scalar values, in the RGI table (§7) | `MALFORMED` / ignored |
| Reactions per message | 20 distinct emoji, 5 per actor; pages carry ≤ 20 counts with ≤ 3 names; voters ≤ 256 per page | `LIMIT_REACHED` / ignored / `more` |
| Reaction requests | `MAX_OUTSTANDING = 16`; 60/min per account, burst 20; SDK keeps the newest intent per (message, emoji); pushes ≤ 1 per target per 250 ms per connection | `RATE_LIMITED` with `retry_after_ms` / coalesced |
| Mentions per message | ≤ 20 users and ≤ 2 roles; label ≤ 128 bytes; ranges ascending, disjoint, on character boundaries | `MALFORMED` / unstyled |
| Role mentions | ≤ 256 recipients; ≤ 5 per account per hour | Send refused / `RATE_LIMITED` |
| Stored mentions | ≤ 1,000 rows per account; counts saturate at 999 | Oldest dropped |
| E2EE receivers | Mention notifications ≤ 10 per sender per 10 min; ≤ 256 pending events for unknown targets per conversation | Ignored / oldest evicted |
| Threads | Depth 1; `MAIN` and `THREAD` pages keep the 100-row and 256 KiB limits; `ThreadQuery` ≤ 100 roots, or ≤ 50 entries from ≤ 200 candidates | `NOT_FOUND` / `more` |
| Threads per client | ≤ 4,096 tracked and ≤ 256 followed roots per conversation; counts saturate at 9,999; ≤ 1,000 messages per thread window | Oldest untracked |
| Drafts | ≤ 30 and ≤ 128 KiB in total, contexts included | Oldest dropped |
| Stock annotations | ≤ 960 bytes; prefix ≤ 4 KiB; ≤ 256 receivers; buffer ≤ 8 per sender and ≤ 64 per connection, 5 s TTL, 2 s late window | Not sent / evicted / plain text |
| Stock reactions | Sent ≤ 2/s per client; accepted ≤ 10/s per sender | Dropped |

## 3. Replies (#191)

**Compatible (`replies/1`):**

```proto
message ChatSend {                 // existing fields 1-6; 7 is the polls lane's
  uint32 reply_revision = 8;       // the parent's `edits` the sender saw
  bool   reply_quiet = 9;          // do not notify the parent's author
}
message ReplyPreview {
  enum State { AVAILABLE = 0; DELETED = 1; UNAVAILABLE = 2; }
  uint64 seq = 1; State state = 2;
  Author author = 3;               // empty unless AVAILABLE or DELETED
  string excerpt = 4;              // AVAILABLE only
  uint32 edits = 5;
}
message Message {                  // 15-16 are the polls lane's
  ReplyPreview reply = 17;         // per reader; only when `reply_to` is set
  uint32 reply_revision = 21;
}
```

- **Validation** inside the append transaction: the parent lies in the sender's
  visible ranges and is a live `MESSAGE` row in `TEXT` or `LEGACY_HTML` with
  `reply_revision ≤ edits`. On failure the seq is voided and the client gets
  `NOT_FOUND`. Replies to deleted parents are refused (D19). `body_hash` covers
  the new fields, so a retry is the same reply.
- **Previews per reader:** `UNAVAILABLE` outside the reader's ranges, expired or
  erased; `DELETED` for a tombstone; otherwise `AVAILABLE`.
- **E2EE:** `ChatBody.reply = EnvelopeRef { hash, seq_hint, revision }`, with no
  excerpt, so a `since_join` reader can't learn a parent it may not read (D18).
  "Original message deleted" needs MLS deletion (E2).
- **Stock:** the annotation's `reply` and `quote`.
- **Client:** `ReplyView` is `Available { author, excerpt, edited_since }`,
  `Deleted`, `Unavailable`, `NotLoaded` (MLS: "Load" pages `around(seq_hint)`
  and checks the hash) or `QuotedBySender`. Previews are never stored as truth.
  The reply line jumps to the parent; excerpts are plain text, never Markdown.
- **Notifications:** a reply to your message counts as a mention unless the
  replier turned off the bell in the reply bar (`reply_quiet`, D4).
- **Older servers** without `replies/1`: Reply is unavailable in extension
  conversations (D20).

**Legacy projection.** `nuschl_legacy_html::reply::quote(author, excerpt, html)`
produces `<blockquote><b>{author}</b>: {excerpt}</blockquote>` (escaped, no
attributes, well-formed XML) or `> {author}: {excerpt}\n`.
- nuschld builds `quote(parent) + legacy_body(text)` **after** the write, from
  the stored parent, never from client text. Stock receivers who may not read
  the parent (outside `since_access`, or guests under `history.guests = none`)
  get it without the quote.
- On stock servers the sender builds the same text from the annotation.
- Stock users keep quotes after the parent changes. The reply bar warns "Stock
  Mumble users also see a quote of this message", and the delete confirmation
  adds "and quotes of it".

## 4. Reactions (#195)

**Compatible (`reactions/1`; state without `seq`, D1):**

| Type | Message | Reply |
| --- | --- | --- |
| 0x4EB0 | `ReactionSet { request_id, conversation, target (seq), emoji, add }` | `ReactionUpdate` |
| 0x4EB1 | `ReactionUpdate { request_id (0 when pushed), conversation, target, summary: ReactionSummary { repeated ReactionCount { emoji, count, mine, repeated Author first (≤ 3) } (≤ 20), version }, applied }` | — (also pushed) |
| 0x4EB2 | `ReactionQuery { request_id, conversation, target, emoji, after (account), limit ≤ 256 }`; with an empty emoji, summaries for up to 100 `targets` | `ReactionVoters`, or `ReactionUpdate`s then `Done` |
| 0x4EB3 | `ReactionVoters { request_id, conversation, target, emoji, repeated Author authors, more }` | — |
| 0x4EB4 | `ReactionClear { request_id, conversation, target, emoji }` (moderators) | `ReactionUpdate` |

`Message.reactions = 20` carries the summary in pages, with `mine` per reader.
`ConversationState.reactions_version = 6` (and on `Direct`) lets a reconnecting
client refresh only when something changed.

- **Server:** `Action::React` needs a registered account with `can_write`, not
  in live-only conversations; the target is a live `MESSAGE` row in the actor's
  ranges. Reactions have their own bucket, since the text bucket would starve
  chat. Add and remove are idempotent. Only one's own rows can be removed;
  `ReactionClear` needs `Action::Moderate`, is logged without content, and shows
  "removed by a moderator". Pushes go only to connections that may read the
  target.
- **E2EE:** kind 4 body `ReactionBody { EnvelopeRef target; string emoji; bool
  add }`, aggregated by the SDK per `(target hash, identity, emoji)` in stream
  order and stored with local history (D21). Archive records give
  `members`-policy joiners the same state. Messages are padded to a common size
  so reactions don't stand out (D23).
- **Stock:** offered only on annotated Nuschl messages ("Reactions need a Nuschl
  message or server", D3), labelled "Reactions seen on this device", with no
  legacy text line (D2). Stock users on Nuschl servers see nothing.
- **Client:** optimistic intents, rolled back on refusal and resent idempotently
  after a reconnect. Chips are pressed when yours and toggle; *React…* opens the
  existing emoji picker; the TUI accepts emoji names.
- Reactions never notify, sound, change unread counts or mention anyone (D22).

## 5. Mentions (#189)

**Compatible (`mentions/1`):**

```proto
message Mention {
  oneof target {
    bytes  account = 1;   // 16 bytes
    uint32 user_id = 2;   // resolved to the account by the server under its state lock
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

| Type | Message | Reply |
| --- | --- | --- |
| 0x4EB5 | `MentionTargets { request_id, conversation }` | `MentionTargetList` |
| 0x4EB6 | `MentionTargetList { request_id, conversation, repeated string roles (mentionable, ≤ 64), repeated string own_roles (≤ 64) }` | — |
| 0x4EB7 | `MentionNotice { conversation, seq }`, pushed to mentioned accounts | — |

- The text keeps a readable `@Name` label. Semantics come only from `mentions`;
  ranges only style the label (D6), and nuschld checks them as byte ranges
  without parsing Markdown.
- **Server:** targets must be registered accounts; only those who can read the
  conversation are notified, and the sender isn't told who couldn't. A role
  mention needs `Action::MentionRole`, by default Write on the channel,
  configurable as `mentions.role_mentions = "moderators" | "members" | "none"`
  (D7). Roles are named, non-meta groups of the channel; `all`, `auth`, `in`,
  `out`, `sub`, `~`, `$…` and `#…` are refused, and none in direct
  conversations. Members are resolved at send time and filtered by `can_read`;
  too many refuse the send. **Edits never notify** new targets (D10).
  `MentionTargets` returns only mentionable and own roles, never members.
- **E2EE:** `ChatBody.mentions` with accounts. Role mentions wait for policy
  admins in the group binding (D8); received role entries render as plain text
  and never notify.
- **Stock:** certificate hashes of online users in the annotation. No roles:
  stock group membership is invisible to clients.
- **Client:** `@` completes present users with accounts, loaded authors, the
  direct peer and mentionable roles, disambiguating equal names by registration,
  channel and certificate suffix. Mentions are ranges that move with edits;
  editing inside a label or pasting one gives plain text. A new
  `ContentRequest::RenderMarked { text, marks }` styles a mention only where its
  range covers visible non-code text.
- **Notifications** come only from structured mentions (compatible: when the
  server says so; E2EE: own account or own reply parent, within the per-sender
  limit; stock: own certificate hash in a correlated annotation). They follow
  existing settings and stay silent while deafened. Levels per server and
  conversation are #210 (D16).

## 6. Threads (#192)

| Path | Offered when | Thread reply carries | Otherwise |
| --- | --- | --- | --- |
| Nuschl compatible | `threads/1` and stored history | `ChatSend.thread_root` (a seq) | No thread actions (D20) |
| E2EE | Green lock and the group requires 0xF4E1 and 0xF4E2 | Kind 13 `ChatBody.thread` (`EnvelopeRef`) | "Threads are available once end-to-end encryption is set up" |
| Stock | Message-26 support, an annotated root, ≤ 256 receivers | `thread_root` and `thread_quote` | Quote reply, offered explicitly (D3) |

**Rules on every path:**
- A thread reply lives in the same conversation as its root, and the root is a
  live top-level message (**depth 1**).
- A thread reply's `reply` names the root or a reply of the same thread; a
  top-level message's `reply` names a top-level message.
- nuschld enforces this at send time. E2EE and stock receivers check it once the
  root is known and show violators as ordinary messages marked "sent as a thread
  reply that this version cannot place".
- A thread reply never silently becomes something else: when the structure can't
  be carried, the composer offers a quote reply, which the user confirms.
- No MLS group per thread (D13).

### 6.1 Nuschl servers

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

| Type | Message | Reply |
| --- | --- | --- |
| 0x4EB8 | `ThreadQuery { request_id, conversation, repeated uint64 roots (≤ 100), before (last_seq cursor), limit (≤ 50) }`: summaries for given roots, or threads by last activity | `ThreadList` |
| 0x4EB9 | `ThreadList { request_id, conversation, repeated ThreadEntry { root_seq, Message root (absent if unreadable), ThreadSummary summary }, more }` | — |

- **Validation** inside the append transaction, beside the reply check: the root
  is in the sender's ranges and is a live `MESSAGE` row in `TEXT` or
  `LEGACY_HTML` without its own `thread_root`. Every failure voids the seq and
  returns one `NOT_FOUND`, so the check can't probe. Edits can't change
  `thread_root`.
- Stored stock messages can be roots, but stock users can't post into threads. A
  deleted root stays a tombstone: replies stay readable, new ones are refused
  (D19).
- `MAIN` pages top-level rows through a partial index, so replies never fill a
  page; `THREAD` pages one root and its replies; `ALL` is unchanged. Summaries
  are per reader, and `count_known = false` when the reader's ranges miss part
  of the thread, so counts never reveal rows outside them.
- Threads with an unreadable root appear in `ThreadQuery` as "Thread (first
  message unavailable)", never as orphans in the main timeline.
- The main unread count leaves thread replies out (D11), which needs a
  server-side count of top-level rows.
- **Stock copy:** `thread::line(root) + reply::quote(parent) +
  legacy_body(content)` from stored rows; receivers who may not read them get
  the label-free `↪ thread` and no quote.

### 6.2 Stock servers and legacy users

- `T = thread::line(thread_quote) + reply::quote(quote) + legacy_body(text)`;
  everything else is §2.4.
- **Root not seen** (joined later, offline, annotation lost): the Threads list
  shows "Alice: first line… (first message not on this device; quoted by the
  sender, not verified)", never matched by text against a message on screen.
- **The line:** `<i>↪ thread: <b>{author}</b>: {excerpt}</i><br>`, or `↪ thread:
  {author}: {excerpt}\n` without HTML, degrading per §2.9 (D25). `LEGACY_HTML`
  roots give the author-only form, since nuschld parses no HTML.
- Legacy users read the conversation flat, with each thread reply naming its
  thread. The thread composer warns once: "Stock Mumble users see the start of
  the first message with each reply."

### 6.3 E2EE

The server sees one more MLS row and can't tell a thread reply from any other
message. Threads inherit membership and history policy, and archive records are
required for kind 13. Thread views load from the local decrypted store, then
page `after(root seq)` through the crypto worker, showing explicit gaps.

### 6.4 Trust

| Path | Can forge or move thread structure | Sees it |
| --- | --- | --- |
| Nuschl compatible | nuschld, not other clients | The server ("not end-to-end encrypted") |
| E2EE | Nobody without the group's keys; withholding and reordering are detected by the sender chain | The server: row metadata only |
| Stock | The server; a sender for its own messages only | The server and every listed receiver |

Thread structure never grants access: a reply attaches only to a root the
receiver holds in the same conversation.

### 6.5 Client

- `crates/client/src/threads.rs`, keyed by `MessageRef` and so the same on every
  path: a per-conversation index (count, last activity, followed, read position)
  and thread views fed by `THREAD` pages, the decrypted store or the local stock
  store.
- You follow threads you started, replied in or were mentioned in, plus any you
  choose. Followed-thread replies notify like direct messages; others only mark
  the thread unread. Follow and read state are per device in v1 (D29).
- **Fail closed:** in E2EE, losing the green state stops thread sending with the
  reason, and a failed worker or capability check hides the thread UI. Without
  `threads/1`, no thread UI. A refused root holds the draft. E2EE edits, deletes
  and typing wait for #133.

## 7. Emoji table

`apps/desktop/assets/emoji.py` already generates the picker's Unicode 17 table
from `emoji-test.txt`. Move the generator and its table to `nuschl-core` and
extend it to keep skin-tone variants, marked so the picker can still hide them.
`nuschl_core::emoji` validates reactions against that table, and the picker
reads the same one. An exact table lookup needs no grapheme segmentation;
`unicode-segmentation` is already in `Cargo.lock` if one is needed. No new
crate.

## 8. Tests

Private loopback nuschld and private Murmur, raw protocol clients and synthetic
text only. Beyond unit tests of each codec, validator and builder, cover what is
not obvious:

- **Stock correlation:** annotation then text; text then annotation within 2 s
  and later; annotation only; text only; a digest mismatch after a server
  rewrite; a prefix that differs from its recomputation; a duplicate `msg_id`; a
  reused session with a new presence instance; buffer eviction; `f(annotation)`
  identical for sender and receiver in HTML and plain modes at extreme text
  limits.
- **Per-reader previews and counts:** previews and thread summaries for readers
  with different `since_access` ranges; stock copies without quote or thread
  line for receivers who can't read the parent or root.
- **The satellite hook** keeps reactions, mentions and thread counts consistent
  through append, delete, expiry, erase, forget and purge in one transaction, on
  SQLite and PostgreSQL.
- **Older clients under group features:** no false gaps in a group without the
  features; kinds 13 and 4 refused there; a client without `threads/1` gets
  `ALL` pages.
- **Probing:** reply and thread-root refusals are one `NOT_FOUND`; mention
  recipients reveal nothing to the sender.
- **Stock Murmur:** a raw stock receiver gets exactly the presentation text and
  one message 26 per text; bursts above the bucket degrade to plain text; a
  cross-channel annotation injection is ignored.
- **E2EE late joiners** reconstruct reactions and threads under `members` and
  see nothing earlier under `since_join`.
- **Fuzz:** new frames into nuschld state, the annotation decoder with
  correlation, and kind 13 body validation.

## 9. Slices

| Slice | Contents | Depends on | Agent-days |
| --- | --- | --- | --- |
| **F0** Foundation | `core::interaction`, the emoji table, reply and thread builders, thread rules, annotation codec and correlation, tickets and composer contexts | — | 3–4 |
| **F1** Protocol freeze | Fields, range, capabilities, MLS features, data ids, `docs/extension.md` | F0; polls; #93; E2EE allocation; review | 1–2 |
| **F2** Storage and model | Both migrations, IPC, satellite hook (with polls), timeline fields | F1 | 4–6 |
| **R1** Replies | Compatible server, SDK, desktop and TUI | F2 | 5–7 |
| **X1** Reactions | Compatible server, SDK, desktop and TUI | F2 | 5–6 |
| **M1** Mentions | Recipients, roles, notices, `RenderMarked`, completion, D9 | F2 | 6–8 |
| **S1** Stock annotations | Send and receive for all four features | #93; R1, X1, M1, T1 | 4–5 |
| **W1** Web | SDK views, reply bar, reactions, mention picker, through the browser action fences | R1, X1, M1; #172 | 4–5 |
| **E1** E2EE interactions | Kind 13 and Reaction bodies, 0xF4E1, archive records, padding | F1; the `MlsEncrypt` and archive changes shared with polls, done once | 7–9 |
| **T1** Threads | Compatible server, views, `ThreadQuery`, stock thread line, `threads.rs` | F2; R1 | 5–7 |
| **T2** Thread UI | Desktop, TUI, web, accessibility | T1; W1 | 5–6 |
| **T3** E2EE threads | 0xF4E2, receiver checks, paging through the crypto worker | E1; T1 | 3–4 |
| **E2** Encrypted edited and deleted parents | States and reaction cleanup | MLS edit and delete | 2 |
| **E3** E2EE role mentions | After policy admins are in the binding | D8 | 3 |

The issues ask for foundation, replies, reactions, mentions, then threads. After
F2, R1, X1 and M1 can run in parallel. About 60–80 agent-days in total.

## 10. Decisions

**Decided by the owner:** structured data on every path, never parsed text;
threads on every path, not only E2EE; **D16** notification levels are a separate
feature, #210.

**For the owner (recommendation first):**
- **D2** Stock reactions between online Nuschl clients only, with no legacy
  text; alternatives are a per-reaction "also tell Mumble users" line, or none.
- **D3** Stock-authored messages on stock servers get quote replies only, no
  reactions or threads.
- **D4** A reply notifies the parent's author by default, with a per-reply
  toggle.
- **D5** The stock quote is the author and first line (§2.9), with a warning
  that stock users keep it; alternative: author only.
- **D7** Role mentions need Write on the channel by default, configurable; named
  groups only, no `@here` in v1.
- **D8** E2EE role mentions wait for policy admins in the binding.
- **D9** Restrict the name heuristic (§2.7); a visible behaviour change.
- **D11** Thread replies don't count in the parent's unread count; followed
  threads notify.
- **D15** The starting limits in §2.9.
- **D22** Later: opt-in notifications for reactions to your messages.
- **D25** The legacy thread line on its own line; alternatives: an inline
  `[thread: …]` prefix, or author only.
- **D26** No thread titles in v1; if wanted, one optional `title` (≤ 64 bytes)
  on the root in all three forms.
- **D29** Thread follow and read state per device in v1, not synced per account.

**Internal (recommended):**
- **D1** Compatible reactions are state without `seq`; `seq` rows would inflate
  unread counts.
- **D6** Mention labels in the text with presentation-only ranges, not
  server-substituted tokens.
- **D10** Edits never notify new mentions.
- **D12** (E2EE lane) Existing groups upgrade by an add-only commit once every
  leaf supports the feature.
- **D13** No thread metadata for the E2EE server and no group per thread.
- **D14** Emoji from the generated RGI table (§7); no custom server emoji.
- **D17** Annotations go to everyone the text reaches, up to 256, without
  discovering Nuschl peers first.
- **D18** No parent excerpt in E2EE reply bodies.
- **D19** Replies to deleted parents and roots are refused.
- **D20** Without `replies/1` or `threads/1`, those actions are unavailable
  rather than writing quotes into stored content.
- **D21** (integrator) E2EE reaction state lives in the SDK, while polls count
  in the crypto worker; pick one convention.
- **D23** (E2EE lane) Pad MLS application messages to a common bucket size.
- **D24** (integrator) All allocations are provisional until the registry
  freeze: the range, field numbers, capabilities, migrations, kind 13,
  0xF4E1/0xF4E2 and the data ids; plus one owner each for the MLS changes and
  the satellite hook shared with polls.
- **D27** Receivers skip the prefix only when it equals their own
  `f(annotation)`, so a sender can't hide its own text from Nuschl users.
- **D28** Older Nuschl clients see thread replies flat, without a line.
