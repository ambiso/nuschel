# RustCrypto DTLS certificate/backend correction

Based on `str0m-rust-crypto` **0.7.0**, upstream str0m commit
`5f2183ce7c67117cd1960dfeb11bed11fc188b66`, `crypto/rust-crypto`.
The registry source is otherwise unchanged. `Cargo.toml.orig` records the
original upstream manifest; the crate-local lockfile/cache marker are omitted.

Upstream requests dimpl's `rust-crypto` **and** `rcgen` features. In dimpl 0.9.0,
`rcgen` forces `aws-lc-rs`, and certificate generation/fingerprinting use AWS-LC.
Additionally, this adapter called dimpl's default config builder: when both crypto
features are enabled, that default selects AWS-LC rather than RustCrypto. This
means the nominal RustCrypto adapter's DTLS operations were also using AWS-LC.

The patch removes the dimpl `rcgen` feature, uses rcgen's public external-signing
interface with the existing RustCrypto P-256 implementation, and explicitly selects
dimpl's RustCrypto provider. The explicit selection remains necessary even after
removing the feature, because unrelated workspace dependencies can unify Cargo
features and reenable AWS-LC. The adapter must not silently change cryptographic
implementations in that case.

Fresh keys use fallible OS entropy. Certificates retain ECDSA P-256/SHA-256,
PKCS#8 private keys, an end-entity self-signed subject and one-year validity.
Positive, bounded serials derive from fresh random public keys and remain unique
across processes. Signatures are encoded as ASN.1 DER, as X.509 requires. Unit
tests parse the certificates independently, match the private/public key, verify
the signature, reject altered signed bytes, and check distinct serials/keys.

No handshake, certificate/fingerprint verification, cipher policy or SRTP replay
protection is relaxed. No new unsafe code is introduced; this adapter now forbids
it. RustCrypto's existing low-level acceleration/OS-entropy dependencies remain
subject to the RTC worker's reviewed boundary. This is not a claim that all
transitive dependencies contain no unsafe code.

Remove this patch when an upstream release generates RustCrypto certificates
without forcing AWS-LC and explicitly selects its DTLS provider. Run real
bidirectional DTLS/SRTP transport, altered-fingerprint, tamper and replay tests
before accepting an upstream replacement.
