# Files on Nuschel servers (`files/1`)

Status: design for review (2026-11-11). Only the object format (§4.1) is implemented,
as a prototype with tests in `crates/files` (`nuschel-files`). It replaces the sketch
in [docs/extension.md](extension.md) §8 and [docs/e2ee.md](e2ee.md) §6.8, which now
point here, and settles the security review's D9
([docs/reviews/2026-11-11-server-security.md](reviews/2026-11-11-server-security.md)).
Open questions for the user are in §11.

On stock Mumble servers nothing changes: a file is a magic-wormhole receive code in
the message text, refreshed after each pickup, sent by the sandboxed transfer worker
(`apps/transfer-worker`, `crates/core/src/transfer.rs`, SECURITY.md). This document
is about Nuschel servers, where the client negotiated `files/1`.

## The conflict, and the short answer

The earlier design made every file an upload to the server's blob store. The user's
requirement is different for large files: "large file transfers should be possible
peer-to-peer (i.e. not saved on the server; but with the server as a relay (with
maybe a small chunk cache; with content defined chunking maybe)); this needs to be
clearly shown in the UI so that you know you're still uploading. They should also be
resumable if you restart the client or intermittently lose connection." Retention is
forever by default because "we don't want people to lose data", and several images or
files may go in one message.

Both are kept, as two classes of one thing:

- A **file object** is a file encrypted under its own random key in chunks that each
  become one 64 KiB piece, hashed with BitTorrent v2's tree (BEP 52). Its root, bound to
  the key and length, goes in the message, so every piece is verified before it is
  written, whoever delivered it. The format is the same for both classes, in both server
  modes.
- A **kept** object is uploaded to the server and lives as long as the message that
  references it (forever by default). Images and small files are kept by default, so
  offline members, and new members with history, get them.
- A **live** object stays on the sender's disk and is sent **magic-wormhole style**:
  for each recipient the two clients exchange sealed connection hints through the
  server, race direct connections, and fall back to the server as relay. It is
  available while any **source** is online: the sender's device, or a recipient who
  downloaded it and helps share it (a private swarm of people who may read the message;
  §2.10). Large files are live by default; the sender can **keep** one later (the server
  then pulls the pieces it lacks; the message does not change). Deleting the message ends
  the file for every source; copies already downloaded remain.
- The relay keeps a **piece cache**: a bounded, short-lived cache of encrypted pieces
  keyed by object and index, so a brief disconnect or restart resumes without the sender
  sending again, several recipients are served from one upload, and a recipient who
  comes online a little later still gets what was already sent. End-to-end encryption
  is per piece, not per pipe, which is what lets the relay cache pieces it cannot read.
- On the server side **sources only answer requests**: the server pulls pieces from a
  source, into its store for a kept object and on behalf of relayed recipients for a live
  one. Uploads, relaying, keeping later and resumption are one mechanism.
- **Shares cost nothing at rest**: the server persists which devices hold which objects,
  so nothing is announced at login, the client's share index is read only when a request
  comes, and a reconnect costs the same with 10,000 shares as with none (§3.5).
- **Wormhole codes for stock users** are offered automatically for new files; codes cannot
  survive a restart, so after one they are re-offered only for recent files and only to
  stock users who saw them, as one digest per conversation (§7).
- **rqbit is not embedded for now**: it has no BitTorrent v2 support yet and no way to run
  over our authenticated channels; the BEP 52 format keeps that door open (§2.10).
- Server traffic runs on a **second TLS connection to the same port** (ALPN
  `nuschel-bulk/1`), so bulk data never queues in front of chat, control or tunnelled
  voice; direct paths are TLS 1.3 between the two clients with per-transfer
  certificates pinned in the sealed hints.
- No content-defined chunking: with a fresh key per file it deduplicates nothing, in the
  cache or elsewhere; resumption does not need it; its chunk lengths would show the
  server information about the plaintext; and the one case it could help, re-sending an
  edited file, costs more than it saves (§2.9).

## 1. Two classes

### 1.1 What each class is

| | Kept | Live |
| --- | --- | --- |
| Where the bytes are | Server's blob store (ciphertext) | Sender's disk; sent directly, or through the server's relay and piece cache |
| Available | As long as a referencing message exists (retention, forever by default) | While a source is online: the sender's device, or a recipient who helps share it |
| Offline recipients | Download any time | See the message; download starts when a source is next online |
| New members with history | Get it (the key is in the archive record, §4.2) | Get the key; download while a source is online |
| Server disk | Charged to the sender's quota | Only the piece cache: bounded, pieces gone within an hour of last use |
| Typical | Images, documents, short clips | Videos, disk images, archives, datasets |

A message may carry kept and live attachments together. The class is server-side
state of the object, not part of the encrypted message: keeping a live object later
(§1.4) changes nothing that recipients already hold.

How the two relate: both are the same object, with the same `FileRef` in the message,
the same pieces and the same verification. A **kept** object is one the server has
pulled completely into its blob store: small items, and everything history must keep
for people who are not online. A **live** object is sent magic-wormhole style, for each
recipient directly when a direct connection works and through the server's relay
otherwise (§2); the relay's piece cache covers the gaps between people being online
at slightly different times, but it is transport, not storage, and forgets within the
hour. Keeping a live object later moves what the cache holds into the store and pulls
the rest.

### 1.2 Where the boundary is

**Recommendation: by size against the server's policy, overridable by the sender.**

- The server announces its file policy in the hello (`files.max_kept_object`, default
  100 MiB, and whether kept files are allowed at all) and each account's remaining
  quota on request (§5.3).
- The composer makes an attachment **kept** if it fits under both, otherwise **live**.
  Each attachment chip shows which and has a menu: "Keep on the server" (allowed up to
  `max_kept_object` and the remaining quota) or "Send from this device".
- If the server refuses a kept upload at send time (quota, disk reserve; §5.3), that
  attachment becomes live and the chip says why. The message is never lost for it.
- Thumbnails of images (and later poster frames of videos) follow their file, with one
  exception: a file that goes live only because of its size still gets a kept thumbnail,
  so the timeline shows something while the sender is away. A file the sender chose to
  send from the device, or whose kept upload the server refused, keeps nothing on the
  server, its thumbnail included.

Alternatives considered:

| Option | Why it loses |
| --- | --- |
| Everything kept | Large files fill the server and contradict "not saved on the server" |
| Everything live | Offline and new members lose images and small files whenever the sender is away; contradicts "don't lose data" |
| Server decides by size only | A sender cannot keep a private file off the server, or keep a large one an operator would allow |
| Sender chooses every time | Friction on every attachment; people cannot judge server limits; most choices would be the default anyway |

The size limit is not a cap on what can be sent: a file over it still goes, live.

### 1.3 What offline recipients see

- **Kept:** the message and a file card; images show their thumbnail at their final
  size (width and height travel in the message, so nothing shifts when the image
  arrives). Download any time; if the upload is still running, the card says so and a
  download follows it.
- **Live, a source online:** "From Alice's computer" (or "From Alice and 2 others"),
  with Download.
- **Live, no source online:** "Nobody who has this file is online", with "Download when
  available". The recipient's client remembers that and starts on its own once the
  server reports a source back (§3.4).
- **Ended** (the sender stopped sharing, the file is gone from the sender's disk, or the
  message was deleted): "No longer available". The message and its text stay.

### 1.4 Keeping and releasing later

The owner can keep a live object at any time ("Keep on the server" on the card): the
server checks size and quota, marks it kept, and pulls the chunks it does not have yet
from the sender (§2.5), verifying each against the root it already holds. Nothing new
is sent to recipients; their cards change from "From Alice's computer" to "On the
server" when the server reports the new state. The owner can also release a kept
object, which becomes live (if the owner's device still shares it) or ended. Only the
owner can keep or release, because the owner's quota pays. Another member who wants a
file kept forwards it, which makes a new object under a new key (§4.3).

### 1.5 Retention, deletion and history grants

- A kept object is reference-counted by the messages that reference it. When the last
  one is deleted, expires (extension.md §6) or is erased by the operator, the object is
  removed with it. The key goes with the message: in compatible mode it is in the
  message row, which is overwritten (`secure_delete`; PostgreSQL keeps old row versions
  until VACUUM, and backups keep what they captured, as for text); in end-to-end
  conversations the server never had it. Ciphertext left on disk is then unreadable.
- In end-to-end conversations a deletion is an envelope inside MLS that the server cannot
  read; attachments are released by the server-visible purge request that accompanies it
  (e2ee.md §6.3, item 4), which is checked against the author. A client that never sends
  the purge leaves its kept attachments until the message expires, and moderators'
  deletions need that request to be open to moderators too, which e2ee.md does not yet
  say.
- A live object ends when its last referencing message goes: the server stops
  relaying (authorisation needs a referencing message, §5.1), drops every source, and
  every source's client stops sharing when it sees the deletion (§2.10).
- A live share has no lifetime of its own: it lasts until the sender stops it, deletes
  the message, or the file changes or disappears on the sender's disk (open question
  2). The sender's Transfers panel lists every share.
- Live-only conversations (retention "none") store no messages, so nothing can be kept
  and there is no stored message to authorise a fetch. The server keeps the reference in
  memory instead, with the sessions the message was delivered to; those sessions may
  fetch the attachment, and it ends with the owner's session.
- **History grants (e2ee.md §6.6).** Attachments travel in the message envelope and in
  its archive record (§4.2), so a member granted history keys can decrypt every
  attachment of the retained history: kept ones from the server, live ones from any
  source that is online. `since_access` intervals apply to files as to text: the server
  serves an object only to someone who may read a referencing message (§5.1).

## 2. Live transfers: direct first, relay fallback, piece cache

### 2.1 The model

Live transfers work like magic-wormhole's transit: for each recipient, the two
clients exchange connection hints, race direct connections, and fall back to a relay
when none works. Here the relay is the Nuschel server, and it keeps a **piece cache**.
A *piece* is one encrypted chunk of the object with its index and inclusion proof
(§4.1).

