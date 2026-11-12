# The desktop client

What the Nuschl desktop app does, in more detail than the README. How it is built is
in [architecture.md](architecture.md); how to install it is in [install.md](install.md).

## Servers and connecting

Nuschl connects to stock Mumble servers (tested against 1.5, 1.4 and 1.3) and to
Nuschl's own server, `nuschld` ([server.md](server.md)), over TLS with a persistent
client certificate. The certificate is your identity on Mumble servers. It is
RSA-2048, so that stock Mumble can import it; **Settings → Identity** imports and
exports it as PKCS#12.

- **+** in the server rail adds a server; a `mumble://` link opens one (and joins
  the channel in its path). **Connect** uses the saved details; the cog beside the
  server name edits them.
- Saved servers connect when Nuschl starts (a switch per server), self-muted and
  deafened until you unmute. **Settings → General** can start Nuschl at login,
  minimised.
- A self-signed server certificate needs your confirmation before any credentials
  are sent; accepted fingerprints are saved. An invite link can carry the
  fingerprint and the server's policy (`mumble://…?fp=sha256:…&policy=nuschl`), which
  pins them before the first connection.
- Server passwords stay in memory for the connection attempt only.
- Lost connections reconnect with backoff, rejoin your channel with its mute state,
  and mark the gap in the timeline.
- Unread messages outside the visible part of the server rail or channel list show
  an **↑ unread** or **↓ unread** button, red when they include mentions. Click it
  to open the nearest unread conversation in that direction. It browses text and
  leaves voice where it is. A closed branch includes its descendants' unread count;
  jumping to a hidden conversation opens the branch.

## Voice

Voice uses Mumble's encrypted UDP transport, falls back to the TLS tunnel when UDP is
blocked, and can be forced onto TCP (**Settings → Network → Force TCP voice**).
Transmission is by voice activity (RNNoise), push to talk, or continuous. Push to
talk and the mute shortcuts work in the window and, where the desktop grants it,
system-wide: through the GlobalShortcuts portal on Linux (the desktop may ask once),
the platform hotkey APIs on Windows and macOS.

