# Developing Nuschel

How to build and check Nuschel, and how its build is put together. What the tests
cover is in [tests/README.md](../tests/README.md); the rules for code and dependencies
are in [SECURITY.md](../SECURITY.md) and [architecture.md](architecture.md).

## The environment

Everything comes from the Nix flake's development shell:

```sh
direnv allow
# Or enter the same pinned environment explicitly:
nix develop --cores 2
```

`cargo add` is built into the pinned Cargo. `Cargo.lock` and `flake.lock` are tracked.
The Nix environment uses Rust's latest stable release from the locked rust-overlay
revision; `nix flake update rust-overlay` advances it. Outside Nix, rustup follows
the `stable` channel independently of `flake.lock`. Commits are made with `jj`.

## Recipes

| Command | What it does |
| --- | --- |
| `just build` | `cargo build --workspace` |
| `just desktop` | build and start the desktop app |
| `just check` | formatting, Clippy, the SDK for wasm32, tests, `cargo deny check` |
| `just workers` | the worker executables against their protocol (`tests/workers.py`) |
| `just vm` | the Linux integration tests in NixOS VMs: packaging, confinement, SQLx/PostgreSQL, service preflight, the desktop against stock Mumble servers |
| `just test-postgres` | the storage worker on a throwaway PostgreSQL |
| `just fuzz <target> [seconds]` | a libFuzzer target from `fuzz/` (`just fuzz-list` lists them; SECURITY.md, "Fuzzing") |
| `just miri` | the Windows sandbox adapter's unit tests under Miri |
| `just rtc` | the WebRTC trial with the worker's Opus (below) |
| `just readme-screenshot` | the picture at the top of the README (`tests/readme_screenshot.py`) |
| `just crate2nix` | regenerate the per-crate Nix build after dependency changes |
| `just fmt` | format Rust, Nix and TOML |

Other VM checks run with `nix build --cores 2 .#checks.x86_64-linux.<name>`:
`vm-server` (the server module), `vm-home-manager`, `home-manager`. UI scenes for
looking at changes run in a private headless GNOME Shell (`tests/screenshots.py`,
tests/README.md).

## Packages and cross builds

The Linux package (`nix build .#default`, also used by the VM tests) is built per
crate with crate2nix, so a change rebuilds only the crates that depend on it; after
changing dependencies run `just crate2nix` (a flake check fails while
`nix/crate2nix/Cargo.nix` is out of date). Clippy, tests and the Windows/macOS cross
builds use crane (`.#default-crane` builds the package the same way).

Windows and macOS packages are cross-built from Linux: `.#windows`, `.#macos-app`,
`.#macos-universal` and `.#macos-aarch64` ([install.md](install.md#how-releases-are-made)).

`nix flake check --cores 2` runs the native build, tests, Clippy, formatting,
dependency policy, and a vulnerability check using a pinned advisory database.
`cargo deny check` also checks the current advisory database and registry yanks
outside the Nix sandbox.

## The icon

The icon's source is `apps/desktop/assets/nuschel.svg`. The Linux package installs
the desktop entry and icons and registers `mumble://` links; after changing the icon,
regenerate the committed Windows `.ico` with the command at the top of
`nix/desktop-files.nix`.

## The WebRTC trial

The modern media transport is a str0m trial in `nuschel-rtc`. Run
`nix run --cores 2 .#rtc-probe` for the headless audio/video loopback probe, or
`just rtc` inside the development shell to include the worker's Opus encoding and
decoding. Both send Opus and a generated VP8 test pattern between two loopback UDP
peers; no microphone, camera, public STUN service or external server is used. The
probe reports encoded-frame delivery delay and checks received bytes; it is not a
microphone-to-speaker latency measurement. Video admission follows the bandwidth
estimate, reserves room for audio and drops frames rather than accumulating a stale
queue. Desktop video calls, authenticated server signaling, TURN, browser
interoperability and adaptive playout remain future work
([tests/README.md](../tests/README.md#webrtc-transport-trial), [clients.md](clients.md#video-queued)).
