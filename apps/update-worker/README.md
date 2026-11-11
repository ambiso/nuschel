# Hybrid update signing (implementation in progress)

The explicit operator tool implements key generation, public-only export and
secret/public matching. It uses RustCrypto `ml-dsa` 0.1.2 for ML-DSA-44 and
`ed25519-dalek` 2.2.0 for Ed25519. The `ed25519` crate supplies signature types and
traits; it is not itself the Ed25519 implementation. No identity/MLS provider has
been changed. The current tool is a separate process; its asymmetric engines are
not dependencies of the UI/updater parent.

This checkpoint does **not** finish the update migration. Two independently
confined fixed-algorithm verification subprocesses, the parent's mandatory AND,
strict v2 manifest/expiry handling, CI publication and Windows fixture migration
remain necessary before a hybrid updater release. The old minisign path is not
an acceptable fallback for that release. No production key is enrolled by this
code or by its tests, and no CI secret has been created.

## Concrete operator step (execute only when enrollment is approved)

Use an external private directory, outside the repository. The tool requires
explicit paths and refuses to overwrite existing outputs. On Unix it creates
the secret file with mode 0600. Do not paste private key values into an issue,
chat, command argument or build log.

```sh
umask 077
mkdir -p "$HOME/.local/share/nuschl-release-keys"
nix run --cores 2 --max-jobs 2 .#update-signer -- keygen \
  --secret-file "$HOME/.local/share/nuschl-release-keys/update-v2.secret.json" \
  --public-file "$HOME/.local/share/nuschl-release-keys/update-v2.public.json"
nix run --cores 2 --max-jobs 2 .#update-signer -- key-status \
  --secret-file "$HOME/.local/share/nuschl-release-keys/update-v2.secret.json" \
  --public-file "$HOME/.local/share/nuschl-release-keys/update-v2.public.json"
```

Both commands print only the public algorithm/version and stable key-pair ID.
The public file contains the complete public pair and no seed fields. Review
and enroll that public-only file as `crates/update/update-trust.json` once the
runtime migration is complete. That file deliberately contains no enrolled keys
yet. Never enroll the private file. Root-key rotation
will require explicitly pinned pair sets and an overlap release, not a public
key supplied by an untrusted download.

To recover/export the public information from an existing private file, choose
a new public output path:

```sh
nix run --cores 2 --max-jobs 2 .#update-signer -- public-key \
  --secret-file "$HOME/.local/share/nuschl-release-keys/update-v2.secret.json" \
  --public-file "$HOME/.local/share/nuschl-release-keys/export.public.json"
```

If public-file creation fails after a valid new secret has been saved, the
secret is retained; use `public-key` to recover its public half. The tool never
silently regenerates or removes a saved key.

The new repository Actions secret name is `UPDATE_HYBRID_SIGNING_KEY`. Its exact
value is the complete UTF-8 private JSON file, including both independent 32-byte
seeds as lowercase 64-character hex strings. It is a different format and secret
name from `UPDATE_SIGNING_KEY` (minisign). Provision it through an authorized
GitHub secret UI/credential flow after matching it to the enrolled public file;
the tool does not read/write GitHub secrets or print the file's private values.
CI must refuse publication when the secret is absent or its derived public pair
does not match the built-in trust store. A CI host holding both seeds remains a
common authority; hybrid signatures do not protect against compromise of that
host or deliberate misuse of both private keys.

## Format and provider review

The public pair is Ed25519 public 32 bytes plus FIPS 204 ML-DSA-44 public 1312 bytes.
The signature packet has fixed magic/version/algorithm, a 32-byte key-pair ID,
the 64-byte Ed25519 signature and 2420-byte ML-DSA-44 signature. No optional half,
alternate algorithm, trailing data or legacy encoding is accepted. Both
algorithms sign identical application-domain/version/full-pair-bound bytes and
the exact raw manifest. JSON re-emission is never used to build signed bytes.
ML-DSA additionally uses a fixed FIPS 204 context. This is application framing of
standard signatures, not a claimed new cryptographic primitive or RFC composite
algorithm implementation.

Dalek verification is strict and adds explicit canonical, prime-order/non-small
point and canonical scalar checks. These checks remain necessary even when AKD
unifies dalek's `legacy_compatibility` elsewhere in a workspace build. RustCrypto
ML-DSA 0.1.2 is newer than the patched prereleases for the disclosed
[signing decomposition timing issue](https://github.com/advisories/GHSA-hcp2-x6j4-29j7)
and [duplicate signature hint issue](https://github.com/RustCrypto/signatures/security/advisories/GHSA-5x2r-hc65-25f9).
The crate's own documentation says it has not been independently audited; using
[FIPS 204's algorithm](https://csrc.nist.gov/pubs/fips/204/final) is not FIPS module
validation. Run the pinned advisory/license/ownership checks before release.

The first-party code forbids unsafe. `hybrid-array`, curve/hash backends and
other transitive code can execute unsafe; this is why production verification
will use separate capability-limited algorithm processes rather than a new UI
crypto exception. Independent process results reduce a single verifier memory
corruption's authority; shared dependencies, OS, build pipeline and trusted
parent remain common-mode limits and are not made independent by this format.