End-to-end encryption is **per piece, not per pipe**: every piece is encrypted under
the file key and verified against the root from the message, whichever path delivered
it. That is what lets the relay cache and fan out pieces it cannot read, and why the
direct and relayed streams are the same frames.

What differs from magic-wormhole:

- No code and no PAKE: both sides are authenticated sessions of the server, and every
  recipient already holds the file key from the message (§4.2). Hints and the direct
  connection are keyed from it (§2.2, §2.4).
- The relay is the Nuschel server, not a third-party transit relay, and it understands
  pieces (by object id and index), so it can cache them and serve several recipients
  from one upload. magic-wormhole's relay pairs two anonymous connections by a hash of
  the transit key and forwards opaque bytes; ours pairs a recipient with the owner's
  session by object id after checking authorisation, and never holds a key either.
- Transfers resume at piece granularity (§3); magic-wormhole cannot resume.

| Connection | Between | Carries |
| --- | --- | --- |
| Control (the existing TLS connection) | Client and server | `FileOpen`, `ChatSend`, `FileConnect` (sealed hints), `FileQuery`/`FileStatus`, `FileHold`, `FileServe` (withdrawals), `BulkWanted` |
| Bulk (TLS, same port, ALPN `nuschel-bulk/1`), opened on demand | Client and server | The relay path and kept uploads: `FilePull`, `FileWant`, `FileChunk`, `FileHalt` |
| Direct (TLS 1.3, ALPN `nuschel-direct/1`, per-transfer certificates) | A source and one recipient | `FileWant`, `FileChunk`, `FileHalt` |

### 2.2 Hint exchange

- A recipient that starts or resumes a download picks a random 128-bit `transfer_id`
  and sends `FileConnect { object_id, transfer_id, sealed }` on its control connection.
  The server checks that it may fetch the object (§5.1) and forwards the message to the
  owner's session that serves it (or answers that the owner is offline, §3.4). The owner
  answers with its own `FileConnect`, relayed back the same way. The server relays these
  only between those two sessions and only for that object.
- `sealed` is ChaCha20-Poly1305 under
  `hint_key = HKDF-SHA256(ikm = file key, salt = transfer_id, info = "nuschel files/1 hints")`,
  nonce 0 for the recipient's message and 1 for the owner's (one key, two messages; every
  race takes a new `transfer_id`, so a key never seals a third message). It
  holds the side's role, its direct candidates (at most 16 `address:port`), the SHA-256
  of its per-transfer TLS certificate (§2.4), and whether it accepts direct connections
  at all.
- **Why sealed:** hints contain LAN addresses, which the server otherwise does not learn,
  and the certificate pins that authenticate the direct connection. In end-to-end
  conversations the server has no file key, so it can neither read hints nor forge them
  to put itself in the middle of a direct connection. In compatible mode it has the key,
  as it has the text.
- **Candidates:** the addresses of each network interface (IPv4 and IPv6) with the port of
  a TCP listener opened for this transfer, and the public address the server sees for the
  client (`BulkWelcome.observed`), which helps with global IPv6 and forwarded ports.
  NAT-PMP or UPnP port mappings can be added later. The relay is always the fallback and
  needs no hint.
- **Who learns addresses:** the owner's go only to sessions that may read a referencing
  message and hold the file key; the recipient's only to the owner. A direct connection
  reveals each side's IP address to the other, which Mumble servers otherwise hide
  (addresses are shown only to users allowed to see `UserStats`). Settings has "Direct
  connections for file transfers", on by default as the user decided; off, a client sends
  no candidates and opens no listener, and its transfers always use the relay (open
  question 3).

### 2.3 Racing

- Both sides open a TCP listener for the transfer and, at the same time, connect to every
  candidate of the other side, IPv6 first and 250 ms apart as in Happy Eyeballs
  (RFC 8305), and accept inbound connections. Simultaneous connects from both ends also
  get through some stateful firewalls. Every connection runs the handshake of §2.4; the
  first that completes with the right pin wins and the others are closed.
- The relay path starts 2 seconds after the race begins, unless a direct path has won
  (magic-wormhole delays its relay hints the same way). A direct path that wins within
  10 seconds takes over from the relay; piece bitmaps make the switch free (§3.1). After
  10 seconds the race ends and the listeners close.
- The race runs again whenever a transfer starts or resumes. A direct connection that
  drops mid-transfer falls back to the relay at once.
