# hpke-rs-libcrux 0.7.0 (vendored from crates.io, MPL-2.0)

OpenMLS 0.9.2's libcrux provider (openmls_libcrux_crypto 0.4.0) uses this crate for
HPKE, and it pins `libcrux-kem = "=0.0.9"`. libcrux-kem 0.0.9 panics when it decodes
an X-Wing public key, ciphertext or seed of the wrong length (RUSTSEC-2026-0330,
RUSTSEC-2026-0331): a peer's key package with a short X-Wing init key crashed whoever
encapsulated to it.

**The patch** changes only `Cargo.toml`: `libcrux-kem = "=0.0.10"`, which decodes with
length checks and returns errors instead. Its API is the same for this crate (the RNG
bound moved from `CryptoRng` to `TryCryptoRng`, which every `CryptoRng` implements);
the X-Wing key derivation and encodings are unchanged, so keys and groups stay the
same. The benchmarks and their dev-dependency were removed. No source file changed.

Wired through `[patch.crates-io]` in the workspace `Cargo.toml`, together with
`vendor/openmls_libcrux_crypto`. nuschel-e2ee still checks every key package and tree
structurally before OpenMLS sees it and runs OpenMLS under `catch_unwind`
(docs/e2ee.md "Phase 4 prototype").

**Remove** this copy (and the `[patch.crates-io]` and `exclude` entries) with the
OpenMLS release whose libcrux provider uses hpke-rs-libcrux 0.8 (OpenMLS `main` already
does); see TODO.md and the bump steps in docs/e2ee.md.
