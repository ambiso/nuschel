# openmls_libcrux_crypto 0.4.0 (vendored from crates.io, MIT)

OpenMLS 0.9.2's libcrux crypto provider. It depends on `libcrux-hmac-drbg 0.0.1`
(its random number generator), which panics on some output lengths
(RUSTSEC-2026-0329), and on hpke-rs-libcrux 0.7.0, which pins the vulnerable
libcrux-kem 0.0.9 (patched in `vendor/hpke-rs-libcrux`).

**The patch** changes only `Cargo.toml`: `libcrux-hmac-drbg = "0.0.2"` with its
`sys_rng` feature, where 0.0.2 moved `HmacDrbgSha256::new_from_sys_rng` (the only
constructor this crate uses). 0.0.2 accepts an empty request instead of refusing it
and renames one error variant this crate never names; the generator's output is
unchanged. No source file changed.

Wired through `[patch.crates-io]` in the workspace `Cargo.toml`. cargo-deny still
confines this crate to `openmls` and `nuschel-e2ee` (deny.toml).

**Remove** this copy (and the `[patch.crates-io]` and `exclude` entries) with the
OpenMLS release that carries the fixed libcrux crates; see TODO.md and the bump steps
in docs/e2ee.md.
