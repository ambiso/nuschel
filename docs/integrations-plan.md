# Integrations: typed commands and a native UI kit

Planning document, 2026-11-13, against main `d82473e3`. Nothing here is
implemented, and every identifier is a proposal. It covers
[#203](https://github.com/ambiso/nuschl/issues/203) and builds on
[#93](https://github.com/ambiso/nuschl/issues/93) (plugin data, frozen at
`0e40c7ac`, not on main) and [#22](https://github.com/ambiso/nuschl/issues/22)
(music bot). **Owner** marks decisions the owner made on 2026-11-13 (§12).

**Owner request:**

> "integrations with commands and auto-completion in the message box (1a) a UI
> kit for integrations - e.g. above the "Voice connected" status there could be
> a current song title, previous/(play/pause)/next, duration + progress bar UI
> and clicking the buttons should work - this needs to be designed thoroughly
> though - ideally we don't need to embed a webview just for these integrations;
> so we effectively have to build a small UI kit here? what does carplay do
> here? they allow full SwiftUI applications? integrations should also be
> dismissable (and recoverable if you want to see them again) and should be
> isolated/not have any security impact by merit of being designed well (not
> necessarily in a separate worker if we can avoid it)"

> "also not just anyone should be able to add integrations to a server - maybe
> we can piggy-back on some permission here - at least for mumble servers, and
> do something better for nuschl servers?"

As in the message-interactions plan, Nuschl servers carry structured protocol
data, stock Mumble carries it as plugin data, legacy text is only presentation,
and nothing is parsed back out of display text.

## 0. Summary

- **CarPlay does not run apps' SwiftUI.** Apps fill fixed system templates and
  the system draws them (§4.1). Nuschl does the same: a closed set of versioned
  templates, filled with typed data and drawn natively. v1 has one,
  `now_playing/1`.
- **No webview and no new worker.** Surfaces are small protobuf values, checked
  against fixed bounds in the protocol layer and the SDK, and drawn with native
  widgets. Nothing is executed, fetched or parsed as markup. This is bounded,
  memory-safe logic in the portable layers; the remaining risks are logic bugs
  and resource use, covered by fuzzing and §9.
- **Stock Mumble has no permission to piggy-back on.** Nothing can stop a user
  or the server from claiming to be an integration (§3.1). So on stock servers
  an integration must be a registered bot, appears only after each user's own
  consent, and is always shown as **Unverified**.
- **Nuschl servers keep a registry** that holders of Write on the root channel
  and the operator CLI manage (Owner). Only registry accounts can publish.
  Anyone may press buttons; the server attests who pressed, and the integration
  decides (Owner).

## 1. Facts that constrain the design

- The music bot's only interface is text addressed to it (`!web`), answered with
  a private link that controls every queue. It runs one Mumble connection per
  channel, each with its own certificate, and has no "previous".
- Plugin data (#93): ≤ 1,000 bytes per message (larger ones are dropped
  silently), Murmur's bucket of 4/s (about 2/s in practice), forwarded to the
  listed sessions **without channel authorisation**, sender session set by the
  server, no acknowledgement. The SDK paces 2/s with a burst of 10. The
  `nuschl/` namespace is reserved. nuschld forwards no plugin data on end-to-end
  servers.
- By default `admin` has Write on root and `all` has **Register self**, as in
  Murmur. Extension policy lives outside the Mumble bitmask, which a stock ACL
  editor would erase. Capabilities are sticky.
- The web client has no extension negotiation yet, so nuschld looks like a stock
  server to it.

## 2. Model

An **integration** publishes typed **surfaces** and a **command catalog**, and
receives typed **actions** (button presses) and **invocations** (commands). Its
ordinary chat messages stay ordinary messages.

- **Surface:** one template instance, `(principal, surface id)`, with a strictly
  increasing `revision` and a `subject` that actions bind to (§5). It lives
  while its publisher is online and is never stored in history.
- **Catalog:** up to 16 command specs with a `revision`.
- **Action or invocation:** one user's request, until its result or a timeout (5
  s for actions, 10 s for commands). Never retried automatically.

| Path | Key the client stores | Shown as |
| --- | --- | --- |
| Nuschl server with `integrations/1` | `(server history identity, IntegrationId)`, 16 random bytes from nuschld | Registry name; About: "Added by Alice · 3 Nov" |
| Stock Murmur, or nuschld without `integrations/1` | `(server history identity, registration number, certificate SHA-1)` from the server's `UserState` | User name tagged "Unverified"; About: "Unverified. You allowed this." |
| End-to-end (`e2ee/1`) | None in v1 (§8.3) | — |

- The stock key is **asserted by the server, not the sender.** It keys consent
  memory and grants nothing. A new certificate or registration is a new
  integration. Names or hashes inside a payload are never read.
- Sessions without a registration or a certificate are ignored. Registration
  gives a stable identity, not authority: anyone may register by default.
- Nothing is ever shown as "verified". About always says "Sees who presses its
  buttons."; outside end-to-end mode also "Not end-to-end encrypted."

## 3. Authority

### 3.1 What a stock client can learn

A stock client learns only its **own** permissions, and sees ACLs only where it
holds Write. It sees every user's name, registration and certificate hash, all
asserted by the server. Murmur forwards plugin data between any sessions, and a
data id says nothing about the sender. So any user can run a bot that sends
integration payloads, and a malicious server can invent any of it. No client
design changes that, so the plan claims no admin authority on stock servers.

### 3.2 Stock policy: personal consent only

- **Registered bots only** (§2). Everything else is dropped and makes no offer.
- **Nothing shows without the user's own consent,** per device.
- **One offer at a time,** from an undecided integration in, or listened to in,
  the user's current voice channel. **Not now** declines for good; **Allow** in
  Integrations brings it back (§7).
- **A card docks only while its sender is,** by the server's report, in one of
  the user's voice places. That limits placement, not who may claim to be an
  integration.

**Rejected as authority:** a marker in a channel description or the welcome text
(authority from display text, which clients can't attribute and a malicious
server can forge); a certificate hash (key continuity at most); registration
(open to everyone); plugin data claiming a role, or an admin's client vouching
over it (the relay checks nothing); channel ACL tricks (invisible to others or
audible to everyone); signed payloads pinned at consent (stop only one kind of
server forgery; not in v1).

**Plugin data stays a transport.** #93 remains a generic carrier with no
integration semantics. The integrations layer (`nuschl-core::integration`,
`crates/client/src/integrations.rs`) decides who counts, through the reserved-id
hook, and treats every reported field as a claim.

### 3.3 Nuschl servers: a registry the server enforces

**Capability** `integrations/1`, compatible mode only (§8.3), sticky. Once
negotiated, the client ignores the stock carrier from that server. A client that
doesn't negotiate it sees a stock server.

**Registry entry, v1** (persistent, one storage migration):

```proto
message IntegrationInfo {
  bytes  id = 1;                 // 16 random bytes, never reused
  string name = 2;               // ≤ 32 bytes, one line, unique on the server (case-insensitive)
  repeated bytes accounts = 3;   // ≤ 8 registered accounts (16-byte ids), each in at most one entry
  string added_by = 4;           // name snapshot of the admin, ≤ 64 bytes, for "Added by …"
  uint64 added_ms = 5;
  uint64 catalog = 6;            // current catalog revision, 0 when none is published
}
```

For policy an entry is just `{id, name, accounts}`, covering the whole server;
the other fields serve display and cache invalidation. Channel scope,
per-template permissions, other audience rules, opt-out and a request audit wait
until a provider needs them (D11).

**Who manages it (Owner: "yes to both"):** a new `Action::ManageIntegrations`,
granted by **Write on the root channel** as extension policy rather than a new
ACL bit, and the operator through `nuschld admin integrations
list|add|set|remove`.

**What the server enforces:**
- **Publishing:** surfaces and catalogs only from a connection logged in with
  persistent credentials as one of the entry's accounts. A surface belongs to
  the publisher's current channel and is withdrawn when the publisher leaves it.
- **Audience:** a surface goes only to `integrations/1` connections in or
  listening to its channel that pass the current `authz_epoch` check. Moves,
  listener changes and ACL changes re-evaluate it, and anyone leaving gets a
  removal.
- **Binding, not policy:** anyone may press or invoke (Owner). The server binds
  an action to the surface, revision and subject it sent that actor, who must
  still be in the audience, and an invocation to a conversation the actor can
  read. It rate-limits (§5) and forwards a server-attested actor:

  ```proto
  message Actor {
    bytes  account = 1;        // 16 bytes; empty for an unregistered user
    uint32 user_id = 2;        // registration number; 0 when unregistered
    bool   registered = 3;
    uint32 session = 4;
    string name = 5;           // snapshot, ≤ 128 bytes
    bytes  certificate = 6;    // SHA-1 of the client certificate the server verified; empty without one
    uint32 permissions = 7;    // the actor's effective Mumble permissions in that channel
  }
  ```

- **Timeouts:** no reply within 5 s (actions) or 10 s (commands) gives the
  client `UNAVAILABLE`; a late reply is dropped.
- **Errors:** an unknown integration and "not allowed" are the same `NOT_FOUND`,
  so the registry can't be probed.
- **Audit:** registry changes (time, admin, change, integration; ≤ 1,000 rows,
  oldest dropped), read through the CLI. No log holds command arguments or
  result text.

## 4. The UI kit

### 4.1 Prior art

- **CarPlay:** apps fill system templates (lists, grids, Now Playing and others)
  and the system draws them; only navigation apps draw, and only their map
  ([CarPlay](https://developer.apple.com/documentation/carplay)). Widgets and
  Live Activities in CarPlay are SwiftUI descriptions the system renders,
  non-interactive there ([WWDC25 session
  216](https://developer.apple.com/videos/play/wwdc2025/216/)).
- **MPRIS:** players report position and rate, clients interpolate, and a
  `Seeked` signal marks jumps ([Player
  interface](https://specifications.freedesktop.org/mpris-spec/latest/Player_Interface.html)).
  §4.4 follows it.

### 4.2 Principles

1. **Data, not code:** no expressions, conditions, styles, colours, fonts,
   positions or sizes.
2. **The client owns presentation:** layout, fixed labels, icons and focus
   order. An integration fills fields and enables or disables controls.
3. **Versioning:** a template is `name/major`. Minor additions are optional
   fields an older client can ignore; anything else is a new major. Unknown
   templates are not shown and are counted once in Integrations as "Needs a
   newer Nuschl".
4. **One model:** the same `Surface` travels on both transports.

### 4.3 Messages

```proto
message Surface {
  bytes  id = 1;            // 8 bytes, chosen by the integration
  uint64 revision = 2;      // strictly increasing per surface; older or equal revisions are dropped
  bytes  subject = 3;       // ≤ 16 bytes: what actions bind to (§5), e.g. the queue entry id
  oneof template {
    NowPlaying now_playing = 10;   // now_playing/1
  }
}

message NowPlaying {
  string title = 1;         // required, ≤ 160 bytes
  string subtitle = 2;      // artist, ≤ 160 bytes
  string detail = 3;        // album or queue, ≤ 160 bytes
  State  state = 4;         // PLAYING | PAUSED | BUFFERING | STOPPED
  uint32 duration_ms = 5;   // 0 = unknown; > 24 h counts as unknown
  uint32 position_ms = 6;   // ≤ duration (+ 2 s tolerance, then clamped)
  uint32 age_ms = 7;        // how old position_ms is when sent; ≤ 60 s from integrations
  uint32 controls = 8;      // bit set: PREVIOUS = 1, PLAY_PAUSE = 2, NEXT = 4 (SEEK = 8 reserved)
  uint32 enabled = 9;       // subset of controls
}
```

- Labels and icons are the client's ("Previous", "Play" or "Pause", "Next"). A
  disabled control is drawn and exposed as disabled; one missing from `controls`
  is not drawn.
- `STOPPED` with an empty title is not allowed; a surface with nothing to show
  is removed.
- A generic `card/1` template is designed when a second provider is approved
  (D3).

### 4.4 Progress

- **No shared clock:** the client anchors on receipt, `anchor = local monotonic
  time − age_ms`. nuschld re-anchors held surfaces for late arrivals by adding
  the time since receipt, up to 1 h; beyond that the position is unknown.
- **Shown:** `min(duration, position + elapsed)` while playing, frozen
  otherwise; with an unknown duration only the elapsed time, without a bar.
- **Corrections:** a new revision with the same subject within 1 s of the
  interpolated value never moves the bar backwards; larger differences jump; a
  new subject resets.
- **Cost:** one repaint a second, only while the card is visible and playing.

### 4.5 Text and icons

- Every string is valid UTF-8 and one line. Control characters and bidirectional
  overrides are removed (#61). An empty title refuses the surface. Text is
  elided when drawn and complete in the accessible name.
- Icons come from a fixed set: music, play, pause, previous, next, stop, info,
  warning, error, check, clock, refresh, people. No lock, shield, certificate or
  microphone glyphs, so a card can't imitate a security or voice indicator.

### 4.6 Placement and copy

- **Dock:** on desktop, a new integration dock sits in the sidebar between the
  channel list and the voice card; the TUI and web put it above their voice
  status. It shows surfaces whose channel is one of your voice places, like the
  voice card. Selecting a text channel changes nothing.
- **At most 3 cards;** the rest are under "2 more" and in Integrations. Stock
  cards carry a fixed "Unverified" tag the integration can't remove or restyle.
- **Accessibility:** named groups and buttons, disabled states, progress value
  text ("1:23 of 3:45"). Track changes are announced only while focus is inside
  the card.
- **Copy:** card "Music · Lounge" with "Previous", "Play", "Pause", "Next",
  "Hide", "About"; offer "Music · Lounge · Unverified", "Show its controls
  here?", **Allow** and **Not now**, then "Find it later in Integrations.";
  results "Done.", "Not allowed.", "Out of date.", "Music didn't answer.";
  composer "/skip — Skip this song · Music" and "No command /skp" with **Send as
  message**.

### 4.7 Not in v1

- **Artwork:** later, on Nuschl servers only: ≤ 64 KiB uploaded by the
  integration, decoded in the existing **content worker**, drawn as validated
  RGBA. Never a URL fetched by the client.
- **Links** (later checked with `allowed_link`), **lists, text input, rich
  text,** and **per-user surfaces** (D9).

## 5. Actions

- **Ticket:** a press captures `{server history identity, connection generation,
  principal, surface id, revision, subject, control}`. The SDK sends it only if
  the generation is the same, the surface still has that subject, and the
  control is still enabled. Results are dropped unless the generation, principal
  and surface still match, so a late result can't change a new card.
- **Who may press is the integration's decision** (Owner). On Nuschl servers it
  gets the attested `Actor`; on stock servers only the server's report of the
  session, without permissions.
- **Subject binding:** the integration refuses an action whose `subject` is no
  longer current with `STALE`, so Next on song A never skips song B.
  Progress-only revisions keep the subject.
- **One in flight** per surface and client. No automatic retry, even after a
  reconnect.
- **Idempotency:** every request carries a random 16-byte key. The integration
  ignores repeats among the last 64 keys per actor; nuschld rejects in-flight
  duplicates.
- **Outcomes:** `OK`, `REFUSED`, `STALE`, `UNAVAILABLE`, `BUSY`, with optional
  cleaned text ≤ 200 bytes, shown locally and never stored.
- **Rates:** Nuschl servers 60 per minute per connection (burst 10), refused
  with `RATE_LIMITED` and `retry_after_ms`; stock servers the shared plugin
  pacer; the integration 2/s per actor.

## 6. Commands and completion

```proto
message CommandSpec {                  // ≤ 900 bytes encoded, so it fits one stock message
  string name = 1;                     // 1–32 bytes, [a-z][a-z0-9-]*
  string summary = 2;                  // ≤ 100 bytes, plain
  repeated ArgSpec args = 3;           // ≤ 4, required ones first
}
message ArgSpec {
  string name = 1;                     // ≤ 16 bytes, [a-z][a-z0-9-]*
  string summary = 2;                  // ≤ 60 bytes
  bool   required = 3;
  oneof kind {
    TextArg    text = 10;              // max_bytes ≤ 256, one line
    IntegerArg integer = 11;           // i32 min ≤ max
    ChoiceArg  choice = 12;            // ≤ 8 options: value ≤ 16 bytes [a-z0-9-], label ≤ 24 bytes
    FlagArg    flag = 13;              // boolean
  }
}
message Invoke {
  bytes  idempotency_key = 1; string command = 2; uint64 catalog = 3;
  repeated ArgValue args = 4;          // ≤ 4, ≤ 600 bytes in total
  oneof context { uint32 channel = 5; bool direct = 6; }
}
```

- The SDK validates an invocation against the catalog revision it holds, nuschld
  again, and the integration last. No user-typed argument: it would expose
  account or certificate choices.
- **Discovery:** a channel conversation offers the commands of integrations
  present in that channel, a direct conversation those of its peer. Nuschl
  servers answer `CommandQuery { conversation }`, cached per generation,
  conversation and catalog revisions. On stock servers the catalog comes from
  allowed integrations, fetched when the composer gets focus, at most once per
  30 s each. With no command available, `/` is ordinary text.
- **Composer:** `/` as the first character opens completion. A chosen command
  becomes a chip with labelled, validated argument slots. A typed `/name`
  resolves only if exactly one command has that name; otherwise "No command
  /skp" offers **Send as message**, and Enter alone never posts it. Mention
  completion uses the same component. In the TUI, built-in commands are never
  shadowed.
- **Results:** an invocation is never posted. The sender sees a local line,
  "/skip · Music", then the result, bound to that conversation and generation
  and gone after 10 s or at the next send.
- Nuschl clients never treat `!…` or `/…` in received text as a command, or draw
  buttons from a bot's text. Stock users keep typing `!web`.

## 7. Consent, dismiss and restore

The owner asked that a rejection be undoable: "if you reject once it should
still be possible to get the integration back (maybe the same place you get it
back from if you dismiss it)". One place, **Integrations**, holds every
decision.

| State | How it gets there | What shows | Undo |
| --- | --- | --- | --- |
| Offered (stock) | An undecided registered bot in your voice channel | One offer card in the dock | **Allow** or **Not now** |
| Active | Nuschl: added by admins. Stock: **Allow** | Card and commands | **Hide** |
| Hidden | **Hide** | Commands only, since typing `/` is explicit | Integrations → **Show** |
| Declined (stock) | **Not now** | Nothing, and no new offers | Integrations → **Allow** |

- **The offer** is one at a time across all servers, sits where the card would
  go, and never takes focus, plays a sound or notifies. An ignored offer returns
  at most once per connection.
- **Integrations** lists Active, Hidden and Declined per server, including
  absent ones as "Offline"; on stock servers **Forget** makes an integration
  undecided again. It is reached from the server menu, every card's menu and the
  "Find it later" line.
- These choices are local and send nothing.
- **Storage:** versioned, ≤ 256 entries and 64 KiB, least recently seen dropped
  first, written atomically (IndexedDB on the web). An unreadable file starts
  empty, which asks again and is never more permissive.

## 8. Transports

### 8.1 Nuschl server, compatible mode (`integrations/1`)

One block of message types after the frozen ranges, written `INT+n` until
allocated (D6):

| Type | Message | Direction | Check |
| --- | --- | --- | --- |
| INT+0 | `IntegrationList { request_id }` | client → server | capability |
| INT+1 | `IntegrationListReply { request_id, repeated IntegrationInfo }` (≤ 64) | server → client | — |
| INT+2 | `IntegrationChanged { info \| removed id }` | server → client | pushed to listers |
| INT+3 | `IntegrationSet { request_id, IntegrationInfo }` (no id creates) | client → server | `ManageIntegrations` |
| INT+4 | `IntegrationRemove { request_id, id }` | client → server | `ManageIntegrations` |
| INT+5 | `SurfacePublish { channel, Surface \| remove id }` | integration → server | entry account, current channel |
| INT+6 | `SurfaceEvent { integration, channel, Surface \| removed id }` | server → client | audience |
| INT+7 | `CommandCatalog { revision, repeated CommandSpec }` (≤ 16 KiB) | integration → server | entry account |
| INT+8 | `CommandQuery { request_id, conversation }` | client → server | conversation readable |
| INT+9 | `CommandList { request_id, repeated (integration, CommandSpec) }` | server → client | — |
| INT+10 | `SurfaceAction { request_id, integration, surface, revision, subject, control, idempotency_key }` | client → server | audience |
| INT+11 | `CommandInvoke { request_id, integration, Invoke }` | client → server | conversation readable; text bucket |
| INT+12 | `IntegrationRequest { ticket, Actor, oneof { action, invoke } }` | server → integration | — |
| INT+13 | `IntegrationReply { ticket, outcome, text }` | integration → server | its own ticket |
| INT+14 | `IntegrationResult { request_id, outcome, text }` | server → client | requester only |

- Surfaces and catalogs live in nuschld's memory, ≤ 4 surfaces per publishing
  connection, dropped when it ends.
- Updates are coalesced to ≤ 2 per second per surface, latest wins.
- Invocations share Murmur's text bucket with chat.
- All of it is plaintext to the server and labelled "Not end-to-end encrypted".

### 8.2 Stock Mumble: plugin data

Used on stock Murmur and on Nuschl servers without `integrations/1`; never where
the sticky record holds `integrations/1`, never on `e2ee/1`. Two data ids, one
per direction, so neither side decodes what it never accepts:

```proto
message FromIntegration {               // "nuschl/int/1", ≤ 960 bytes encoded
  uint32 version = 1;                   // 1
  oneof body {
    Hello       hello = 2;              // { catalog revision, command count ≤ 16, surface count ≤ 4 }
    Surface     surface = 3;            // §4.3, the same message as on Nuschl servers
    bytes       removed = 4;            // surface id
    CommandItem command = 5;            // { catalog revision, index, count, CommandSpec }
    Result      result = 6;             // { key (the request's idempotency key), outcome, text ≤ 200 bytes }
  }
}
message ToIntegration {                 // "nuschl/int-req/1", ≤ 960 bytes encoded
  uint32 version = 1;
  oneof body {
    Sync   sync = 2;                    // { catalog, surfaces }: ≤ 1 per 30 s per integration
    Action action = 3;                  // { key, surface, revision, subject, control }
    Invoke invoke = 4;                  // §6
  }
}
```

- A full `now_playing/1` surface is about 560 bytes.
- Catalogs are **itemised, not byte-chunked:** one command per message, complete
  only when all `count` items of one revision arrive within 10 s; otherwise
  discarded.
- **Integration side:** sends `hello` and surfaces to its channel and listeners,
  batching newcomers once a second, and surfaces only on state changes; answers
  a `sync` only to the requester.
- **Client side:** strict decoding; accept only from a present, registered
  session with a certificate; apply consent (§3.2); ≤ 4 surfaces, ≤ 16 commands
  and ≤ 4 accepted messages a second per sender; place a surface in the sender's
  current channel as the server reports it; accept results only from the same
  presence, for an outstanding key, within the timeout. These checks limit
  noise; they don't make the sender verified.

### 8.3 End-to-end servers: not in v1

The owner asked to "prioritize webrtc and e2ee". nuschld doesn't offer
`integrations/1` with `e2ee/1`, so there are no surfaces or commands on `e2ee/1`
servers or in MLS conversations. Voice is blocked there until SFrame, commands
outside MLS would be a quiet privacy downgrade, and an integration inside MLS
needs its own key-transparency-bound identity. That needs a separate design.

### 8.4 Web client

The SDK logic compiles to WASM, so validation and bounds are identical. The
stock path works once plugin data reaches the browser session; the Nuschl path
waits for browser extension negotiation. A Web Worker is not an OS sandbox, and
none is claimed.

## 9. Security and bounds

- **Look-alike UI:** a fixed frame names the integration, and the dock is its
  only place: no notices, dialogs, timeline rows, sounds, notifications, focus
  or window raising, no security icons and no custom colours.
- **Phishing:** fixed transport labels, no links or inputs in v1, and actions
  carry only ids.
- **Confused deputy:** actions and commands only send typed data to the
  integration. They can't join channels, post as the user, open URLs, touch
  audio or change settings.
- **Fail closed:** no consent shows nothing; unknown templates are not shown; an
  incomplete catalog is discarded; `e2ee/1` has no integrations; a missing
  capability hides the feature and never falls back to text parsing.

| Item | Bound | Over the bound |
| --- | --- | --- |
| Registry | ≤ 64 entries; ≤ 8 accounts each; name ≤ 32 bytes, unique | `LIMIT_REACHED` / `MALFORMED` |
| Surfaces | ≤ 4 per publisher; per client ≤ 32 per server, ≤ 64 KiB per server, ≤ 256 KiB in total; dock ≤ 3 cards | Refused / oldest dropped / "2 more" |
| Surface size | ≤ 2 KiB on Nuschl servers; ≤ 960 bytes on stock | Refused |
| Strings | Title, subtitle, detail ≤ 160 bytes; result ≤ 200; one line, cleaned | Refused (surface), cut (result) |
| Timing | Duration ≤ 24 h; `age_ms` ≤ 60 s from integrations, ≤ 1 h after re-anchoring | Unknown / clamped |
| Update rate | Nuschl ≤ 2/s per surface, coalesced; stock ≤ 4 accepted messages/s per sender | Coalesced / dropped |
| Repaint | ≤ 1 Hz per visible playing card | — |
| Actions | 1 in flight per surface per client; ≤ 8 pending per client; 5 s timeout; Nuschl 60/min (burst 10) per connection; 2/s per actor; 64 remembered keys per actor | Ignored / `RATE_LIMITED` / `UNAVAILABLE` |
| Commands | ≤ 16 per catalog; spec ≤ 900 bytes; catalog ≤ 16 KiB; ≤ 4 args, ≤ 600 bytes in total; text arg ≤ 256 bytes; ≤ 8 choices; 10 s timeout | Refused |
| Stock catalog | One revision per sender at a time, complete within 10 s | Discarded |
| Integrations tracked | ≤ 32 per server per client | Least recently seen dropped |
| Local state | ≤ 256 entries, ≤ 64 KiB | Least recently seen dropped |
| Registry audit | ≤ 1,000 rows | Oldest dropped |

## 10. The music bot as first provider

- **Surface:** each connection publishes one `now_playing/1` for its channel:
  track title, artist and album; position from the player; the current queue
  entry id as the subject. It republishes on player changes, never on a progress
  timer, and removes the surface when nothing is queued.
- **Actions** use the existing player functions: play/pause → `set_paused`, next
  → `skip`, previous → restart if more than 3 s in, otherwise replay the last
  history entry with the current song back at the front of the queue (Owner:
  "sure").
- **Commands:** `/web` (flag `new`; the link still arrives as a private text
  message), `/pause`, `/play`, `/skip`, `/previous`.
- **Who may press** is the bot's policy (Owner). Proposed: registered users in
  its audience may use the controls and commands, anyone may use `/web`, and a
  per-channel `controls = "registered" | "everyone"` setting changes it (D10).
- **Registration on stock servers is required.** The operator registers each
  connection, or a new `register = true` option registers it where Register self
  is allowed. Unregistered, the bot publishes nothing, and its web page says
  "Register this bot to show its controls."
- **Transport:** typed messages with `integrations/1` and a registry entry;
  plugin data on stock servers; nothing with the capability but no entry ("Not
  added to this server's integrations."). The web page shows each connection's
  certificate SHA-1 so admins can find the account.
- **Unchanged:** `!web`, the codec worker, the web API and every boundary in
  `docs/musicbot.md`. No new dependency.

## 11. Slices and tests

| Slice | Contents | Depends on | Agent-days |
| --- | --- | --- | --- |
| **F0** Foundation | `nuschl-core::integration`: view models, validators, sanitiser, interpolation, command validation, consent rules, provider helpers | — | 3–4 |
| **F1** Protocol freeze | `integrations/1`, the INT block, data ids, migration number, `docs/extension.md` | F0; review | 1 |
| **S1** SDK | `crates/client/src/integrations.rs`: surfaces, provenance, local state, tickets, completion, results | F1 | 4–5 |
| **N1** nuschld | Registry, CLI, audit, publish rules, audience, routing with attested actors, timeouts, rates | F1 | 6–8 |
| **P1** Stock path | Receive rules, consent and offers, catalog sync, actions over plugin data | F1; #93 on main | 4–5 |
| **B1** Music bot | §10 on both transports | F0; N1 or P1 | 4–5 |
| **U1–U3** Frontends | Desktop, TUI, web: dock, offer, Integrations, completion (shared with mentions), accessibility | S1 | 10–13 |
| **A1** Artwork | Nuschl only, through the content worker | N1, U1; approval | 3–4 |

v1 (F0–U3 and B1) is about 35–45 agent-days; N1 and P1 can run in parallel after
F1, U1–U3 after S1. **Owner priority:** WebRTC and E2EE come first, and this
work doesn't take shared files, review time or build windows they need.

**Tests that are not obvious from the code** (private loopback nuschld and
private Murmur only):
- **Consent keyed on server-asserted fields:** a new certificate or registration
  under the same name asks again; a hash or name in the payload is ignored;
  unregistered or certificate-less sessions are ignored; a raw client claiming
  integration status on stock Murmur gets only an offer.
- **Stale actions:** refused after a reconnect, generation change, subject
  change or disabled control; a late result doesn't touch a new card; Next on a
  replaced song gives `STALE`.
- **Audience re-evaluation:** a move, listener change or ACL change withdraws a
  surface; a guest's press reaches the integration with an unregistered `Actor`;
  someone outside the audience is refused.
- **Downgrade:** sticky `integrations/1` ignores plugin-data surfaces; `e2ee/1`
  shows nothing; `NOT_FOUND` is uniform; a non-entry account cannot publish.
- **Stock interop:** a stock Mumble client still uses `!web`; bursts above
  Murmur's bucket degrade without wrong state.
- **Fuzz:** the INT block into nuschld state, both stock payloads, catalog
  assembly. **Storage:** the migration on SQLite and PostgreSQL.

## 12. Decisions

**Made by the owner (2026-11-13):**
- **D1 Stock authority:** consent only, registered bots only ("maybe we can on
  mumble servers at least require that the bot is registered?"), always
  "Unverified".
- **D2 Registry managers:** holders of Write on root and the operator CLI ("yes
  to both").
- **D3 v1 kit:** `now_playing/1` only; `card/1` with a second provider ("sure";
  confirm if it meant otherwise). Candidate providers are [listed on
  #203](https://github.com/ambiso/nuschl/issues/203#issuecomment-6858735940).
- **D4 Who may press:** anyone; the integration decides from the identity the
  server passes ("maybe anyone and it's up to the integration to decide the
  permission model here?").
- **D5 Previous:** restart if more than 3 s in, otherwise the previous track
  ("sure").
- **D7 E2EE:** deferred ("please prioritize webrtc and e2ee").

**Open:**
- **D6 Allocations:** the message block, the capability name, the two data ids
  and the migration number.
- **D8 Argument suggestions:** static choices in v1 (recommended), dynamic ones
  later with their own rate budget.
- **D9 Per-user surfaces:** needed by a per-user stats integration; a separate
  feature needing approval.
- **D10 Music bot controls:** registered users by default, with the per-channel
  setting (recommended).
- **D11 Minimal registry (recommended):** `{id, name, accounts}` for the whole
  server in v1; channel scope, template permissions, audience rules, opt-out and
  a request audit when a provider needs them.
