default:
    @just --list

# Infer local release inputs, confirm once, then keep signed artifacts for review.
# Positional arguments are passed through argv, never interpolated into shell code.
[positional-arguments]
release *args:
    python3 release/release.py "$@"

# Open issues in AGENTS.md queue order (read-only GraphQL through `gh`); also
# `why N`, `blocked-urgent` and local claims (`acquire-next --agent NAME`).
[positional-arguments]
next *args:
    python3 -I tools/next-issues.py "$@"

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

# The music bot's Svelte UI, served by nuschl-musicbot from this directory.
musicbot-web:
    cd apps/musicbot/web && npm ci --no-audit --no-fund && npm test && npm run build

workers:
    cargo build --workspace
    python3 tests/workers.py

rtc:
    cargo build -p nuschl-rtc-probe -p nuschl-media-worker
    python3 tests/rtc_probe.py

vm:
    nix build --cores 2 .#checks.x86_64-linux.vm-smoke --no-link -L

# `just vm-driver --root ~/.cache/nuschl-vm-r1 [--probe] /nix/store/...-nixos-test-driver-...`
# (tests/vm_driver.py) refuses to boot on RAM-backed or escaped state paths and
# over-long socket paths, and runs outside the Nix daemon.
# A built NixOS test driver with TMPDIR and XDG_RUNTIME_DIR in a fresh owned disk root.
[positional-arguments]
vm-driver *args:
    python3 tests/vm_driver.py "$@"

# The runner's and pre-boot guard's host-only tests; no VM boots.
vm-driver-check:
    python3 -m unittest discover -s tests -p vm_driver_test.py

# Host checks only; a working macOS/SwiftUI/Simulator guest is a separate gate.
ios-preflight:
    python3 tests/ios_vm.py preflight

ios-vm-check:
    python3 -m unittest discover -s tests -p ios_vm_test.py

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
    just audit-scope

# deny.toml ignores RUSTSEC-2026-0173 only while proc-macro-error2's one resolved
# edge is hax-lib-macros 0.3.7 under cfg(hax), which no Nuschl build sets
# (docs/dependency-review.md). Runs offline: `cargo deny check` or another full
# fetch must have downloaded the packages.
# Fail unless proc-macro-error2 is reachable only via hax-lib-macros under cfg(hax).
audit-scope:
    #!/usr/bin/env python3
    import json, subprocess, sys

    expected = ["hax-lib-macros@0.3.7 cfg(hax)"]
    command = ["cargo", "metadata", "--offline", "--locked", "--format-version", "1", "--all-features"]
    result = subprocess.run(command, stdout=subprocess.PIPE)
    if result.returncode != 0:
        sys.exit("audit-scope: `" + " ".join(command) + "` failed")
    metadata = json.loads(result.stdout)
    packages = {package["id"]: package for package in metadata["packages"]}

    def edge(node, kind):
        package = packages[node["id"]]
        label = package["name"] + "@" + package["version"]
        if kind["kind"] is not None:
            label += " " + kind["kind"]
        return label + " " + (kind["target"] or "unconditional")

    edges = sorted(
        edge(node, kind)
        for node in metadata["resolve"]["nodes"]
        for dependency in node["deps"]
        if packages[dependency["pkg"]]["name"] == "proc-macro-error2"
        for kind in dependency["dep_kinds"]
    )
    if edges != expected:
        sys.exit(
            "audit-scope: deny.toml ignores RUSTSEC-2026-0173 (proc-macro-error2, unmaintained)"
            f" only while its resolved edges are {expected}, but they are {edges}. Review the"
            " new path before keeping the ignore, or remove it if no edge remains"
            " (docs/dependency-review.md)."
        )
    print("audit-scope: proc-macro-error2 is reachable only through hax-lib-macros 0.3.7 under cfg(hax)")

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

# Regenerate after changing dependencies/manifests, including new local vendors.
crate2nix:
    nix run --cores 2 --max-jobs 2 --inputs-from . nixpkgs#crate2nix -- generate -f Cargo.toml -o nix/crate2nix/Cargo.nix -h nix/crate2nix/crate-hashes.json
    python3 nix/crate2nix-inputs.py > nix/crate2nix/inputs.sha256

fmt:
    cargo fmt --all
    nixfmt flake.nix nix/*.nix
    rg --files -g '*.toml' -0 | xargs -0 taplo fmt
