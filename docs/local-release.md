# Publish a release with a local signing key

After `direnv allow`, run:

```sh
just release
```

A terminal confirmation defaults to **No**. It shows the GitHub repository,
immutable app tag and commit, public hybrid key ID, local private-key path and
output directory. Tab selects Yes; Enter confirms. This needs an interactive
terminal. It never generates a key: the enrolled private pair must already be at
`~/.local/share/nuschl-release-keys/update-v2.secret.json`.

The command infers the version from committed `Cargo.toml` and the repository
from the single `origin` URL; additional remotes such as contributor forks are
ignored. The checkout must be clean. An existing `v{version}` tag selects the
application, even when this publisher checkout is newer. Tags are never moved.
For a new version without a tag, the confirmation proposes a local tag at clean
committed HEAD; it creates that tag only after Yes, without pushing it.

The command builds the exact tag's signer and products, then runs the audited
preparation, hybrid signing and verification checks below. It uploads nothing.
It prints a unique private temporary directory under
`${XDG_CACHE_HOME:-~/.cache}/nuschl-release/`. This directory **remains after the
command exits**: test the `stage/install/windows/` and `stage/install/Nuschl.app/`
trees, archives in `stage/dist/`, and Linux Nix references in the receipt there.
Build logs and failed attempts remain available too. Use a disk-backed cache,
not `/tmp` or another tmpfs.

When those exact artifacts are ready:

```sh
just release --publish
```

This selects the last **successfully signed** stage recorded for this repository
and tag. Failed or cancelled preparation never replaces that record. It checks
the remembered ledger/manifest/signature/checksum identity and reruns the backend's
source, Nix-product, artifact and hybrid-signature verification before its one
publication confirmation. It neither rebuilds nor signs again and needs no
private key. After Yes, only that exact tag is pushed without force; an identical
remote tag is unchanged and a different remote tag is refused. The backend then
uploads a draft and verifies every uploaded digest before making it public. A changed,
expired, missing, unrelated or already-published stage is refused; no fallback
stage is guessed. Publication still needs `gh` authentication with the repository
permissions described below. Preserve failed drafts for explicit inspection.

Both commands use the existing local publisher below; the manual commands are
available for inspecting individual steps or supplying optional native Mac proof.
The experimental Mac release works entirely locally with the mandatory ad-hoc
integrity checks and explicitly records `native_macos_verified=false` when native
proof is absent. It does not require Apple Developer ID or notarisation.

The long-lived Ed25519 + ML-DSA-44 private pair stays on the operator's machine.
GitHub builds are optional, read-only verification; no signing secret belongs in
Actions. The tag workflow builds artifacts but cannot sign or publish them.
The existing `v0.1.0` tag names `ab843f7a213e9673078a120930019258d9760ce3`;
do not move or recreate it to obtain newer publisher tooling. That immutable
tag retains its historical hosted-publication workflow; its run was canceled.
Do not rerun that old publishing job. The read-only workflow change applies to
newer tooling/source, while the independent build workflow can verify the tag.

Use a checkout containing `release/local-release.py` (the publisher tool can be
newer than the application tag). The helper records that tool's hash separately
from the immutable tagged application source. It exports committed source only;
uncommitted checkout contents never become release inputs. It neither generates
nor enrolls a key, and never reads a private key except during an explicit `sign`
command supplied by the operator.

## Prepare local Nix products

Run on x86_64 Linux with the pinned Nix environment, Git and Python 3.12+. Use an
owned disk-backed cache; do not put a multi-gigabyte release in RAM-backed `/tmp`.
The examples assume the repository is `~/Projects/nuschel`; adjust that path.

```sh
cd ~/Projects/nuschel
export TMPDIR="$HOME/.cache/nuschl-local-release/tmp"
mkdir -p "$TMPDIR"
stage="$HOME/.cache/nuschl-local-release/v0.1.0-review"
python3 release/local-release.py prepare \
  --source-repo "$PWD" --tag v0.1.0 --repository ambiso/nuschl \
  --output "$stage" --build
```

The output directory must not already exist. Omit `--build` to use already-built
local products and fail if any exact-tag product is absent. `--build` selects the
actual tag's `windows`, `macos-app`, `default`, `server`, `web-relay` and `web-site`
outputs sequentially with `--cores 2 --max-jobs 2`. The guard stops only its own
build if `/tmp` free space or `MemAvailable` drops below 8 GiB. Client `TMPDIR`
does not relocate daemon scratch; monitor/coordinate substantial builds.

Preparation records each actual `.drv`, output store path and NAR hash, with
`local-nix` provenance. It does not invent a CI run ID. The helper copies all nine
Windows and macOS programs, complete scoped notices, static web files and exact
Linux Nix references; Linux is not a portable native tarball. Windows/Mac archives
are canonical checked copies. Original notice bytes and the 256-file updater
limit remain enforced. Both entry points must exist in the static site.

Review `ledger.json`, `dist/release-ledger.json`, `dist/update.json`, the archives
and any build logs. The public ledger omits the local repository path; local-only
`local.json` holds it. The signed manifest binds the public ledger's digest.
Keep staging exclusive to this operation: do not edit it or run two helpers on
it concurrently. Revalidation catches changed source, products, payloads and tags;
it is not a defense against a compromised operator account, Nix daemon or OS.

## Optional native Apple static verification

