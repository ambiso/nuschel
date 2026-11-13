# Integrations: typed commands and a native UI kit

Planning document, 2026-11-13, written against main `d82473e3` (line references are
at that revision unless a frozen revision is named). Nothing here is implemented, and
no source, manifest or graph file was edited for this plan. It covers
[#203](https://github.com/ambiso/nuschl/issues/203) (owner request) and builds on
[#93](https://github.com/ambiso/nuschl/issues/93) (plugin data) and
[#22](https://github.com/ambiso/nuschl/issues/22) (music bot). It is design input
for review and grants no approval.

**Owner request (2026-11-13), binding for this plan:**

> "integrations with commands and auto-completion in the message box (1a) a UI kit for
> integrations - e.g. above the "Voice connected" status there could be a current song
> title, previous/(play/pause)/next, duration + progress bar UI and clicking the
> buttons should work - this needs to be designed thoroughly though - ideally we don't
> need to embed a webview just for these integrations; so we effectively have to build
> a small UI kit here? what does carplay do here? they allow full SwiftUI
> applications? integrations should also be dismissable (and recoverable if you want
> to see them again) and should be isolated/not have any security impact by merit of
> being designed well (not necessarily in a separate worker if we can avoid it)"

> "also not just anyone should be able to add integrations to a server - maybe we can
> piggy-back on some permission here - at least for mumble servers, and do something
> better for nuschl servers?"

The project rule from the message-interactions plan (`f2089e98`,
`docs/message-interactions-plan.md`) applies unchanged: structured protocol data on
Nuschl servers, legacy text only as presentation for stock users, plugin data as the
carrier of structure on stock Mumble, and nothing parsed back out of display text.

## 0. Short answers

- **CarPlay does not run apps' SwiftUI.** Apps fill a fixed set of system templates
  with data and the system draws them (§4.1). Only navigation apps draw a map. Nuschl
  should do the same: a few versioned templates, filled with typed data, drawn
  natively by each client.
- **Yes, a small UI kit, but a closed one.** In v1 it is one template, `now_playing/1`.
  A second one, `card/1`, is specified now and built when a second provider needs it.
  There is no layout language, no expressions and no code.
- **No webview and no new worker.** Surfaces are small protobuf values. They are
  checked in the protocol layer and the SDK against fixed limits, then drawn with
  native widgets. Nothing is executed, fetched or parsed as markup. Only artwork
  would need the existing content worker, and artwork is not in v1.
- **Authority on stock Mumble.** A client cannot see anyone else's permissions, so
  the only permission it can piggy-back on is **Write on the root channel**. On
  Murmur's defaults that means administrators. Admins can list integrations with a
  strict, versioned marker in the root channel's description (§3.2). Everything else
  needs each user's own consent. Section 3.1 states what a stock server cannot
  guarantee. This is owner decision D1.
- **Authority on Nuschl servers.** nuschld keeps an integration registry. Holders of
  Write on the root manage it, as an extension policy without a new ACL bit. Entries
  bind integrations to accounts, scope them to channels, and let the server decide
  who receives a surface and who may press its buttons. The server attests the entry
  to clients and keeps an audit trail (§3.3).

## 1. What exists today (at d82473e3)

| What | Where | Notes |
|---|---|---|
| Music bot text command | `apps/musicbot/src/mumble.rs:541-575` (`answer`), `:582-597` (`plain_text`) | `!web` and `!web new`, in the channel or privately; the link comes back as a private `TextMessage`. The bot reads text addressed to itself; no Nuschl client interprets it |
| Music bot comment | `mumble.rs:486-497` | Public instructions: "Send me `!web` …". Republished on every connect |
| Music bot controls | `apps/musicbot/src/web.rs:929-980` (`command`); `player.rs:513` (`enqueue`), `:553` (`seek`), `:634` (`skip`), `:643` (`set_paused`) | `pause`, `resume`, `skip`, `clear`, `seek`, `volume`, `ducking`, `remove`, `move`, `add`, `attach`. There is no "previous". Any valid link controls every queue (`docs/musicbot.md` "Links") |
| Music bot state | `player.rs:117-155` (`Playing`, `State`); `subsonic.rs:61-77` (`Track`) | Title, artist, album, duration in seconds, position as 20 ms frames, paused, buffering, queue entry ids, history |
| Music bot identity | `docs/musicbot.md` "Channels" | One Mumble connection per configured channel, each with its own client certificate. Registration is not automated |
| Desktop sidebar | `apps/desktop/src/view.rs:807-812`; `voice.rs:494` (`card`); `docs/voice-status.md` | The channel list, then the voice card ("Voice connected"), then the user panel. The voice card is server-scoped and independent of the text selection |
| TUI commands | `apps/tui/src/app.rs:1515` (`command`) | Built-in `/join`, `/edit`, `/help`, … parsed from the user's own composer |
| Inert content model | `crates/core/src/lib.rs:126-245` (`ContentBlock`, `TextRole`, `Span`, `Style`); `:258` (`allowed_link`) | Text and style runs, no HTML. Links only to allowed schemes |
| Plugin data (#93) | Frozen `0e40c7ac` (not on main): `crates/protocol/src/plugin.rs`, `crates/client/src/client/plugin.rs`, session and nuschld hunks | Data ≤ 1,000 bytes, data id ≤ 100 bytes, Murmur bucket 4/s with a burst of 15 (it only reliably drains about 2/s), forwarded to listed sessions **without channel authorisation**, sender session set by the server, no delivery acknowledgement. The SDK paces 2/s with a burst of 10 and allows ≤ 128 receivers per send. The `nuschl/` namespace is reserved and neither sent nor delivered by the generic paths. nuschld forwards nothing on an end-to-end server |
| Extension registry | `docs/extension.md:120-134`; Root's freeze (relayed by the coordinator): polls 0x4E98–0x4E9F, RTC 0x4EA0–0x4EAF, interactions 0x4EB0–0x4EBF | Capabilities are sticky and never downgraded (`extension.md:58-78`) |
| Authorisation | `apps/server/src/authz.rs:65+` (`Action`), `:600-602` (`ChannelSettings` = Write on the channel); `apps/server/src/state.rs:872-876` (default root ACL) | Defaults: `admin` gets Write, `auth` gets Make temporary channel, `all` gets **Register self**. Extension policy lives outside the Mumble bitmask (`extension.md:212-221`) |
| Descriptions | `docs/hash-documents.md`; `crates/client/src/platform.rs:46` (`RequestBlobs`) | Long channel descriptions arrive as a hash. The SDK requests a body on demand and renders it in the content worker |
| Live region | `apps/desktop/src/typing.rs` | A polite AccessKit live region, reused for completion and results |
| Web client | `docs/web.md:298+` | No extension negotiation yet. On nuschld it is a stock client |

## 2. Model

### 2.1 What an integration is

An **integration** is a principal that publishes typed **surfaces** and a typed
**command catalog**, and receives typed **actions** (button presses) and
**invocations** (commands). It does not post into conversations through this
feature. It may still send ordinary chat messages like any user, and those stay
ordinary messages.

| Part | What it is | Lifetime |
|---|---|---|
| Principal | Nuschl: a registry entry (§3.3) with up to 8 registered accounts, one per connection (the music bot has one per channel). Later perhaps also an in-process nuschld service. Stock: one session's certificate hash | Nuschl: until an admin removes the entry; ids are never reused. Stock: while that certificate is used |
| Surface | One template instance: `(principal, surface id)` with a monotonic `revision` and a `subject` (§5) | While the publishing connection or presence is online, or until removed. Never stored in history |
| Catalog | Up to 16 command specs with a `revision` | While the publishing connection or presence is online |
| Action / invocation | A request from one user to the principal, bound to a ticket (§5) | Until its result or a timeout (5 s for actions, 10 s for commands). Never retried automatically |
| Local state | Hidden or shown, and consent on stock servers (§7) | On the device, bounded |

### 2.2 Identity per path

| Path | Key the client stores | Display name | Not used |
|---|---|---|---|
| Nuschl server with `integrations/1` | `(server history identity, IntegrationId)`: 16 random bytes from nuschld | The registry's name, which an admin sets | The bot's nickname, session ids, registration numbers |
| Stock Murmur, or a Nuschl server without `integrations/1` | `(server history identity, certificate SHA-1)` from `UserState.hash` | The sender's name in `UserState`, as for any user. A short id ("3f1c 9a…") appears in About | A name inside the payload. Sessions without a certificate are ignored |
| E2EE (`e2ee/1`) | None in v1 (§8.3) | — | — |

A new certificate is a new integration on stock servers, so consent is asked again.

### 2.3 Provenance the client shows

| Level | When | Words in About |
|---|---|---|
| Added by admins | Nuschl registry entry | "Added by Alice · 3 Nov" |
| Listed by this server | Stock: its certificate hash is in the root marker (§3.2) | "Listed by this server" |
| You allowed this | Stock: local consent | "You allowed this" |

No path shows a check mark or the word "verified". About always says "Sees who
presses its buttons." On stock servers and in compatible mode it also says "Not end-to-end
encrypted."

## 3. Authority: who may add and operate integrations

### 3.1 Stock Mumble: what a client can observe

A stock client can query **its own** permissions (`PermissionQuery`), not anyone
else's. It sees ACLs and group membership only with Write on that channel. It sees
every user's name, registration (`user_id`) and certificate hash. What could carry an
administrator's say-so:

| Signal | Who can produce it | Verdict |
|---|---|---|
| The integration is registered | Anyone, by default: `all` holds Register self (`state.rs:875`, as in Murmur) | Not authority. It only stops a second session using the same registered name. Required for offers, as cheap noise control |
| Its certificate hash | The key holder; TLS proves possession on an honest server | Identity continuity, not approval |
| Root channel description | Write on root, which is the `admin` group on defaults | **The only admin-only data every client can read.** Recommended carrier for a strict marker (§3.2, D1) |
| Another channel's description | Write on that channel | Later, as per-channel scope. Not in v1 |
| Welcome text | The operator's config file | Rejected: it needs a restart, is shown to everyone at every login and is not editable from a client |
| Priority speaker, server mute | Mute/Deafen holders | Rejected: changes audio for everyone |
| Presence in an Enter-restricted channel | ACL editors, or someone who moved it | Rejected: others cannot see the restriction, and Move overrides it |
| Plugin data claiming approval | Anyone; Murmur checks no channel or permission | Never authority |

**What a stock server cannot guarantee, even an honest one:** who pressed a button
(only the server's session claim, which the integration checks itself); delivery;
that no other user receives or sends `nuschl/` plugin data; an audit of who listed
what; that the marker's writer is an "admin" rather than anyone granted Write on root.
**A malicious stock server** can forge sessions, certificate hashes, descriptions and
plugin data, and can drop anything. Nothing on the stock path claims more than "this
server says so". The UI never uses the word "verified" here.

### 3.2 Recommended stock policy: admin marker plus personal consent

**Marker.** It is one token in the **root channel's description**, written by someone
with Write on root:

```text
marker = "nuschl-integrations/1:" mode ":" [ hash *15( "," hash ) ]
mode   = "allow" / "only"
hash   = 40( DIGIT / %x61-66 )   ; lower-case SHA-1 hex of the integration's certificate
```

Example line in the description: `Integrations: nuschl-integrations/1:allow:3f1c…e2`.

- `allow`: listed integrations show without asking. Others may still offer themselves
  for personal consent.
- `only`: listed integrations show, and no others are offered.
- **Extraction** is an exact byte match over the raw description string the session
  already holds, at most 128 KiB, with no HTML parsing. The prefix must occur exactly
  once. The byte before it must be the start of the description or a byte outside
  `[A-Za-z0-9_/-]`. After the last hash or the second colon must come the end or a
  byte outside `[0-9a-f,]`. The grammar uses only ASCII letters, digits, `/`, `:`
  and `,`, which an HTML editor has no reason to escape. That stock Mumble's editor
  keeps the token intact is still to be checked in the stock-server test (§12.3).
- **Fail closed:** two prefixes, a bad mode, a short hash, more than 16 hashes or an
  oversized description all count as `only` with an empty list. Nothing shows, and the
  Integrations list says "This server's list can't be read." A missing marker
  means "no admin list", and consent applies.
- **The SDK asks for the root description body** (one `RequestBlobs` per advertised
  hash) only while the stock path is active and some `nuschl/int/1` sender is present.
  A description change re-evaluates the list. A removed hash withdraws that
  integration's surfaces at once.
- The marker is visible to stock users. That is deliberate: everyone can see what the
  admins listed. The marker can also be hidden in an HTML comment, but whether stock
  Mumble's description editor keeps comments when it saves is unverified. The visible
  form is recommended.

**Why this does not break the "never parse legacy text" rule.** That rule forbids
recovering *message semantics* from text written for display. The marker is a
declaration written deliberately as configuration. It is carried in the only
admin-writable field a stock server offers, and is matched exactly, with no heuristics
and no fallback. It never applies to chat messages, comments or other channels, and
it only narrows what is shown. It cannot grant a permission, start an action or show
anything a user has not been offered. If the owner prefers no exception at all, the
alternative is consent only (D1).

**Consent and offers.** An integration that is neither listed nor declined may make
**one offer**, under all of these conditions:

- its sender is registered and has a certificate;
- the marker is absent or says `allow`;
- the sender is in, or listened to in, the user's current voice channel;
- no other offer is showing.

The offer is a row in the dock: "Show controls from Music?" with **Show** and **No**.
The decision is stored per server and certificate (§7). There is never more than one
offer row, and a declined integration never asks again until the user resets it in
the Integrations list.

**Admins with Write on root** see a "Copy ID" item in About, so they can paste the
hash into the marker. The music bot prints its hash at startup and on its web page
(§11).

### 3.3 Nuschl servers: a registry the server enforces

**Capability** `integrations/1` (proposal), only in compatible mode (§8.3). It is
sticky like every capability. Once negotiated, the client ignores `nuschl/int*`
plugin data from that server. nuschld with `integrations/1` drops `nuschl/int`
plugin data instead of forwarding it, so no user can get round the registry on the
stock path.

**Registry entry** (persistent; one storage migration, `00NN_integrations.sql`, number
by Root):

```proto
message IntegrationInfo {
  bytes  id = 1;                 // 16 random bytes, never reused
  string name = 2;               // ≤ 32 bytes, one line, unique on the server (case-insensitive)
  repeated bytes accounts = 3;   // ≤ 8 registered accounts (16-byte ids), each in at most one entry
  repeated uint32 channels = 4;  // scope: ≤ 16 channels, each with its subtree; empty = the whole server
  uint32 templates = 5;          // bit set of templates it may publish
  bool   commands = 6;           // may publish a catalog
  Audience audience = 7;         // VOICE (default): in or listening to the surface's channel; READERS: can_read
  bool   guests_act = 8;         // unregistered users may press and invoke (default false)
  string added_by = 9;           // name snapshot of the admin, ≤ 64 bytes
  uint64 added_ms = 10;
  uint64 catalog = 11;           // current catalog revision, 0 when none is published
  bool   opt_out = 12;           // supports "stop sending to me"
}
```

**Who manages it.** A new `Action::ManageIntegrations` is granted by **Write on the
root channel**, the existing administrator permission, as an extension policy rather
than a new ACL bit (a stock ACL editor would erase one, `extension.md:212-221`). It is
also available offline through `nuschld admin integrations list|add|set|remove`. In
v1, channel moderators cannot add integrations. Hiding one per channel by Write on that
channel is a possible later step.

**What the server enforces:**

- **Publishing.** `SurfacePublish` and `CommandCatalog` are accepted only from a
  connection authenticated with persistent credentials (certificate or password) as one
  of the entry's accounts. The surface's channel must be within scope, and its template
  must be allowed. A bot moved out of scope loses its surfaces at once.
- **Audience.** A surface goes only to connections with `integrations/1` that are in
  the audience, are not opted out, and pass the current `authz_epoch` check. On moves,
  ACL changes and listener changes it is re-evaluated, and anyone leaving gets a removal.
- **Actions and invocations.** The server checks the actor:
  - they are in the audience (for an action), or the conversation is in scope and they
    may write there (for a command);
  - they are registered unless `guests_act`;
  - they are within the rate limits.

  It forwards a server-attested actor `{ account, user_id, session, name }` to the
  connection that published the surface or catalog. The integration applies its own
  policy on top.
- **Timeouts.** No reply within 5 s (actions) or 10 s (commands) gives the client
  `UNAVAILABLE`, and the late reply is dropped.
- **Errors** follow `extension.md` §2: an unknown integration and "not allowed" are the
  same `NOT_FOUND`, so the registry cannot be probed.

**Attestation and audit.** `IntegrationList` returns the entries in scope for the
requester over the authenticated extension channel. Clients show the registry name and
"Added by …". Changes are pushed (`IntegrationChanged`). nuschld keeps an
`integration_audit` table (time, admin account, change, integration id; ≤ 1,000 rows,
oldest dropped) that managers can read through the CLI. Logs never contain command
arguments or result text.

### 3.4 Side by side

| Question | Stock Mumble | Nuschl server |
|---|---|---|
| Who adds an integration | Anyone can run a bot. Holders of Write on root can list or exclude integrations through the marker | Holders of Write on root, or the operator CLI, through the registry |
| What stops unwanted surfaces | The client: `only` mode, consent, one offer at a time | The server: unregistered principals cannot publish at all |
| Who receives surfaces | Whoever the integration lists; the client checks the sender's channel | The server's audience rule, re-evaluated on every change |
| Who may press | The integration decides from the server's session claim | The server checks the audience and registration, then the integration decides |
| Trust in the result | As much as the stock server, and no more | As much as the Nuschl server, with sticky negotiation and no downgrade |
| Audit | None | `integration_audit` |

## 4. The UI kit

### 4.1 Prior art (from memory unless a source is given)

| System | How it works | What we take |
|---|---|---|
| **CarPlay** | Apps choose from fixed templates (`CPListTemplate`, `CPGridTemplate`, `CPTabBarTemplate`, `CPInformationTemplate`, `CPPointOfInterestTemplate`, `CPActionSheetTemplate`/`CPAlertTemplate`, `CPContactTemplate`, `CPVoiceControlTemplate`, `CPSearchTemplate`, and the shared `CPNowPlayingTemplate`). An entitlement category (audio, communication, navigation, EV charging, parking, quick food, fueling, driving task, …) decides which templates an app may use. The system draws them, enforcing item counts and template depth while driving (exact limits from memory, not verified). Now Playing reads `MPNowPlayingInfoCenter` (title, artist, artwork, duration, elapsed time, rate) and sends `MPRemoteCommandCenter` commands. Apps may add a few typed buttons. **Only navigation apps draw**, and only their map base view. Since iOS 26, CarPlay also shows WidgetKit widgets and Live Activities: SwiftUI *descriptions* that the system renders. Apple says Live Activities in CarPlay are non-interactive ([WWDC25 session 216](https://developer.apple.com/videos/play/wwdc2025/216/)) | Fixed templates; data in, system UI out; fixed transport labels; the system owns layout and limits. **No full SwiftUI apps** |
| **Android for Cars** | Media apps publish a `MediaSession` and a browse tree, and Android Auto draws browsing and playback. Other categories use the Car App Library's templates (list, grid, pane, message, and navigation with an app-drawn map surface), with host-enforced item and step limits | Media as a typed session, not UI |
| **MPRIS, `MPNowPlayingInfoCenter`** | Position, rate and timestamp. Clients interpolate, and a `Seeked` signal marks jumps | Anchored, interpolated progress (§4.5) |
| **Slack Block Kit** | JSON blocks that Slack renders. Interactions are posted to the app with `action_id`, which must answer within 3 s. Images are by URL (fetched by Slack) | Typed action ids and timely answers. Rejected: URLs and markup in text |
| **Discord** | Buttons and selects in rows. Slash commands with typed options (string, integer, number, boolean, user, channel, role, attachment), up to 25 choices, autocomplete round trips. Interactions answered within 3 s. Adding a bot to a server needs **Manage Server** | Typed options and choices; installing needs a server permission. Autocomplete deferred (D8) |
| **Telegram** | Command lists with scopes (all chats, group admins, one chat), names ≤ 32 lower-case characters, `callback_data` ≤ 64 bytes, Mini Apps as webviews | Scopes; small opaque action payloads. Rejected: webviews |
| **Adaptive Cards** | Versioned JSON schema, host-rendered, with fallbacks | Versions with additions that are safe to ignore |
| **Matrix widgets** | iframes with a postMessage API | The path this plan avoids |

### 4.2 Principles

1. **Data, not code.** A surface is a value of a known template. There are no
   expressions, conditions, loops, styles, colours, fonts, positions or sizes.
2. **The client owns presentation.** Layout, labels of fixed controls, icons, colours
   and focus order come from the client. An integration can fill fields, set controls
   enabled or disabled, and nothing else.
3. **Versioning.** A template is `name/major`. A minor addition is an optional field
   that an older client can ignore safely, such as a new transport control it does not
   draw. Anything unsafe to ignore gets a new major version. Unknown templates and
   majors are not shown, and are counted once in the Integrations list as "Needs a
   newer Nuschl".
4. **One structured model.** The same `Surface` message travels on both transports
   (§8). Only the envelope differs.

### 4.3 The common envelope

```proto
message Surface {
  bytes  id = 1;            // 8 bytes, chosen by the integration
  uint64 revision = 2;      // strictly increasing per surface; older or equal revisions are dropped
  bytes  subject = 3;       // ≤ 16 bytes: what actions bind to (§5), e.g. the queue entry id
  oneof template {
    NowPlaying now_playing = 10;   // now_playing/1
    Card       card = 11;          // card/1 (§4.6)
  }
}
```

### 4.4 `now_playing/1`

```proto
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

- Labels and icons are the client's: "Previous", "Play" or "Pause" (from `state`),
  and "Next". An integration cannot rename a transport button.
- A disabled control is drawn disabled and exposed to assistive technology as
  disabled. A control missing from `controls` is not drawn.
- `STOPPED` with an empty title is not allowed. A surface with nothing to show is
  removed instead.
- Seeking by dragging the bar is a later minor version (`SEEK`). Older clients ignore
  it and keep a display-only bar.

### 4.5 Progress: anchoring and interpolation

- **No shared clock is needed.** The client anchors the position when it receives the
  surface: `anchor = local monotonic time − age_ms` (the SDK's `set_time`). Network
  delay of a few tens of milliseconds does not matter for a progress bar.
- **nuschld re-anchors** surfaces it holds for audience members who arrive later: it
  adds the time since receipt to `age_ms`, up to 1 h. Beyond that the position counts
  as unknown.
- **Shown position:** `min(duration, position + elapsed)` while `PLAYING`, frozen
  otherwise. With an unknown duration only the elapsed time is shown, without a bar.
  It stops at the duration and never runs past it.
- **Corrections:** a new revision with the same subject that is within 1 s of the
  interpolated value never moves the bar backwards. Larger differences jump. A new
  subject resets.
- **Cost:** a repaint once a second, only while the card is visible and playing.
  Nothing animates per frame, so reduced motion needs nothing extra.

### 4.6 `card/1` (specified now, built with a second provider)

```proto
message Card {
  Icon   icon = 1;                    // fixed set (§4.8)
  string heading = 2;                 // ≤ 80 bytes
  repeated CardLine lines = 3;        // ≤ 2
  CardProgress progress = 4;          // optional: value/max, or the timing fields of NowPlaying
  repeated CardButton buttons = 5;    // ≤ 3
}
message CardLine   { string text = 1; repeated CardSpan spans = 2; }  // ≤ 160 bytes, ≤ 8 spans
message CardSpan   { uint32 start = 1; uint32 end = 2; bool bold = 3; bool italic = 4; bool code = 5; }
message CardButton { string label = 1; Icon icon = 2; bool enabled = 3; bool confirm = 4; Tone tone = 5; }
```

- Lines become the existing inert `ContentBlock::Text` with `Span` styles restricted
  to bold, italic and code. They have no link, math or colour, and only the paragraph
  role. The SDK validates spans as sorted, disjoint, within bounds and on character
  boundaries.
- Button labels are ≤ 24 bytes. Actions name the button by its index.
- `confirm` or a destructive `tone` makes the client ask first: "Music: Clear queue?"
  with **Clear** and **Cancel**.
- `card/1` is justified as the generic status template, for example a game server's
  state, a stream being live or a running backup. It ships once a second provider is
  approved (D3).

### 4.7 Not in v1

| Item | Why not yet | Later shape |
|---|---|---|
| Artwork | Encoded images are untrusted. Stock plugin data cannot carry them (1,000 bytes; 64 KiB would take more than 30 s at 2/s) | Nuschl servers only: the integration uploads ≤ 64 KiB of encoded bytes (inline or a `files/1` object). The client decodes them in the **content worker** with its dimension and allocation limits and draws validated RGBA. Never a URL, and never fetched by the client |
| Links | Phishing surface; no need in v1 | A `link` field, checked with `allowed_link`, opened only on click with the same handling as chat links |
| Lists (`list/1`) | Queue browsing already lives on the bot's web page. Lists need paging and per-row actions, which means more look-alike surface | ≤ 20 rows, one action per row, paged |
| Text input in surfaces | Commands cover input | — |

### 4.8 Text, icons and sanitising

- Every string is valid UTF-8 and a single line. Control characters and bidirectional
  overrides are removed (#61; `apps/tui/src/document.rs` precedent). Empty after
  cleaning means absent, and an empty title refuses the surface. Text is elided when
  drawn and complete in the accessible name.
- Icons come from a fixed set: music, play, pause, previous, next, stop, info, warning,
  error, check, clock, refresh, people. There are no lock, shield, certificate or
  microphone glyphs, so a card cannot imitate a security or voice indicator.

### 4.9 Placement and rendering

**Desktop (GPUI).** A new **integration dock** sits in the sidebar between the
channel list and the voice card (`view.rs:807-812`):

```text
┌──────────────────────────────┐
│ ♪ Music · Lounge          ⋯  │  integration name · channel; menu: Hide, About
│ Song title that may elide…   │
│ Artist                       │
│ 1:23 ━━━━━━━○────────── 3:45 │
│        ⏮      ⏸      ⏭       │
└──────────────────────────────┘
  Voice connected · Lounge …     (existing voice card)
```

- **What shows:** surfaces whose channel is a voice place of yours (in it, or
  listening) on a connected server, matching the voice card. Selecting a text channel
  changes nothing. Server-wide surfaces show while that server is a voice place.
- **How many:** at most 3 cards. The rest are reachable from "2 more" and the
  Integrations list. The dock takes at most 30% of the sidebar height, with the shared
  `layout::scrollbar` and focus-reveal contracts (`docs/desktop-layout.md`). When the
  sidebar is short, a card collapses to one line: title and play/pause.
- **Server names:** with voice on two servers, the header shows "Music · Lounge ·
  Server name".
- **Accessibility (AccessKit):**
  - the card is a group named "Music, Lounge";
  - title and subtitle are labels;
  - the progress bar is a progress indicator with value text such as "1:23 of 3:45";
  - buttons have names and a disabled state;
  - the menu button is "More for Music".

  A track change is announced through the polite live region only while focus is
  inside the card, so it never talks over people.

**TUI (ratatui).** One line above the status lines:
`♪ Music · Lounge: Song — Artist  1:23/3:45  paused`. Tab reaches it like a pane.
Left and Right choose a control, Enter presses it, and the focused control starts
with `>`. `/integrations` lists integrations with their numbers, and `/show N` and
`/hide N` change them. `--plain` prints one line when the subject or state changes
("Music (Lounge): now playing Song — Artist"), never once a second. States are
words, never colour alone.

**Web (Svelte).** `IntegrationDock.svelte` sits above the voice controls as a
`<section aria-label="Music, Lounge">`:

- buttons use `--control-target` (44 CSS px);
- the progress bar is `role="progressbar"` with `aria-valuetext`, updated at most
  once a second while visible;
- actions pass through the existing action-context fences (`action-context.js`).

The state is computed in the SDK Worker (`crates/client-wasm`), and the UI receives
validated view models only.

### 4.10 Copy

Short and human, as the owner asks:

- Card: "Music · Lounge", "Previous", "Play", "Pause", "Next", "Hide", "About".
- Offer: "Show controls from Music?" with **Show** and **No**.
- About: "Added by Alice · 3 Nov", "Listed by this server" or "You allowed this";
  "Sees who presses its buttons."; "Not end-to-end encrypted."
- Results: "Done.", "Not allowed.", "Out of date.", "Music didn't answer."
- Integrations list: "Music · Hidden" with **Show**.
- Composer: "/skip — Skip this song · Music"; "No command /skp" with
  **Send as message**.

## 5. Actions

- **Ticket.** A press captures `ActionTicket { server history identity, connection
  generation, principal (IntegrationId, or certificate hash plus presence instance),
  surface id, revision, subject, control }`. Before sending, the SDK checks:
  - the connection generation is the same;
  - the surface is still present with the same subject;
  - the control is still in `enabled`.

  Otherwise it shows nothing and refreshes the button. Results carry the request id
  and are dropped if the ticket's generation, principal or surface no longer matches
  (`navigation.rs` precedent). A late result cannot change a new card.
- **Subject binding.** The integration refuses an action whose `subject` is no longer
  current, with `STALE` ("Out of date."). Pressing Next on song A therefore never
  skips song B. Revisions that only move the progress keep the subject, so presses
  are not refused needlessly.
- **One in flight per surface and client.** The pressed button shows busy until the
  result or the timeout, and further presses on that surface are ignored meanwhile.
  There is no automatic retry, even after a reconnect: an unconfirmed press is
  reported as "Music didn't answer." and never resent.
- **Idempotency.** Every action and invocation carries a random 16-byte key. The
  integration ignores repeats among the last 64 keys per actor, and nuschld rejects
  in-flight duplicates (`duplicate_request`).
- **Outcomes:** `OK`, `REFUSED`, `STALE`, `UNAVAILABLE`, `BUSY`, plus optional text of
  at most 200 bytes, plain and cleaned (§4.8). It is shown as a short local line, never
  stored in history.
- **Rates:**
  - on Nuschl servers, `integrations.actions_per_minute` per connection (default 60,
    burst 10), refused with `RATE_LIMITED` and `retry_after_ms`;
  - on the stock path, the shared plugin pacer (2/s, burst 10), after interaction
    annotations (`f2089e98` §2.5);
  - on the integration side, 2/s per actor.
- **Confirmation** applies only to `card/1` buttons marked `confirm` or destructive.
  The `now_playing/1` controls are not destructive.

## 6. Commands and completion

### 6.1 Schema

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

A catalog has at most 16 commands. The SDK validates invocations against the catalog
revision it holds, nuschld validates them again, and the integration validates them
last. A user argument type is left for later: it would expose account or certificate
choices and needs its own review.

### 6.2 Discovery per conversation

- **Nuschl server:** `CommandQuery { conversation }` returns the specs of integrations
  whose scope covers that channel, or whose account is the peer of a direct
  conversation, and that the requester may invoke. The client caches the answer per
  `(generation, conversation, catalog revisions)`. A revision bump in
  `IntegrationChanged` invalidates it.
- **Stock:** the catalog comes from consented or listed integrations present in the
  channel, or from the direct-message peer (§8.2). It is fetched when the composer of
  such a conversation gets focus, at most once per 30 s per integration.
- If no command is available in a conversation, `/` is ordinary text, as today.

### 6.3 Composer

- **Desktop:**
  - `/` as the first character of the composer opens a completion popup under the
    caret: "skip — Skip this song · Music". Arrows move, Tab or Enter chooses, Escape
    closes, and the polite live region says "3 commands".
  - The chosen command becomes a chip followed by labelled argument slots. Tab moves
    between slots. Choice and flag slots complete from their options, and text and
    integer slots are checked as you type. Enter sends once the required slots are
    valid.
  - A typed `/name` resolves only if exactly one available command has that name.
    Otherwise the popup stays open until the user chooses.
  - An unresolved `/word` shows "No command /skp" and **Send as message**. Enter alone
    never posts it by accident.

  The popup is the same component that mention completion (`@`, MI §5.6) needs, built
  once (§12.4).
- **TUI:** built-in commands win and integration commands never shadow them. Tab after
  `/` cycles the matching names in the status line, then the choice values. Arguments
  are positional in their declared order. The last text argument takes the rest of the
  line, and earlier ones accept double quotes. `/help` lists "Here:
  /skip (Music) …". This parses the user's own typing locally, as `/join` already does.
- **Web:** an ARIA combobox (`role="combobox"`, a listbox, `aria-activedescendant`),
  shared with mention completion. On touch the list sits above the keyboard.

### 6.4 Invocation and results

An invocation is never posted to the conversation. The sender sees a local line,
"/skip · Music", followed by the result ("Done."). The line belongs to that
conversation and generation, disappears after 10 s or at the next send, and is
announced politely. If the integration wants others to know, it posts an ordinary
message itself.

### 6.5 Stock users and text

- Stock users keep typing `!web`. The bot reads text addressed to it, which is its own
  legacy interface. That stays and is not extended here.
- Nuschl clients never treat `!…` or `/…` in received text as a command, and never
  draw buttons from a bot's text. Bot replies are ordinary messages.
- A Nuschl user may still type `!web` as text, and it is sent as text, unchanged.

## 7. Dismiss and restore

| State per `(server history identity, integration key)` | Effect |
|---|---|
| `shown` (default once listed, added or allowed) | Card in the dock, commands in completion |
| `hidden` | No card. Commands stay, because typing `/` is explicit |
| Stock consent `allowed` / `declined` / `unasked` | Gates both card and commands. `declined` never offers again |

- **Hide:** the card's menu → **Hide**. **Restore:** the Integrations list (desktop:
  the server menu → "Integrations"; TUI: `/integrations`; web: the server menu). It
  lists every integration seen on that server with its provenance and state, plus
  **Show**, **Hide** and, on stock servers, **Allow** and **Forget** (which resets
  consent).
- **No network effect.** Hiding changes local state only. If the integration supports
  it (`opt_out`), hiding also offers "Stop sending to me". On Nuschl servers that is
  `IntegrationOptOut`, and the server stops sending to that connection. The client
  repeats it after reconnects while the integration stays hidden. On stock servers it
  is a best-effort request to the integration. Showing again undoes it.
- **Storage:** `$XDG_STATE_HOME/nuschl/integrations.json`. It is versioned, bounded to
  256 entries and 64 KiB, with the least recently seen dropped first, and written
  atomically like other state files. An unreadable file starts empty: consent is asked
  again and hidden integrations reappear, which is never more permissive than before
  the file existed. The web client uses an IndexedDB store with the same bounds, scoped
  like local history.

## 8. Transports

### 8.1 Nuschl server, compatible mode (`integrations/1`)

The plan needs **one block of 16 message types**. The next block after Root's freeze
would be 0x4EC0–0x4ECF; this plan **does not claim it**. Offsets are written `INT+n`
until Root allocates the block (D6).

| Type | Message | Direction | Permission |
|---|---|---|---|
| INT+0 | `IntegrationList { request_id }` | client → server | any connection with the capability |
| INT+1 | `IntegrationListReply { request_id, repeated IntegrationInfo }` (≤ 64) | server → client | entries in scope for the requester |
| INT+2 | `IntegrationChanged { info | removed id }` | server → client | pushed to connections that listed |
| INT+3 | `IntegrationSet { request_id, IntegrationInfo }` (no id creates) → `Done` | client → server | `ManageIntegrations` |
| INT+4 | `IntegrationRemove { request_id, id }` → `Done` | client → server | `ManageIntegrations` |
| INT+5 | `SurfacePublish { channel, Surface | remove id }` | integration → server | entry account, scope, template |
| INT+6 | `SurfaceEvent { integration, channel, Surface | removed id }` | server → client | audience |
| INT+7 | `CommandCatalog { revision, repeated CommandSpec }` (≤ 16, ≤ 16 KiB) | integration → server | entry account, `commands` |
| INT+8 | `CommandQuery { request_id, conversation }` | client → server | conversation readable |
| INT+9 | `CommandList { request_id, repeated (integration, CommandSpec) }` | server → client | — |
| INT+10 | `SurfaceAction { request_id, integration, surface, revision, subject, control, idempotency_key }` | client → server | audience and registration |
| INT+11 | `CommandInvoke { request_id, integration, Invoke }` | client → server | scope, write and registration; text bucket |
| INT+12 | `IntegrationRequest { ticket, actor, oneof { action, invoke } }` | server → integration | — |
| INT+13 | `IntegrationReply { ticket, outcome, text }` | integration → server | must answer its own ticket |
| INT+14 | `IntegrationResult { request_id, outcome, text }` | server → client | the requester only |
| INT+15 | `IntegrationOptOut { integration, receive }` | client → server | — |

- Surfaces and catalogs live in nuschld's memory, not in storage. There are at most 4
  surfaces per publishing connection, and they are dropped when that connection ends.
- Updates are coalesced to at most 2 per second per surface, with the latest
  winning. A slow reader gets the newest state, never a backlog.
- Invocations share Murmur's text bucket with `ChatSend` (`extension.md:198-200`),
  so commands get no second allowance.
- Everything here is plaintext to the server and is labelled "Not end-to-end
  encrypted", like every compatible-mode extra.

### 8.2 Stock Mumble: plugin data

**Scope.** Stock Murmur, and Nuschl servers without `integrations/1`. Never on a
server whose sticky record holds `integrations/1`, and never on `e2ee/1`. Session
support for the reserved namespace (sending to exact receivers, pacing, delivery to
the SDK with sender session, presence instance and certificate hash) belongs to the
PD lane, equivalently in `nuschl-session` and `crates/client-wasm`. It is not on main
yet (`0e40c7ac`).

**Data ids** (proposal, PD allocates; D6). There are two, one per direction, so
neither side ever decodes what it never accepts:

```proto
message FromIntegration {               // "nuschl/int/1", ≤ 960 bytes encoded
  uint32 version = 1;                   // 1
  oneof body {
    Hello       hello = 2;              // { catalog revision, command count ≤ 16, surface count ≤ 4, opt_out }
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
    Invoke invoke = 4;                  // §6.1
    Opt    opt = 5;                     // { receive }
  }
}
```

**Sizes.** A `now_playing/1` surface at every cap encodes to about 560 bytes, and a
full `card/1` to about 800, both within the 960-byte budget below Murmur's 1,000-byte
silent drop. Catalogs are **itemised, not byte-chunked**: one command per message,
complete only when all `count` items of one revision have arrived within 10 s. A
partial catalog is discarded, so there is no reassembly of split bytes.

**Integration side** (provider helpers in `nuschl-core::integration`, used by the bot):

- It sends `hello` and its surfaces to everyone in its channel and its listeners.
- Newcomers are batched once a second, with ≤ 128 receivers per message (the
  session's limit) and the session pacer at 2/s, burst 10.
- Surface changes are sent on state changes only. Progress is never resent on a timer
  (§4.5).
- On a `sync` it answers only the requester.

**Client side:**

1. **Decode** strictly: ≤ 1,000 bytes, version 1, every field bound, unknown versions
   dropped.
2. **Accept** only from a present session with a certificate hash, tied to its
   presence instance. When the presence ends, its surfaces and catalog go.
3. **Apply the authority rule** (§3.2): listed, or allowed → show; `only` and not
   listed → drop; declined → drop; otherwise offer (once).
4. **Bound** each sender to ≤ 4 surfaces, ≤ 16 commands and ≤ 4 accepted messages a
   second; the excess is dropped.
5. **Place** the surface: its channel is the sender's current channel as this client
   sees it. A surface from a sender outside the user's voice places goes to the
   Integrations list only, never to the dock.
6. **Results** count only from the same integration presence, for an outstanding
   key, within the timeout.

**Trust.** It is as described in §3.1. Plugin data has no channel authorisation, so
the client's own checks (sender present, certificate, authority rule, placement by
the sender's real channel) are the only filter. The server sees everything. Stock
clients' plugin frameworks receive these ids and, as far as is known, ignore them.

### 8.3 End-to-end encrypted servers: not in v1

- nuschld does not offer `integrations/1` together with `e2ee/1`, and already forwards
  no plugin data there. Clients show no integration surfaces or commands on an `e2ee/1`
  server, and the composer of an MLS conversation offers no integration commands.
- Reasons:
  - Voice is blocked on `e2ee/1` servers until SFrame, so the first provider has
    nothing to play there.
  - Commands and surfaces passing outside MLS would be a quiet privacy downgrade.
  - An integration inside MLS needs its own identity: devices, a key-transparency
    binding and group membership.
- **Later (separate design):** an integration as an MLS member with a KT-bound
  identity, surfaces and invocations as a new MLS kind behind a group feature (as MI
  §2.6 does for interactions), and nothing visible to the server.

### 8.4 Web client

The SDK logic (`crates/client/src/integrations.rs`) compiles for WASM, so validation
and bounds are identical. The stock path works once PD's message-26 support reaches
`crates/client-wasm/src/session.rs`. The Nuschl path waits for browser extension
negotiation (`docs/web.md:298+`). Until then the web client shows no integration UI on
nuschld, which is honest, because nuschld with `integrations/1` drops the stock
carrier.

## 9. Isolation and security

### 9.1 Why no webview and no worker

- A webview would put a browser engine, scripts and resource loading into the client
  for every integration. It would also need per-integration origins, could draw
  convincing fake Nuschl UI, and would bring accessibility of its own.
- What makes untrusted content dangerous is code execution, resource fetching, complex
  parsing and layout injection. **None exist here.** Values arrive as length-checked
  protobuf, decoded in `nuschl-protocol` the way every Mumble and extension message
  already is (the plugin decoder's checks before allocation are the model). They are
  validated in the SDK into typed view models and drawn by native widgets from a closed
  template set.
- This is bounded, memory-safe domain logic in the portable layers, which AGENTS.md
  keeps there, under `unsafe_code = "forbid"`. The remaining risks are logic bugs and
  resource use. Fuzzing (§12.3) and the bounds in §10 address them.
- A thread, `catch_unwind` or "it's Rust" is not offered as isolation. None is needed,
  because nothing complex is processed.

### 9.2 What still needs isolation

| Input | Where | When |
|---|---|---|
| Artwork bytes | The existing content worker, with its image limits; validated RGBA to the UI | Not in v1 (§4.7) |
| Rich text beyond `CardSpan` | Not accepted. Markdown or HTML from an integration would go through the content worker, which this plan avoids | Never in v1 |
| The music bot's own media | The existing codec worker | Unchanged |

### 9.3 Threats

| Threat | Mitigation |
|---|---|
| Any user adds an integration | Nuschl: only registry accounts can publish. Stock: marker, consent, one offer, `only` mode |
| An impersonator with the same name | Keys are the IntegrationId or certificate hash, never names. Consent is per certificate. About shows the short id. Stock names come from `UserState`, not the payload |
| A look-alike of Nuschl UI | A fixed frame always names the integration. The dock is its only place: no notices, dialogs, timeline rows, sounds, notifications, focus or window raising. No security or microphone icons, and no custom colours |
| Phishing through buttons | Transport labels are fixed. Card labels are ≤ 24 bytes in a named frame. No links or inputs in v1. An action carries only ids, never user data |
| A confused deputy | Actions and commands only send typed data to the integration. They cannot join channels, send messages as the user, open URLs, touch audio or change settings |
| Flooding | Per-surface coalescing, per-integration caps, client per-sender limits, dock ≤ 3 cards, repaint ≤ 1 Hz (§10) |
| A stale action hitting a new target | Ticket, generation and subject binding; no automatic retries (§5) |
| A downgrade on Nuschl servers | Sticky `integrations/1`; plugin-data surfaces ignored there and dropped by nuschld |
| A malicious stock server | Out of reach. Labels say "Listed by this server" and never "verified" |
| Privacy | The integration learns who pressed or invoked and when, and that the sender runs Nuschl. The server sees all of it in compatible and stock modes. About says so. E2EE is excluded |
| Resource exhaustion in the client | Decode and aggregate bounds before allocation (§10) |

### 9.4 AGENTS.md contracts, checked

- **Workers:** no new executable, worker, argument shape or IPC message. Artwork, when
  approved, uses the content worker's existing image request.
- **Trusted selection:** integrations cannot select executables, files, SQL or URLs.
  nuschld's registry is storage-worker data with fixed statements.
- **Bounds on inputs and aggregates:** §10.
- **Validation before allocation and rendering:** strict decoding, cleaned strings,
  validated spans; nothing is rendered from unvalidated values.
- **Binding of asynchronous results:** §5 and §6.4.
- **Fail closed:**
  - a malformed marker means nothing is shown;
  - unknown templates are not shown;
  - an incomplete catalog is discarded;
  - `e2ee/1` has no integrations;
  - a missing capability hides the feature and never falls back to text parsing.
- **Browser parity:** same SDK and bounds. A Web Worker is not an OS sandbox
  (`docs/browser-content.md`), and none is claimed.
- **Audio:** nothing here touches capture, mute or deafen. The bot's audio and its
  boundaries are unchanged.

## 10. Bounds

| Item | Bound | Enforced by | Over the bound |
|---|---|---|---|
| Registry | ≤ 64 entries; ≤ 8 accounts and ≤ 16 scope channels each; name ≤ 32 bytes, unique | nuschld | `LIMIT_REACHED` / `MALFORMED` |
| Surfaces | ≤ 4 per publishing connection or stock sender; ≤ 32 per server per client; ≤ 64 KiB of surface data per server and 256 KiB across all servers per client; dock ≤ 3 cards | nuschld; SDK | Refused / oldest-arriving dropped / "2 more" |
| Surface size | ≤ 2 KiB on Nuschl servers; ≤ 960 bytes on stock (worst case about 560 or 800) | both ends | Refused |
| Strings | Title, subtitle and detail ≤ 160 bytes; heading ≤ 80; card lines ≤ 160 with ≤ 8 spans; button labels ≤ 24; result text ≤ 200; one line, cleaned | SDK; nuschld; provider helpers | Refused (surface), cut (result) |
| Timing | duration ≤ 24 h, else unknown; `age_ms` ≤ 60 s from integrations, ≤ 1 h after nuschld re-anchors; position clamped | SDK; nuschld | Unknown / clamped |
| Update rate | Nuschl: ≤ 2/s per surface, coalesced. Stock: the integration sends only on change, and the client accepts ≤ 4 messages/s per sender | nuschld; SDK | Coalesced / dropped |
| Repaint | ≤ 1 Hz per visible playing card | each frontend | — |
| Actions | 1 in flight per surface per client; ≤ 8 pending per client; 5 s timeout; Nuschl 60/min (burst 10) per connection; integration 2/s per actor; key memory 64 per actor | SDK; nuschld; integration | Ignored / `RATE_LIMITED` / `UNAVAILABLE` |
| Commands | ≤ 16 per catalog; spec ≤ 900 bytes; catalog ≤ 16 KiB; ≤ 4 args; text arg ≤ 256 bytes; args ≤ 600 bytes in total; ≤ 8 choices; 10 s timeout; text bucket (Nuschl) or plugin pacer (stock) | SDK; nuschld; integration | Refused |
| Stock catalog assembly | One revision per sender at a time, complete within 10 s, ≤ 16 items | SDK | Discarded |
| Integrations per server per client | ≤ 32 tracked (Nuschl list ≤ 64 entries, of which only those in scope arrive) | SDK | Least recently seen dropped |
| Marker | Description ≤ 128 KiB scanned; ≤ 16 hashes; exactly one prefix | SDK | Treated as `only` with an empty list |
| Local state | ≤ 256 entries, ≤ 64 KiB | SDK, each platform store | Least recently seen dropped |
| Audit | ≤ 1,000 rows | nuschld storage | Oldest dropped |

## 11. The music bot as first provider

**What changes in `apps/musicbot`:**

- **Surface.** Each connection publishes one `now_playing/1` for its channel from the
  player state:

  | Field | Source |
  |---|---|
  | title, subtitle, detail | `Track.title`, `artist`, `album` |
  | state | paused, buffering or playing; `STOPPED` when nothing is queued, so the surface is removed |
  | duration | `Track.duration` × 1000 |
  | position | frames × 20 ms |
  | subject | the current queue entry id |
  | enabled controls | previous: something to restart or replay; play/pause: a current song; next: a current song |

  It republishes on player changes (the existing `Changes` notifications), never on
  the web page's 2 s progress tick. A channel sharing another channel's queue shows
  the same song with its own surface.
- **Actions** use the existing player functions:
  - play/pause → `set_paused(!paused)`;
  - next → `skip()`;
  - previous → seek to 0 if more than 5 s in; otherwise the current song goes back to
    the front of the queue and the last history entry plays again (D5).

  A stale subject gives `STALE`. The actor's name is logged as for web commands, never
  the arguments.
- **Commands:**

  | Typed | Existing legacy or web form | Result |
  |---|---|---|
  | `/web` (flag `new`) | `!web`, `!web new` | The link still arrives as a **private text message**, as today, so it lands in local history; the typed result says "Sent you a private message." |
  | `/pause`, `/play` | web `pause`, `resume` | "Done." |
  | `/skip` | web `skip` | "Done." |
  | `/previous` | — (new, D5) | "Done." |

  Search-and-add (`/play <query>`) and stock text forms such as `!skip` are new
  features outside this issue.
- **Who may press:** anyone in the bot's audience (in its channel or listening), and
  registered users only on Nuschl servers by default (D4). This matches today's model,
  where anyone who can write to the bot gets a link that controls everything.
- **Transport choice:** with `integrations/1` and a registry entry for its account, the
  bot uses typed messages. On stock servers, or Nuschl servers without the capability,
  it uses plugin data (after PD). With `integrations/1` but no entry it publishes
  nothing, and the web page says "Not added to this server's integrations."
- **Identity for admins:** the web page's connection status and the startup log show
  each connection's certificate SHA-1, for the stock marker or for registering the
  account in the nuschld registry.
- **Unchanged:** the `!web` text command, the public comment, the codec worker, the
  web API and every security boundary in `docs/musicbot.md`. The bot gains no
  dependency: protobuf types come from `nuschl-protocol` and helpers from
  `nuschl-core`.

## 12. Slices, tests and ownership

### 12.1 Owner keys

| Key | Owner |
|---|---|
| **INT** | This lane (to be assigned) |
| **Root** | Codex: registry block, capability, migration numbers, `docs/extension.md`, SECURITY.md, TODO.md, manifests, final integration |
| **PD** | Plugin data (#93): session and wasm support for `nuschl/` ids, data-id allocation |
| **BOT** | Music bot (#22), if separate from INT |
| **MI** | Message interactions: shares the composer completion popup and the plugin pacer priority |
| **BROKER** | Browser action fences (`action-context.js`) |

### 12.2 Slices

Effort is focused agent-days including tests, excluding review queues and VM waits.

| Slice | Contents | Depends on | Effort |
|---|---|---|---|
| **F0** Pure foundation | `nuschl-core::integration`: ids, `Surface` and template view models, validators, sanitiser, interpolation, command spec and argument validation, marker scanner (if D1), provider helpers (revision counter, itemised catalog, per-actor rate, key memory); proto drafts | — | M: 3–4 |
| **F1** Protocol freeze | `integrations/1`, the INT block, both data ids, migration number, `docs/extension.md` section | F0; Root; PD; design review | S: 1 |
| **S1** SDK | `crates/client/src/integrations.rs`: per-server integrations, surfaces, provenance, local state sidecar, tickets, completion candidates, results; native plumbing | F1 | M: 4–5 |
| **N1** nuschld | Registry storage and CLI, `IntegrationSet`/`Remove` under `ManageIntegrations`, audit, publish enforcement, audience fan-out with epoch re-evaluation, action and command routing with attested actors, timeouts and budgets, dropping `nuschl/int` plugin data | F1 | L: 6–8 |
| **P1** Stock path | Session and wasm hookup of `nuschl/int*` (after PD), receive rules, offers, marker, catalog sync, actions | F1; PD on main | M: 4–5 |
| **B1** Music bot | §11 on both transports | F0; N1 and/or P1 | M: 4–5 |
| **U1** Desktop | Dock and card, menu and About, Integrations list, completion popup and chips (shared with MI), result line, AccessKit | S1 | M: 5–6 |
| **U2** TUI | Bar, `/integrations`, Tab completion, plain lines | S1 | S–M: 2–3 |
| **U3** Web | Dock and combobox through BROKER fences; stock path first | S1; PD wasm; BROKER | M: 3–4 |
| **C1** `card/1` | Rendering in three clients | U1–U3; a second provider approved | S–M: 2–3 |
| **A1** Artwork | Nuschl only, through the content worker | N1, U1; D3 | M: 3–4 |

v1 (F0–U3 and B1) comes to about 35–45 agent-days. N1 and P1 can run in parallel
after F1, and U1–U3 after S1.

### 12.3 Tests per layer

Private resources only: loopback nuschld, private Murmur, raw protocol clients,
synthetic tracks, disposable profiles. No public servers, host microphones or
profiles.

| Layer | Cases |
|---|---|
| Unit: core | Every bound at the edge and one past; sanitiser (controls, bidi overrides, invalid UTF-8, empty after cleaning); interpolation (play, pause, unknown duration, 24 h, `age_ms` limits, sub-second corrections never backwards, subject change resets); spans; argument validation; marker grammar (both modes, empty list, 16 and 17 hashes, upper-case hex, two prefixes, adjacent letters, HTML around it, 128 KiB + 1 → `only` empty); itemised catalog (missing item, mixed revisions, timeout) |
| Unit: SDK | Tickets refused after reconnect, generation change, subject change and disabled control; late results dropped; one in flight; authority matrix (listed, allowed, declined, `only`, no marker, unregistered, no certificate, sender left, sender outside voice places); offer limits; local state bounds and a corrupt file; sticky `integrations/1` ignores plugin-data surfaces; `e2ee/1` shows nothing |
| Unit: server | Publishing by a non-entry account, outside scope, a disallowed template, too large, too fast; audience on move, listen, ACL change and opt-out; actions by a guest, someone outside the audience, a duplicate key, a timeout, a late reply; commands outside scope, an invalid argument, the text bucket; `ManageIntegrations` = Write on root; audit cap; `NOT_FOUND` uniformity; `nuschl/int` plugin data dropped |
| Storage | Registry migration in SQLite and PostgreSQL; audit trimming |
| Fuzz | `integration_messages` (the INT block into nuschld state), `integration_plugin` (both stock payloads, catalog assembly, marker scanner), additions to `client_messages` |
| Integration: nuschld (`tests/integrations.py`, new) | Bot plus two Nuschl clients plus a raw extension client: surface appears for the audience only; a move withdraws it; Next by a member skips; a raw unregistered client and a non-entry account cannot publish; `IntegrationSet` by a non-admin refused; admin add and remove visible live |
| Integration: stock Murmur (`tests/stock_integrations.py`, new) | Bot plus two Nuschl clients (TUI test build) plus a raw stock client on a private Murmur: the stock client still uses `!web`; marker `allow` and `only` (with the root description edited by an owned admin); an unlisted bot gets one offer; an impostor with the same unregistered name and another certificate is a separate integration; a cross-channel injection by a raw client is not docked; burst above Murmur's bucket degrades without wrong state |
| Music bot (`tests/musicbot.py` extension) | Surface tracks play, pause, seek and skip; previous semantics; stale subject refused; `/web` still sends the private link |
| Accessibility (`tests/accessibility.py`) | Card group name, button names and disabled states, progress value text, Hide and restore through the Integrations list, completion listbox announcements, TUI plain transcript |
| GUI | Desktop scenes at 100% and 200%, a narrow sidebar, long Unicode titles, three cards and "2 more" (`tests/screenshots.py` pattern) |
| Browser | Dock and combobox with the synthetic SDK at 320, 390 and 1280 px, 44 px targets, focus visibility (`tests/web_controls.mjs` pattern) |

Results are reported as build, unit, private runtime, VM or physical. None implies
another.

### 12.4 Shared files and conflicts

| Path | Hunk | Risk |
|---|---|---|
| `crates/protocol/proto/nuschl.proto`, `src/nuschl/mod.rs` | INT block, capability | Medium: MI and polls are adding to the same files; freeze them together |
| `crates/session/src/connection.rs`, `crates/client-wasm/src/session.rs` | Reserved-id delivery and pacer priority | **High:** PD and MI annotations share the outbox. Land after PD |
| `apps/server/src/{handlers,authz,extension,config}.rs` | Plugin drop, `ManageIntegrations`, capability, settings | Medium |
| `crates/client/src/client.rs` | Hooks into the integrations module | High: a hub file shared with MI and LP |
| `apps/desktop/src/view.rs` | Dock between the channel list and the voice card; composer popup | Medium: MI mention popup. Build one completion component |
| `apps/tui/src/app.rs` | `/integrations`, Tab completion | Low–medium |
| `web/app/src/App.svelte`, `action-context.js` | Dock, combobox | Medium: BROKER |
| `apps/musicbot/src/{mumble,player,web}.rs` | Provider | Low |
| `Cargo.toml`, `Cargo.lock`, `nix/crate2nix/Cargo.nix` | **None expected** | — |

## 13. Open decisions

Recommendations come first. **(owner)** marks owner-level decisions.

- **D1 (owner) Stock authority:** the root-description marker (`allow`/`only`,
  certificate hashes, fail closed) plus personal consent with one offer (recommended),
  versus consent only with no exception to the text rule. A third option, no
  integrations on stock servers, is not recommended.
- **D2 (owner) Nuschl authority:** Write on root manages the registry as an extension
  policy, with per-entry channel scope, the voice audience by default and an audit
  table (recommended), versus the operator CLI only.
- **D3 (owner) v1 kit:** `now_playing/1` only, `card/1` specified and built with a
  second provider, and no artwork, links, lists or seeking in v1 (recommended).
  Artwork later only on Nuschl servers through the content worker.
- **D4 (owner) Who may press and invoke:** the integration's audience, registered
  users only on Nuschl servers unless the entry allows guests, with the integration
  free to narrow it (recommended).
- **D5 (owner) "Previous" in the bot:** restart if more than 5 s in, otherwise put the
  current song back and replay the last finished one (recommended), versus restart
  only, versus no previous button.
- **D6 (integrator) Allocations:** the INT block (Root), the capability name
  `integrations/1`, data ids `nuschl/int/1` and `nuschl/int-req/1` (PD), and the
  migration number.
- **D7 (owner) E2EE:** no integrations on `e2ee/1` servers in v1 (recommended); an
  MLS-member integration is a separate design.
- **D8 Argument suggestions:** static choices only in v1 (recommended). Dynamic
  suggestions from the integration later, with their own rate budget.
