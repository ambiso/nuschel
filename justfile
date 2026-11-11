default:
    @just --list

check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    cargo deny check

workers:
    cargo build --workspace
    python3 tests/workers.py

rtc:
    cargo build -p nuschel-rtc-probe -p nuschel-media-worker
    python3 tests/rtc_probe.py

vm:
    nix build --cores 2 .#checks.x86_64-linux.vm-smoke --no-link -L

build:
    cargo build --workspace

desktop:
    cargo build --workspace
    cargo run -p nuschel

server:
    cargo run -p nuscheld -- --help

licenses:
    cargo deny check licenses bans sources

audit:
    cargo deny check advisories

# Regenerate the per-crate Nix build after changing dependencies (Cargo.lock).
crate2nix:
    nix run --inputs-from . nixpkgs#crate2nix -- generate -f Cargo.toml -o nix/crate2nix/Cargo.nix -h nix/crate2nix/crate-hashes.json
    nix eval --raw .#legacyPackages.x86_64-linux.crate2nixInputs > nix/crate2nix/inputs.sha256

fmt:
    cargo fmt --all
    nixfmt flake.nix nix/*.nix
    rg --files -g '*.toml' -0 | xargs -0 taplo fmt
