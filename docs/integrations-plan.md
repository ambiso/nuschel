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

**Owner answers (2026-11-13, relayed by the coordinator), binding for this
revision:**

- Registry: "yes to both", meaning holders of Write on root and the operator CLI
  (§3.5).
- Who may press: "maybe anyone and it's up to the integration to decide the
  permission model here? not all integrations would control global state like the
  music bot - e.g. you could also have some sort of per-user stats; there we don't
  really care about an un-registered user pressing reset for their own stats."
  Servers pass the presser's identity and still bind, rate-limit and audit (§3.5,
  §5).
- Previous: "sure", meaning restart if more than 3 s in, otherwise the previous
  track (§11).
- "please prioritize webrtc and e2ee": integrations on `e2ee/1` are deferred, and
  this work yields to those lanes (§8.3, §12.2).
- Consent: "we need to think about a good UI/UX here. if you reject once it should
  still be possible to get the integration back (maybe the same place you get it
  back from if you dismiss it)" (§7).
- "how do integrations work on nuschl vs mumble servers? are both bots that connect?
  maybe we can on mumble servers at least require that the bot is registered?"
  §2.1 answers this. Registration is a hard requirement on stock servers.
- "what other integrations could be cool here?": Appendix A.

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
- **Authority on stock Mumble: there is no permission to piggy-back on.** A client
  learns only its own permissions, and a stock server relays plugin data without any
  permission check. Nothing can stop a server, or any user, from claiming to be an
  integration (§3.1). On stock servers an integration must be a **registered bot**,
  and it appears only after each user's own consent, at most one offer at a time.
  It is shown as **unverified** (§3.2). Text markers, such as a line in the root channel's
  description, and certificate hashes are rejected as sources of admin authority
  (§3.3). Operators who want admins to decide run nuschld.
- **Authority on Nuschl servers.** nuschld keeps an integration registry. Holders of
  Write on the root manage it as an extension policy without a new ACL bit, and so
  does the operator through the CLI. Entries
  bind integrations to accounts, scope them to channels, and let the server decide
  who receives a surface. Anyone may press its buttons. The server passes an attested
  identity with each press, and the integration decides. The server attests the
  entry to clients and keeps an audit trail (§3.5).
- **Both are bots.** On stock Mumble an integration is a bot connected as a
  registered user, speaking plugin data. On a Nuschl server it is a registered bot
  account bound in the registry, and the server carries and attests its cards
  (§2.1).

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

### 2.1 How integrations connect

| | Stock Mumble | Nuschl server (`integrations/1`) | Later |
|---|---|---|---|
| What it is | A bot that connects as an ordinary Mumble user, like the music bot today | A bot that connects as a registered account, which an admin bound to a registry entry | A server-side integration in or beside nuschld, with no voice connection or Mumble session |
| How cards and presses travel | Plugin data between the bot and each client; the server only relays it | Typed extension messages; nuschld checks, delivers and attests them | Typed messages from the server itself |
| Hard requirement | **Registered** (the server assigned a `user_id`) **and** has a certificate. Payloads from anyone else are ignored | The account is in a registry entry and logs in with persistent credentials | Separate design and approval |
| Who vouches | Nobody: "Unverified" plus the user's consent | The server: "Added by …" | The server |

- **Registration on stock servers** gives the integration a stable, server-assigned
  identity and a name no other session can take. It is **not** authority:
  registration is server-asserted, and on default servers anyone may register
  themselves (§3.3).
- **Voice is optional for integrations.** The music bot connects to voice because it
  plays music. Cards and commands do not need a voice connection, which is what makes
  server-side integrations possible later.

### 2.2 What an integration is

An **integration** is a principal that publishes typed **surfaces** and a typed
**command catalog**, and receives typed **actions** (button presses) and
**invocations** (commands). It does not post into conversations through this
feature. It may still send ordinary chat messages like any user, and those stay
ordinary messages.

| Part | What it is | Lifetime |
|---|---|---|
| Principal | Nuschl: a registry entry (§3.5) with up to 8 registered accounts, one per connection (the music bot has one per channel). Later perhaps also an in-process nuschld service. Stock: one registered session, remembered for consent by the registration number and certificate hash the server reports for it | Nuschl: until an admin removes the entry; ids are never reused. Stock: while the server reports that registration and certificate |
| Surface | One template instance: `(principal, surface id)` with a monotonic `revision` and a `subject` (§5) | While the publishing connection or presence is online, or until removed. Never stored in history |
| Catalog | Up to 16 command specs with a `revision` | While the publishing connection or presence is online |
| Action / invocation | A request from one user to the principal, bound to a ticket (§5) | Until its result or a timeout (5 s for actions, 10 s for commands). Never retried automatically |
| Local state | Hidden or shown, and consent on stock servers (§7) | On the device, bounded |

