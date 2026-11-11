# The desktop client

What the Nuschel desktop app does, in more detail than the README. How it is built is
in [architecture.md](architecture.md); how to install it is in [install.md](install.md).

## Servers and connecting

Nuschel connects to stock Mumble servers (tested against 1.5, 1.4 and 1.3) and to
Nuschel's own server, `nuscheld` ([server.md](server.md)), over TLS with a persistent
client certificate. The certificate is your identity on Mumble servers. It is
RSA-2048, so that stock Mumble can import it; **Settings → Identity** imports and
exports it as PKCS#12.

- **+** in the server rail adds a server; a `mumble://` link opens one (and joins
  the channel in its path). **Connect** uses the saved details; the cog beside the
  server name edits them.
- Saved servers connect when Nuschel starts (a switch per server), self-muted and
  deafened until you unmute. **Settings → General** can start Nuschel at login,
  minimised.
- A self-signed server certificate needs your confirmation before any credentials
  are sent; accepted fingerprints are saved. An invite link can carry the
  fingerprint and the server's policy (`mumble://…?fp=sha256:…&policy=nuschel`), which
  pins them before the first connection.
- Server passwords stay in memory for the connection attempt only.
- Lost connections reconnect with backoff, rejoin your channel with its mute state,
  and mark the gap in the timeline.

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
- **On Linux** Nuschel follows PipeWire's graph upstream from the microphone. A
  system echo canceller (PipeWire or PulseAudio echo-cancel) anywhere in the chain
  replaces Nuschel's and gets Nuschel's playback as its reference; a noise suppressor
  there (an RNNoise filter chain, say) replaces Nuschel's suppressor. A
  per-microphone override remains.
- **Settings → Microphone test** meters the microphone, plays back what others would
  hear, and runs an echo test, without a server and without unmuting; nothing is
  sent.
- An adaptive controller watches for playback stutters, residual echo and a clipping
  microphone. It adjusts only Nuschel's own buffer and voice gate, and tells you what
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
  sends it. Other files go through the paperclip or a drop: Nuschel offers them
  peer to peer, with a magic-wormhole code in the message that stock Mumble users
  can use with any wormhole client, and shows offers as file cards.
- Long messages collapse by measured height behind **Show more**.
- Ctrl+. or Ctrl+; opens an emoji picker.

### History

Local history is on by default, on stock Mumble servers too. There it contains the
messages your device saw, not those sent while you were offline.

On a Nuschel server, history comes from the server: all of it that you may read,
including messages from before you joined, fetched in bounded pages as you scroll.
Pages you fetched are kept on your device by their sequence number for offline
reading and search: edits overwrite them, deleted messages stay as tombstones
without text.

## On a Nuschel server

With Nuschel clients, `nuscheld` negotiates its extension ([extension.md](extension.md)):

- durable server history that new members can read (kept forever unless the server
  or a channel sets a shorter retention, or "none" for live-only channels), sends
  that survive reconnects, and read markers;
- edit and delete your messages (right-click a message, or Up in an empty composer
  to edit your last one); moderators can delete others' where the server allows it;
- see who is typing under the composer (**Settings → Privacy** stops showing that you
  type);
- direct messages between registered users are stored by the server, so they reach
  someone who is offline and come back with their history.

Nuschel never falls back to plain Mumble with a server that once offered the
extension. Like Signal, a server can require a current client: older Nuschel
versions are told to update and, after an announced date, refused
(`minimum_client`, [server.md](server.md)). Stock Mumble clients are not affected.

## Your Nuschel identity

Besides the Mumble certificate, Nuschel users have an identity of their own,
independent of any server ([e2ee.md](e2ee.md)): a hybrid ML-DSA-65 + Ed25519 root
key certifying each device, kept in the sandboxed crypto worker. Servers with key
transparency (`kt/1`, on by default in `nuscheld`) publish it in a log, as in
Signal's automatic key verification. Nuschel pins contacts on first use, checks them
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

## Where Nuschel keeps its files

On Linux Nuschel follows the XDG Base Directory specification (unset, empty or
relative variables fall back to the defaults):

| Folder | Holds |
| --- | --- |
| `$XDG_CONFIG_HOME/nuschel` (`~/.config/nuschel`) | `settings.json`, `servers.json` (bookmarks and accepted certificates), `managed.json`: safe to keep with your dotfiles |
| `$XDG_DATA_HOME/nuschel` (`~/.local/share/nuschel`) | `identity/` (your Mumble certificate and its private key), `keys/` (end-to-end keys), `history/` (local history), `nuschel-policy.json` (servers' security records): back this up, keep it private |
| `$XDG_STATE_HOME/nuschel` (`~/.local/state/nuschel`) | last channel per server, the update check |
| `$XDG_CACHE_HOME/nuschel` (`~/.cache/nuschel`) | channel trees and server icons: safe to delete |

`nuschel/managed.json` in `$XDG_CONFIG_DIRS` (`/etc/xdg`) is read as well
([install.md](install.md#home-manager)). macOS uses
`~/Library/Application Support/org.Nuschel.Nuschel` (state in its `state` folder) and
`~/Library/Caches/org.Nuschel.Nuschel`; Windows `%APPDATA%\Nuschel\Nuschel\config`,
`%LOCALAPPDATA%\Nuschel\Nuschel\data` (and `data\state`) and `…\cache`.
`--profile-dir DIR` puts the four under `DIR/config`, `data`, `state` and `cache`.
Folders are created owner-only (0700) when first needed. Which worker can reach what:
[SECURITY.md](../SECURITY.md#where-secrets-live).

Earlier versions kept the identity, keys, security records and caches in the
configuration folder. The first start moves each file to its new place (renamed, or
copied, synced and only then removed), never over a newer one, and logs what moved;
an interrupted move finishes on the next start, and anything that cannot be moved is
used where it is and reported. Quit older Nuschel instances first: one still running
would create a fresh identity in the old place, which Nuschel then reports and leaves
alone. A downgrade does not find the moved identity; copy
`~/.local/share/nuschel/identity/identity.json` back to `~/.config/nuschel` for it.

## Troubleshooting

**A broken configuration file.** Nuschel never overwrites a configuration file it
cannot read: a broken `settings.json` or `servers.json` is renamed to
`settings.json.invalid` (or `.invalid.1`, …), and the app starts without it and says
so. An invalid `managed.json` is reported and ignored until it is fixed.
