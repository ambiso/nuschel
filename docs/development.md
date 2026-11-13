# Developing Nuschl

How to build and check Nuschl, and how its build is put together. What the tests
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
| `just vm-driver --root ROOT DRIVER` | a built NixOS test driver run by hand, with all VM state on disk (below) |
| `just vm-driver-check` | the hand runner's and its pre-boot guard's tests, without a VM |
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

A test's built `.driver` can also run outside the Nix daemon. The pinned driver
keeps each machine's disk image, sockets and shared directory below an inherited
`XDG_RUNTIME_DIR` (normally the tmpfs `/run/user/<uid>`) and uses `TMPDIR` only
when that is unset, so setting `TMPDIR` alone leaves VM state in RAM. Run drivers
through `just vm-driver --root ~/.cache/nuschl-vm-<run> DRIVER`
(`tests/vm_driver.py`). It points both variables into a fresh mode-0700 root inside
the cache directory, below an existing parent only you can write, with a clean
environment; `--env` adds only `NUSCHL_*` fixture inputs. Node names such as
`machine` then name state private to the run. Before the driver's own script,
`tests/vm_driver_scratch.py` runs inside the actual driver. It refuses to boot if
any machine's real state, shared or VLAN directory, or a socket its start script
creates, is RAM-backed, outside `ROOT/run` or too long for a Unix socket. Keep the
root short (about 50 bytes). `--probe` runs only that check and boots no guest.
The runner stops its own process group below 8 GiB free `/tmp`, scratch space or
available memory, or after `--timeout`. It leaves `driver.log`,
`scratch-proof.json` and `status.json` in the root and deletes nothing. Flake
checks such as `just vm` still run inside the Nix daemon; client variables do not
move its build scratch.

The worker exchange diagnostic has its own `main` (`harness = false`). Run:

```sh
nix build --cores 2 --max-jobs 1 \
  .#checks.x86_64-linux.ipc-exchange \
  .#checks.x86_64-linux.ipc-exchange-negative-controls
```

This exercises crash, malformed reply, timeout and lingering-worker recovery
with real bubblewrap/prlimit. The
ordinary IPC unit-test leaf compiles this fixture under libtest and reports zero
tests for it. This check requires a working sandbox and rejects a substituted
successful executable; it checks eventual cleanup, not atomic worker-tree exit
or platform-wide confinement.

## Continuous integration

Pull requests to `main` and pushes to `main` run one command on the self-hosted
runner, the `ci` check: `nix build -f ci.nix`, every flake check including the
NixOS VM tests, except the release packaging proofs. Run the same command before
pushing with `scripts/pre-pr`; Nix reads only tracked files, so commit new files
first. jj runs no git hooks: push bookmarks with `scripts/pr-push --bookmark NAME`.
With plain git, `git config core.hooksPath .githooks` runs it before each push.

Changes reach `main` only through pull requests from this repository's branches
whose required checks pass. Pull requests from forks never run on the
self-hosted runner; a maintainer pushes a reviewed fork branch here instead.
Every workflow has a read-only token; CodeQL's `security-events: write`, to
upload results, is the only exception.

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

### Reusing Cargo build artifacts

Prefer Crane's dependency-only artifacts when reusing a build. A full workspace
check also contains first-party metadata. With Nix's fixed source timestamps,
Cargo can incorrectly reuse that metadata after a source change, producing stale
API errors or skipping the code that a check should exercise.

If a coordinated check reuses full workspace artifacts, run
`cargo clean --release --workspace --offline` inside that check's owned build
directory before the release-profile check. Match the profile if checking another
one. This removes workspace outputs while retaining third-party dependencies;
do not clean another agent's build or the shared artifact source. Confirm from the
build log that the intended workspace packages were checked or compiled. Record
the source revision, seed artifact, command and result with the evidence.

## The icon

The icon's source is `apps/desktop/assets/nuschl.svg`. The Linux package installs
the desktop entry and icons and registers `mumble://` links; after changing the icon,
regenerate the committed Windows `.ico` with the command at the top of
`nix/desktop-files.nix`.

## The WebRTC trial

The modern media transport is a str0m trial in `nuschl-rtc`. Run
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
