# First alpha release readiness

Source audit: 2026-11-11, baseline `15a733bf`. This is a release checklist,
not a new platform test result. The original baseline used minisign; the current hybrid runtime is described in
[the update worker README](../apps/update-worker/README.md), with Linux actual-worker
proof and explicit remaining platform/enrollment gates. Recheck the final integrated
commit and record the actual CI run before publishing.

The chosen first release is the normal `0.1.0` release, described as an early
preview. It is not a separate prerelease update channel. No tag, release or CI
dispatch was created by this audit.

## Smallest coherent scope

An explicitly experimental desktop alpha can contain the Windows x86_64 portable
zip and the macOS Intel/Apple-silicon universal app. Linux users install the tagged
Nix flake; there is no portable Linux archive, AppImage, Flatpak, or distro package.
The Linux package also contains the terminal client and server. The cross desktop
packages currently contain the desktop and its helpers, not a Windows/macOS server
or terminal distribution. Web assets and relay can build in CI without claiming
browser voice/video or a finished mobile client.

The user deferred Windows Authenticode and macOS Developer ID/notarisation. That
does not waive update authentication. Windows downloads remain unsigned, macOS
bundles retain their existing ad-hoc signature, and release notes must describe
the documented OS first-start restrictions. Neither cross compilation nor an
ad-hoc signature establishes native runtime support. Windows has private Windows
11 VM evidence; macOS has no native runtime proof, including Seatbelt, microphone
permissions, playback and self-update. Mark the Mac artifact untested/experimental.

## Gates before publication

- [ ] Merge and validate the hybrid updater, its two confined verifier executables,
  CI signer and tests. Include both verifiers in every desktop platform package;
  exclude the signing tool from client installations. Do not publish with the old
  minisign-only path or turn verification off to get an artifact.
- [ ] Enrol the real hybrid public key pair in source, retain protected backups,
  and provision the matching CI signing secret using the reviewed tool. No key
  material or secret availability was verified by this audit. The implementation
  owner tool now specifies the private v2 JSON format and
  `UPDATE_HYBRID_SIGNING_KEY`; `crates/update/update-trust.json` deliberately has no
  enrolled public pair. No legacy-key fallback remains.
- [x] Choose `0.1.0` as the normal early-preview version. The eventual tag must be
  `v0.1.0`, matching the workspace and package versions; this decision has not
  created that tag.
- [ ] Run the reusable build workflow at the intended commit and retain all three
  independent platform artifacts and logs. No successful GitHub build is claimed
  by this source review. A failed cross job must not hide another platform's
  downloadable CI artifact; publication must require all intended jobs to pass.
- [ ] Finish the existing dependency attribution/notices release gate. Linux's
  `packageLayout` includes several vendored/font licences, while baseline Windows
  and Mac packaging copies only EUPL-1.2. The deployable web tar must include the
  full runtime inventory at `licenses/nuschl/THIRD-PARTY.json`, including actual
  client/audio/content WASM and JS/assets, beyond its existing Temml notices. Passing `cargo deny` is not a complete
  attribution inventory. Inspect the exact final archives, including helper files.
  The publisher now rejects missing/incomplete/mismatched bundled inventories;
  this is a gate, not proof the remaining notices were recovered. Use the source
  `release/notice-roots.json` (including both verifier features and all three WASM
  roots). Recover unresolved upstream/native/asset terms, then compact notices
  into a combined payload plus provenance/index: copying a per-crate tree can
  exceed the updater's existing 256-file installation bound. Do not truncate
  notices or silently increase that bound.
- [ ] Run the final integrated formatting, Clippy, dependency/advisory, unit and
  private interoperability/confinement checks. CI package builds do not replace
  private NixOS/Windows VM evidence or native Mac testing. Record remaining native
  accessibility/audio/hardware gaps without calling them tested.
- [ ] Verify the signed manifest with the exact embedded public pair, verify the
  complete installation file sets and hashes, and exercise update installation
  with disposable profiles. Hybrid migration invalidates the old minisign fixture
  as proof for the new signature path.
- [ ] Implement and document an operator metadata refresh procedure before the
  latest signed manifest's 30-day maximum expiry, including unchanged binaries.
  The tag-create workflow is not that refresh path. Keep expiry/clock errors
  actionable; do not add an automatic signing cron or waive expiry.
- [ ] Rerun the migrated updater in the private Windows VM with a separately
  compiled temporary public fixture pair, including both LPAC verifiers. Current
  Linux worker proof does not renew the old minisign Windows regression evidence.