### 2.3 Identity per path

| Path | Key the client stores | Display name | Not used |
|---|---|---|---|
| Nuschl server with `integrations/1` | `(server history identity, IntegrationId)`: 16 random bytes from nuschld | The registry's name, which an admin sets | The bot's nickname, session ids, registration numbers |
| Stock Murmur, or a Nuschl server without `integrations/1` | `(server history identity, registration number, certificate SHA-1)` from `UserState.user_id` and `UserState.hash`. The server asserts them, not the sender, so they are only as trustworthy as that server. They are the key for consent memory and grant nothing | The sender's name in `UserState`, as for any user, tagged "Unverified". A short id ("3f1c 9a…") appears in About | Any name or hash inside the payload. Sessions without a registration or a certificate are ignored |
| E2EE (`e2ee/1`) | None in v1 (§8.3) | — | — |

A new certificate or registration is a new integration on stock servers, so consent is asked again.

### 2.4 Provenance the client shows

| Level | When | Words in About |
|---|---|---|
| Added by admins | Nuschl registry entry, enforced and attested by the server | "Added by Alice · 3 Nov" |
| Unverified, you allowed this | Stock: the user's own consent | Card tag "Unverified"; About: "Unverified. You allowed this." |

There is no check mark and no "verified" on any path. About always says "Sees who
presses its buttons." On stock servers and in compatible mode it also says "Not end-to-end
encrypted."

## 3. Authority: who may add and operate integrations

### 3.1 What a stock Mumble client can and cannot learn

- A client learns **only its own** permissions, per channel, through
  `PermissionQuery`. It cannot ask what anyone else may do.
- It sees ACLs and group membership only where it holds Write itself, so an ordinary
  user never learns who the admins are.
- It sees every user's name, registration (`user_id`) and certificate hash
  (`UserState.hash`). All three are **asserted by the server**, not by the user they
  describe. Registration is open to everyone by default (`all` holds Register self,
  `state.rs:875`, as in Murmur).
- Murmur forwards plugin data from any authenticated session to any listed session,
  with no channel or permission check (`0e40c7ac`, `plugin.rs`). A data id is a name,
  not evidence of who sent the data.

So on a stock server **nothing can prevent a claim of integration status**:

- any user can run a client or bot that sends integration payloads;
- a malicious stock server can invent sessions, names, certificate hashes and payloads,
  and drop anything;
- the integration learns who pressed a button only from the server's session report.

No client design can change this without server support. The plan therefore claims
no admin authority on stock servers, and the UI marks every stock integration as
unverified.

### 3.2 Stock policy: personal consent only

The coordinator's policy requires this, and it is also the recommendation.

- **Registered bots only.** Integration payloads count only from a session that the
  server reports as registered (it assigned a `user_id`) and that has a certificate
  (§2.1). Everything else is dropped and makes no offer. This gives a stable
  identity, not authority.
- **Nothing shows by default.** A stock integration's cards and commands appear only
  after **each user's own consent**, on that device.
- **One offer at a time.** An integration that is neither allowed nor declined may
  make one offer while it is in, or listened to in, the user's current voice channel
  and no other offer is showing. **Not now** declines: it never asks again by itself,
  and **Allow** in Integrations brings it back. The offer card and the Integrations
  place are designed in §7.
- **What identifies it.** Consent is remembered under `(server history identity,
  registration number, certificate hash)`. Both values come from the server's
  `UserState`: **server-asserted, not sender-claimed**, and only as trustworthy as
  that stock server. On an honest server they mean "the same registered bot with the
  same key as when you allowed it", and a change of either asks again. They identify
  an integration for consent memory and grant nothing. A hash or name inside a
  payload is never read. About shows a short id ("ID 3f1c 9a…") only to tell two
  integrations apart.
- **Unverified, explicitly.** Stock cards carry an "Unverified" tag in their frame.
  About says "Unverified. You allowed this." No path on a stock server shows a check
  mark or "verified".
- **Placement is not authority.** A card docks only while its sender is, by the
  server's report, in one of the user's voice places. Admins can use the existing
  Enter and Speak ACLs to decide where bots may be. That limits placement only; it
  does not stop anyone from claiming to be an integration.
- **Admin control needs server support.** Operators who want admins to decide run
  nuschld (§3.5).

### 3.3 Considered and rejected

