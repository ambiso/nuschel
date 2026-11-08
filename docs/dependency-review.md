# Dependency review

The initial dependency graph prefers Rust implementations and a small portable core.
`Cargo.lock` pins the resolved graph. `deny.toml` checks licenses, crate ownership,
registry sources, yanks, vulnerabilities, and unsoundness across all features and
platforms. First-party source forbids unsafe code; dependencies require separate review.

## Processing choices

| Component | Initial choice | Review boundary |
| --- | --- | --- |
| Mumble protobuf | Prost, generated from vendored schemas | Framing limits precede decoding; full sessions are not implemented |
| Content | pulldown-cmark and Syntect with fancy-regex | No native Oniguruma; fixed bundled syntax/theme set; content worker and timeout |
| Echo cancellation | Sonora | Rust port of WebRTC processing; assess quality/timing before routing real calls |
| Noise suppression | nnnoiseless | Rust RNNoise-derived implementation; enable only when route policy calls for suppression |
| Opus | opus-pure | Rust implementation with unsafe SIMD; media worker; conformance and stock-client audio tests required |
| History | SQLx/Tokio, bundled SQLite locally and PostgreSQL for servers | SQLite FFI and database access remain in the storage worker; extension loading/deserialization are disabled |
| Desktop | GPUI Kit without default component/assets features | Native rendering exception, not a message HTML or embedded-browser runtime |
| TLS | rustls with ring | Explicit crypto exception; no OpenSSL dependency |

Sonora and nnnoiseless are dependencies for the next audio pipeline milestone. The
current media harness exercises raw Opus encode/decode, not live AEC, noise suppression,
or device routing. Selection does not certify these ports for production audio.

## Unmaintained transitive dependencies

The following are unmaintained advisories, not vulnerability or unsoundness waivers.
They cannot be removed merely by updating Nuschel's direct dependencies at these
versions. The allowlist names each advisory explicitly and explains its owner. New
advisories against the same packages are still checked.

| Advisory | Dependency and reason to retain temporarily |
| --- | --- |
| [RUSTSEC-2025-0141](https://rustsec.org/advisories/RUSTSEC-2025-0141) | Syntect 5.3 uses bincode 1.3.3 for trusted bundled syntax/theme dumps; Nuschel accepts no external dumps |
| [RUSTSEC-2024-0384](https://rustsec.org/advisories/RUSTSEC-2024-0384) | gpui-base 0.9 uses instant; wait for its web-time migration |
| [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436) | paste is a transitive compile-time macro dependency of the selected native/DSP stack |
| [RUSTSEC-2026-0206](https://rustsec.org/advisories/RUSTSEC-2026-0206) | GPUI's usvg stack uses rustybuzz; Nuschel's message view accepts neither SVG nor remote fonts |
| [RUSTSEC-2026-0192](https://rustsec.org/advisories/RUSTSEC-2026-0192) | GPUI's font stack uses ttf-parser; replace through an upstream font-stack update |
| [RUSTSEC-2026-0371](https://rustsec.org/advisories/RUSTSEC-2026-0371) | GPUI/accessibility dependencies use zbus_names from the zbus 5 family |
| [RUSTSEC-2026-0370](https://rustsec.org/advisories/RUSTSEC-2026-0370) | The same stack uses zvariant; functionality moved into zbus 6 |
| [RUSTSEC-2026-0372](https://rustsec.org/advisories/RUSTSEC-2026-0372) | zvariant_derive moves with that coordinated zbus 6 migration |

Revisit the corresponding entries whenever GPUI Kit or Syntect is upgraded. Do not
add a workspace-direct dependency on these packages to exploit a transitive allowance.
The release process also needs a third-party notice inventory; cargo-deny alone does
not establish fulfillment of attribution or copyleft conditions.
