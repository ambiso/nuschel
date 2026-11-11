# Dependency review

The initial dependency graph prefers Rust implementations and a small portable core.
`Cargo.lock` pins the resolved graph. `deny.toml` checks licenses, crate ownership,
registry sources, yanks, vulnerabilities, and unsoundness across all features and
platforms. First-party source forbids unsafe code; dependencies require separate review.

## Processing choices

| Component | Initial choice | Review boundary |
| --- | --- | --- |
| Mumble protobuf | Prost, generated from vendored schemas | Framing limits precede decoding; native sessions use bounded Tokio queues and TLS |
| Content | pulldown-cmark and Syntect with fancy-regex | No native Oniguruma; fixed bundled syntax/theme set; content worker and timeout |
| Echo cancellation | Sonora | Rust port of WebRTC processing; assess quality/timing before routing real calls |
| Noise suppression | nnnoiseless | Rust RNNoise-derived implementation; enable only when route policy calls for suppression |
| Opus | opus-pure | Rust implementation with unsafe SIMD; media worker; conformance and stock-client audio tests required |
| History | SQLx/Tokio, bundled SQLite locally and PostgreSQL for servers | SQLite FFI and database access remain in the storage worker; extension loading/deserialization are disabled |
| Desktop | GPUI Kit without default component/assets features | Native rendering exception, not a message HTML or embedded-browser runtime |
| TLS | rustls with ring | Explicit crypto exception; no OpenSSL dependency |
| WebRTC trial | str0m 0.25, defaults disabled, `rust-crypto` | Rust protocol core forbids unsafe; codecs stay in workers; DTLS crypto exception described below |
| Windows worker sandbox | Own adapter `nuschl-sandbox-windows` over `windows-sys` | The one first-party crate with unsafe code; crates that wrap AppContainers were reviewed and rejected in docs/windows-sandbox.md |

The live media worker owns CPAL streams, per-speaker Opus decoders, mixing, resampling,
RNNoise and Sonora echo cancellation. It detects upstream effects, selects its own
processing and monitors the result; unknown upstream effects are not treated as
absent. Native Windows/macOS effect detection and validation on real hardware remain
open, with per-device overrides available.

str0m is confined to `nuschl-rtc`, with examples and native WebRTC bindings disabled.
The reviewed `str0m-rust-crypto` adapter now selects dimpl's RustCrypto DTLS provider
explicitly. Its certificate generator signs through rcgen's external-key interface
using P-256, without dimpl's `rcgen` feature. This removed an accidental AWS-LC
provider selection as well as the certificate dependency; the locked native RTC
normal/build graph contains neither AWS-LC nor ring. Two certificate tests and six
actual DTLS/SRTP transport tests passed. This is not a claim that every cryptographic
dependency is free of unsafe code. No OpenSSL or C++/Go WebRTC engine is introduced.
Review the actual Cargo feature graph on updates rather than relying on a feature's
name. The trial accepts bounded signaling/datagrams and encoded media; it must never
load a codec or open a microphone in the network process.

The native session crate uses rustls/tokio-rustls with the explicitly selected ring
provider (the existing crypto exception), WebPKI roots, and rcgen for a persistent
client identity. Certificate pins still require TLS handshake signature verification;
an untrusted leaf stops the handshake before any Mumble password is transmitted.
rcgen's certificate construction is Rust; its signing operations use the reviewed
provider. `sha2` supplies certificate fingerprints within that crypto boundary.
`uuid` uses OS randomness for local bookmark/temporary-file identities; `directories`
uses platform configuration-directory APIs, under the existing OS-service exception.

Legacy HTML normalization uses html5ever and markup5ever_rcdom in the confined content
worker. Their optimized string/DOM internals are not assumed free of unsafe code.
No remote image, font, stylesheet, script or URL is loaded by this path. First-party
code continues to forbid unsafe. The GUI consumes normalized text and style spans.

