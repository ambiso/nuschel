# Nuschel

Nuschel is an EUPL-1.2 Rust workspace for a Mumble-compatible desktop client and
server. GPUI is the desktop renderer. Shared state, protocol framing, and message
presentation do not depend on GPUI, leaving mobile and web adapters open.

The desktop connects to stock Mumble servers (tested against 1.5, 1.4 and 1.3) with
TLS and a persistent client certificate. Identities are RSA-2048 so that stock Mumble
can import them; PKCS#12 import and export are in **Settings → Identity**. Use
**+** in the server rail to add a server, or open a `mumble://` link. **Connect**
uses the saved details; the cog beside the server name edits them. Saved servers
connect when Nuschel starts (per server), self-muted and deafened until you unmute,
and **Settings → General** can start Nuschel at login (minimised). Self-signed
certificates require confirmation before credentials are sent, and accepted
fingerprints are saved. Passwords stay in memory for the connection attempt only.

Voice uses Mumble's encrypted UDP transport, with automatic fallback to the TLS
tunnel when UDP is blocked and a force-TCP option. Transmission is by voice activity
(RNNoise), push-to-talk (in the window, and system-wide where the desktop grants a
shortcut), or continuous. Capture runs echo cancellation, noise suppression and
automatic gain in Rust. Nuschel's canceller works from a recording of what the
speakers play (all applications, mono or stereo), time-aligned with the microphone,
so music and other programs are removed from your microphone too; the recording is
made only while the microphone is open and never leaves the device (macOS asks for
system audio permission). On Linux Nuschel follows PipeWire's graph upstream from
the microphone: a system echo canceller (PipeWire/PulseAudio echo-cancel) anywhere
in the chain replaces Nuschel's and receives Nuschel's playback as its reference, and
a noise suppressor (e.g. an RNNoise filter-chain) replaces Nuschel's suppressor; a
per-microphone override remains. **Settings → Microphone test** meters the
microphone, plays back what others would hear, and runs an echo test, without a
server and without unmuting; nothing is sent. An adaptive controller watches for playback stutters, residual echo and
a clipping microphone. It adjusts only Nuschel's own buffer and voice gate, and
tells you what it cannot fix. Microphone capture starts muted. Audio service
failures restart only the media worker and keep the connection and mute state.

