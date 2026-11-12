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
- **Covers.** The bot fetches covers from the library with its own credentials and
  passes them on unchanged (JPEG, PNG, WebP, GIF or AVIF, at most 8 MiB); it does
  not decode them. The browser may keep a cover for a day.
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


For a source build, run `direnv exec . just musicbot-web`, then:

```sh
direnv exec . cargo run -p nuschl-musicbot -- \
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

## Security

The bot decodes the songs in its own process (SECURITY.md lists it among the
exceptions). The songs come from libraries the operator configured. For ducking it
also decodes the voice packets of people in its channels, which are untrusted: with
opus-pure (Rust) and RNNoise (`nnnoiseless`, Rust), packets bounded by the session
(1 KiB), at most 64 talkers at a time. The process holds the bot's own client
certificates, library credentials, access links, and queue/history metadata. The
page sends `Referrer-Policy: no-referrer` and a strict content security policy and loads
nothing from elsewhere. Pages are not cached; covers are cached privately by the
browser, since their addresses contain the link.

Standalone state is owner-only as well: the configured directory is made `0700`
and token/queue files, including temporary and existing files, are made `0600` on
Unix. Ancestor directories are left unchanged.

Library metadata, artwork and transcode-decision calls have a 30-second total
deadline. At most eight web library calls run or wait in the blocking pool;
overload returns 503 immediately. A search walks larger library lists with that
same concurrency bound. Song streams have no total-song deadline, but stalled
reads fail after 30 seconds. Authenticated requests never follow redirects; use
the service's canonical endpoint. Remote error diagnostics are not sent to viewers.

Only one decoder worker per queue may remain active. A seek or skip cancels
download/probe/demux work and waits for its actual exit before replacing it.
Decoded input is admitted at 8–384 kHz and up to eight channels (Opus: mono/stereo).
Real FFT plans are capped at 16,384 samples; each probe/packet operation may read
at most 4 MiB. These bounds apply to streamed metadata as well as downloaded MP4,
without imposing a duration cap on ordinary streaming songs.

## Tests

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
cargo build -p nuschl-cli -p nuschl-musicbot
direnv exec . just musicbot-web
python3 tests/with_local_murmur.py "$(nix build --inputs-from . nixpkgs#murmur --print-out-paths)/bin/mumble-server" \
  tests/musicbot.py target/debug PORT --web-assets apps/musicbot/web/dist \
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
and `id`. The authenticated `art/<library>/<id>?size=…` route serves covers.
API errors carry an HTTP error status and `{"error":"message"}`.

