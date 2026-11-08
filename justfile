default:
    @just --list

check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    cargo test --workspace
    cargo deny check

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
    nixfmt flake.nix
    taplo fmt
