# First alpha release readiness

Source audit: 2026-11-11, baseline `15a733bf`. This is a release checklist,
not a new platform test result. The original baseline used minisign; the current hybrid runtime is described in
[the update worker README](../apps/update-worker/README.md), with Linux actual-worker
proof and explicit remaining platform/enrollment gates. Recheck the final integrated
commit and record actual artifact/local-integrity evidence before publishing; native
Apple verification is optional and must be reported honestly. Local Nix
products are accepted with exact derivation/NAR/source receipts; CI is optional.

The chosen first release is the normal `0.1.0` release, described as an early
preview. It is not a separate prerelease update channel. No tag, release or CI
dispatch was created by this audit.

## Smallest coherent scope

An explicitly experimental desktop alpha can contain the Windows x86_64 portable
zip and the macOS Intel/Apple-silicon universal app. Linux users install the tagged
Nix flake; there is no portable Linux archive, AppImage, Flatpak, or distro package.
The Linux package also contains the terminal client and server. The cross desktop
packages currently contain the desktop and its helpers, not a Windows/macOS server
or terminal distribution. Browser voice has private synthetic loopback proof;
rich-message content bridging, full mobile/native browser coverage and video
remain separate work. CI can produce a deployable static web artifact without
claiming the complete web/mobile client is finished.

The user deferred Windows Authenticode and macOS Developer ID/notarisation. That
does not waive update authentication. Windows downloads remain unsigned, macOS
bundles retain their existing ad-hoc signature, and release notes must describe
the documented OS first-start restrictions. Neither cross compilation nor an
ad-hoc signature establishes native runtime support. Windows has private Windows
11 VM evidence; macOS has no native runtime proof, including Seatbelt, microphone
permissions, playback and self-update. Mark the Mac artifact untested/experimental.

## Gates before publication

- [x] Merge the strict hybrid-v2 runtime, two independently featured/confined
  verifier processes, explicit signer and source/provenance publication gates.
  Linux actual-worker and private Windows LPAC/GUI fixtures pass; both verifier
  programs are packaged and signer/test tools are excluded. This is local runtime
  evidence, not a successful remote release or native macOS validation.
- [x] Enrol the dedicated production hybrid public pair in source (2026-11-12),
  key ID `dbc40ac478b51573eaebebb0a40532bec7cf2a87282a98ae08f1a25ee5b42535`.
  This is public-only enrollment; historical fixture results remain scoped to
  their recorded trust/source snapshots. No legacy-key fallback remains.
- [ ] Retain protected local private-key backups and run the reviewed [local
  prepare/sign/publish procedure](local-release.md). No long-lived signing key
  belongs in GitHub Actions; CI verification remains key-free. Public enrollment
  does not establish artifact/native-Mac acceptance or publication readiness.
- [x] Choose `0.1.0` as the normal early-preview version. The eventual tag must be
  `v0.1.0`, matching the workspace and package versions; this decision has not
  created that tag.
- [ ] Prepare and validate all intended exact-tag local Nix products and mandatory
  local Mac structural/ad-hoc integrity. Native Apple/CI verification is optional
  additional evidence; mark Mac experimental and not natively verified when absent. The
  reusable build workflow remains optional artifact/verification infrastructure;
  local receipts must not claim invented CI provenance.
- [x] Implement scoped, fail-closed notice packaging and actual source evidence,
  including two updater engines, native runtimes, explicit curated MPL/source
  decisions, the math font/syntax/theme data and actual emitted web owners. The
  production reader checks scope/roots/features and exact notice/source bytes.
  Compact native/web payloads respect the existing 256-file limit. Independent
  Nix metadata bundle/gate proofs pass; see [the notice pipeline](release-notices.md).
- [ ] Review the exact final integrated native/cross/web archives after later
  dependency/vendor changes. Verify target scopes, all helpers and notice/source
  hashes. Current metadata evidence does not establish legal compliance or a
  successful public all-system CI/release run.
- [ ] Run the final integrated formatting, Clippy, dependency/advisory, unit and
  private interoperability/confinement checks. CI package builds do not replace
  private NixOS/Windows VM evidence or native Mac testing. Record remaining native
  accessibility/audio/hardware gaps without calling them tested.
- [ ] Verify the eventual production signed manifest with the enrolled embedded public pair, verify the
  complete installation file sets and hashes, and exercise update installation
  with disposable profiles. Hybrid migration invalidates the old minisign fixture
  as proof for the new signature path.
- [x] Implement and document offline/manual metadata renewal for unchanged
  binaries, including explicit authenticated expired-source recovery and clock
  diagnostics. Original artifact bytes/provenance and same-pair authority remain
  bound; the renewed document must pass ordinary current-time verification.
  See [the procedure](update-metadata-refresh.md).
- [ ] After separate enrollment/publication approval, execute the first actual
  renewal and verify/download/archive its pair and next manual deadline. The
  tag-create workflow is not an automatic refresh/reminder service; no cron or
  expiry waiver is introduced.
- [x] Run the hybrid native library fixture in the private Windows VM: both
  individually featured LPAC verifiers, temporary keys, bound forged answers,
  corrupt siblings, bounded output/stalls, and loopback fetch/apply/cleanup pass.
  See [proof and limits](windows-hybrid-update.md); this is not desktop GUI proof.
- [x] Correct the GUI harness's omitted verifier binaries and unowned-process
  stopping. Complete-payload host tests and actual guest same-name foreign-path
  process preservation pass; install/profile/listener/file namespaces are unique.
