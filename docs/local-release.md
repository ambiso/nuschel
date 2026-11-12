# Publish a release with a local signing key

The long-lived Ed25519 + ML-DSA-44 private pair stays on the operator's machine.
GitHub builds are optional, read-only verification; no signing secret belongs in
Actions. The tag workflow builds artifacts but cannot sign or publish them.
The existing `v0.1.0` tag names `ab843f7a213e9673078a120930019258d9760ce3`;
do not move or recreate it to obtain newer publisher tooling.

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

## Obtain native Apple static verification

Linux cross compilation is not native Apple verification. Signing/publication
remain blocked until one of these two paths checks the *exact* prepared Mac tree.
Neither path executes the app, proves Seatbelt/audio/updater runtime behavior,
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
evidence again. Expired or unavailable artifacts fail closed; use a native Mac or
a fresh appropriate build rather than editing a receipt. No signing key enters CI.

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
bytes to those immutable products, checks native proof and all notice scopes,
then uses the existing signer to sign and verify both algorithms over exact raw
v2 bytes. It writes canonical `SHA256SUMS` only after successful verification.
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

These fixtures test orchestration and tamper/refusal behavior; they are not native
Mac acceptance or a production publication. Record the actual native proof and
final remote result separately.
