# Nuschel

Nuschel is an EUPL-1.2 Rust workspace for a Mumble-compatible desktop client and
server. GPUI is the desktop renderer. Shared state, protocol framing, and message
presentation do not depend on GPUI, leaving mobile and web adapters open.

The desktop connects to stock Mumble servers with TLS, persistent client certificates,
channel trees, live chat, direct messages, mute/deafen, and Opus voice over Mumble's
TCP tunnel. Use **+** in the server rail to add a server. Self-signed certificates
require confirmation before credentials are sent; accepted fingerprints are saved.
Passwords remain in memory for the connection attempt and are not saved.

Click a channel to browse its local history. Double-click it or use **Join voice**
to move your voice connection. Initially only the root is expanded; deeper branches
open on demand. Long channel names use an ellipsis with their full name on hover.
The channel list has a draggable scrollbar. Wheel steps scroll smoothly in the list
and chat, while precise touchpad input retains its native motion. Microphone capture starts muted. Enter inserts a newline; Ctrl+Enter
(or Cmd+Enter) sends. Messages preserve blank lines, highlight fenced code, and offer
**Show more** for long content. Legacy HTML is converted into inert text in a worker.
History and cached channel trees survive application restarts, including offline browsing.

This is an early client: Mumble UDP transport, CELT-only servers, whisper controls, automatic
reconnection, and comprehensive administration are not implemented. Voice currently
transmits continuously while unmuted. Audio service failures restart only the media
worker and preserve the server connection and mute state. Automatic AEC/effect detection
is still planned; the client does not enable a speculative second echo canceller.
Linux is exercised in integration tests; Windows/macOS audio need native validation.
The Nuschel server and MLS implementation are deferred.

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

`cargo add` is built into the pinned Cargo. `Cargo.lock` and `flake.lock` are tracked.
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
