default:
    @just --list

check:
    cargo fmt --all -- --check
    cargo clippy --workspace --all-targets -- -D warnings
    just wasm
    cargo test --workspace
    cargo deny check

# The SDK's core builds for browsers (docs/clients.md, "The SDK"): no native
# session, runtime or clock in it.
wasm:
    cargo clippy -p nuschl-core -p nuschl-client --target wasm32-unknown-unknown --no-default-features -- -D warnings

# The SDK for browsers (crates/client-wasm, docs/web.md), for the Svelte client
# in web/app (src/sdk). ring's C needs an unwrapped clang for wasm32, and the
# wasm-bindgen CLI must be the crate's exact version.
wasm-client profile="release":
    nix shell --inputs-from . nixpkgs#llvmPackages.clang-unwrapped nixpkgs#llvmPackages.bintools-unwrapped nixpkgs#wasm-bindgen-cli_0_2_130 --command sh -euc '\
        export CC_wasm32_unknown_unknown="$(command -v clang)" AR_wasm32_unknown_unknown="$(command -v llvm-ar)"; \
        cargo clippy -p nuschl-client-wasm --target wasm32-unknown-unknown -- -D warnings; \
        cargo build -p nuschl-client-wasm --target wasm32-unknown-unknown --profile {{profile}}; \
        directory={{ if profile == "dev" { "debug" } else { profile } }}; \
        wasm-bindgen --target web --out-dir web/app/src/sdk \
            "${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/$directory/nuschl_client_wasm.wasm"'

# The Svelte client (web/app) with the SDK, into web/app/dist.
web: wasm-client
    cd web/app && npm ci --no-audit --no-fund && npm test && npm run build

workers:
    cargo build --workspace
    python3 tests/workers.py

rtc:
    cargo build -p nuschl-rtc-probe -p nuschl-media-worker
    python3 tests/rtc_probe.py

vm:
    nix build --cores 2 .#checks.x86_64-linux.vm-smoke --no-link -L

build:
    cargo build --workspace

desktop:
    cargo build --workspace
    cargo run -p nuschl

server:
    cargo run -p nuschld -- --help

# The storage worker on a throwaway PostgreSQL from nixpkgs (a unix socket in a
# temporary directory; nothing listens on the network), and on SQLite.
test-postgres:
    nix shell --inputs-from . nixpkgs#postgresql -c cargo test -p nuschl-storage-worker --test postgres -- --nocapture

licenses:
    cargo deny check licenses bans sources

audit:
    cargo deny check advisories

# Safe Rust needs no sanitizer, and the targets run about four times faster
# without one. FUZZ_SANITIZER=address also checks the unsafe code in
# dependencies (html5ever, the image decoders), e.g. over a corpus with
# `just fuzz <target> 0 -runs=0`; switching rebuilds every target.
# The Windows sandbox adapter's unit tests (its launch steps against a mock of
# Win32) under Miri, for Linux and for Windows; docs/windows-sandbox.md.
miri:
    nix develop .#miri --command cargo miri test -p nuschl-sandbox-windows --lib
    nix develop .#miri --command cargo miri test -p nuschl-sandbox-windows --lib --target x86_64-pc-windows-gnu

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

# A loopback nuschld, scripted people and the desktop in a private headless GNOME
# Shell on this machine's GPU: nothing of yours is read or shown. Look at the result.
# Regenerate the README's screenshot, docs/images/nuschl.png (tests/readme_screenshot.py).
readme-screenshot:
    cargo build --workspace
    nix shell --inputs-from . nixpkgs#gnome-shell nixpkgs#glib.bin nixpkgs#dbus nixpkgs#imagemagick nixpkgs#oxipng \
        --command python3 tests/readme_screenshot.py "${CARGO_TARGET_DIR:-target}/debug" --output docs/images/nuschl.png

# Regenerate the per-crate Nix build after changing dependencies (Cargo.lock).
crate2nix:
    nix run --inputs-from . nixpkgs#crate2nix -- generate -f Cargo.toml -o nix/crate2nix/Cargo.nix -h nix/crate2nix/crate-hashes.json
    nix eval --raw .#legacyPackages.x86_64-linux.crate2nixInputs > nix/crate2nix/inputs.sha256

fmt:
    cargo fmt --all
    nixfmt flake.nix nix/*.nix
    rg --files -g '*.toml' -0 | xargs -0 taplo fmt
