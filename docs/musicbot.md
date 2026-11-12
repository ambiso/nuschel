# Music bot

`nuschl-musicbot` plays music from Subsonic servers in Mumble channels. People
control what plays on a web page; they get their link to it by writing `!web` to the
bot in Mumble. It works with any Mumble server, stock Murmur as well as `nuschld`.

## What it does

- **Libraries.** Any number of Subsonic-compatible servers: Navidrome, Gonic,
  Ampache, Airsonic and others. With OpenSubsonic servers the bot logs in with an
  API key (`apiKeyAuthentication`) and, if the server has the `transcoding`
  extension, tells it which formats it decodes, so the server transcodes only what
  the bot cannot play. Plain Subsonic servers get a salted password token and send
  the original file, or always a transcoded one when `transcode` is set.
- **Browsing.** The web page browses each library much like Navidrome's own: album
  lists (recently added, recently played, most played, random, by name, starred)
  with covers, the artist index, an artist's albums, an album's songs, and
  playlists. A search asks all libraries at once for artists, albums and songs.
  Every song, album and playlist has *Add* and *Play next*. Choose the destination
  queue in the player bar; the browser remembers the selection for that link.
- **Covers.** The bot fetches at most 8 MiB of encoded art, then a credential-free,
  confined content worker decodes PNG, JPEG, WebP or the first GIF frame. Source
  dimensions are capped at 4096 per edge with a 64 MiB decoder allocation budget;
  the returned thumbnail is at most 400×400 RGBA pixels (300×300 by default).
  The browser validates these pixels and paints Canvas; encoded image bytes never
  reach its image decoder. Unsupported formats and failed helpers show a placeholder.
- **Channels.** One Mumble connection per configured channel, each with its own
  client certificate. Every channel starts with its own queue. On the web page a
  channel can play another channel's queue instead: then both hear the same song,
  encoded once and sent to both. Channels may be on different servers.
  People may also subscribe to listen to the bot's channel while staying in
  another channel. These subscriptions count as listeners and keep the queue
  playing; others in their current channel do not receive the music.
- **Queues.** Drag songs, albums or playlists from the library into the queue,
  then drag entries to reorder them. The move handles also accept Alt+Up/Down.
  Remove entries, clear upcoming tracks, skip, pause and set the volume. The player
  bar has a live progress display: click or drag to seek, or use Left/Right for
  five-second steps and Home/End. Space toggles playback outside controls.
  A queue waits when no channel with listeners is receiving it. Deafened people
  do not count as listeners. Changes appear for every viewer through a live stream.
- **History.** The History view keeps the latest 1,000 finished plays or failed
  attempts per queue, newest first. It shows track details, who queued the track,
  local start/end times, transmitted duration, and whether it completed, was
  skipped, or failed. History survives restarts and loads in pages of 50.