`vendor/pulseaudio` retains upstream 0.3.2 under MIT with two narrowly documented
authentication fixes. This preserves CPAL's Rust PulseAudio implementation inside
the media worker. See [the patch notes](../vendor/pulseaudio/NUSCHEL-PATCHES.md).
Crane's dependency build preserves that dependency's actual API; it must not replace
it with a dummy library while compiling registry CPAL. Remove the override once the
upstream release includes the fixes. PulseAudio buffers are configured per stream;
native Windows/macOS device-wide buffer settings are left to the OS.

## Unmaintained transitive dependencies

The following are unmaintained advisories, not vulnerability or unsoundness waivers.
They cannot be removed merely by updating Nuschl's direct dependencies at these
versions. The allowlist names each advisory explicitly and explains its owner. New
advisories against the same packages are still checked.

| Advisory | Dependency and reason to retain temporarily |
| --- | --- |
| [RUSTSEC-2025-0141](https://rustsec.org/advisories/RUSTSEC-2025-0141) | Syntect 5.3 uses bincode 1.3.3 for trusted bundled syntax/theme dumps; Nuschl accepts no external dumps |
| [RUSTSEC-2024-0384](https://rustsec.org/advisories/RUSTSEC-2024-0384) | gpui-base 0.9 uses instant; wait for its web-time migration |
| [RUSTSEC-2024-0436](https://rustsec.org/advisories/RUSTSEC-2024-0436) | paste is a transitive compile-time macro dependency of the selected native/DSP stack |
| [RUSTSEC-2026-0206](https://rustsec.org/advisories/RUSTSEC-2026-0206) | GPUI's usvg stack uses rustybuzz; Nuschl's message view accepts neither SVG nor remote fonts |
| [RUSTSEC-2026-0192](https://rustsec.org/advisories/RUSTSEC-2026-0192) | GPUI's font stack uses ttf-parser; replace through an upstream font-stack update |
| [RUSTSEC-2026-0371](https://rustsec.org/advisories/RUSTSEC-2026-0371) | GPUI/accessibility dependencies use zbus_names from the zbus 5 family |
| [RUSTSEC-2026-0370](https://rustsec.org/advisories/RUSTSEC-2026-0370) | The same stack uses zvariant; functionality moved into zbus 6 |
| [RUSTSEC-2026-0372](https://rustsec.org/advisories/RUSTSEC-2026-0372) | zvariant_derive moves with that coordinated zbus 6 migration |
| [RUSTSEC-2026-0459](https://rustsec.org/advisories/RUSTSEC-2026-0459) | SQLite before 3.54 can return stale memory for a crafted database plus WAL; the storage worker opens only its own profile database. sqlx 0.9 caps libsqlite3-sys below the fixed 0.39: upgrade with the next sqlx release |

Revisit the corresponding entries whenever GPUI Kit or Syntect is upgraded. Do not
add a workspace-direct dependency on these packages to exploit a transitive allowance.
The release process also needs a third-party notice inventory; cargo-deny alone does
not establish fulfillment of attribution or copyleft conditions.

## Audit scope

The Nix `audit` check uses the advisory database pinned by `flake.lock` and the
same individually reviewed advisory exceptions as `deny.toml`. That includes the
quick-xml 0.38 citation-parser exceptions: the sandboxed math worker removes
bibliographies, citations and arbitrary Typst code from its accepted language.
Upgrade Typst's citation stack when it accepts quick-xml 0.41 or later.

`cargo audit` also checks optional dependencies retained in `Cargo.lock` that no
workspace target selects. Its additional exception for
[RUSTSEC-2026-0387](https://rustsec.org/advisories/RUSTSEC-2026-0387.html) applies to
crypto-bigint 0.5.5 in OpenMLS's unselected RustCrypto/test-utility provider. The
selected libcrux provider does not use it; the reachable RustCrypto stack uses
crypto-bigint 0.7.10, newer than the fixed 0.7.6. Verify this on updates with
`cargo tree --workspace --all-features --target all -i crypto-bigint@0.5.5`.
This advisory is deliberately **not** ignored in `deny.toml`: if that older
provider becomes reachable, `cargo deny check advisories` must fail.
