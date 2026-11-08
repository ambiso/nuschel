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

vm:
    nix build .#checks.x86_64-linux.vm-smoke --no-link -L

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

fmt:
    cargo fmt --all
    nixfmt flake.nix nix/*.nix
    taplo fmt