- **Ducking.** While somebody in a channel speaks, the queue playing there gets
  quieter (by default to 30 % of its volume) and comes back 0.8 s after the last
  speech. It fades down in about 60 ms and up in about half a second, so nothing
  clicks. It reacts to speech, not to a microphone that is merely open: the bot
  decodes what each person sends and judges every 10 ms. A frame counts as speech
  when RNNoise's speech probability passes the threshold and the frame is at least
  9 dB above that person's background level, the quietest frame of the last 1.5 s.
  Speech has gaps that quiet between syllables; steady hiss, hum, fans and silence
  do not. 30 ms of speech in a row are needed, so a click does not duck. Each queue
  has its own settings on the web page: on or off, how quiet, for how long, and how
  sensitive the speech detection is (the same scale as Nuschl's voice activation).
  The page notes when a queue is ducked. The bot decodes voice only while a queue
  with ducking plays to that channel.
- **Formats.** MP3, FLAC, Ogg Vorbis, Opus, WAV, AAC and ALAC (MP4 is downloaded
  before it plays, up to 512 MiB). Everything is resampled to 48 kHz stereo and
  sent in 20 ms Opus packets (960 samples per channel), at 96 kbit/s by default.
  Increasing bitrate changes packet size rather than audio duration: 320 kbit/s
  averages 800 payload bytes per packet. The encoder caps each payload at 992 bytes
  to leave room for Mumble headers within its 1 KiB packet limit. This prevents
  large VBR peaks from being discarded. The bot also reserves transport overhead
  when the server allows less bandwidth; a 320 kbit/s server budget permits up to
  294.4 kbit/s of Opus with the current overhead allowance.

## Links

Every bot connection publishes a Mumble comment with the configured public web
address and instructions for requesting a personal link. The public comment
contains no authenticated URL and is republished after reconnecting.

Write `!web` to the bot, in its channel or privately. It answers privately with a
link like `https://music.example.org/u/5f0c…/`. The link is the only key to the page,
so do not share it. A person with a Mumble certificate keeps the same link; `!web new`
replaces it, and the old one stops working. Without a certificate anybody could use
the same name later, so every `!web` gives a new link. Links are kept in
`tokens.json` in the state directory.

At most 4,096 links are retained. When the limit is reached, issuance fails
without replacing another person's link. Token files are capped at 4 MiB.
Link changes use synced atomic replacement; a failed write rolls back the in-memory
change, and the next issuance confirms that rollback on disk before returning a link.

Anyone who can write to the bot on its servers can get a link, and every link
controls all queues of the bot.

Library detail links and drag actions belong to the current browser connection.
A reconnect or library change clears old browse data and returns detail pages to
a safe list. Expired detail URLs cannot send old IDs to a replacement backend.

## Running it

On NixOS:

```nix
imports = [ inputs.nuschl.nixosModules.musicbot ];
services.nuschl-musicbot = {
  enable = true;
  virtualHost = "music.example.org";   # nginx in front, links use https://
  enableACME = true;
  credentials.navidrome = "/run/secrets/navidrome-api-key";
  settings = {
    subsonic = [
      { name = "Navidrome"; url = "https://navidrome.example.org"; api_key_file = "navidrome"; }
    ];
    channel = [
      { name = "lounge"; host = "voice.example.org"; username = "Music"; channel = "Lounge"; }
    ];
  };
};
```

`credentials` are passed with systemd's `LoadCredential`; a relative `*_file` in the
settings names one of them. The service runs as a dynamic user with its state in
`/var/lib/nuschl-musicbot` (certificates in `identities/<channel>/`, links in
`tokens.json`, playback state in `queues.json`). The package includes the Svelte
frontend. nginx keeps no access log for the page, since the links are secrets.

The NixOS module validates the generated TOML during the build with the selected
package's `--check-config` command. Unknown keys, missing or mistyped fields,
invalid authentication combinations, ranges and endpoint syntax fail the build.
The service depends on the checked file, so it cannot deploy an unchecked config.
The checker uses the same Rust schema and validation rules as normal startup;
it reads no credentials, creates no state and makes no network connections.
To check a configuration manually:

```sh
direnv exec . cargo run -p nuschl-musicbot -- --config musicbot.toml --check-config
```

Credential availability and correctness, server reachability, certificates, channel
existence and permissions still depend on the running deployment.


For a source build, run `direnv exec . just musicbot-web`. The bot plays only
with its codec worker in place beside it, so lay out a prefix as the package does:

```sh
direnv exec . cargo build -p nuschl-musicbot -p nuschl-musicbot-codec-worker
mkdir -p prefix/bin prefix/libexec/nuschl-musicbot
cp target/debug/nuschl-musicbot prefix/bin/
cp target/debug/nuschl-musicbot-codec-worker prefix/libexec/nuschl-musicbot/
prefix/bin/nuschl-musicbot \
  --config musicbot.toml --state-directory DIR --web-assets apps/musicbot/web/dist
```

`apps/musicbot/nuschl-musicbot.example.toml` explains every setting. Relative secret
file names resolve beside the configuration, or in systemd's credentials directory.
The page speaks HTTP; put a TLS proxy in front of it. Assets and API URLs are
relative to the authenticated link, including when a proxy serves it under a path.
For frontend development, run `npm run dev` in `apps/musicbot/web` through direnv,
and open its `/u/<token>/` path. Vite proxies API and art requests to a bot on :8474.

Servers with a self-signed certificate need `certificate_sha256`, the SHA-256 of
the server certificate. `channel` is the path from the root channel, so a
subchannel needs its parents: `Games/Music`, not `Music`. A channel the bot cannot
find is reported on the web page, with the full paths of channels of that name if
there are any, and the bot stays in the root channel. When a connection drops, the
bot reconnects with growing pauses up to a minute.

## Security and codec roles

The parent owns Mumble TLS, Subsonic credentials, bearer links and queue metadata.
It transports opaque library bytes; it does not call a decoder, resampler, voice
classifier or encoder, and does not link one: the codec crates belong to the separate
`nuschl-musicbot-codec-worker`, which `cargo-deny` alone allows to depend on them.
The bot starts that worker only from its fixed place beside itself
(`<prefix>/libexec/nuschl-musicbot/`; a missing worker fails the operation) as
`--codec-worker <role>`, in one fixed role, through versioned, size-capped IPC
(`nuschl_ipc::musicbot`):

- Decode: one child for the current track of each active player. The parent streams
  bounded opaque chunks; the child returns exactly 20 ms of finite stereo PCM per
  reply. The retained parent queue is 250 frames (5 seconds, 1,920,000 PCM bytes), plus at most
  five 32 KiB pump/read/write buffers, one 64 KiB response and bounded pipe storage.
- Encode: one lazy persistent child per player, one 20 ms request at a time.
  It uses Opus Audio, constrained VBR, and at most 992 encoded bytes per reply.
- Duck: one lazy persistent child per Mumble connection, one packet at a time,
  at most 1 KiB per packet and 64 retained speaker states.

Each role also has bubblewrap namespace supervision processes. These are process
costs, not additional user audio streams. Children have no audio
socket or device access. The Linux launcher clears the environment, exposes only
read-only runtime files and private temporary storage, unshares networking and other
namespaces, and limits address space to 1 GiB per child. It fails closed if required
sandbox tools are unavailable. On Windows and macOS no confinement for these
persistent roles has been designed or proven, so codec launch fails closed there
and the bot plays nothing.

After clearing the inherited environment, [bubblewrap 0.12](https://github.com/containers/bubblewrap/blob/v0.12.0/bubblewrap.c#L3195)
sets only the fixed public `PWD=/tmp` for Rust codec children. No credential or
parent environment value is admitted by the service inspection fixture.

IPC frames are capped at 64 KiB before allocation. Encoder/duck exchanges have a
2-second deadline; a decode operation must produce its next reply within 35 seconds
(excluding consumer backpressure). Network reads have the existing 30-second bound.
Cancellation closes the consumer promptly but replacement awaits actual child exit
and the outstanding network read. Kill/reap failure retains the old process slot;
no replacement is created. A song has no artificial total-duration limit: retained
queues and each operation are bounded. An HTTP sender can stream indefinitely only
while bounded decoding continues and the configured queue retains that track.

MP4 downloads are confined to the child and capped at 512 MiB. Decoder input rates
are 8–384 kHz with at most eight channels (Opus: one or two). Real FFT lengths are
capped at 16,384 samples; each probe/packet operation may read at most 4 MiB.
Replies are validated before use, and a seek/skip/pause/destination change is checked
again after asynchronous encoding, before broadcasting or advancing playback.
An encoded frame rejected by that fence resets the encoder before its next use,
so an untransmitted frame cannot become hidden predictive encoder history.
The NixOS service retains DynamicUser, ProtectSystem=strict, ProtectHome,
PrivateDevices, NoNewPrivileges, empty capabilities, MDWE and its syscall filter.
Namespace setup is restricted to the kinds used by bubblewrap; mount/unshare/setns/
capset are explicitly permitted. AF_NETLINK is needed to initialize the child's
private loopback. The parent uses ProcSubset=all so bwrap can read overflow IDs.
ProtectProc=invisible remains enabled. ProtectHostname=private preserves UTS
isolation. ProtectKernelTunables/ProtectKernelLogs are disabled because their
locked proc bind mounts prevent Linux from mounting a genuinely private proc in
an unprivileged nested namespace. Sysfs stays read-only; DynamicUser/empty
capabilities prevent host sysctl writes, kernel-log syscalls are explicitly denied,
and PrivateDevices excludes /dev/kmsg. No host proc is bound into a codec child.
These are measured service-policy changes, not an unrestricted worker fallback.

The owned Linux launcher and exact service-profile replica denied a synthetic
credential file, a loopback listener and a synthetic credential environment
variable. The final NixOS VM passed actual service playback, then inspected the
running encoder: fixed role arguments, only stdio file descriptors, private PID
and network namespaces, a 1 GiB address-space limit, and no profile/credential
mounts or inherited parent environment. Under the actual service UID and mount
namespace, non-mutating checks confirmed empty effective capabilities, unwritable
host sysctl/sysrq paths and writable service-owned state. The VM also passed the
full standalone stock-Murmur/fake-Subsonic suite. Its network was restricted and
all media was generated; it opened no host microphone or audio service.

These checks do not establish Windows/macOS confinement, a complete hostile-media
corpus for every supported format, physical audio quality or instantaneous
revocation of packets already submitted to the session. A canceled blocking
library read can delay replacement until its existing 30-second read bound.

Authenticated SSE streams share 32 lifetime permits; overload returns HTTP 503
without an admission wait queue. Artwork has two global jobs. Each owned task
retains its permit through the worker's bounded response, EOF and actual process
reaping, even if the HTTP client disconnects. A helper failure never falls back
to parent decoding or encoded browser images. The fixed packaged content helper
has no network, credentials, profile or audio devices.

The browser permits two active fetches and 64 queued requests, with absolute
10-second consumer deadlines. Cancellation retains an active fetch's capacity
until its real settlement. Cached pixel storage is bounded to 64 entries/8 MiB;
live Canvas storage is bounded to 64 bitmaps/16 MiB logical RGBA. Offscreen or
obsolete-library bitmaps are cleared. CSP forbids image resource loading.
These are application-level budgets, not a complete accounting of browser memory.

Use `tokens_file = "channel-tokens"` for private Mumble ACL tokens; its contents
are a JSON array of token strings supplied through `credentials.channel-tokens`.
The NixOS module rejects inline `channel.tokens`, which would otherwise enter the
world-readable Nix store. Standalone inline tokens remain supported, so protect
that configuration file like the other credentials. API keys/passwords retain
existing credential-file handling. Never put secret values directly in Nix settings.

## Tests

The original codec-isolation checkpoint passed 90 Rust tests, 23 frontend tests and eleven
real NixOS module-evaluation invariants, plus package/deny/audit/fmt/crate2nix
checks. `tests/musicbot_workers.py` exercises actual confined decode/encode/duck
roles (malformed media and framing, fresh-owner recovery, finite PCM, encoder
reset and independent C libopus interoperability). Its companion
`musicbot_sandbox.rs` links the production IPC launcher; `musicbot_sandbox.py`
uses only owned file/network/environment sentinels. `musicbot_module.nix` evaluates
credential handling and retained parent restrictions without starting a service.
`musicbot_service_isolation.py` runs only inside the owned NixOS VM and verifies
actual service/child metadata and non-mutating access checks.

The artwork/SSE follow-up passed 91 bot tests, 30 frontend tests, 20 IPC tests
(one existing ignored test), and one content-worker unit plus four integration
tests (one existing ignored test). `musicbot_artwork_boundary.py` drives the real
supervisor against malformed replies, oversized advertised lengths, missing
helpers, lingering children, malformed media and a compressed oversized PNG.
Its 17 cases passed. `musicbot_artwork.py --cancel-probe …` verifies actual HTTP
cancellation while both confined children are stalled, immediate overload, real
PID exit and capacity reuse. Browser mode runs the packaged UI and CSP at
320/390/1280 pixels: exact generated red/blue pixels, library changes, held stale
responses, malformed JSON, offscreen Canvas release and no image/media requests.
The browser is isolated from host devices and profiles; its non-local background
requests meet an owned rejecting proxy. This proves synthetic artwork behavior,
not full browser resource-exhaustion resistance or platform-wide confinement.
The final NixOS VM also passed the actual service artwork route, codec-child
capability checks and full standalone interoperability suite. An earlier run
failed because its best-effort event drain raced the actor's subscription-switch
end marker; the fixture now observes that specific bounded boundary before
starting new music, retaining the original packet-count/end assertions.

Rust tests cover queue ordering, seek boundaries, history lifecycle and retention,
durable state, library identity across reordering/removal, gain ramps, and VAD.
Frontend tests cover progress interpolation, scrubbing, drag indices, route
encoding, shortcuts, and history pagination during live refreshes.
`tests/musicbot.py` drives the JSON API and two SSE viewers against stock Murmur
and a plain/OpenSubsonic fixture. It checks synchronized mutations, shared and
separate audio, speech-only ducking, seeking, public presence comments, completed,
skipped and failed history, pagination, and restart recovery with libraries
reordered or removed.

```sh
cargo build -p nuschl-cli -p nuschl-musicbot -p nuschl-musicbot-codec-worker
mkdir -p prefix/bin prefix/libexec/nuschl-musicbot
cp target/debug/nuschl-cli target/debug/nuschl-musicbot prefix/bin/
cp target/debug/nuschl-musicbot-codec-worker prefix/libexec/nuschl-musicbot/
direnv exec . just musicbot-web
python3 tests/with_local_murmur.py "$(nix build --inputs-from . nixpkgs#murmur --print-out-paths)/bin/mumble-server" \
  tests/musicbot.py prefix/bin PORT --web-assets apps/musicbot/web/dist \
  --espeak "$(nix build --inputs-from . nixpkgs#espeak-ng --print-out-paths)/bin/espeak-ng" \
  --opus-library "$(nix build --inputs-from . nixpkgs#libopus --print-out-paths)/lib/libopus.so"
```

`nix build .#checks.x86_64-linux.vm-musicbot` runs the bot under its NixOS module
against Murmur in a VM, then the same script.

## Playback state

Deployment restarts retain the current song and position, upcoming entries and
their order, pause, volume, ducking settings, and channel-to-queue assignments.
There is a brief audio gap while the bot reconnects, then playback resumes.
Back up `queues.json` together with the bot's state directory. Keep the channel
names and library names/URLs stable: queues are matched by channel name, and
tracks by library name plus URL, rather than configuration order.

History starts with the first successfully transmitted audio packet. Its
transmitted duration counts audio sent by the bot; it excludes paused/buffering
time, listenerless waits and skipped-over audio after a seek. Reaching EOF marks a
play completed even if the user sought past part of it. Failed attempts that sent
no audio show **Never started**. Removing an upcoming track or skipping before
any audio does not enter history. Pause, seek and restart preserve one ongoing
play; its history entry appears when it finishes or an error is processed.

History lives in `queues.json`, including ongoing-play start and transmitted
duration. Older queue files load with empty history; past plays are not inferred.
Historical metadata remains readable if its library is removed; its browsing and
artwork links become unavailable. Queues removed from the configuration are dropped.

Queue changes are checkpointed after one second and playing positions every five
seconds. SIGTERM and Ctrl-C write a final checkpoint before exit. Writes are
serialized, synced and atomically replaced. A crash can lose changes since the
last checkpoint; a corrupt or unsupported file stops startup and remains untouched.
Upcoming tracks whose library was removed are dropped with a warning. If the
current track's library disappears during deployment, its ongoing play is retained
as failed before decoding resumes.

Server bandwidth changes apply live. A channel whose budget cannot support the
6 kbit/s codec minimum plus transport overhead stops receiving music; other
attached channels may continue. With no eligible channel, position and history
duration stop advancing until the budget recovers.

Seeking restarts decoding and discards audio before the requested position, which
works with all supported formats and server transcodes. On a slow connection or a
long track, seeking can take time; the player shows buffering while preparing it.

## Web API

The Svelte app uses endpoints below `/u/<token>/api/`. `GET state` returns a
snapshot; `GET events` sends `event: state` SSE frames immediately, after changes,
and roughly every two seconds for progress correction. Slow viewers receive the
latest state when they can read again. Revoked links lose access to the stream.
At most 32 streams may remain open across all tokens; excess viewers receive 503.
The permit belongs to the response stream, not merely its request handler.
Proxies must pass SSE through without buffering; the bot sends `X-Accel-Buffering: no`.

The authenticated `GET /u/<token>/licenses` route serves the installed, bounded
licence text. Nix packages are gated by the scoped Rust/npm notice inventory;
the frontend retains actual emitted-owner licence files and its source lock.
The musicbot scope is separate from the main web client's WASM build.

`GET history?player=N&before=ID&limit=N` returns newest-first `items`, an exclusive
`next_before` cursor, the oldest retained ID (`oldest_id`), and a `revision`.
The default page size is 50, capped at 100; zero is invalid. Each item includes a
stable history ID, the queue entry and track metadata, library name, nullable
`started_at_ms`, `ended_at_ms` (Unix milliseconds), `played_ms`, `position_ms`,
`outcome` (`completed`, `skipped`, `failed`) and a nullable safe error message.
The history ID orders finished plays independently of reordered queue-entry IDs.
A missing library has `entry.track.library: null`; backend URLs and credentials
are never returned. State/SSE snapshots include only each player's
`history_revision`, so open history views refresh when a record is added without
receiving the entire retained history every playback tick.

`POST command` accepts JSON tagged by `command`: `pause`, `resume`, `skip`,
`clear`, `seek` (`position_ms`), `volume`, `ducking`, `remove`, `move`, `add`, and
`attach`. Commands include a player index; entries have stable IDs. `move.index`
is the index after removing the moved entry. `add.index` is an insertion position,
or null for the end. Successful commands return 204 and broadcast the new state.

Browsing uses `albums`, `album`, `artists`, `artist`, `playlists`, `playlist`, and
`search` JSON endpoints. Library identity in requests is its current config index.
Album lists accept `library`, `list`, and `page`; detail endpoints accept `library`
and `id`. The authenticated `art/<library>/<id>?size=…` route returns
`{"width":N,"height":N,"rgba":"<canonical base64 RGBA>"}` with private/no-store
caching. Size is clamped to 32–400. There are exactly these three fields, no
encoded-file or source-URL authority. Invalid images return 404, unavailable or
busy workers return 503. Library failures preserve the existing redacted errors.
API errors carry an HTTP error status and `{"error":"message"}`.