- **Honest expectations:** direct TCP works on a LAN, between hosts with global IPv6
  addresses where firewalls allow it, and with forwarded ports. Between two home NATs on
  IPv4 it usually fails and the relay carries the transfer, as with magic-wormhole's
  transit today. UDP hole punching would raise the success rate; it can come later as a
  further candidate type (str0m's ICE, already on trial for media, or QUIC) carrying the
  same frames, after measuring its throughput for bulk data.

### 2.4 The direct path

- TLS 1.3 (rustls, already in the session) with ALPN `nuschel-direct/1`. Each side
  presents a self-signed certificate made for this transfer (rcgen, already in the
  session), and each verifies the other's against the pin from its sealed hint, both as
  client and as server. The Mumble client certificate is not used: it is a stable identity
  across servers, and the peer has no need for it.
- The pins came sealed under a key derived from the file key, so only someone who
  received the message can complete the handshake. A member who knows the key and
  pretends to be another recipient gets ciphertext it could fetch through the relay
  anyway; pretending to be the owner, it can only send pieces that verify against the
  root. TLS adds forward secrecy and hides from network observers which pieces move.
- Inside, the bulk framing with `FileWant`, `FileChunk` and `FileHalt`: the recipient
  asks the owner directly, with the same windows (§2.6).
- The owner's client accepts inbound connections only during a race, only with a pinned
  certificate, with the same frame caps as the server's bulk connection, and serves at
  most a few direct peers at once (§2.7).

### 2.5 The relay path and kept uploads

**Recommendation: a second TLS connection to the server's TCP port, ALPN
`nuschel-bulk/1`, opened while transfers run.** It carries the relay path of live
transfers and the uploads of kept objects.

- The client asks for a ticket on its control connection (`BulkTicket`: 32 random
  bytes, single use, valid 60 seconds, bound to the session) and opens a TLS connection
  to the same address and port, offering only `nuschel-bulk/1` and presenting the same
  client certificate (if the session has one). Its first frame is `BulkHello { ticket }`
  (or a serve token, §3.4); the server checks the ticket and the certificate hash
  against the session, or closes the connection. Before
  that, the only accepted frame is `BulkHello` (≤ 64 bytes) within the handshake
  timeout, as for `Version` on the control connection.
- One bulk connection per session. It ends with the session; both reconnect together
  and transfers resume (§3). It does not count as a user (`max_users`), and the
  per-address connection caps count it with its session.
- Framing is the Mumble control framing with types from the `nuschel.v1` block (§9),
  so `crates/protocol` and the per-type limits apply unchanged. A bulk frame on the
  control connection, or a control frame on the bulk connection, is a protocol error.

| Option | Verdict |
| --- | --- |
| A bulk lane in the existing control connection | No. Chunks queue in TCP in front of chat, control and voice tunnelled over TCP (head-of-line blocking; 64 KiB is 50 ms at 10 Mbit/s). The connection's byte-bounded output queue disconnects a client that lets it fill, which a slow downloader would; bulk would need its own queue, scheduler and exemptions on a path that must stay simple. Chunk bytes would pass through the connection task that feeds the state lock |
| **Second TLS connection, same port** | **Yes.** Its own TCP flow and congestion window; no head-of-line blocking with control or voice; can be re-established without disturbing the control connection; same port and certificate, so no firewall or NAT change for operators; no new dependency (rustls and the existing framing) |
| QUIC (quinn) | Not now. Streams without head-of-line blocking and connection migration are attractive, but it needs another UDP port (Mumble's encrypted voice datagrams start with an arbitrary IV byte, so QUIC cannot share the voice port), a new dependency, and a TCP fallback anyway for networks that block UDP |
| WebRTC data channels through the server | No: the server would terminate DTLS and SCTP for every client. ICE belongs on the direct path, as a later candidate type (§2.3) |

The security review's P0-4 asks for a bulk lane in each connection's output queue for
Mumble blobs and history pages. That stays for them, which are kilobytes to a page; file
pieces are gigabytes and get a connection of their own.

**The pull model.** Three parties: the **owner** (the sender's device that has the
file), the **server**, and **recipients**.

- The server knows which devices hold which objects (§3.5); a source opens its bulk
  connection when the server asks for it (`BulkWanted` on the control connection) and
  announces nothing at login.
- A recipient on the relay path asks for piece ranges (`FileWant { object, ranges,
  window }`). The server checks authorisation (§5.1) and answers from its store (kept
  objects), from the piece cache (§2.8), or by pulling from a source: the owner, or a
  recipient who helps share (§2.10).
- The server pulls (`FilePull { object, ranges }`); the owner answers with `FileChunk`
  frames, each carrying its inclusion proof. The owner never sends anything unasked.
  It keeps no ciphertext copy: encryption is deterministic for a key, index and
  plaintext, so it encrypts each requested chunk again from the file and checks it
  against its tree before sending (§3.3).
- For a kept object the server pulls every piece it lacks into its store, at a pace
  set by its write budget, and pulls pieces a recipient is waiting for first. Keeping a
  live object later (§1.4) is the same, with pieces already in the cache moved into the
  store instead of being pulled again.
- The server verifies every piece from a source against the object's `R` before it
  stores, caches or forwards it, and forwards the proof with it. Recipients verify
  again against the root from the message (§4.2), which the server cannot change.

A live file sent to two recipients: Bob on the owner's network gets it directly, Carol
behind a NAT through the relay.

```mermaid
sequenceDiagram
  participant A as Alice (owner)
  participant S as nuscheld (relay, piece cache)
  participant B as Bob (same LAN)
  participant C as Carol (behind NAT)
  A->>S: FileOpen(root, object_len, keep=false) → object_id
  A->>S: ChatSend(text, attachments=[object_id])
  S->>B: ChatEvent (FileRef with the key, inside MLS)
  S->>C: ChatEvent
  B->>S: FileConnect(transfer_id, sealed hints)
  S->>A: FileConnect (after can_read)
  A->>S: FileConnect(sealed hints)
  S->>B: FileConnect
  Note over A,B: race: Bob reaches Alice's LAN address, TLS with pinned certificates
  B->>A: FileWant (direct)
  A->>B: FileChunk(i, data, proof) × n (direct)
  C->>S: FileConnect … (hints exchanged; every direct attempt fails)
  Note over C,S: after 2 s: relay path on the bulk connection
  C->>S: FileWant
  S->>A: FilePull, ahead of Carol into the cache
  A->>S: FileChunk × n
  S->>C: FileChunk × n (verified, cached for later recipients)
```

### 2.6 Flow control

Credit at each hop, in pieces:

- **Recipient → server or owner:** `FileWant.window` is how many pieces the recipient
  accepts in flight for that object (it raises the window as its measured throughput
  grows, up to 256 pieces, 16 MiB; 64 pieces cover 320 Mbit/s at 100 ms). The sender
  sends no more than that unacknowledged; each piece the recipient writes, it
  acknowledges by its next `FileWant` (the ranges it still wants are the
  acknowledgement).
- **Server → owner:** the server pulls only what it has room for: per object, what
  waiting recipients' windows allow plus the store's write budget and the read-ahead
  allowed by the object's share of the piece cache (§2.8). An owner that stops answering
  is not penalised; outstanding pulls are reissued after it reconnects.
- **Bandwidth:** relay bytes are metered per account (§5.2). An empty bucket pauses that
  account's pulls and forwards until it refills (`FileHalt { throttled, retry_after_ms }`)
  instead of failing the transfer. Between accounts the server shares its relay capacity
  round robin, so one large transfer does not starve others. Direct paths cost the server
  nothing and are not metered.
- **Memory:** windows bound what is in flight on the network, not what the server
  holds. A bulk connection's output queue is small (`files.bulk_queue_bytes`, 1 MiB,
  16 pieces; the control connection's 8 MiB would allow gigabytes across a thousand
  sessions), and queued pieces are shared, reference-counted buffers of the cache, so a
  piece sent to twenty recipients is held once. The server writes to a bulk connection
  only while its queue has room; pieces waiting for room stay in the cache, and pulls stop
  when the object's cache share is full. Server memory is therefore the small queues plus
  the cache's memory tier, whatever recipients ask for.
- Nothing in the relay path holds the state lock: authorisation is checked under it when
  a transfer opens and again whenever `authz_epoch` moves (as for history pages); pieces
  move between the two connections' tasks through a relay task with bounded queues.
- The client keeps voice smooth while it transmits: an upload limit in Settings, and
  while the user is speaking it answers requests at a reduced rate, so the uplink's queue
  stays short.

### 2.7 Several recipients

Each recipient gets its own path to each source it uses, raced independently, and each
source's client decides what to offer when it answers a `FileConnect`. What follows is
said of the owner; it holds for every source (§2.10):

- **Direct paths cost the owner one upload each; the relay costs one upload for
  everyone.** All recipients receive the same pieces (one key per object), so the server
  coalesces: a piece wanted by several relay recipients is pulled once, and the piece
  cache serves the rest. A channel of 20 people taking a 2 GB file through the relay costs
  the owner about 2 GB of upload, not 40, as long as they stay within the object's cache
  share of each other (1 GiB by default: about 14 minutes apart at 10 Mbit/s, 90 seconds
  at 100 Mbit/s). Recipients further apart cause a second pull of what they missed, which
  their own budgets pay for (§5.2).
- So the owner offers **LAN candidates always** (they do not use its internet uplink)
  and **internet candidates to one recipient of an object at a time** by default
  (a setting). Further recipients over the internet get only the relay, and are served
  from one upload. When the direct recipient finishes, the next one to (re)connect may
  race again.
- Switching a recipient between paths costs nothing: its bitmap says what is missing.
- Recipients who finish become sources themselves if they help share (§2.10), so a
  channel's later recipients mostly fetch from them and the owner's upload is spent once.
- A kept object costs the owner one upload whatever the number of recipients; recipients
  of a kept object always use the server.

### 2.8 The piece cache

A bounded cache of verified pieces on the relay, fed by whichever source served them (the
owner, or a recipient who helps share, §2.10; "owner" below stands for either). It exists
so that:

- a recipient's brief disconnect or client restart resumes from the cache, without the
  owner sending those pieces again;
- several recipients of the same object are served from one upload (§2.7);
- a recipient who comes online a little later still gets the pieces the owner already
  sent, even if the owner has gone offline meanwhile;
- a brief disconnect of the owner does not stall recipients: while recipients are active
  on the relay, the server pulls ahead of the furthest one, up to the object's share.

**Keying.** Entries are keyed by `(object_id, index)` and hold the encrypted piece, its
proof and its leaf hash (SHA-256 of the encrypted piece), which the server checks again
when it serves the piece from disk. All recipients of an object share its entries; that
is the fan-out. Entries are never looked up by hash and never shared between objects:
fresh keys make identical files produce different pieces anyway, and a cache addressed by
hash would answer "does anyone hold a piece with hash H?", the confirmation-of-file oracle
that extension.md §8 and D9 rule out. Pieces that travel on a direct path never reach the
server and are not cached.

**Size and eviction** (operator settings, generous by default):

| Setting | Default | Meaning |
| --- | --- | --- |
| `files.piece_cache_memory` | 64 MiB | Memory tier for the hottest pieces |
| `files.piece_cache_bytes` | 4 GiB | Disk tier under `state_directory/piece-cache/`, never past the free-disk reserve (§5.3); 0 keeps the cache in memory only |
| `files.piece_cache_per_object` | a quarter of the cache | The most one object may hold |
| `files.piece_cache_ttl` | 1 hour | A piece goes this long after it was last written or read |

- Eviction takes first the pieces every active recipient of their object already has,
  then the least recently used. Each active object keeps a minimum share (64 MiB, or the
  cache divided by the active objects if that is less), taken from the objects that hold
  the most.
- When the cache cannot give an object its minimum (more active transfers than the cache
  holds), that transfer runs **uncached**: pieces pass straight through and the server does
  not pull ahead. It loses the cache's benefits (a disconnect means the owner sends again)
  and nothing else; no transfer is refused and no other transfer is affected.

**Lifetime.** Independent of message retention: a message kept forever does not keep its
pieces in the cache, and the cache is not storage. A piece leaves after the TTL, at once
when its object ends (the owner stops sharing, the referencing messages are deleted or
expire), or when its object is kept (it moves into the blob store). The disk tier is
emptied when the server starts.

**Authorisation.** A piece is served only to a session that may fetch the object (§5.1:
`can_read` on a referencing message, within its visible ranges), checked when the want
arrives and again when `authz_epoch` moves. A cache hit is checked exactly like a pull.

**What the server learns** from the cache: nothing beyond what relaying shows (object
ids, sizes, piece indexes, timing, which accounts fetch which object, and now how far
apart recipients are). It holds ciphertext under keys it does not have in end-to-end
conversations; in compatible mode the key is in the message row, as the text is. Disk
entries are files of 0600 in the state directory, removed on eviction; remnants on disk
are ciphertext.

**Budgets.** Cache residency counts against the object's cache share, not against the
owner's storage quota (§5.3): it is transport and it expires. A source's upload counts
once against its relay bucket, read-ahead included; pieces served from the cache count
against each receiving account's bucket; no source is charged again for a cache hit.

### 2.9 Content-defined chunking

FastCDC splits data at content-dependent boundaries so that an insertion moves only
nearby boundaries and identical content yields identical chunks. Every object here is
encrypted under a fresh random key, so:

- **Across files, and in the cache:** identical plaintext gives unrelated ciphertext; the
  server cannot deduplicate, by design (no cross-user deduplication, no convergent
  encryption: D9), and the piece cache is keyed by object and index. Making pieces
  comparable across objects would need convergent keys, which turn "upload finished
  instantly" into a confirmation-of-file oracle.
- **Within one file:** repeated content at shifted offsets is rare in the large files
  that go live (compressed video, archives, images); where it occurs, compression wins.
- **Resumption of the same file:** resumption needs stable piece indexes for the same
  content. An object is immutable (its root is in a sent message), so fixed-size chunks
  are perfectly stable.
- **It leaks.** CDC boundaries depend on the plaintext, so the sequence of chunk lengths
  is visible to the server and fingerprints known content even under encryption (keyed
  CDC weakens but does not remove that). Fixed-size chunks show the server only the total
  size, which padding (§4.1) blurs.
- **It costs:** a chunk table to locate data, variable AEAD units, and no O(1) offset for
  random access and parallel fetch.

**A sender resuming a modified file** is the one case where it could help, so precisely:
an edited file is a new object, because its root is in a sent message and re-encrypting
changed content under the old key would reuse nonces. For unchanged regions to give
identical pieces, version 2 would have to reuse version 1's key with nonces derived from
the content (deterministic, SIV-style encryption) and CDC boundaries; then the relay could
serve unchanged pieces from its cache and recipients holding version 1 could skip them.
The price: the server learns exactly which regions changed between the versions and the
plaintext's chunk-length sequence; the format needs a second, deterministic mode; and the
gain lasts only while version 1's pieces are cached (an hour) or for recipients who kept
version 1. Not worth it in `files/1`. If edited large files turn out to matter, a delta
transfer between the two clients (rsync-like, over the direct path, comparing chunk hashes
under a key both hold) gets the gain without the server learning anything and without
changing the object format. **Fixed 64 KiB chunks.**

### 2.10 Recipients as sources: a private swarm

The user decided that recipients who have a live file should help share it (§11,
decision 5), so that it stays available while anyone who has it is online, while "the
original person sharing should still have authority over deleting a message/file".

**Who is a source.** The owner's device, and every device that has downloaded and
verified the whole object, belongs to an account that may read a referencing message,
and has "Help share files I received" on (a user setting, on by default; sources serve
only while Nuschel runs, under the client's upload limit, below its own transfers). With
it off, a device never serves. Partial holders do not serve in this version; the format
would allow it, since every piece verifies on its own.

**How the server tracks them.** A recipient whose download completes sends `FileHold {
object_id }` once; the server checks that the account may read a referencing message and
adds the device to the object's persisted sources (§3.5). Nothing is repeated at login.
A device withdraws with `FileServe { stop }` (the user deleted the file, turned the
setting off, or the file changed on disk), or is dropped when a request finds it gone.
When an account loses `can_read`, its devices stop being sources at the next
`authz_epoch` re-evaluation, and transfers from them stop.

**How recipients use several sources.** For a `FileWant` or `FileConnect`, the server
picks up to three online sources (the owner when online, the others by least recent use,
so load spreads). The recipient races a direct path to each (§2.3, keyed by the file key,
which every source holds), splits the ranges it still needs between the paths that work,
and in the last few pieces asks a second source for anything outstanding (BitTorrent's
endgame). The relay pulls from any online source, so the piece cache (§2.8) keeps being
fed while the owner is away. Every piece is verified against `R` whatever its source;
a source whose pieces fail is dropped for the object (§3.2).

**Deletion authority stays with the sender.** Deleting the message (by its author, or a
moderator), its expiry, and "Stop sharing" by the owner all **end the object** for
everyone:

- the server drops every source record, refuses new wants and `FileConnect`s for the
  object with `not_found`, purges its pieces from the cache, stops relays in progress, and
  pushes `FileStatus { ended }` and `FileHalt { ended }` to every online source and
  recipient, including both ends of running direct transfers;
- clients honour the tombstone wherever they see it (the push, a history page, or in
  end-to-end conversations the deletion envelope inside MLS, even if the server never
  acted on it): they stop serving, drop the share index row and keep their downloaded
  copy;
- since direct paths are only ever set up through `FileConnect`, no new transfer starts
  after a deletion.

Only the owner going offline, or its own copy changing, does not end the object: the
other sources carry on. What deletion cannot do, honestly: copies already downloaded stay
on recipients' disks, a modified client could finish a direct transfer already running or
pass the file on by other means, and a recipient offline at the time learns of the
deletion only when it next sees the conversation. Deletion withdraws distribution through
Nuschel; it cannot recall copies, as with message text.

**Privacy.** The server learns which devices hold which objects (it already knew who
fetched them). Peers learn each other's IP addresses, LAN addresses included, as with
"direct first everywhere" (§11, decision 3); the "relay only" setting applies to serving
as to fetching. A recipient does not learn which account a source belongs to: hints carry
addresses and a per-transfer certificate only.

**Budgets.** A source's bucket pays for the first pull of each piece from it (§5.2), and
its client's per-share budget (ten times the file a day) covers serving as a helper too.

**Embedding rqbit instead?** The user suggested embedding rqbit and encrypting the file
before transmission. librqbit, as of 9.1.0:

| Question | Finding |
| --- | --- |
| Maintenance | Active: 9.1.0 on 2026-10-29, 9.0 in August 2026, about 190,000 downloads |
| Licence | Apache-2.0, already allowed in `deny.toml`; compatible with distributing Nuschel under EUPL-1.2 |
| BitTorrent v2 (BEP 52) | **Not supported.** It verifies v1 SHA-1 piece hashes. Issue #70 ("Support BitTorrent v2") and the design issue #546 are open; the SHA-256 groundwork (#562) is merged, the Merkle tree module (#568) was closed unmerged, the hybrid verification work (#554) is open |
| Private swarm | Possible at run time: `SessionOptions { dht: None, disable_local_service_discovery, disable_trackers }` and `AddTorrentOptions::initial_peers` from the Nuschel server. But the DHT, local service discovery, UPnP, uTP and tracker code is always compiled in (ten workspace crates of its own), and there is no switch for UPnP in `SessionOptions` |
| Transport | Plain BitTorrent over TCP or uTP. No hook to run it over our authenticated direct TLS or the bulk connection (a SOCKS proxy for outgoing connections only). The info hash crosses the network in clear in every handshake, and a peer is anyone who knows an address and the info hash |
| `unsafe` and C | librqbit's own 19,000 lines have 7 `unsafe` (sparse files, read buffers, an mmap example, tests) and do not forbid it; it depends on memmap2, nix and socket2; the default TLS for its HTTP client is the platform's (OpenSSL on Linux), `rust-tls` uses ring |
| What it would bring | A mature piece picker, choking and endgame; a storage trait that could serve the ciphertext by encrypting on read |

**Verdict: not now; build multi-source fetching into `files/1`, and keep the format
ready for rqbit later.** Without BEP 52, rqbit would verify a second, SHA-1 piece list,
and a v1 info dictionary per object would have to travel through the server, instead of
the root the message already carries. It cannot run over our authenticated channels or
the relay, so its peers would be reachable by anyone who learns the info hash, the
server's relay and piece cache would not see its traffic, and "relay only" would mean
not using it. And it would bring a large network-facing surface (DHT, local discovery,
UPnP, uTP, an HTTP client) into the desktop, or a new worker with network access, mostly
for features we would switch off. Our protocol already has what a private swarm needs:
the server as tracker and relay, peers limited to `can_read`, per-piece verification and
direct paths; multi-source fetching adds a source list, range splitting and an endgame.

Because the object format now hashes the ciphertext exactly as BEP 52 does (§4.1), the
question can be reopened when rqbit ships BitTorrent v2 and a way to plug in a transport:
it would verify the same pieces root, could run in a sandboxed worker allowed to reach
only the peers the server names, and its traffic could go over our TLS paths. Until then
a BitTorrent swarm would cost more security than it saves code.

## 3. Resumability

### 3.1 Unit of progress

A chunk is done when it has been verified against the root and written. Every party
tracks done chunks as a sparse bitmap over chunk indexes; ranges in `FileWant` and
`FilePull` are the complement. Progress is never "bytes so far", so any order, parallel
fetch and gaps work.

### 3.2 Verified streaming

Each `FileChunk` carries one piece and its inclusion proof: the sibling hashes from the
piece layer to the pieces root, ≤ 32 hashes (14, 448 bytes, for a 1 GB file: 0.7 %
overhead). The receiver checks the piece's length for its index, hashes its four 16 KiB
blocks up to the piece hash, folds the proof to the pieces root, computes `R` with its
own key commitment and compares it with the object's `R`, then decrypts
(ChaCha20-Poly1305 checks the tag again under the key) and writes the plaintext at its
offset. A piece that fails is dropped and asked for again, from another source if there
is one (§2.10); three failures from the same source end that source for the object, and
when no source is left the download ends with "the file was corrupted in transit", which
the client reports (never silently retrying forever).

Alternatives considered: BLAKE3's own tree with bao (1 KiB granularity: an outboard of
about 6 %, 3 GB for a 50 GB file, or rehashing subtrees on demand; bao-tree's chunk groups
fix that with an encoding of their own); the RFC 9162 tree over whole chunks that the
first version of this design used (domain-separated, but a format of our own that no
BitTorrent engine verifies); and fetching the whole piece layer first, as BitTorrent v2
peers may, which saves the per-piece proof but needs a layered scheme for very large
files. Per-piece proofs cost ≤ 1.6 % even at 2³² pieces and need nothing else; the proof
could later shrink by omitting levels the receiver already holds.

### 3.3 State on each side

| Party | Persisted | Where |
| --- | --- | --- |
| Owner | Each share, one row of the share index: the file key, the `FileRef`, both idempotency keys (`FileOpen`, `ChatSend`), object id once known, server, conversation and `seq` once acknowledged, path, size, modification time and file id at preparation; the piece layer (32 bytes per 64 KiB, 0.05 % of the file) | Local storage worker database (which already holds the plaintext of local history); the piece layer in a file beside it, read only when a request comes (§3.5) |
| Recipient | Each download: object id, message, destination, done bitmap, and the hash of each verified piece; the partial file `name.part` beside the destination. A finished download that the user lets help share (§2.10) becomes a share index row like the owner's | Same database; bitmap and piece hashes written atomically after the data they cover is synced (every 8 MiB or 2 s) |
| Server | Object record (owner account, `R`, pieces root, key commitment, `object_len`, class, state), references to messages, the **sources** (the devices that hold the whole object: the owner's, and recipients' that help share), for kept objects the bitmap of stored pieces | Server database, the bitmap updated only after the pieces it covers are synced; blobs and the piece layer as files (§5.4) |
| Server (transient) | Which devices are online, wants, pulls in flight, watchers, the piece cache (memory and disk tier) | Gone after a restart (the disk tier is emptied); recipients re-want; sources announce nothing (§3.5) |

**The owner's order of writes** makes every step repeatable after a crash: the key, the
`FileRef` and both idempotency keys are saved before `FileOpen`; the object id when
`FileOpened` arrives; the `seq` when `ChatAck` arrives. A restart repeats the first step
not known to be done, with the same idempotency keys, so it never creates a second
object or a second message. Preparation saves its piece hashes as it goes and resumes
where it stopped (a 50 GB file is a minute or two on an NVMe disk, five minutes or more
on a hard disk). An end-to-end sender needs its own record of the key: it cannot decrypt
its own MLS PrivateMessage.

A recipient keeps the hash of every piece it verified, so it can check what is on disk
at any time: encryption is deterministic for a key, index and plaintext, so it
re-encrypts the partial file and compares. If its state is lost entirely, the partial file
is fetched again. A source checks the same way before answering a request: if the
chunk it reads no longer encrypts to its piece hash, the file changed, no ciphertext of
the new content is ever sent (it would reuse a nonce under the same key), and that device
stops being a source for the object ("the file changed on your disk since you sent it");
other sources are unaffected.

On file systems with reflinks (btrfs, XFS, APFS) the owner's client clones the file into
its transfer directory at preparation, so later edits do not end the share and it costs
no space until the original changes.

### 3.4 When someone is offline

- **Every source offline** (the owner quit, is asleep or lost its connection, and no
  recipient who helps share is online): the server marks the object unavailable and
  pushes `FileStatus { unavailable }` to recipients watching it; their cards say "Paused:
  waiting for Alice's device" (or "for someone who has it") and keep the bitmap. When a
  source's device logs in, the server, which knows the object's sources (§3.5), pushes
  `available`, and recipients re-want what is missing. A kept object that is still
  uploading keeps serving the pieces it has, and the piece cache keeps serving what it
  holds.
- **Owner logged in elsewhere.** Mumble allows one session per account, so opening
  Nuschel on a second device takes the session over and would take the first device's
  shares offline. Instead, the server keeps the first device's bulk connection as
  **serve-only**: it answers pulls and relayed `FileConnect`s for its own shares and can do
  nothing else (no chat, no new objects). A serve-only device that reconnects asks with
  its certificate and a serve token it got while it held the session (random, bound to the
  account and certificate, revoked when the account's certificate changes). The account's
  other devices then show these shares as "From your other device".
- **A source not responding:** a pull without progress for 30 seconds sets that source
  aside for a minute (with no other source: "Alice's device is not responding") and is
  then retried. A piece from a source that fails verification ends that source for the
  object for the session and tells it; its client will usually have caught a changed file
  first.
- **Recipient offline or restarting:** on the next connection its client re-opens the
  bulk connection and re-wants each unfinished download, automatically.
- **Server restart:** objects, references, sources and kept pieces survive (database and
  blob files); cache and wants do not; recipients re-want, sources announce nothing.
- **Owner restarting:** nothing is reloaded or announced (§3.5); the server already knows
  the device's shares and sends requests when they come. Quitting with active uploads or
  shares asks first ("2 uploads will continue when you start Nuschel again; 1 shared file
  is unavailable while Nuschel is closed").

### 3.5 Thousands of shares: nothing at rest

Live shares last forever (§11, decision 2), so a device may hold thousands of them. The
user's condition: no cost "in network traffic and startup time when you have a lot of
files you're sharing". So shares are records, not running tasks, and every cost is paid
when a recipient asks, not before.

- **The server knows the shares.** Each live object's record lists its sources: the
  owner's device (recorded at `FileOpen`) and every recipient device that finished the
  download and helps share it (one `FileHold` message, once, §2.10). A device is
  identified by its account and certificate hash (or its serve token, §3.4). Both are
  persisted, so neither a client restart nor a server restart needs a re-announcement.
  `FileServe` remains only to withdraw a share (or to restore one the user stopped), one
  message per change.
- **Login stays O(1).** When a device logs in, the server marks it online in a map from
  device to session. Nothing is sent about shares. Watchers (recipients waiting for an
  unavailable object) are indexed by source device, so the server pushes `available`
  only to those: the work at login is O(waiting recipients of that device's objects),
  usually zero, never O(shares).
- **Requests find the source.** A `FileWant` or `FileConnect` for an object looks up its
  sources (an indexed query), keeps those online, and picks one or more (§2.10). If a
  chosen source has no bulk connection open, the server sends it `BulkWanted` on its
  control connection and the client opens one (§2.5). Bulk connections close again after
  a minute without transfers.
- **The client's share index is loaded lazily.** It is a table in the local storage
  database, keyed by object id; nothing in it is read at startup. A request reads one row,
  checks the file (size, modification time, file id), and loads that object's piece layer
  into a small cache of active objects. Pieces are verified as they are read (§3.3), so a
  changed or missing file is found when someone asks, and the device then withdraws only
  that share.
- **Nothing runs in the background.** No periodic scans of shared files, no keep-alives
  per share, no timers. The rail's "Sharing 3,214 files from this device" is one
  `count(*)`; the Transfers panel pages through the index and asks the server for the
  state of the rows on screen only (`FileQuery`, ≤ 64 per request).
- **Withdrawn and deleted shares are tidied lazily.** When a referencing message is
  deleted or expires, the server ends the object and drops its sources at once (§2.10);
  a client removes its index row when it sees the deletion in a conversation it reads,
  or when the Transfers panel, asking the server about the rows on screen, finds the
  object ended. A user short of disk can let shares drop their piece layers, which are
  then recomputed with one read of the file on the next request.
  Sources the server has not seen for 90 days (e2ee.md's inactive-device bound) are
  forgotten.

Costs, for 10,000 shares of 1 GB each on one device:

| | At rest | Startup or reconnect | Per request |
| --- | --- | --- | --- |
| Owner or helping recipient | One index row each (about 300 bytes: 3 MB in all); piece layers 0.05 % of the files (5 GB of shares cost 2.5 MB, 10 TB cost 5 GB) | Nothing beyond the login; the rail's count is one query | One row read, a `stat`, the piece layer loaded once (512 KB for 1 GB), the requested pieces read and encrypted |
| Server | One object record and one row per source (about 100 bytes each: a few MB); no memory | Marking the device online; pushes to waiting recipients only | Source lookup, authorisation, the relay and cache if direct fails |
| Network | Nothing | The login only | The transfer, plus one `BulkWanted` if the source had no bulk connection |

## 4. End-to-end encryption and the object format

### 4.1 The object format (prototype: `crates/files`)

One format for both classes and both server modes:

- **Key:** 32 random bytes per object, never derived from content. One object, one key,
  one root.
- **Padding:** the file is padded with zeros to Padmé length (Nikitin et al., PETS 2019):
  under 12 % (at worst 129 bytes become 144), under 3.2 % from 64 KiB and under 1.6 %
  from 4 GiB (a 32 GiB file rounds up to a multiple of 512 MiB). The server then learns
  O(log log n) bits of the size instead of the exact size, which is a good fingerprint
  for known files. `object_len` is the padded length; the true `file_len` travels only
  in the message. Recipients fetch the padding pieces too, so their requests do not give
  `file_len` away.
- **Chunks and pieces:** `object_len` is split into plaintext chunks of 65,520 bytes,
  the last one shorter; an empty object is one empty chunk. At most 2³² chunks (2⁴⁸ −
  2³⁶ bytes, just under 256 TiB; Padmé pushes files above 254 TiB past that, and they are
  refused). Each chunk encrypts to exactly one 65,536-byte **piece** (the last one
  shorter): 65,520 bytes and a 16-byte tag. The pieces in order form the *ciphertext
  object*, `object_len + 16 n` bytes, never empty.
- **Encryption:** ChaCha20-Poly1305, STREAM construction (Hoang, Reyhanitabar, Rogaway,
  Vizár): nonce = 7 zero bytes ‖ chunk index (u32, big-endian) ‖ last-chunk flag (1
  byte); empty associated data.
- **Tree: BitTorrent v2's.** The ciphertext object is hashed exactly as BEP 52 hashes a
  file: 16 KiB blocks (the last one shorter), leaf `SHA-256(block)`, leaves padded with
  zero hashes to a power of two, node `SHA-256(left ‖ right)`. Its root is BEP 52's
  **pieces root** for a file with those bytes. With a piece length of 64 KiB, BEP 52's
  *piece layer* is one hash per piece, the root of its four blocks, so a piece is
  verified by hashing its blocks up to its piece hash and folding the inclusion proof
  (the sibling hashes from the piece layer to the root, one fixed length per object) to
  the pieces root. An object of one piece is padded only to the next power of two of its
  blocks, as BEP 52 and libtorrent do. The prototype's pieces roots and piece layers are
  identical to those libtorrent 2.1.2 computes for the same ciphertext (§8).
- **Object root:** key commitment `C = SHA-256(0x03 ‖ "nuschel files/1 key" ‖ key)`;
  `R = SHA-256(0x02 ‖ "nuschel files/1 root" ‖ piece_log2 ‖ object_len (u64 BE) ‖ C ‖
  pieces root)`, which binds length, piece size and key to the BEP 52 tree. The message
  carries the pieces root (§4.2); the server computes `R` from what `FileOpen` gives it
  and reports it; recipients compute `R` from their key and compare.
- **Why BEP 52:** each piece is exactly one AEAD chunk, so "this piece verifies" and
  "this chunk decrypts" are the same unit; the tree is a published one with independent
  implementations (libtorrent 2, Transmission 4, qBittorrent), so a BitTorrent v2 engine,
  if one is ever embedded (§2.10), verifies the same pieces root as our own transfer; and
  it costs nothing at run time (the same SHA-256 over every byte, three extra node hashes
  per piece, proofs of the same size). BEP 52 has no domain-separation prefixes, unlike
  the RFC 9162 tree this replaces. That is safe here: the tree is perfect (padded to a
  power of two), every leaf is at the same depth, proofs have one fixed length, and `R`
  binds the length, so an inner node cannot pass for a leaf.
- **Primitives:** ChaCha20-Poly1305, SHA-256 and the random keys come from ring, the TLS
  provider in both the session and `nuscheld`, so the format adds no crate and no new
  security exception (SECURITY.md). ChaCha20-Poly1305 is what e2ee.md §6.8 named; SHA-256
  rather than BLAKE3 because BEP 52 uses it and ring has it with hardware acceleration.
- **The root is not secret.** It is a hash of ciphertext under a random key, so the server
  learns nothing from it, and it lets the server verify uploads chunk by chunk (D9's
  "verify the hash at completion" becomes "verify every piece on arrival") without the
  key.
- **The root binds the key.** ChaCha20-Poly1305 does not commit to its key: one
  ciphertext can be built to decrypt validly under two keys (multi-key collisions, as in
  partitioning-oracle attacks). One MLS message gives every member the same key, but an
  owner may reference one object from several messages and could hand out different keys
  for it, so recipients would see different files under one object id. With `C` in `R`,
  a recipient whose key differs from the one the object was opened with computes another
  `R` than the server reports and refuses the attachment before decrypting anything. The
  server gets `C` in `FileOpen`; it reveals nothing useful about a random 256-bit key.

### 4.2 Keys and metadata in the message

Attachments travel in the message that carries the text, as a list, so text and files
arrive together with one `seq`. Each is a `FileRef`:

```
struct {
  opaque object_id[16];      // random, server-assigned (§9); also listed for the server
  opaque key[32];
  opaque pieces_root[32];    // BEP 52 root of the ciphertext object
  uint64 object_len;         // padded, as the server knows it
  uint64 file_len;           // true length, <= object_len
  uint8  piece_log2;         // 16: 64 KiB pieces of 65,520 plaintext bytes
  opaque name<1..255>;       // UTF-8; shown as offered, sanitised on save
  opaque media_type<0..127>; // the sender's claim; never trusted for rendering
  uint16 width; uint16 height; // images and videos, 0 otherwise
  optional<FileRef> thumbnail; // its own object and key; a thumbnail has no thumbnail
} FileRef;
```

- **End-to-end conversations:** the `FileRef`s are part of the `chat` envelope's body
  inside the MLS PrivateMessage (e2ee.md §6.4; the separate `file` kind there is not
  needed) and of the archive record, whose hash the envelope carries (e2ee.md §6.6).
  The server sees only the list of object ids in `ChatSend.attachments`, which it needs
  for authorisation and reference counting. Receivers check that the list matches the
  envelope, and that the `R` and length the server reports for each object
  (`FileStatus`) match the `FileRef`; a mismatch is shown as a delivery problem, never
  silently trusted. (A sender who gave the server another root than its message only
  makes its own attachment fail to download.)
- **Compatible mode:** there is no end-to-end encryption, so `FileRef`s travel in
  `ChatSend.attachments` in the clear and are stored with the message. Encrypting the
  object still pays: the blob store holds only ciphertext, keys live in message rows that
  deletion and expiry overwrite, and the stock-client bridge (§7) and end-to-end mode use
  the same code.
- The server sees: object sizes (padded), timing, the owner, which messages reference
  which objects, and who fetches what. It does not see names, dimensions, keys or content
  in end-to-end conversations. It can guess the kind of some attachments: an object with
  a small companion object is probably an image or video with its thumbnail, and objects
  that every recipient fetches at once are probably images downloaded on their own
  (§6.3).

### 4.3 Rules

- No convergent encryption, no cross-user deduplication. An owner may reference one of
  its own objects from several of its messages (forwarding its own file); anyone else
  forwarding a file creates a new object under a new key.
- One object, one key: an object is never re-encrypted. Keeping a live object later is
  storing the same ciphertext.
- Thumbnails are made by the sender's content worker (sandboxed, as for pasted images)
  and are objects of their own.
- Clients never trust a server-supplied type; images are decoded only by the content
  worker, as today.

## 5. Authorisation, abuse and logs

### 5.1 Who may do what

- **Fetch** an object, or ask its state: whoever `can_read` a message that
  references it, at the current `authz_epoch`, within their visible ranges for
  `since_access` channels (extension.md §5). The check runs under the state lock when a
  transfer starts and again when the epoch moves; a reader who loses access has their
  transfers stopped with the uniform `not_found`. Object ids are not capabilities.
- **Reference** an object in `ChatSend`: only its owner (the account that opened it),
  with the permission to send in that conversation. Files need no new ACL bit (a stock
  ACL editor would drop it, extension.md §5); a channel setting `files` (`on` by default,
  or `off`), set with `ChannelSettings` and Write like retention, turns them off.
- **Keep, release, end:** only the owner. **Serve:** the owner's device, and devices
  that hold the whole object and whose account may read a referencing message (§2.10).
- **Unregistered users** have no account to own an object or be charged, and no durable
  identity to resume with, so they cannot open, keep or share objects; their Nuschel
  client sends files as magic-wormhole codes, as on a stock server. They may download
  the attachments of messages delivered to them live in their current session (as stock
  users see inline images live), and of history only where `history.guests = full` lets
  them read it. Their downloads resume within the session, not across reconnects. Their
  bulk connection is opened with the ticket alone when they have no client certificate.
- The sender's client does not authorise downloads itself: it cannot evaluate ACLs or
  intervals, and ciphertext without the key reveals only its size. It sees how many
  people are receiving, not who, as with wormhole offers today (receivers are often
  invisible subscribers, and read receipts are off by default).

### 5.2 Bandwidth budgets

Token buckets keyed as in `page_limits.rs`, charged once bytes are sent: the account for
registered users, the network (IPv4 /24, IPv6 /56) for unregistered ones.

- Relay bytes (`files.relay_bytes_per_minute`, default 6 GiB, about 850 Mbit/s, with a
  burst of the same). The bucket of the source a piece came from pays for its first
  pull, read-ahead and kept uploads included; each recipient's bucket pays for what is
  sent to it, including pieces served from the cache, and for pieces pulled again
  because they had been evicted before it took them. A recipient that asks out of step
  therefore spends its own budget, not the source's.
- Unregistered users' downloads always count per network, as their history pages do.
  For registered users a per-network bucket (`files.network_relay_bytes_per_minute`) is
  **off by default**, for operators with open self-registration who see many accounts
  from one network: history counts registered users by account, not network, so that a
  busy NAT never locks them out, and files keep that rule. `unlimited_networks` (shared
  with history) exempts networks.
- **The owner's own budget.** A hostile server, or readers who keep falling out of the
  cache, could make an owner upload one file again and again. The owner's client shows
  per share how much it has sent ("sent 6.1 GB of a 2.0 GB file") and pauses a share, with
  a notice and a Resume button, once it has sent ten times the file in a day (a setting),
  like the session's other budgets against a hostile server (SECURITY.md).
- An empty bucket slows that account's transfers (`FileHalt { throttled }`) and never
  fails or disconnects. Between accounts the relay is shared round robin (§2.6).
- **Requests** (D13): `FileOpen` has a bucket per account (`files.opens_per_minute`, 600
  with a burst of 200, so a message with 100 attachments goes at once); `FileQuery` and
  `FileConnect` share the query bucket; frames on the bulk connection have a flood bucket
  of their own, like the control connection's. At most 64 downloads in progress and 1,024
  watched objects per connection; objects served by an owner are not capped (they cost a
  set entry each). An object opened and never referenced is removed after a day
  (`files.unreferenced_expiry_hours`), and a live one reserves nothing meanwhile.
- Element and frame caps per message type (§9) and 16 in-flight control requests
  (extension.md §2) bound memory per connection; the piece cache bounds it globally.
  Process limits (`MemoryMax`, `CPUQuota`, traffic shaping in the NixOS unit) stay the
  main defence, as for the rest of `nuscheld`.

### 5.3 Storage quotas (operator settings)

| Setting | Default | Effect when exceeded |
| --- | --- | --- |
| `files.max_kept_object` | 100 MiB | That attachment goes live |
| `files.account_quota` | 10 GiB (0: none) | New kept uploads go live; the chip says so |
| `files.conversation_quota` | 0 (none) | As above |
| `files.global_quota` | 0 (none; the disk reserve below protects the server) | As above |
| `files.min_free_disk` | 5 % of the file system or 2 GiB, whichever is less | New kept uploads go live, so the database never runs out of disk |
| `files.partial_expiry_days` | 30 | A kept upload with no progress for that long gives its stored pieces up and becomes live again if the owner still shares it (ended otherwise); the card says "upload not finished" |
| `files.unreferenced_expiry_hours` | 24 | An object opened but never referenced by a message is removed |

Quota is reserved at `FileOpen` with the declared `object_len` (D9) and released when
the object goes. The piece cache (§2.8) has settings of its own and counts against no
quota. Nothing here refuses a message: when storage runs out, files go live instead,
which the user can see and change.

### 5.4 Blob store

Kept ciphertext lives in files under `state_directory/files/` (`ab/cd/<object id hex>`,
0600, opened with `O_NOFOLLOW`), written by `nuscheld`'s relay task: it never parses
their contents. Beside the blob, `<id>.tree` keeps the piece layer and every node above
it that the server has learned from proofs, at most two hashes per piece.
With them the server can prove any piece it holds, so recipients can download a kept
object while its upload is still running. The bitmap of stored pieces is in the database
with the object record. Large blobs in PostgreSQL `bytea` would bloat backups and
VACUUM. `nuscheld admin files usage` (per account), `files list --account NAME` and
`files remove OBJECT --yes` manage them; `history erase-user` also removes the user's
objects. Back `files/` up with the database: a database restored without its blobs shows
those kept files as ended.

### 5.5 What the server logs

- Info: kept uploads started, finished, released and removed (object id, account,
  padded size), quota refusals, expiry runs. These cost disk and the operator answers
  for them.
- Debug only: relay transfers, downloads and relayed `FileConnect`s. Who fetched which
  object is who read what, which the server otherwise does not log. Direct transfers
  never reach the server.
- Never: names, types, keys, roots, message text.
- The `statistics` line gains counters: relay bytes in and out, piece cache hits,
  misses and bytes held, uncached transfers, hint exchanges, kept bytes added and
  removed, objects kept, throttled transfers.

## 6. UI

Grounded in what exists: the composer tray (`apps/desktop/src/attachments.rs`, chips in
`view.rs`: paste, paperclip or drop; images prepared by the content worker; up to 100
images) and the file card (`apps/desktop/src/transfer.rs::card`: kind tile, name,
"size · author", a progress bar with `Role::ProgressIndicator`, a status line with
`Role::Status`, and buttons).

### 6.1 Composing

- Any number of images and files in the tray. On a Nuschel server up to 100 go **in one
  message** with the text (§9); more go as several messages, the text with the first. The
  one-image-per-message pacing stays for stock servers.
- Each chip shows its class: "On the server" or "From this device", with the menu of
  §1.2, and why if the choice was made for the user ("larger than this server keeps").
- Sending prepares each attachment (padding, encryption and the tree in one read,
  spread over cores, resumable, §3.3), opens its object, then sends the message. The
  message appears at once as pending, as text does; its cards show "Preparing 40 %".
  Images and small files are ready in milliseconds. Uploads start with thumbnails and
  small files, so previews arrive first.
- Details of a transfer (in the Transfers panel and the card's tooltip) say how it runs:
  "directly" or "through the server".

### 6.2 The sender's states

| State | Card says | Actions |
| --- | --- | --- |
| Preparing | "Preparing · 40 %" | Cancel |
| Uploading (kept) | "Uploading · 40 % · 12 MB/s · 2 min left" | Pause, Cancel |
| Shared (live) | "From this device · waiting for downloads" or "Sending to 3 people · 40 %"; "also shared by 2 others" once recipients help | Keep on the server, Stop sharing (for everyone, helpers included) |
| Paused | "Paused: no connection, retrying in 5 s", "Paused: the server is busy", "Paused: limited by the server" | Retry now, Cancel |
| On the server | "On the server" | Release (owner) |
| Failed | The reason ("the file changed on your disk since you sent it", "the server refused it: …") | Retry (re-sends as a new object), Remove |
| Stopped / ended | "No longer available" | |

"You are still uploading" is visible beyond the card: while any upload or live transfer
runs, the server rail and the window title show it ("↑ 2 files · 45 %"); while live shares
exist but nobody is downloading, a quieter "Sharing 3 files from this device" stays in the
rail, because those files depend on this device staying online. The Transfers panel lists
every upload, share and download across servers, and quitting asks first (§3.4). The
owner's other devices show its live shares as "From your other device".

### 6.3 The recipient's states

| State | Card says | Actions |
| --- | --- | --- |
| Kept, uploading | "Alice is uploading · 40 %" | Download (follows the upload) |
| Kept, upload paused | "Alice's upload is paused (her device is offline) · 40 %" | Download when available |
| Kept | (thumbnail for images) | Download; images up to 8 MB download on their own (Settings) |
| Live, a source online | "From Alice's computer", or "From Alice and 2 others" | Download |
| Live, no source online | "Nobody who has this file is online" (or "Alice's device is offline" when she is the only source) | Download when available |
| Live, owner not responding | "Alice's device is not responding; trying again" | Cancel |
| Downloading | "Downloading · 40 % · 3.1 of 7.8 GB" | Pause, Cancel |
| Paused | "Paused: waiting for Alice's device", "Paused: no connection" | Resume, Cancel |
| Saved | "Saved as name.mkv"; "Helping share it" while this device is a source | Open, Show in folder, Stop helping |
| Ended | "No longer available" | |

Cancel asks whether to keep the partial file for later. Downloads write `name.part` beside
the chosen destination and rename it when every piece is verified. Images keep their
final size from the message's width and height while loading.

Deleting a message with a live attachment says what it does: "Delete for everyone also
stops everyone sharing the file. Copies people have already downloaded stay with them."
Settings has "Help share files I received" (on; §2.10) next to "Direct connections for
file transfers" and the upload limit. The rail's "Sharing 3,214 files" counts the
device's shares, its own and the ones it helps with, from one query (§3.5).

### 6.4 Accessibility

- The card keeps its group label, extended with class and availability: "Video
  holiday.mkv, 7.8 GB, from Alice, from her computer, available while she is online.
  Downloading, 40 percent."
- Progress has `Role::ProgressIndicator` with its value; the status line is a polite live
  region that announces state changes (paused, resumed, saved, failed) and progress only
  every 25 % or once a minute, never every percent.
- Every state has text and an icon; colour alone never says paused or failed. All actions
  are buttons reachable from the keyboard, with the card's file name in their labels
  ("Download holiday.mkv"). The Transfers panel is a list with one item per transfer.
- `tests/accessibility.py` gains a case: attach two files, send, check the cards' names,
  roles and the announced states over AT-SPI.

## 7. Stock clients on a Nuschel server in compatible mode

Stock clients do not speak `files/1`; the server makes their copy of a message
(extension.md §4) and adds, per attachment:

| Attachment | What stock clients see |
| --- | --- |
| Kept image | Its thumbnail as an inline `<img>` (PNG or JPEG, checked by magic bytes and size against `image_message_length`), plus "📎 name (size) — open it with Nuschel for the full image". The server decrypts the thumbnail with the key in the plaintext message; it decodes no image, so it needs no image parser, and stock users see exactly the preview Nuschel users see |
| Other kept file | "📎 report.pdf (2.1 MB) — sent with Nuschel; open it with a Nuschel client", and a magic-wormhole code while the owner's client is online (below) |
| Live file | The same notice, and, while the owner shares it, a magic-wormhole code as on stock servers (below) |

- **Timing.** The text of the stock copy goes at once, with the notices, so stock users
  see messages in the order they were sent. The thumbnails follow as messages of their
  own once they are in the store (they are small and pulled first), one image per
  message within `image_message_length` and paced under the sender's text bucket, as the
  desktop sends images to stock servers today.
- **Wormhole bridge.** When `ChatAck` reports stock clients in the conversation's live
  audience, the owner's client also offers the file through the existing transfer worker
  and sends the code with `FileStockOffer`. The server checks it with the same functions
  as the client (`nuschel_core::transfer::valid_code`, `valid_url`) and posts
  `📎 name (size) — receive with magic-wormhole: wormhole receive CODE` (with
  `--relay-url=…` when the rendezvous is not the public one), in the exact format
  `format_offer` writes, with name and size from the `FileRef`. Refreshed codes go only to
  the stock sessions that received the message's stock copy, not to whoever is in the
  channel later; they are never stored and never shown to Nuschel clients (they have
  `files/1`). A fresh code after each pickup follows the existing rhythm: at least two
  seconds apart, charged to the owner's text bucket (`messagelimit`/`messageburst`), so the
  bridge cannot be used to flood. The card shows "also offered to Mumble users via
  magic-wormhole" (open question 4).
- **Codes do not survive a restart, so restarts re-offer on purpose, not all at once.**
  A code cannot outlive the client that made it: magic-wormhole 0.8.1 draws a fresh
  random `side` for every rendezvous connection (`RendezvousServer::connect`, with no way
  to supply one), the SPAKE2 secret behind the PAKE message already in the mailbox exists
  only in memory, and the mailbox server deletes a mailbox about 11 minutes after its last
  listener leaves (it prunes every 10 minutes) and refuses a third side as "crowded". A
  waiting code also needs its client connected to the rendezvous server the whole time.
  So after a restart:
  - nothing is posted at startup;
  - a share gets a fresh code automatically only if its message is recent (at most a day
    old, the existing offer lifetime) **and** a stock session that received the message's
    stock copy is still in the conversation's live audience; stock users who arrived later
    never saw the message and get nothing;
  - those codes go out as **one digest per conversation** ("📎 Files from Alice still on
    offer:" and one line per file in the `format_offer` form), at most the five newest
    shares, at most once every ten minutes per conversation (`FileStockOffer`, charged once
    to the sender's text bucket);
  - older shares, or more of them, are offered only when the sender asks: "Offer to Mumble
    users again" on the card, which posts one code for that file;
  - each waiting code holds a rendezvous connection, so the client keeps at most 16 at once
    (a setting); a code picked up is replaced as today, at least two seconds apart.
- **What the bridge gives up.** The bytes behind a wormhole code are whatever the owner's
  client sends; neither the server nor Nuschel users can check that they match the
  object (extension.md §4 made the stock copy server-made to avoid exactly this for
  text). It is no worse than a stock user's own wormhole offer today, and the code line
  names the file the message names. magic-wormhole's transit also tries direct
  connections, so it reveals the owner's addresses to the stock receiver, as the direct
  path of §2.4 does.
- No download links: a link stock clients could open would be a bearer capability in
  their logs, and `nuscheld` has no HTTP server.
- End-to-end mode admits no stock clients, so none of this applies there.

## 8. Implementation outline

1. `nuschel-files`: the object format with the BEP 52 tree, key commitment, proofs,
   Padmé (§4.1). Done as a prototype: a pure library on ring (no I/O), tested against a
   naive BEP 52 implementation for many piece counts, tampering and wrong-key cases, and
   frozen values that a separate Python implementation reproduces. **Interoperable:**
   v2-only torrents made by libtorrent 2.1.2 with 64 KiB pieces from the same ciphertext
   have the same pieces root and piece layer, for the frozen objects and for 60 random
   files from 16 bytes to 2.2 MB (single- and multi-piece).
2. Server: object records, references and reference counting with retention and
   erasure, `FileOpen`, `ChatSend.attachments`, the bulk connection with tickets, kept
   uploads by pull, downloads of kept objects, quotas and the disk reserve.
3. Client: preparation, the bulk connection in `nuschel-session`, persisted shares and
   downloads, the composer and card states of §6, the Transfers panel.
4. Live transfers: persisted sources and on-demand bulk connections (§3.5), wants,
   coalescing, the piece cache, availability pushes, throttling; then the hint exchange,
   racing and the direct path (§2.2–§2.4); then recipients as sources with several
   sources per download and deletion ending every source (§2.10).
5. Keep and release; the stock bridge.
6. End-to-end: `FileRef`s in envelopes and archive records (with e2ee.md phase 4d).
7. Later: UDP hole punching as a further direct candidate type (str0m's ICE or QUIC),
   after measuring its bulk throughput; NAT-PMP/UPnP mappings.

Tests follow the server's pattern: loopback tests between `nuschel-session` and
`nuscheld` for every state of §6 (owner restart mid-transfer, recipient restart, server
restart, owner switching devices, file changed on disk, quota exhausted, access revoked
mid-transfer, coalescing with several recipients, several sources with one failing,
deletion while transfers run, a stock user present across a sender's restart), a test
that a device with 10,000 shares logs in with the same messages and time as one with
none, fuzzing of the new frames (`extension_messages`) and of piece and proof decoding,
and a NixOS VM test with a 2 GB file over a lossy link.

## 9. Wire messages

New types in the `nuschel.v1` block, capability `files/1`: 0x4E40–0x4E4F on the control
connection, 0x4E50–0x4E5F on the bulk connection and the direct path. 0x4E20–0x4E2C are
taken by `mls/1` on its branch (e2ee.md §6.3), and 0x4E2D–0x4E3F stay free for it. Each
type has a wire cap and element caps checked before and after decoding (extension.md
§2). Errors are the existing uniform codes (`not_found` for "no such object" and "not
allowed").

**Control connection**

| Type | Message | Fields | Cap |
| --- | --- | --- | --- |
| 0x4E40 | `FileOpen` | `request_id`, `idempotency_key` (16), `object_len` (u64), `piece_log2` (16), `pieces_root` (32), `key_commitment` (32), `keep` (bool); the server computes `R` | 160 B |
| 0x4E41 | `FileOpened` | `request_id`, `object_id` (16, random), `kept` (accepted for storage) | 64 B |
| 0x4E42 | `FileQuery` | `request_id`, `object_ids` (≤ 64), `watch` (push changes while subscribed; ≤ 1,024 watched per connection) | 1.2 KiB |
| 0x4E43 | `FileStatus` | per object: `object_id`, `object_len`, `R`, `state` (uploading, kept, live, available, unavailable, not responding, ended), `sources` (how many are online), `kept_pieces` (a count, for progress); reply and push | 10 KiB |
| 0x4E44 | `FileChange` | `request_id`, `object_id`, `action` (keep, release, end) → `Done` | 64 B |
| 0x4E45 | `BulkTicket` | `request_id`, `serve_only` → reply with `ticket` (32), `expires_ms`, and a serve token (32) when asked (§3.4) | 96 B |
| 0x4E46 | `FileStockOffer` | `conversation`, entries (≤ 5: `object_id`, `code` ≤ 64, `rendezvous` ≤ 256); one message to the stock sessions that received those messages' stock copies; compatible mode only | 2 KiB |
| 0x4E47 | `FileConnect` | `object_id`, `transfer_id` (16), `sealed` hints (≤ 1 KiB: role, ≤ 16 candidates, certificate pin, direct allowed); relayed between a recipient and a source after authorisation | 1.2 KiB |
| 0x4E48 | `FileHold` | `request_id`, `object_id`: this device has the whole object and helps share it; sent once, persisted by the server (§2.10) → `Done` | 64 B |
| 0x4E49 | `FileServe` | `request_id`, `stop` or `restore`: object ids (≤ 1,024); only for changes, never at login (§3.5) → `Done` | 20 KiB |
| 0x4E4A | `BulkWanted` | server → source: open a bulk connection, a request is waiting (no fields) | 8 B |

`ChatSend` and `Message` gain `repeated Attachment attachments` (≤ 200 objects: 100 files
and their thumbnails), `Attachment { object_id, file_ref }`, with `file_ref` (the encoded
`FileRef`, ≤ 1 KiB) only in compatible mode. The `ChatSend` cap grows by the attachments'
size. `ChatAck` gains `stock_audience` (compatible mode: whether a stock client was in the
message's live audience), which starts the wormhole bridge (§7).

**Bulk connection** (`nuschel-bulk/1`), and the direct path (`nuschel-direct/1`), which
uses only `FileWant`, `FileChunk` and `FileHalt` between recipient and owner

| Type | Message | Fields | Cap |
| --- | --- | --- | --- |
| 0x4E50 | `BulkHello` | `ticket` (32) or serve token (32); the only frame accepted first | 64 B |
| 0x4E51 | `BulkWelcome` | `max_window` (pieces), `observed` (the client's address as the server sees it) | 64 B |
| 0x4E52 | `FilePull` | server → source: `object_id`, `ranges` (≤ 64 × first, count) | 1 KiB |
| 0x4E53 | `FileWant` | recipient → server or source: `object_id`, `ranges` (≤ 64), `window` (≤ 256); no ranges cancels | 1 KiB |
| 0x4E54 | `FileChunk` | `object_id`, `index` (u32), `data` (one piece, ≤ 65,536), `proof` (≤ 32 × 32) | 67 KiB |
| 0x4E55 | `FileHalt` | `object_id`, `reason` (unavailable, not responding, throttled, ended, not_found), `retry_after_ms` | 64 B |

`FileWant.ranges` are the next pieces the recipient wants, in the order it wants them;
they need not list everything missing. The sender sends from them, at most `window` pieces
beyond what the recipient's latest `FileWant` acknowledged, and the recipient sends a fresh
`FileWant` at least every `window / 2` pieces, which acknowledges what arrived and moves
the window. A piece in flight when a newer `FileWant` no longer lists it is still
delivered and simply counted.

**Sequence numbers, idempotency and the MLS stream**

- Objects take no `seq`; only the message that references them does, as any `ChatSend`.
  State changes (uploaded, kept, released, ended, owner online) are server state read
  with `FileQuery`, not stream events, so they cost no `seq` and no MLS message.
- `FileOpen` is idempotent per `(account, idempotency_key)`: a retry returns the same
  `object_id`; the same key with another root, commitment or length is
  `idempotency_conflict`. What a kept object already holds needs no answer: the server
  pulls what it lacks. A client that crashed between `FileOpen` and `FileOpened`
  therefore leaves no orphan.
- `ChatSend`'s idempotency rules are unchanged, and the body hash now covers the
  attachment list, so a retry with other attachments is `idempotency_conflict`. Edits
  cannot change attachments (extension.md §6); a deletion releases them.
- In end-to-end conversations `ChatSend.content` is the MLS PrivateMessage of the
  `chat` envelope with its `FileRef`s, `ChatSend.archive` the archive record with the
  same, and `ChatSend.attachments` the object ids alone. The idempotency key inside the
  envelope equals `ChatSend`'s (e2ee.md §6.4) as for any message.

## 10. What changes elsewhere

- **extension.md §8:** `files/1` has two classes; `upload_id` becomes `object_id`
  (server-assigned, owned by the account, the same role); the relay-only mode becomes
  the live class, sent directly when possible and through the relay and its piece cache
  otherwise, from the sender or from recipients who help share it, with the budgets of
  §5.2; hashes are verified per piece. The rest of §8
  (authorisation by referencing message, no cross-user deduplication, quota reserved at
  start, reference counting, no trusted MIME types) stands; its global quota is
  `files.global_quota`, off by default behind the free-disk reserve (§5.3).
- **e2ee.md §6.8:** the "hash of the ciphertext" is BEP 52's SHA-256 tree over 16 KiB
  blocks of the ciphertext (§4.1), bound to the key and length in the object root;
  objects are padded (Padmé); attachments ride in the `chat` envelope as a list of
  `FileRef`s rather than a separate `file` envelope; the same format serves live objects,
  served by recipients too, and the file key also keys their hint exchange
  and direct connections.
- **SECURITY.md**, when implemented: the bulk connection, blob store and piece cache
  under `nuscheld`; on the client, the bulk connection and the direct path in the
  session (a TCP listener only during a race, TLS with pinned per-transfer
  certificates), which reads offered and helped files and writes downloads only to the
  path the user chose; no new worker and no new crypto dependency.

## 11. Decisions by the user (2026-11-11)

The user answered the questions this section asked:

1. **Default boundary:** accepted as proposed: files up to 100 MiB are kept on the
   server, with a 10 GiB quota per account (operator settings; above them files go
   live and never fail), and the sender can switch any attachment either way.
2. **Live shares' lifetime:** forever, until the sender stops sharing, deletes the
   message or changes or removes the file. The user added: "we should be careful that
   this doesn't become a performance issue both in network traffic and startup time
   when you have a lot of files you're sharing." So shares must cost nothing at
   startup and nothing on the network while nobody asks for them: no re-announcing
   every share after a restart, a lazily loaded share index, and work only when a
   recipient requests pieces.
3. **Direct connections:** direct first everywhere by default (channels too), with the
   per-user "relay only" setting.
4. **Stock users in compatible mode:** yes, offer live files to them automatically
   through magic-wormhole codes, which go through magic-wormhole's public servers
   unless Settings name others. But a restart must not spam the channel with every
   file still on offer (the same concern as 2): codes are re-offered only when someone
   could use them, not all at once on startup.
5. **Recipients as sources:** yes, this would be nice. The user suggested embedding
   rqbit (a Rust BitTorrent library) and encrypting the file before it is
   transmitted: a private swarm of the people who may read the message, over the
   encrypted object. The original sender keeps authority over deleting the message
   and the file (message deletion exists since 2026-11-11). To work out: a private
   swarm only (no DHT, no public trackers, the Nuschel server as tracker and relay,
   peers limited to those with `can_read`), whether the object format can match
   BitTorrent v2's SHA-256 Merkle tree over 16 KiB blocks so that rqbit verifies the
   same root, and how a deletion makes peers stop seeding.

Where the design follows them: 1 in §1.2 and §5.3; 2 in §3.5 (shares cost nothing at
rest, with the costs); 3 in §2.2–§2.4; 4 in §7 (what a restart re-offers, and why codes
cannot survive one); 5 in §2.10 (sources, deletion, privacy, and the rqbit evaluation)
and §4.1 (the BEP 52 tree).

The original questions follow for reference.

### The questions as asked

1. **Default boundary:** keep files up to 100 MiB on the server by default, with a 10 GiB
   quota per account (both operator settings; above them files go live, never fail), and
   let the sender switch any attachment either way within the limits. Are those defaults
   right, or should the sender always be asked?
2. **Live shares' lifetime:** a live file stays available until the sender stops sharing,
   deletes the message, or changes or removes the file (recommended, matching "forever").
   Or should shares end after a default period (the wormhole offers end after 24 hours)?
3. **Direct connections** (magic-wormhole style, as decided) reveal each side's IP
   addresses, LAN addresses included, to the other side. Direct first everywhere by
   default with a per-user "relay only" setting (this design), or relay only by default
   in channels, where the sender does not know who downloads, and direct first in direct
   conversations?
4. **Stock users in compatible mode:** offer live files to them automatically through
   magic-wormhole codes whenever a stock client is present (recommended; the codes go
   through magic-wormhole's public servers unless Settings name others), or only when the
   sender asks?
5. **Recipients as sources:** a recipient who has a live file could serve it through the
   relay while the sender is offline (the tree makes any holder's chunks verifiable). It
   raises availability to "while anyone who has it is online" but makes recipients'
   devices serve data and tells the server who holds the file. Leave it out (this
   design), or plan it as an opt-in per conversation?