Click a channel to open its conversation: one continuous timeline that follows new
messages at the bottom. Scrolling up loads older local history without moving what
is on screen, down to **Beginning of local history**; while you read further up, a
pill counts new messages and jumps back to the latest. Double-click a channel, or use **Join
channel**, to move your voice connection. Right-click users, channels and servers
for whisper and shout, channel listening and links, per-user volume and local mute,
direct messages, comments and avatars, registration, moderation, the channel
editor, ACLs, bans and registered users. Entries appear only when the server's
permissions allow them. Lost connections reconnect with backoff, rejoin your
channel with mute state intact, and mark the gap in the timeline. Enter sends and
Shift+Enter starts a new line (switchable in Settings). Drag the channel list's edge
to fit long names; collapsed branches show how many people are inside, and branches
with people open on connect. Messages keep blank lines, highlight fenced
code, show inline images (decoded in a sandboxed worker), and collapse by measured
height behind **Show more**. Ctrl+V pastes an image (re-encoded in the sandboxed
worker to fit the server's limit and sent the way stock Mumble sends it), and Ctrl+.
or Ctrl+; opens an emoji picker. Volume mixers name each stream (microphone, voice
chat, echo reference). Search finds messages saved on this device. History and
cached channel trees survive restarts, including offline browsing. Controls and
text fields are exposed to screen readers (AT-SPI on Linux is tested).

Linux is covered by integration tests in NixOS VMs. Windows and macOS packages are
cross-built from Linux (`.#windows`, `.#macos-app`, `.#macos-universal`,
`.#macos-aarch64`). The Windows build runs in an unattended Windows 11 VM
(`tests/windows_vm.py`): GUI, connection, autostart, audio devices, the microphone
and echo tests, and in-place updates. macOS has not been run. The macOS build needs
macOS 14.2 or later because the audio library links Core Audio's process-tap API.

## Releases and updates

Tagging `vX.Y.Z` runs `.github/workflows/release.yml` on one Linux runner: the
Windows build and a universal, ad-hoc signed `Nuschel.app` are cross-compiled by
the flake (no macOS runner), zipped, and published with `update.json`, which lists
every file with its size and SHA-256 and is signed with minisign. The workflow needs
the repository secret `UPDATE_SIGNING_KEY`; its public half is
`crates/update/update-key.pub` and is built into the client.

Windows and macOS copies check for a newer release 20 seconds after start and every
six hours (**Settings → General → Check for updates**). They verify the signature,
accept only newer versions, check each file's hash, and swap the files by renaming,
rolling back on failure; old files are removed on the next start. Linux installs
update through their package manager (`nix run github:ambiso/nuschel`).

On macOS the app is ad-hoc signed, not notarised: the first start of a downloaded
copy needs **System Settings → Privacy & Security → Open Anyway**. Updates fetched by
Nuschel itself are not quarantined and start normally, as long as `Nuschel.app` was
moved out of Downloads (macOS runs quarantined apps from a read-only copy). Developer
ID signing and notarisation can be added from the same Linux job with `rcodesign`
and an Apple Developer account.
See [SECURITY.md](SECURITY.md) for what each platform's worker confinement does and
does not provide. The Nuschel server and MLS implementation are deferred.

The modern media transport is now a str0m trial in `nuschel-rtc`. Run
`nix run --cores 2 .#rtc-probe` for the headless audio/video loopback probe, or
`just rtc` inside the development shell to include worker Opus encoding/decoding.
Both send Opus and a generated VP8 test pattern between two loopback UDP peers.
No microphone, camera, public STUN service or external server is used.
The probe reports encoded-frame delivery delay and
checks received bytes; it is not a microphone-to-speaker latency measurement.
Video admission follows the bandwidth estimate, reserves room for audio and drops
frames rather than accumulating a stale queue. Desktop video calls, authenticated
server signaling, TURN, browser interoperability and adaptive playout remain future work.

## Development

```sh
direnv allow
# Or enter the same pinned environment explicitly:
nix develop --cores 2
just build
just check
just workers
# Linux VM packaging, confinement, SQLx/PostgreSQL and service preflight:
just vm
```

The icon's source is `apps/desktop/assets/nuschel.svg`. The Linux package installs
the desktop entry and icons and registers `mumble://` links; after changing the icon,
regenerate the committed Windows `.ico` with the command at the top of
`nix/desktop-files.nix`.

`cargo add` is built into the pinned Cargo. `Cargo.lock` and `flake.lock` are tracked.
The Linux package (`nix build .#default`, also used by the VM tests) is built per
crate with crate2nix, so a change rebuilds only the crates that depend on it; after
changing dependencies run `just crate2nix` (a flake check fails while
`nix/crate2nix/Cargo.nix` is out of date). Clippy, tests and the Windows/macOS cross
builds use crane (`.#default-crane` builds the package the same way).
The Nix environment uses Rust's latest stable release from the locked rust-overlay
revision; `nix flake update rust-overlay` advances it. Outside Nix, rustup follows
the `stable` channel independently of `flake.lock`.
`nix flake check --cores 2` runs the native build, tests, Clippy, formatting, dependency policy,
and a vulnerability check using a pinned advisory database. `cargo deny check` also
checks the current advisory database and registry yanks outside the Nix sandbox.

Use `jj` for incremental commits. See [the architecture](docs/architecture.md),
[the safety policy](SECURITY.md), and [the integration test plan](tests/README.md).

On NixOS, the desktop loads the host's Vulkan driver. The flake's glibc must be new
enough to load that driver; an older nixpkgs pin can produce `No GPU adapters found`
even when other applications work. Diagnose driver loading with
`VK_LOADER_DEBUG=error,warn RUST_LOG=nuschel=info,gpui_wgpu=info nix run .`.
If the log reports a missing `GLIBC_*` version, update the pin with
`nix flake update nixpkgs` and rebuild. Adding driver search paths cannot fix that
ABI mismatch.

## Server module and databases

The flake exports `nixosModules.default` and a headless `packages.<system>.server`
containing `nuscheld` and the storage worker, without the GPUI/audio stack. The module
defaults to SQLite and also supports PostgreSQL, either through `database.urlFile`
or local peer authentication:

```nix
imports = [ inputs.nuschel.nixosModules.default ];
services.nuschel = {
  enable = true;
  database.type = "postgresql";
  database.createLocally = true;
};
```

The module is a development interface: its database preflight is implemented, but
`nuscheld` deliberately refuses to start a listener until Mumble sessions exist.
Enabling it is not yet a deployable voice server. The VM smoke test substitutes a
configuration-only command while retaining the real hardened unit and migrations.

External PostgreSQL URLs and TLS keys come from runtime credential files, not inline
Nix values. `openFirewall` is off by default. The service uses DynamicUser, a private
state directory, restricted permissions and systemd hardening. See
[module options](nix/module.nix) for the configuration interface.

## Message defaults

- Newlines and blank lines remain visible. Ordinary text is literal; HTML is inert.
- Long messages collapse with a keyboard-accessible **Show more** control.
- Fenced code is highlighted in the content worker with Syntect's Rust regex backend.
- Local chat history is enabled by default, including on legacy Mumble servers.
  Legacy history contains messages observed locally, not messages sent while offline.
- Nuschel server archives use bounded cursor pages so the full retained history is
  reachable. The SQLite/PostgreSQL storage worker implements this; session negotiation
  and history fetching over the network remain unimplemented.

Red open locks mean **no end-to-end encryption**, not an absence of Mumble transport
encryption. Green locks are reserved for authenticated MLS sessions. MLS is a planned
strict server mode, incompatible with stock clients; no MLS session exists yet.
See [the MLS and archive design](docs/architecture.md#mls-and-encrypted-history).

These rules live in shared crates; a mobile renderer can implement the same behavior.

## License

Nuschel is licensed under [EUPL-1.2](LICENSE). Vendored Mumble schemas retain their
[upstream BSD license](crates/protocol/proto/LICENSE.mumble). Dependency license
allowances are reviewed separately in `deny.toml`; SPDX allowlisting does not replace
release attribution or license-compliance review.
