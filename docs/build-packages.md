# Standalone server and RTC packages

`nix build .#server .#rtc-probe --cores 2 --max-jobs 2` now assembles existing
crate2nix members. No workspace/package manifest, Cargo.lock or generated graph
changed. Windows/Darwin cross builds and the explicit `default-crane` fallback
remain on their separately tracked migration paths. Notice metadata tools may
still use locked Crane vendoring; they do not compile another Rust graph.

The server output contains regular copies of `nuschld` and
`nuschl-storage-worker`, plus its existing complete native-family notice superset.
A symlink join would be wrong: `current_exe()` chooses the sibling storage worker,
so a symlink to the original isolated server crate would lose that adjacency.
Both installed bodies match the already-tested per-crate outputs byte for byte.
Server storage mode still directly spawns its cleared-environment child. NixOS
service hardening applies to the whole service; the client storage bwrap path is
separate. A stronger standalone server-worker boundary remains an explicit TODO.

The RTC probe contains one compiled executable and its own complete scoped
notices. Its real 4,297-byte VP8 frame was previously included relative to the
whole workspace, which fails in an isolated crate. A checked probe-only Nix
`postPatch` points that compile-time inclusion at the exact declared Nix fixture
input. The original frame bytes remain embedded; production Rust is unchanged.
The initial failed build is retained, not counted as a successful probe.

## Notice authority

The old probe installed only EUPL; no runtime inventory selected its real
str0m/dimpl/RustCrypto dependencies. Each supported native target now declares a
named `standalone.rtc-probe` group in `release/notice-roots.json`, with an explicit
source-selected `rtc-probe-<RustTarget>` scope and Rust standard-library notices.
Existing desktop/cross scopes select their original main roots/features unchanged.
The all-target audit unions main and standalone roots deterministically. Nix and
Python normalize to the same target/packages/features records. Scope selection
comes from the package/platform, never the artifact's claimed metadata.

The actual Linux x86_64 inventory selects 98 runtime packages, with no ring or
AWS-LC. Its first inspectable result kept three unresolved entries:
`is0.12.0`, `str0m-proto0.8.0`, and the patched `str0m-rust-crypto0.7.0`.
Their published archives omit license files. Each retained VCS record names
str0m revision `5f2183ce7c67117cd1960dfeb11bed11fc188b66`. The exact upstream
monorepo MIT grant and root/three package/author declarations are retained under
`release/licenses/rtc-upstreams`, with immutable URLs and SHA-256 provenance.
Their declarations permit MIT or Apache2; this supplement chooses the actual MIT
alternative. The nested packet-specific Apache text is not presented as a blanket
monorepo grant. No generic SPDX text or legal-completeness assurance was introduced.
The real strict RTC bundle now has zero unresolved entries.

`tests/build_package_notices.py` checks actual compact byte ranges against these
source files, rejects real payload corruption, compares main roots/features with
the baseline, and validates the real prior Windows/Mac bundles under unchanged
main policy. Existing release tests also pass. Other native scope definitions are
not additional hardware/runtime test claims.

## Actual Linux validation

The targeted product build passed in 15 seconds without a Cargo dependency pass.
The exact embedded VP8 bytes match the source. The migrated probe completed real
owned-loopback UDP transport: 50 Opus frames each way and 17 VP8 frames each way,
with multi-packet 4,297-byte frames and a measured maximum encoded delivery delay
of 9ms in this run. This is synthetic encoded delivery, not microphone latency,
capture/rendering, video calling or A/V synchronization.

`nix/vm-server.nix` now accepts `serverPackage ? nuschl`: the four server services
use the actual standalone product, while client and witness packages stay as
before. Each service's `/proc/MainPID/exe` is checked against that product. The
entire existing six-node suite passed in 300 seconds, including both database
backends, history/persistence, stock client/voice/protocol compatibility, key
transparency/witnesses, encrypted conversations, restarts and Murmur import.
This exercises real default sibling-storage discovery and module hardening;
it does not replace the separately queued stronger worker boundary.

The warm driver build took 20 seconds (metadata/configuration only). The driver
then ran outside the Nix daemon with disk-backed cwd/TMPDIR, no host display or
audio backend and a five-second 8GiB `/tmp` guard. The pinned driver actually
prioritized inherited `XDG_RUNTIME_DIR`: its six machine state directories used
`/run/user/1000` (tmpfs), occupying 197,558,272 allocated bytes after the run.
Final log inspection caught and corrected the initial disk-state assumption. All
six guests stopped; only their six owned state directories were then removed.
Future runs must set both `XDG_RUNTIME_DIR` and `TMPDIR` to an owned short disk
path before startup, avoiding generic host runtime state names. The first launch
was rejected before boot because its output directory did not exist; that
diagnostic is retained, and the complete retry passed. No host devices, public
Mumble server, production key, publication or global/daemon configuration change
was used.

See [the structured evidence](reviews/2026-11-12-server-rtc-packages.json). This
proof freezes the tested package/driver paths and source revision; later unrelated
source changes are not silently covered. Cross/fallback migration, source-filter
cleanup and other platform/runtime validation remain in TODO.