| Candidate | Who can produce it | Why it is rejected |
|---|---|---|
| **A marker in the root channel's description** (an earlier draft: a `nuschl-integrations/1` token with an `allow` or `only` mode and certificate hashes, written by holders of Write on root) | Write on root, or a malicious server | It takes authority from text written for display, which breaks the structured-data rule ("legacy text is presentation only"). Clients cannot see who holds Write on root, a malicious server can show any description, and the stock editor gives no guarantee the token survives. Stock clients and servers cannot make it trustworthy |
| A marker in another channel's description, or in the welcome text | Write on that channel; the operator's config | Same objections |
| A certificate hash as approval, from a payload or from `UserState.hash` | The sender, or the server's report | Key continuity at best, never approval. Used only as the key of a user's own consent (§3.2) |
| Registration (`user_id`) | Anyone, by default | Register self is open to all. Kept as a hard requirement for a stable identity (§2.1), never as authority |
| Plugin data claiming approval or a role | Anyone | The relay checks no permission |
| An admin's Nuschl client vouching over plugin data | That client | Receivers cannot check that the sender holds any permission |
| Presence in an Enter-restricted channel, priority speaker, server mute | ACL editors, Mute/Deafen holders | Others cannot see the restriction, Move overrides Enter, and the others change audio for everyone |
| Signed payloads pinned at consent | The integration's own key | Would stop a malicious server forging a card from an integration you already allowed, but nothing else such a server can do. It would also add a signature dependency to the SDK, and users have no key to sign their presses. Not in v1 |

### 3.4 Integration authentication is not the plugin-data transport

