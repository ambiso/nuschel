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

The live media worker owns CPAL streams, per-speaker Opus decoders, mixing, resampling,
and optional RNNoise. AEC route detection and automatic DSP selection remain subsequent
work; unknown upstream effects must not be treated as absent. Sonora is not active yet.

str0m is confined to `nuschel-rtc`, with examples and native WebRTC bindings disabled.
Its `rust-crypto` feature selects the RustCrypto SRTP/hash provider, but its dimpl 0.9
DTLS dependency also enables AWS-LC through the `rcgen` feature. dimpl's default
provider prefers AWS-LC when both backends are enabled. Consequently DTLS and
certificate generation still use native crypto under the existing crypto exception;
this is not a fully memory-safe dependency graph. No OpenSSL or C++/Go WebRTC engine
is introduced. Review the actual Cargo feature graph on updates rather than relying
on a feature's name. The trial accepts bounded signaling/datagrams and encoded media;
it must never load a codec or open a microphone in the network process.

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
