# Hybrid update verification and signing

The updater accepts **Ed25519 AND ML-DSA-44**, using RustCrypto
`ed25519-dalek` 2.2.0 and `ml-dsa` 0.1.2. Each fixed verifier executable is
built with only its algorithm feature. The HTTP/UI updater parent does not link
these update-signature engines. No identity/MLS provider has changed.

`nuschl-update-verify-ed25519` and `nuschl-update-verify-mldsa44` receive one
bounded binary request through stdin, verify their own public component/signature
against the identical pair-bound transcript, and exit. The parent requires both
successful exits and exact request-bound replies. It discovers fixed filenames
beside the application; neither the manifest nor an adjacent key file chooses a
worker or trust root. There is no minisign fallback. Production accepts only
`crates/update/update-trust.json` compiled into the program; that file deliberately
has **no enrolled keys**. No production secret or CI enrollment was created.

The operator tool supports `keygen`, `public-key`, `key-status`, `sign`, and
`verify`. It is an explicit offline/CI tool, excluded from client payloads. Its
combined verification is a publication check, not the runtime's process boundary.

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
and enroll that public-only file as `crates/update/update-trust.json` after reviewing the
final integrated migration. That file deliberately contains no enrolled keys
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
uses separate capability-limited algorithm processes rather than a new UI
crypto exception. Independent process results reduce a single verifier memory
corruption's authority; shared dependencies, OS, build pipeline and trusted
parent remain common-mode limits and are not made independent by this format.


## Signing a prepared manifest

```sh
nix run --cores 2 --max-jobs 2 .#update-signer -- sign \
  --secret-file /private/path/update-v2.secret.json \
  --public-file crates/update/update-trust.json \
  --manifest dist/update.json --signature-file dist/update.json.sig
nix run --cores 2 --max-jobs 2 .#update-signer -- verify \
  --public-file crates/update/update-trust.json \
  --manifest dist/update.json --signature-file dist/update.json.sig
```

`sign` refuses an existing output and requires the private pair to match an
explicitly enrolled public pair. It signs exact bounded raw bytes, including
whitespace. `verify` also checks strict manifest v2 semantics and expiry. The
release helper uses both operations; it never generates/enrolls a key. The secret
file must be in an operator-controlled directory; on Windows protect that
directory with appropriate ACLs (Unix mode 0600 is not a Windows ACL policy).

Manifests require `format: 2`, `algorithm: "ed25519+mldsa44"`, `released`,
`expires`, semantic version and bounded platform file lists. Recursive duplicate
JSON keys, including escaped aliases and duplicate platform map entries, unknown
fields, noncanonical signature envelopes, unsafe/case-colliding paths and
credential-bearing URLs fail closed. Each platform has at most 256 files of at
most 512 MiB; the raw manifest is at most 256 KiB. File URLs require HTTPS, except
literal-loopback HTTP for private fixtures. Signed release time/version cannot
move backwards in persisted update state. Expiry is rechecked before download
and before installation.

Validity is at most 30 days, with at most five minutes of future clock skew.
Operators must refresh/re-sign the latest metadata before expiry even when
binaries have not changed, using a nondecreasing version and release time. The
current tag-create workflow does not implement that refresh operation or an
automatic signing cron. Expired metadata produces an actionable check-for-current-
release error; a wrong clock can also reject an otherwise valid release. An
unseen withheld newer release remains undetectable; expiry limits how long an
old manifest can remain acceptable.

## Confinement and actual proof

Linux uses a distinct one-shot bubblewrap namespace per engine, no network or
home/data directory, read-only runtime libraries/worker, empty environment,
256 MiB address-space limit, five CPU seconds and a six-second parent wall-clock
deadline. macOS uses the existing deny-default content Seatbelt profile with the
same parent deadline; this change does not establish a native CPU/memory limit
there. Windows uses distinct LPAC profiles (`nuschl.update.ed25519` and
`nuschl.update.mldsa44`), no capabilities/files, 256 MiB/five CPU seconds, inherited
standard pipes only, and kill-on-close parent/job containment. Windows/macOS
package discovery includes both workers; **new hybrid Windows LPAC and macOS
Seatbelt runtime tests have not been run**. Cross compilation is not that proof.

Only each engine's own signature is sent to it. The transcript necessarily
contains the full **public** pair, not the other signature or any private seed.
A compromised verifier can fabricate its own reply; it cannot substitute for the
other verifier's success. The parent, SHA-256/transcript code, OS/kernel, compiler,
shared dependency defects and a CI signer holding both seeds remain common
failure boundaries. This is not a claim of protection against two compromised
engines or a compromised application installation.

Native focused tests cover framing/strict parsing/expiry/rollback/installation
and both crypto engines. The disk-only `nuschl-update-runtime-fixture` executes
both real independently built Linux workers with temporary keys, rejects either
corrupt half/raw-byte changes/wrong pair/legacy/v1/expired/duplicate input, proves
mandatory AND despite one forged reply, probes private file/write/host-network
confinement, enforces output/deadline/reaping, and completes a private loopback
HTTP download/install/cleanup. It is available only with the `runtime-fixture`
feature and is not distributed with the signer or clients.

```sh
nix build .#update-verifier-ed25519 .#update-verifier-mldsa44 \
  .#update-runtime-fixture .#checks.x86_64-linux.update-unit \
  .#checks.x86_64-linux.update-crypto --cores 2 --max-jobs 2
# Run the fixture outside the Nix build sandbox with disk TMPDIR and these
# output paths: nuschl-update-runtime-fixture ED_VERIFIER ML_VERIFIER PYTHON
python3 tests/update_release.py --signer /nix/store/…/bin/nuschl-update-sign
```

The Python tests use temporary fixture keys and real signing/verification CLI,
plus synthetic tar/metadata/notices to test release-gate mechanics. They do not
establish legal completeness or run GitHub publication. The Windows updater
harness now requires a separately built fixture tree embedding its temporary
public pair, with explicit fixture secret/public inputs; copying a public key
beside a production executable cannot authorize an update.

Publication reuses independent SHA-named build artifacts and checks repository,
commit, run ID, platform, version, hashes, safe tar members and mandatory worker
presence before accessing a signing secret. Earlier successful attempts from the
same run/commit are allowed; future/foreign attempts fail. Missing/incomplete
bundled runtime notices also block signing. **That attribution packaging gate is
not complete**: upstream/native/asset notice recovery and compact payloads that
respect the 256-file installation bound remain queued. No release is ready merely
because hybrid signatures work. No tag/push/dispatch/deploy was performed.