Linux cross compilation is not native Apple verification. The experimental
release can be prepared, signed and published entirely on Linux. Mandatory local
checks reuse the bounded `tests/cross_macho.py` ad-hoc integrity inspector for both
slices of all nine programs, their code pages, bundle Info.plist/CodeResources
special slots and every resource hash. Unknown formats fail closed. These are
local structural checks, not Apple acceptance.

`dist/macos-verification.json` records `native_macos_verified=false` by default;
its exact digest and status are bound by the hybrid-signed manifest notes. The
Mac download is experimental and not natively tested. Developer ID/notarisation
was explicitly deferred; our hybrid signature authenticates the release without
claiming an Apple-approved publisher or Gatekeeper acceptance. **Skip to Sign
locally to use the all-local release path.**

Either optional path below adds native static evidence for the exact prepared
Mac tree and records `native_macos_verified=true`. Neither path executes the app
or proves Seatbelt/audio/updater runtime behavior,
adds Developer ID/notarisation, or claims Gatekeeper publisher acceptance.

**An owned native Mac:** copy the prepared `source/`, `install/Nuschl.app/`,
`macos-install.tar` and `ledger.json` to the same relative layout on the Mac.
Copy/use this newer publisher checkout there too, then run:

```sh
python3 release/local-release.py verify-macos /path/to/v0.1.0-review
```

This invokes only fixed `/usr/bin/codesign`, `/usr/bin/lipo` and `/usr/bin/otool`.
It checks strict/deep/all-architecture signatures, both slices and every program,
minimum macOS 14.2, identifiers and portable dylib imports. Copy the resulting
`macos-native/` directory back into the original Linux stage. This is an
operator-controlled local receipt, not a cryptographic remote attestation. Linux
cannot mint a successful native receipt with this command.

**Optional key-free CI:** a successful same-commit Platform builds or tag-build
run may supply the existing native verification. It must contain the successful
`macos-native-verify` job and exact named Mac/report artifacts. Run with a GitHub
CLI authorized to read that repository's Actions artifacts:

```sh
python3 release/local-release.py import-ci-macos "$stage" --run-id RUN_ID
```

The helper queries the real run/job/artifact identities, downloads the native
report and original artifact, validates the original same-run metadata/notices,
and compares every file to the locally selected Mac product. A self-asserted JSON
report is insufficient for this channel. Publication fetches and checks this CI
evidence again. Expired, unavailable or invalid optional artifacts are rejected; do not edit a
receipt to manufacture success. No signing key enters CI. Omitting native
evidence keeps the honest experimental/not-natively-verified status.

## Sign locally, then review

Build or select the explicit signer from the reviewed publisher checkout. The
operator supplies the existing private path; do not paste its content into a
command, log, issue, GitHub secret or chat.

```sh
nix build --cores 2 --max-jobs 2 .#update-signer -o result-update-signer
python3 release/local-release.py sign "$stage" \
  --signer "$PWD/result-update-signer/bin/nuschl-update-sign" \
  --sign-key "$HOME/.local/share/nuschl-release-keys/update-v2.secret.json"
```

The embedded *tagged* public pair must match the supplied private pair. The helper
reselects the tag's derivations/NAR receipts, compares installation and archive
bytes to those immutable products, checks local ad-hoc integrity, all notice
scopes and any supplied optional native proof,
then uses the existing signer to sign and verify both algorithms over exact raw
v2 bytes. It writes canonical `SHA256SUMS` only after successful verification. The hybrid
signature covers `update.json`; its ledger digest binds the public ledger and all
archive/static-web hashes, and its Mac-verification digest binds the explicit
native-status report. These authenticate download bytes without an OS publisher
certificate. A checksum file alone is not a substitute for verifying those bindings.
Do not regenerate the key or replace tagged trust to get past a mismatch.
The prepared metadata retains its ordinary maximum 30-day expiry; if it expires,
prepare a fresh review rather than bypassing verification. Later unchanged-binary
renewal follows [the separate procedure](update-metadata-refresh.md).

## Publish already-signed assets

This final command makes remote changes. It requires operator authorization and
a GitHub CLI login/token with repository Contents write. GitHub may additionally
require Workflows write when the resolved tag changes workflow files relative to
the default branch; the publisher does not broaden token permissions itself.
A 403 is an authentication/authorization problem, not a reason to bypass checks.

```sh
python3 release/local-release.py publish "$stage" \
  --signer "$PWD/result-update-signer/bin/nuschl-update-sign"
```

The remote tag must already exist and match the prepared commit (`--verify-tag`);
no tag creation, movement, `latest` substitution, asset clobber or existing-release
replacement is allowed. A draft receives only the pre-signed files. Every uploaded
name, size and GitHub-reported SHA-256 must match before the draft becomes public.
Failures leave an unpublished draft for explicit operator inspection; automatic
retry/resume/deletion is deliberately absent. Never turn a partial draft public
manually. Check the failure, preserve the review directory, then reconcile the
draft explicitly before a new attempt.

Local tests use temporary keys and synthetic disk-only products:

```sh
python3 tests/local_release.py --signer "$PWD/result-update-signer/bin/nuschl-update-sign"
python3 tests/update_release.py --signer "$PWD/result-update-signer/bin/nuschl-update-sign"
```

These fixtures test orchestration and tamper/refusal behavior; an optional
`--macos-app /path/to/existing/Nuschl.app` additionally exercises the local
inspector on produced public bytes. Neither is native Apple acceptance or a
production publication. Record the actual local-integrity result, optional
native proof and final remote result separately.