**On several servers at once** you hear every server you are not deafened on, mixed
together (each with its own volume in its menu), and your voice goes to one: the
server whose channel you last joined, or the one you chose with **Use voice here**. It
has a green microphone ring in the server rail; the servers you only listen to have a
headphone badge that lights up while someone talks there, and see you muted. The card
above your name says where you are ("Talking on Beta #Lobby · listening to Alpha
#Lobby"); click a place to go there. Unmuting on a second server asks first, **Speak
on two servers?**; push to talk and the mute shortcuts act where your voice goes.
([clients.md](clients.md#voice-on-several-servers) has the design.)

The microphone starts muted. Capture runs echo cancellation, noise suppression and
automatic gain control, all in Rust:

- **Echo cancellation** works from a recording of what the speakers play (every
  application, mono or stereo), time-aligned with the microphone, so music and
  other programs are removed from your microphone too. The recording is made only
  while the microphone is open and never leaves the device (macOS asks for system
  audio permission).
- **On Linux** Nuschl follows PipeWire's graph upstream from the microphone. A
  system echo canceller (PipeWire or PulseAudio echo-cancel) anywhere in the chain
  replaces Nuschl's and gets Nuschl's playback as its reference; a noise suppressor
  there (an RNNoise filter chain, say) replaces Nuschl's suppressor. A
  per-microphone override remains.
- **Settings → Microphone test** meters the microphone, plays back what others would
  hear, and runs an echo test, without a server and without unmuting; nothing is
  sent.
- An adaptive controller watches for playback stutters, residual echo and a clipping
  microphone. It adjusts only Nuschl's own buffer and voice gate, and tells you what
  it cannot fix.
- If the audio service fails, only the media worker restarts; the connection and
  mute state stay. Volume mixers name each stream (microphone, voice chat, echo
  reference).

The design and its measurements: [architecture.md](architecture.md#audio-configuration-and-recovery),
[dsp-evaluation.md](dsp-evaluation.md).

## Channels and conversations

- Click a channel to open its conversation; double-click it, or use **Join
  channel**, to move your voice there. Ctrl+K finds a channel or person.
- A conversation is one timeline that follows new messages at the bottom. Scrolling
  up loads older history without moving what is on screen, down to **Beginning of
  local history**; while you read further up, a pill counts new messages and jumps
  back to the latest.
- Right-click users, channels and servers for whisper and shout, listening to a
  channel, links, per-user volume and local mute, direct messages, comments and
  avatars, registration, moderation, the channel editor, ACLs, bans and registered
  users. Entries appear only where the server's permissions allow them.
- Drag the channel list's edge to fit long names. Collapsed branches show how many
  people are inside; branches with people open on connect.
- Search (Ctrl+F) finds messages saved on this device. History and the channel
  trees survive restarts, so you can browse offline.

### Messages

- Enter sends and Shift+Enter starts a new line (switchable in Settings).
- Newlines and blank lines stay visible. Ordinary text is literal; HTML from stock
  clients is made inert.
- Fenced code is highlighted (in the content worker, with Syntect's Rust regex
  backend). LaTeX between `$…$` and `$$…$$` is typeset ([math.md](math.md)).
- Inline images are decoded in a sandboxed worker. Ctrl+V pastes an image: it is
  re-encoded in the worker to fit the server's limit and sent the way stock Mumble
  sends it. Other files go through the paperclip or a drop: Nuschl offers them
  peer to peer, with a magic-wormhole code in the message that stock Mumble users
  can use with any wormhole client, and shows offers as file cards.

- Links in welcome cards, channel descriptions and user comments support clicking,
  Tab/Shift+Tab and Enter, and screen-reader activation. They use the same URL
  policy as message links; rendering a document does not fetch its links.
- Long messages collapse by measured height behind **Show more**.
- Ctrl+. or Ctrl+; opens an emoji picker.

Settings → Privacy → **Relay-only connections** (`relay_only`, off by default)
uses the configured rendezvous and transit relay without gathering or sharing
peer addresses. Enabling it stops direct-capable transfers; offer or download
again to start through the relay. It never falls back to direct connections or
a relay suggested by another user. Changing it reconnects server sessions so
supporting servers receive the privacy choice before publishing your presence.
Configured servers/relays still see your connection address; legacy Mumble
servers cannot enforce address privacy for other users.

### History

Local history is on by default, on stock Mumble servers too. There it contains the
messages your device saw, not those sent while you were offline.

On a Nuschl server, history comes from the server: all of it that you may read,
including messages from before you joined, fetched in bounded pages as you scroll.
Pages you fetched are kept on your device by their sequence number for offline
reading and search: edits overwrite them, deleted messages stay as tombstones
without text.

## On a Nuschl server

With Nuschl clients, `nuschld` negotiates its extension ([extension.md](extension.md)):

- durable server history that new members can read (kept forever unless the server
  or a channel sets a shorter retention, or "none" for live-only channels), sends
  that survive reconnects, and read markers;
- edit and delete your messages (right-click a message, or Up in an empty composer
  to edit your last one); moderators can delete others' where the server allows it;
- see who is typing under the composer (**Settings → Privacy** stops showing that you
  type);
- direct messages between registered users are stored by the server, so they reach
  someone who is offline and come back with their history.

Nuschl never falls back to plain Mumble with a server that once offered the
extension. Like Signal, a server can require a current client: older Nuschl
versions are told to update and, after an announced date, refused
(`minimum_client`, [server.md](server.md)). Stock Mumble clients are not affected.

## Your Nuschl identity

Besides the Mumble certificate, Nuschl users have an identity of their own,
independent of any server ([e2ee.md](e2ee.md)): a hybrid ML-DSA-65 + Ed25519 root
key certifying each device, kept in the sandboxed crypto worker. Servers with key
transparency (`kt/1`, on by default in `nuschld`) publish it in a log, as in
Signal's automatic key verification. Nuschl pins contacts on first use, checks them
against the log, shows "Verified (same identity as on …)" across servers, and flags
a changed key or a server that lies about it. Safety numbers allow verification in
person. End-to-end encryption with MLS builds on this; its direct messages are an
experiment, off by default (`e2ee_direct_messages` in `settings.json`), and the app
does not show them yet.

## Locks

A red open lock means **no end-to-end encryption**: the server can read the messages
(Mumble's transport encryption still protects them on the way). Green locks are
reserved for authenticated MLS sessions ([e2ee.md](e2ee.md) §7.4,
[architecture.md](architecture.md#mls-and-encrypted-history)).

## Keyboard and screen readers

Controls and text fields are exposed to screen readers; AT-SPI on Linux is tested
(`tests/accessibility.py`). Screen readers reach the whole loaded conversation: the
timeline is one Tab stop, walked with ↑/↓, Page Up/Down and Home/End (Home loads
older history); Enter or Space expands a long message or a collapsed run of joins
and leaves. Tab steps through a message's links (the line under the composer says
where each leads) and Enter opens one, or a message's only link. Typing while the
timeline has focus goes to the composer.

## Where Nuschl keeps its files

On Linux Nuschl follows the XDG Base Directory specification (unset, empty or
relative variables fall back to the defaults):

| Folder | Holds |
| --- | --- |
| `$XDG_CONFIG_HOME/nuschl` (`~/.config/nuschl`) | `settings.json`, `servers.json` (bookmarks and accepted certificates), `managed.json`: safe to keep with your dotfiles |
| `$XDG_DATA_HOME/nuschl` (`~/.local/share/nuschl`) | `identity/` (your Mumble certificate and its private key), `keys/` (end-to-end keys), `history/` (local history), `nuschl-policy.json` (servers' security records): back this up, keep it private |
| `$XDG_STATE_HOME/nuschl` (`~/.local/state/nuschl`) | last channel per server, the update check |
| `$XDG_CACHE_HOME/nuschl` (`~/.cache/nuschl`) | channel trees and server icons: safe to delete |

`nuschl/managed.json` in `$XDG_CONFIG_DIRS` (`/etc/xdg`) is read as well
([install.md](install.md#home-manager)). macOS uses
`~/Library/Application Support/org.Nuschl.Nuschl` (state in its `state` folder) and
`~/Library/Caches/org.Nuschl.Nuschl`; Windows `%APPDATA%\Nuschl\Nuschl\config`,
`%LOCALAPPDATA%\Nuschl\Nuschl\data` (and `data\state`) and `…\cache`.
The first start after the Nuschel → Nuschl rename moves the old `nuschel`
folders to `nuschl` (including the corresponding macOS and Windows folders).
Quit older clients first. Mumble certificates and private keys, accepted server
certificates, bookmarks, settings, history and saved security requirements are
preserved. Published client SQL migration files retain their original bytes
(including old-name comments), because SQLx verifies their checksums. Different files at both destinations stop startup with both copies
retained; resolve the conflict before retrying. Old managed configuration under
`nuschel/managed.json` remains readable, with the new name taking precedence.

Experimental end-to-end stores use different cryptographic domain labels after
this rename. Their entire old store is retained in `data/keys.nuschel-v1` or
`data/keys.nuschel-config-v1`, outside the workers' writable directories. A reset
notice appears on migration, and a new end-to-end identity is created when needed.
Old encrypted messages, recovery blobs, MLS state and transparency proofs belong
to the old namespace. No deployed Nuschl server compatibility is promised across
this development transition. `config/nuschl-namespace-v1` records that the move
finished, including for explicit profiles, so new keys are never archived again.
On Windows, old worker grants are revoked before the old AppContainer profiles
are removed. The desktop also transfers its existing login-start preference.
NixOS and Home Manager accept the old `services.nuschel`,
`services.nuschel-witness`, `services.nuschel-web` and `programs.nuschel`
options through warning aliases; switch configurations to the new names.

`--profile-dir DIR` puts the four under `DIR/config`, `data`, `state` and `cache`.
Folders are created owner-only (0700) when first needed. Which worker can reach what:
[SECURITY.md](../SECURITY.md#where-secrets-live).

Earlier versions kept the identity, keys, security records and caches in the
configuration folder. The first start moves each file to its new place (renamed, or
copied, synced and only then removed), never over a newer one, and logs what moved;
an interrupted move finishes on the next start, and anything that cannot be moved is
used where it is and reported. Quit older Nuschl instances first: one still running
would create a fresh identity in the old place, which Nuschl then reports and leaves
alone. A downgrade does not find the moved identity; copy
`~/.local/share/nuschl/identity/identity.json` back to `~/.config/nuschl` for it.

## Troubleshooting

**A broken configuration file.** Nuschl never overwrites a configuration file it
cannot read: a broken `settings.json` or `servers.json` is renamed to
`settings.json.invalid` (or `.invalid.1`, …), and the app starts without it and says
so. An invalid `managed.json` is reported and ignored until it is fixed.