- [ ] Review README/install/security/release notes against the final artifacts:
  early-development status; compatible TLS/Mumble voice; MLS experiments and
  historical-key limits; no completed RTC video/screen-sharing claim; Linux via
  Nix; Windows VM-only and Mac untested status; no OS code-signing claim.
- Deferred for a future prerelease channel: decide how alpha clients find subsequent
  prereleases. The baseline default
  URL uses GitHub's `releases/latest/download`, whereas the release workflow marks
  versions containing `-` as prereleases. A first prerelease is not a stable
  latest release. Do not promise automatic alpha-to-alpha updates without a
  deliberate authenticated channel/discovery policy.

## Implemented build separation

`.github/workflows/build.yml` is build-only, manually dispatchable and reusable
from the release workflow. Three independent Ubuntu jobs build Linux native
packages/web assets, Windows x86_64 and macOS universal. They need no signing
secret and only have read permission to repository contents. The Linux job also
runs formatting, dependency/advisory and generated-graph checks. Native desktop,
audio and sandbox runtime tests are not implied by these package jobs.

The reusable outputs are `linux-artifact`, `windows-artifact`, and
`macos-artifact`, with artifact names containing the exact source commit. Windows
and Mac artifacts contain `install.tar` rooted at `windows/` and `Nuschl.app/`,
respectively, plus `build-info.json` and `SHA256SUMS`. Tar retains executable modes
and complete app signatures through Actions artifact storage. Linux's artifact
contains Nix installation references/build evidence and a static web archive;
its native program files are not advertised as portable.

Repeated uploads use `overwrite: true`. Publication must verify repository,
commit, run ID, expected platform and hashes, and reject future attempts. It may
reuse a successful earlier attempt from the same run/commit when only failed
jobs are rerun. Build workflow success and immutable same-run artifact identity
remain required; an unrelated run's artifact is not a substitute.

Each job uses two Nix cores/two jobs, an explicit disk build directory, bounded
timeout/retention, and separate Nix cache keys. The 3 GiB cache GC setting is best
effort; it is not a hard disk-size guarantee. Source syntax was checked with
actionlint; first-run cache effectiveness, disk use, elapsed time and actual
platform build success remain unproven until an authenticated GitHub dispatch.

Downloadable CI snapshots also distribute their contents: the shared notice
bundle/check must gate those artifacts as well as public release assets. The
source inventory in `nix/release-licenses.nix` is inspectable when incomplete;
its `check`/`bundle` fail on unresolved source notices or required external
runtime/embedded-asset records. Generic SPDX text is not silently substituted for
missing upstream attribution.

## Baseline workflow findings

Only `.github/workflows/release.yml` exists at the audited baseline. It triggers
on pushed `v*` tags, checks exact workspace-version equality, builds Windows and
Mac sequentially on one Ubuntu runner, signs an update manifest, then publishes.
It has no manual build entry point, Linux build, independent downloadable CI
artifacts, or prior successful CI proof. A missing signing key prevents publication
after those expensive builds. There is no independent branch/PR CI gate in source.

The existing portable archive names are
`nuschl-<version>-windows-x86_64.zip` and
`nuschl-<version>-macos-universal.zip`. The helper additionally flattens every
installation file into an individual release asset for in-place updates. Preserve
that complete helper/bundle file set when splitting build and publication jobs.

Build authority should remain read-only and need no signing secrets. Only a
separate release job receives release-write permission and update-signing material,
after required builds and trust/verification gates. A manual CI dispatch can prove
all cross targets before key enrolment or a public release exists. It must never
create a release merely to obtain build logs.

## Evidence and limits

Inspect `.github/workflows/release.yml`, `flake.nix` (`desktopCrates`,
`crossCommon`, `windowsPackage`, `macSlice`, `macUniversal`, `macApp`,
`packageLayout`), `release/make-update.py`, `crates/update/src/lib.rs`,
`docs/install.md`, `docs/windows-vm.md`, `docs/windows-sandbox.md`,
`tests/windows_update.py`, `tests/README.md`, and `SECURITY.md`'s release gates.
Local test documentation describes earlier native/VM results; this audit did not
rerun them and does not certify changed binaries.

GitHub's documentation distinguishes prereleases from the latest full release:
[latest release API](https://docs.github.com/en/rest/releases/releases#get-the-latest-release).
Reusable build/publication jobs can only retain or reduce caller token permissions:
[reusable workflow permissions](https://docs.github.com/en/actions/reference/workflows-and-actions/reusing-workflow-configurations#access-and-permissions-for-nested-workflows).
