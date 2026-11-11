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

# The storage worker on a throwaway PostgreSQL from nixpkgs (a unix socket in a
# temporary directory; nothing listens on the network), and on SQLite.
test-postgres:
    nix shell --inputs-from . nixpkgs#postgresql -c cargo test -p nuschel-storage-worker --test postgres -- --nocapture

licenses:
    cargo deny check licenses bans sources

audit:
    cargo deny check advisories

# Safe Rust needs no sanitizer, and the targets run about four times faster
# without one. FUZZ_SANITIZER=address also checks the unsafe code in
# dependencies (html5ever, the image decoders), e.g. over a corpus with
# `just fuzz <target> 0 -runs=0`; switching rebuilds every target.
fuzz_sanitizer := env_var_or_default("FUZZ_SANITIZER", "none")

# Run a libFuzzer target (`fuzz/fuzz_targets/<target>.rs`) for `seconds`, from
# fuzz/seeds/<target>; new inputs go to fuzz/corpus/<target>, crashes to
# fuzz/artifacts/<target> (neither is committed). Extra arguments go to libFuzzer,
# e.g. `just fuzz html 600 -fork=4`. Nightly and cargo-fuzz come from the
# flake's fuzz shell. Targets: `just fuzz-list`. See SECURITY.md, "Fuzzing".
fuzz target seconds="600" *libfuzzer:
    mkdir -p fuzz/corpus/{{target}} fuzz/seeds/{{target}}
    nix develop .#fuzz --command cargo fuzz run --fuzz-dir fuzz --debug-assertions \
        --sanitizer {{fuzz_sanitizer}} {{target}} fuzz/corpus/{{target}} fuzz/seeds/{{target}} -- \
        -max_total_time={{seconds}} -timeout=10 -rss_limit_mb=2048 {{libfuzzer}}

fuzz-list:
    nix develop .#fuzz --command cargo fuzz list --fuzz-dir fuzz

# Build every fuzz target (as `just fuzz` does), without running one.
fuzz-build:
    nix develop .#fuzz --command cargo fuzz build --fuzz-dir fuzz --debug-assertions \
        --sanitizer {{fuzz_sanitizer}}

# Regenerate the per-crate Nix build after changing dependencies (Cargo.lock).
crate2nix:
    nix run --inputs-from . nixpkgs#crate2nix -- generate -f Cargo.toml -o nix/crate2nix/Cargo.nix -h nix/crate2nix/crate-hashes.json
    nix eval --raw .#legacyPackages.x86_64-linux.crate2nixInputs > nix/crate2nix/inputs.sha256

fmt:
    cargo fmt --all
    nixfmt flake.nix nix/*.nix
    rg --files -g '*.toml' -0 | xargs -0 taplo fmt