- [x] Run the actual Windows update/restart flow with separately built temporary-
  public-trust sources, owned focus/button geometry, complete installed hashes and
  cleanup. Fix the discovered lost-profile restart and prove exact absolute
  selection from a relative space/Unicode profile plus unchanged update-state SHA.
  Exact official-feature helper bytes also pass the native adversarial suite.
- [x] Inspect complete frozen-source cross/archive products at `c26529d3`
  (merged `0c1452be`; same tree as `39ea7132`). Both separately featured Windows
  engines and all four Darwin engine slices are present in full nine-program
  distributions. Actual source-bound notice/original-byte checks, Windows imports,
  Mac minOS/bundle metadata/universal helper copies and real release tar/ZIP/update
  preparation pass (15 Windows/18 Mac files). Private Windows native LPAC and
  complete signed GUI update/profile continuity pass using a public-only source
  fixture and the exact production helper bytes. See [the structured proof](reviews/2026-11-12-cross-distributions.json).
- [ ] Recheck the eventual release/tag source and first actual remote CI run
  after later code changes; this frozen local proof cannot authenticate future
  packages or replace operator key enrollment and publication approval.
- [ ] Perform native Apple codesign/Gatekeeper and macOS runtime verification.
  Pinned rcodesign 0.29's verifier reports its known empty-CMS bug on these ad-hoc
  binaries. Raw failures remain; independently bounded SHA-256 page and bundle
  special-slot checks pass 11 real-byte tampering cases. This is scoped integrity
  evidence, not Apple acceptance, Developer ID, notarisation or publisher identity.
  Production signer/publisher behavior was not changed to bypass this test tool.
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


## Prepared native Apple static CI gate (not yet run)

`.github/workflows/build.yml` now prepares `macos-native-verify` after the existing
Linux/Nix `macos` cross-build. The reusable `macos-artifact` output is supplied
only by that successful native job, preserving the artifact name. A native
verification failure therefore fails the reusable workflow and blocks its release
caller. No remote run, push, dispatch, publication or signing-secret operation was
performed to prepare this gate; actual Apple success remains unchecked.

The explicit standard runner is `macos-26` (ARM64). GitHub's
[current image table](https://github.com/actions/runner-images/blob/main/README.md)
and [macOS 26 GA announcement](https://github.com/actions/runner-images/issues/13739)
were checked on 2026-11-12: `macos-26-intel` is Intel, while `macos-26` is ARM64;
macOS 14 retired from Actions on November 2. One native host statically checks
both slices; it establishes neither Intel program execution nor macOS 14.2
runtime compatibility.

The early inline guard fails unsupported events before checkout/download. It
accepts the existing manual dispatch and release tag-push callers; reusable
workflows inherit their caller's event. Checkout uses the exact workflow SHA and
persists no Git credential. The pinned artifact action downloads only
`needs.macos.outputs.artifact` in this run; it has no cross-run/repository/token
overrides. The [pinned action contract](https://github.com/actions/download-artifact/blob/d3f86a106a0bac45b974a628896c90dbdf5c8093/action.yml)
defines this same-run default. Repository, commit, run, prior successful attempt
(up to the current attempt), tar checksums, safe extraction, all nine programs and
source-selected Mac notice coverage are rechecked before native tools run.
This supports rerunning failed jobs without accepting a different run/source.

`tests/macos_native.py` invokes only fixed `/usr/bin/codesign`, `lipo` and `otool`.
It requires strict/deep/all-architecture bundle validation, strict checks of all
nine binaries and explicit arm64/x86_64 signatures, exact universal architecture
sets, source version/bundle metadata, minOS14.2 and portable library imports.
The app and helper programs never execute; no profile, GPU, microphone, camera,
new Nix/SDK build or Apple developer account is needed. Ad-hoc signatures provide
integrity, not Developer ID, notarisation or Gatekeeper publisher acceptance.
Apple documents [strict/deep verification](https://developer.apple.com/library/archive/technotes/tn2206/)
and [independent universal-architecture hashes](https://developer.apple.com/documentation/technotes/tn3126-inside-code-signing-hashes).
The existing producer/publisher signature rules are unchanged.

Each Apple command has a 30-second/128KiB limit, with a five-minute/4MiB aggregate
budget; failures remain failures. Bounded reports/raw logs upload even on failure,
with no extracted application in the diagnostics. Local policy regressions use
the real full frozen Mac tar and original notice bytes, test foreign metadata,
retry/name/hash/scope/traversal/link rejection and the non-Mac/CI-preparation
guards. Another test executes the actual inline Bash for permitted dispatch/tag
retry contexts and forbidden PR/workflow-run/branch contexts. Actionlint checks
both build/release DAGs. These tests do not fake or claim an Apple tool pass.

### Separate native runtime work still open

The native `update-runtime-fixture` package attribute exists, but the official
Linux-hosted Darwin cross products do not include/export a corresponding fixture;
it must remain absent from distributed apps. The current `cfg(unix)` adversarial
fixture uses a Python shebang, while the Mac content profile permits execution
and reading of only the literal selected worker (plus its imported system policy).
Interpreter/runtime access may therefore be denied: that is a source-derived
risk, not an observed Mac result. A native test-only fault executable and separate
fixture product need actual Seatbelt canary/forgery/timeout/reaping tests before
claiming parity. Do not broaden production grants to make a script pass.

Mac update workers currently get the Seatbelt profile and supervisor's bounded
request/deadline/reaping, but no explicit address-space/CPU resource limit like
the Linux launcher/Windows Job. Native resource-containment validation and any
reviewed platform adapter remain in TODO. No runtime implementation changed here.