- **PD (#93) stays a generic, opaque carrier.** It handles bounded bytes, the receiver
  list and pacing, and reports the sender as the session knew it on arrival. Its
  sender fields "authorise nothing" (`client/plugin.rs`, frozen `0e40c7ac`), and this
  plan keeps it that way. PD gains no integration semantics, consent state, registry
  or filtering.
- **The integrations layer decides who counts as an integration** and what may be
  shown. That layer is `nuschl-core::integration` and
  `crates/client/src/integrations.rs`. It uses the registry the Nuschl server enforces
  (§3.5), or the user's own consent on stock servers (§3.2). It reaches PD only through
  the reserved-id hook the PD lane provides, and treats every field PD reports as a
  claim.
- **nuschld's plugin-data relay stays generic.** It does not filter by integration
  data ids. Clients that negotiated `integrations/1` with a server ignore the stock
  carrier from it (sticky), so the registry is their only path.
- **Nuschl servers authenticate integrations themselves.** The publishing connection
  must log in with persistent credentials as an account in the registry entry. Clients
  never authenticate an integration from a hash.

### 3.5 Nuschl servers: a registry the server enforces

**Capability** `integrations/1` (a proposed name, not allocated), only in compatible
mode (§8.3). It is sticky like every capability. Once negotiated, the client ignores
the stock carrier (`nuschl/int*` plugin data) from that server, so for Nuschl clients
the registry is the only path. nuschld's plugin-data relay stays generic (§3.4).
A client that does not negotiate the extension sees the server as a stock server:
consent only, unverified. Today's web client is one (§8.4).

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
  string added_by = 8;           // name snapshot of the admin, ≤ 64 bytes
  uint64 added_ms = 9;
  uint64 catalog = 10;           // current catalog revision, 0 when none is published
  bool   opt_out = 11;           // supports "stop sending to me"
}
```

**Who manages it** (owner: "yes to both"). A new `Action::ManageIntegrations` is
granted by **Write on the root channel**, the existing administrator permission, as
an extension policy rather than a new ACL bit (a stock ACL editor would erase one,
`extension.md:212-221`). The operator has the same powers offline through
`nuschld admin integrations list|add|set|remove`. In
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
- **Actions and invocations: binding, not policy.** Anyone may press or invoke, and
  each integration decides its own permission model (owner). The server only:
  - binds an action to the integration, surface, revision and subject it sent that
    actor. The actor must currently be in that surface's audience, so a press always
    refers to something they can see;
  - binds an invocation to its conversation: it must be in scope and readable by the
    actor;
  - applies the rate limits (§5);
  - records the request in the audit log (below).

  With every request it forwards a server-attested actor to the connection that
  published the surface or catalog:

  ```proto
  message Actor {
    bytes  account = 1;        // 16 bytes; empty for an unregistered user
    uint32 user_id = 2;        // registration number; 0 when unregistered
    bool   registered = 3;
    uint32 session = 4;
    string name = 5;           // snapshot, ≤ 128 bytes
    bytes  certificate = 6;    // SHA-1 of the client certificate the server verified; empty without one
    uint32 permissions = 7;    // the actor's effective Mumble permissions in the surface's or conversation's channel
  }
  ```

  The integration applies its own rules on top. The music bot may limit queue control
  to registered users (§11), while a per-user stats integration lets anyone reset
  their own numbers.
- **Timeouts.** No reply within 5 s (actions) or 10 s (commands) gives the client
  `UNAVAILABLE`, and the late reply is dropped.
- **Errors** follow `extension.md` §2: an unknown integration and "not allowed" are the
  same `NOT_FOUND`, so the registry cannot be probed.

**Attestation and audit.** `IntegrationList` returns the entries in scope for the
requester over the authenticated extension channel. Clients show the registry name and
"Added by …". Changes are pushed (`IntegrationChanged`). nuschld keeps two bounded
audit tables, which managers read through the CLI:
- registry changes: time, admin account, change and integration id (≤ 1,000 rows);
- requests: time, integration, surface and control or command name, actor account or
  session, and outcome (≤ 10,000 rows).

The oldest rows are dropped first. Neither table, nor any log, holds command
arguments or result text.

### 3.6 Side by side

| Question | Stock Mumble | Nuschl server |
|---|---|---|
| Who adds an integration | Anyone with a registered bot; no admin control is possible | Holders of Write on root and the operator CLI, through the registry |
| What stops unwanted surfaces | Only each user's consent, with one offer at a time | The server: principals outside the registry cannot publish at all |
| Who receives surfaces | Whoever the integration lists; the client docks only senders in its voice places, by the server's report | The server's audience rule, re-evaluated on every change |
| Who may press | Anyone the integration accepts. It judges from the server's report of the presser (registration, certificate) and cannot learn their permissions | Anyone in the audience. The server binds, rate-limits and audits, and passes an attested identity with permissions; the integration decides |
| What the client shows | "Unverified", from a key the server asserts; the server can forge it | "Added by …", attested by the server, with sticky negotiation and no downgrade |
| Audit | None | Registry changes and requests |

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
| Per-user surfaces | Every v1 surface goes to the whole audience. The owner's per-user stats example needs one addressed to a single person | Nuschl: `SurfacePublish` for one actor. Stock: plugin data to one receiver (D9) |

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
  On stock servers only allowed integrations show, and their frame carries a fixed
  "Unverified" tag that the integration cannot remove or restyle.
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
- Offer card (stock): "Music · Lounge · Unverified", "Show its controls here?",
  with **Allow** and **Not now**; then "Find it later in Integrations."
- Card tag (stock): "Unverified".
- About: "Added by Alice · 3 Nov" (Nuschl) or "Unverified. You allowed this."
  (stock); "Sees who presses its buttons."; "Not end-to-end encrypted."
- Results: "Done.", "Not allowed.", "Out of date.", "Music didn't answer."
- Integrations: groups "Active", "Hidden" and "Declined"; buttons **Hide**,
  **Show** and **Allow**; "Offline"; "No integrations here."
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
- **Who may press is the integration's decision** (owner). On Nuschl servers it gets
  the attested `Actor` (§3.5): registration, verified certificate and the actor's
  permissions in that channel. On stock servers it sees only the server's report of
  the session (name, registration, certificate) and cannot learn anyone's
  permissions (§3.1). A refusal shows as "Not allowed."
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

## 7. Consent, dismiss and restore

The owner asked for this to be easy to undo: "if you reject once it should still be
possible to get the integration back (maybe the same place you get it back from if
you dismiss it)". One place, **Integrations**, holds every decision, and nothing nags.

### 7.1 States

| State | How it gets there | What shows | How to change it |
|---|---|---|---|
| Offered (stock only) | A registered bot in your voice channel that you have not decided on | One offer card in the dock | The card: **Allow** or **Not now** |
| Active | Nuschl: added by admins (the default). Stock: **Allow** | Card in the dock, commands in completion | Card menu → **Hide**, or Integrations |
| Hidden | **Hide** | No card. Commands stay, because typing `/` is explicit | Integrations → **Show** |
| Declined (stock only) | **Not now** | Nothing. It never offers again by itself | Integrations → **Allow** |

State is kept per `(server history identity, integration key)`, where the key is the
IntegrationId on Nuschl servers and `(registration number, certificate hash)` on
stock servers (§3.2).

### 7.2 The offer card

```text
┌──────────────────────────────┐
│ ♪ Music · Lounge · Unverified│
│ Show its controls here?      │
│        [Allow]  [Not now]    │
└──────────────────────────────┘
```

- **One at a time** across all servers. It sits where the card would go and has the
  height of a collapsed card, so nothing else in the dock jumps.
- **Quiet.** It never takes focus, plays a sound or notifies. Screen readers find it
  as the group "Offer from Music" in the dock. It is announced politely only if focus
  is already in the dock.
- **Not now** declines. For five seconds a line under it says "Find it later in
  Integrations." Then nothing asks again.
- **Ignored** offers (no click) go away when the bot leaves your voice places. They
  return at most once per connection to that server, never in a loop.

### 7.3 One place: Integrations

- **Per server, three groups:** **Active**, **Hidden** and **Declined**. Each row
  shows the name, its channel, where it comes from ("Added by Alice" or
  "Unverified") and one button: **Hide**, **Show** or **Allow**. A row menu has
  **About** and, on stock servers, **Forget**, which makes it undecided again so it
  may offer once more.
- **Absent integrations** with a remembered decision stay listed as "Offline", within
  the storage bound, so a decision can be undone while the bot is away.
- **Empty:** "No integrations here."
- **Reached from:**
  - desktop: the server menu → "Integrations", the quick switcher (Ctrl+K,
    "Integrations"), every card's menu, and the "Find it later" line;
  - TUI: `/integrations`, then `/show N`, `/hide N`, `/allow N`, `/forget N`;
  - web: the server menu.

### 7.4 No network effect

Hiding, declining and allowing change local state only. If the integration supports
it (`opt_out`), **Hide** also offers "Stop sending to me". On Nuschl servers that is
`IntegrationOptOut`, and the server stops sending to that connection. The client
repeats it after reconnects while the integration stays hidden. On stock servers it
is a best-effort request to the integration. **Show** undoes it.

### 7.5 Storage

`$XDG_STATE_HOME/nuschl/integrations.json` is versioned and bounded to 256 entries
and 64 KiB. The least recently seen are dropped first, and it is written atomically
like other state files. An unreadable file starts empty: consent is asked again and
hidden integrations reappear, which is never more permissive than before the file
existed. The web client uses an IndexedDB store with the same bounds, scoped like
local history.

## 8. Transports

### 8.1 Nuschl server, compatible mode (`integrations/1`)

The plan needs **one block of 16 message types**. This is a proposal, not an
allocation: the registry is frozen at polls 0x4E98–0x4E9F, RTC 0x4EA0–0x4EAF and
interactions 0x4EB0–0x4EBF. Root may assign the next free block (0x4EC0–0x4ECF
would be one) or another. Offsets are written `INT+n` until Root allocates (D6).

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
| INT+10 | `SurfaceAction { request_id, integration, surface, revision, subject, control, idempotency_key }` | client → server | audience (binding only; the integration decides) |
| INT+11 | `CommandInvoke { request_id, integration, Invoke }` | client → server | scope, conversation readable; text bucket |
| INT+12 | `IntegrationRequest { ticket, Actor, oneof { action, invoke } }` | server → integration | the attested `Actor` (§3.5) |
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
the SDK with sender session, presence instance and the certificate hash the server
reports) belongs to the PD lane, equivalently in `nuschl-session` and
`crates/client-wasm`. It is not on main yet (`0e40c7ac`). PD is transport only:
deciding who counts as an integration stays in the integrations layer (§3.4).

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
2. **Accept** only from a present session that the server reports as registered
   and with a certificate hash (§2.1), tied to its presence instance. When the presence ends, its surfaces and catalog go.
3. **Apply consent** (§3.2), keyed by the certificate hash the server asserts in
   `UserState`, never by anything in the payload: allowed → show, tagged
   "Unverified"; declined → drop; otherwise at most one offer.
4. **Bound** each sender to ≤ 4 surfaces, ≤ 16 commands and ≤ 4 accepted messages a
   second; the excess is dropped.
5. **Place** the surface: its channel is the sender's current channel as this client
   sees it. A surface from a sender outside the user's voice places goes to the
   Integrations list only, never to the dock.
6. **Results** count only from the same integration presence, for an outstanding
   key, within the timeout.

**Trust.** It is as described in §3.1: none beyond the user's own consent. Plugin
data has no channel authorisation. The client's checks are the only filter: sender
present, a server-reported certificate, consent, and placement by the sender's
channel as the server reports it. They limit noise and keep consent attached to the
same key, but do not make the sender verified. The server sees everything. Stock
clients' plugin frameworks receive these ids and, as far as is known, ignore them.

### 8.3 End-to-end encrypted servers: not in v1

The owner asked to "prioritize webrtc and e2ee" (2026-11-13). Integrations on
`e2ee/1` wait until that work has settled, and nothing here may delay it.

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
negotiation (`docs/web.md:298+`). Until then the web client treats nuschld as a stock
server. Registry integrations do not reach it, because they use typed messages.
Anything on the stock carrier is consent-only and unverified, as on Murmur.

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
| Any user adds an integration | Nuschl: only registry accounts can publish. Stock: **cannot be prevented**. Only registered bots count, nothing shows without the user's consent, there is at most one offer at a time, and stock integrations are always tagged "Unverified" |
| An impersonator with the same name | Keys are the IntegrationId or the server-asserted certificate hash, never names. Consent is per certificate, so another certificate is another integration and must be allowed separately. About shows the short id. Stock names and hashes come from the server's `UserState`, never the payload. Against a malicious stock server none of this holds, hence "Unverified" |
| A look-alike of Nuschl UI | A fixed frame always names the integration. The dock is its only place: no notices, dialogs, timeline rows, sounds, notifications, focus or window raising. No security or microphone icons, and no custom colours |
| Phishing through buttons | Transport labels are fixed. Card labels are ≤ 24 bytes in a named frame. No links or inputs in v1. An action carries only ids, never user data |
| A confused deputy | Actions and commands only send typed data to the integration. They cannot join channels, send messages as the user, open URLs, touch audio or change settings |
| Flooding | Per-surface coalescing, per-integration caps, client per-sender limits, dock ≤ 3 cards, repaint ≤ 1 Hz (§10) |
| A stale action hitting a new target | Ticket, generation and subject binding; no automatic retries (§5) |
| A downgrade on Nuschl servers | Sticky `integrations/1`; clients that negotiated it ignore the stock carrier from that server |
| A malicious stock server | Out of reach. It can forge any integration, including one the user allowed, because the certificate hash is its own assertion. Stock integrations are always shown as "Unverified" |
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
  - a stock integration without the user's consent shows nothing;
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
| Local state | ≤ 256 entries, ≤ 64 KiB | SDK, each platform store | Least recently seen dropped |
| Audit | Registry changes ≤ 1,000 rows; requests ≤ 10,000 rows | nuschld storage | Oldest dropped |

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
  - previous → seek to 0 if more than 3 s in; otherwise the current song goes back to
    the front of the queue and the last history entry plays again (owner: "sure",
    D5).

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
- **Who may press is the bot's own policy** (owner). The bot gets the presser's
  identity: the attested `Actor` on Nuschl servers, the server's session report on
  stock servers. Proposed default: registered users in its audience may use the
  transport controls and `/pause`, `/play`, `/skip`, `/previous`, and anyone may use
  `/web` as today. A per-channel `controls = "registered" | "everyone"` setting
  changes it (D10). The default is stricter than today's web link, which anyone who
  can write to the bot receives and which controls everything. That link is
  unchanged.
- **Registration on stock servers is required** (§2.1). The operator registers each
  bot connection, either as an admin with Register or, where Register self is
  allowed, through a new `register = true` option that asks the server to register
  the bot's certificate. Unregistered, the bot publishes nothing, and its web page
  says "Register this bot to show its controls."
- **Transport choice:** with `integrations/1` and a registry entry for its account, the
  bot uses typed messages. On stock servers, or Nuschl servers without the capability,
  it uses plugin data (after PD). With `integrations/1` but no entry it publishes
  nothing, and the web page says "Not added to this server's integrations."
- **Identity for admins:** the web page's connection status and the startup log show
  each connection's certificate SHA-1. Admins use it to find the right account when
  they add the bot to the nuschld registry. On stock servers it is only a way to
  recognise the instance, never authority.
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
| **F0** Pure foundation | `nuschl-core::integration`: ids, `Surface` and template view models, validators, sanitiser, interpolation, command spec and argument validation, consent state rules, provider helpers (revision counter, itemised catalog, per-actor rate, key memory); proto drafts | — | M: 3–4 |
| **F1** Protocol freeze | `integrations/1`, the INT block, both data ids, migration number, `docs/extension.md` section | F0; Root; PD; design review | S: 1 |
| **S1** SDK | `crates/client/src/integrations.rs`: per-server integrations, surfaces, provenance, local state sidecar, tickets, completion candidates, results; native plumbing | F1 | M: 4–5 |
| **N1** nuschld | Registry storage and CLI, `IntegrationSet`/`Remove` under `ManageIntegrations`, audit, publish enforcement, audience fan-out with epoch re-evaluation, action and command routing with attested actors, timeouts and budgets, dropping `nuschl/int` plugin data | F1 | L: 6–8 |
| **P1** Stock path | The integrations layer on PD's reserved-id hook (transport only, §3.4), receive rules, consent and offers, the "Unverified" tag, catalog sync, actions | F1; PD on main | M: 4–5 |
| **B1** Music bot | §11 on both transports, including the `controls` policy and the `register` option | F0; N1 and/or P1 | M: 4–5 |
| **U1** Desktop | Dock and card, offer card, menu and About, the Integrations place (Active, Hidden, Declined), completion popup and chips (shared with MI), result line, AccessKit | S1 | M: 5–6 |
| **U2** TUI | Bar, `/integrations`, Tab completion, plain lines | S1 | S–M: 2–3 |
| **U3** Web | Dock and combobox through BROKER fences; stock path first | S1; PD wasm; BROKER | M: 3–4 |
| **C1** `card/1` | Rendering in three clients | U1–U3; a second provider approved | S–M: 2–3 |
| **A1** Artwork | Nuschl only, through the content worker | N1, U1; D3 | M: 3–4 |

v1 (F0–U3 and B1) comes to about 35–45 agent-days. N1 and P1 can run in parallel
after F1, and U1–U3 after S1. **Owner priority:** WebRTC and E2EE come first. These
slices are scheduled so they take no shared files, review time or build windows those
lanes need. The E2EE path (§8.3) is deferred.

### 12.3 Tests per layer

Private resources only: loopback nuschld, private Murmur, raw protocol clients,
synthetic tracks, disposable profiles. No public servers, host microphones or
profiles.

| Layer | Cases |
|---|---|
| Unit: core | Every bound at the edge and one past; sanitiser (controls, bidi overrides, invalid UTF-8, empty after cleaning); interpolation (play, pause, unknown duration, 24 h, `age_ms` limits, sub-second corrections never backwards, subject change resets); spans; argument validation; itemised catalog (missing item, mixed revisions, timeout) |
| Unit: SDK | Tickets refused after reconnect, generation change, subject change and disabled control; late results dropped; one in flight; consent matrix (allowed, declined, unasked, unregistered bot ignored, no certificate, a new certificate or registration under the same name, sender left, sender outside voice places, a hash or name inside the payload ignored); "Unverified" on every stock view; offer rules (one across servers, ignored offers at most once per connection, Not now → Declined with no re-prompt, Allow from Integrations, Forget makes it undecided); local state bounds and a corrupt file; sticky `integrations/1` ignores plugin-data surfaces; `e2ee/1` shows nothing |
| Unit: server | Publishing by a non-entry account, outside scope, a disallowed template, too large, too fast; audience on move, listen, ACL change and opt-out; actions by a guest forwarded with an unregistered `Actor` (not refused), `Actor` fields (certificate, permissions in the right channel), someone outside the audience refused, a duplicate key, a timeout, a late reply; commands outside scope, an invalid argument, the text bucket; `ManageIntegrations` = Write on root, and the CLI; both audit caps; `NOT_FOUND` uniformity; the plugin-data relay unchanged |
| Storage | Registry migration in SQLite and PostgreSQL; audit trimming |
| Fuzz | `integration_messages` (the INT block into nuschld state), `integration_plugin` (both stock payloads, catalog assembly), additions to `client_messages` |
| Integration: nuschld (`tests/integrations.py`, new) | Bot plus two Nuschl clients plus a raw extension client: surface appears for the audience only; a move withdraws it; Next by a member skips; a raw unregistered client and a non-entry account cannot publish; `IntegrationSet` by a non-admin refused; admin add and remove visible live |
| Integration: stock Murmur (`tests/stock_integrations.py`, new) | Bot plus two Nuschl clients (TUI test build) plus a raw stock client on a private Murmur: the stock client still uses `!web`; an unregistered bot is ignored and the same bot after registration makes one offer; nothing shows until allowed; a raw client claiming integration status also gets only an offer (the documented limit: shown, not prevented); an impostor with the same unregistered name and another certificate is a separate integration; a cross-channel injection by a raw client is not docked; burst above Murmur's bucket degrades without wrong state |
| Music bot (`tests/musicbot.py` extension) | Surface tracks play, pause, seek and skip; previous semantics; stale subject refused; `/web` still sends the private link |
| Accessibility (`tests/accessibility.py`) | Offer card group and its two buttons; Integrations groups and row buttons; card group name, button names and disabled states, progress value text, Hide and restore through the Integrations list, completion listbox announcements, TUI plain transcript |
| GUI | Desktop scenes at 100% and 200%, a narrow sidebar, long Unicode titles, three cards and "2 more" (`tests/screenshots.py` pattern) |
| Browser | Dock and combobox with the synthetic SDK at 320, 390 and 1280 px, 44 px targets, focus visibility (`tests/web_controls.mjs` pattern) |

Results are reported as build, unit, private runtime, VM or physical. None implies
another.

### 12.4 Shared files and conflicts

| Path | Hunk | Risk |
|---|---|---|
| `crates/protocol/proto/nuschl.proto`, `src/nuschl/mod.rs` | INT block, capability | Medium: MI and polls are adding to the same files; freeze them together |
| `crates/session/src/connection.rs`, `crates/client-wasm/src/session.rs` | Reserved-id delivery and pacer priority | **High:** PD and MI annotations share the outbox. Land after PD |
| `apps/server/src/{authz,extension,config}.rs`, new `integrations.rs` | `ManageIntegrations`, capability, settings, routing. The plugin-data relay is unchanged | Medium |
| `crates/client/src/client.rs` | Hooks into the integrations module | High: a hub file shared with MI and LP |
| `apps/desktop/src/view.rs` | Dock between the channel list and the voice card; composer popup | Medium: MI mention popup. Build one completion component |
| `apps/tui/src/app.rs` | `/integrations`, Tab completion | Low–medium |
| `web/app/src/App.svelte`, `action-context.js` | Dock, combobox | Medium: BROKER |
| `apps/musicbot/src/{mumble,player,web}.rs` | Provider | Low |
| `Cargo.toml`, `Cargo.lock`, `nix/crate2nix/Cargo.nix` | **None expected** | — |

## 13. Decisions

### 13.1 Settled (owner answers of 2026-11-13, relayed by the coordinator)

- **D1 Stock authority:** consent only, from registered bots only, always shown as
  "Unverified" (coordinator policy; §3.2). Text markers and certificate hashes are
  not authority (§3.3).
- **D2 Nuschl registry:** managed by holders of Write on root **and** by the
  operator CLI ("yes to both"; §3.5).
- **D3 v1 kit:** `now_playing/1` only, with `card/1` built once a second provider is
  approved. This reads answer f ("sure") as acceptance; confirm if it meant something
  else. Appendix A lists candidate providers.
- **D4 Who may press and invoke:** anyone. Each integration decides its own
  permission model from the identity the server passes, and servers still bind,
  rate-limit and audit (§3.5, §5).
- **D5 Previous in the music bot:** restart if more than 3 s in, otherwise the
  previous track ("sure"; §11).
- **D7 E2EE:** deferred. The owner prioritises WebRTC and E2EE, and this work does
  not compete with those lanes (§8.3, §12.2).

### 13.2 Open

- **D6 (integrator) Allocations, all proposals and nothing allocated here:** a
  16-type block after the frozen ranges (Root), the capability name
  `integrations/1`, data ids `nuschl/int/1` and `nuschl/int-req/1` (PD), and the
  migration number (Root).
- **D8 Argument suggestions:** static choices only in v1 (recommended). Dynamic
  suggestions from the integration later, with their own rate budget.
- **D9 (owner, later) Per-user surfaces:** a card addressed to one person, which the
  per-user stats idea needs (Appendix A). Not in v1. It is a separate feature
  needing approval.
- **D10 (bot policy) Music bot controls:** registered users in its audience by
  default, with a per-channel `controls = "registered" | "everyone"` setting
  (recommended; §11). The owner left this to the integration.

## Appendix A. Candidate integrations (ideas only)

These are ideas the owner asked for. Each would need its own approval, and none is
planned.

| Idea | What people see | Template | Needs beyond v1 |
|---|---|---|---|
| Per-user stats | Your own numbers (time in voice, songs queued) and **Reset** | `card/1` | Per-user surfaces (D9). Anyone may reset their own stats, so the integration allows unregistered users here |
| Raise hand / speaking queue | Who is next, **Raise hand**, **Lower hand**, and **Next** for moderators | `card/1`; `list/1` for the queue | `list/1`. Moderators judged from the attested permissions (Nuschl only) |
| Shared timer | A name, time left, **Start**, **Pause**, **Reset** | `card/1` with anchored progress | — |
| Game server status | Map, players 12/16, online or offline | `card/1` | A join link (links) |
| CI or build status | Branch, passing or failing, **Retry** (with confirmation) | `card/1` | Links |
| Stream live | "Live: title", viewers | `card/1` | A watch link (links) |
| Dice and initiative | `/roll 2d6`; turn order with **Next** | Commands; `card/1`, `list/1` for the order | Posting results for everyone (the bot sends a message) |
| Next event | "Raid night · in 2 h" | `card/1` with a countdown | — |
| Radio or another player | Station, song, **Play**/**Pause** | `now_playing/1` | — |
| Maintenance notice | "Restart at 22:00" | `card/1` with the warning icon | — |

Polls have their own native design (#26) and are not an integration. Speaking-time
statistics would raise privacy questions of their own.
