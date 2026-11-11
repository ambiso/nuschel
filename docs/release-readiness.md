# First alpha release readiness

Source audit: 2026-11-11, baseline `15a733bf`. This is a release checklist,
not a new platform test result. The hybrid updater is being implemented in a
separate change; the baseline still uses minisign. Recheck the final integrated
commit and record the actual CI run before publishing.

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
  owner will specify the final key format and secret name; `UPDATE_SIGNING_KEY`
  and `update-key.pub` describe the baseline, not the pending hybrid contract.
- [ ] Choose the alpha version and make the tag match the workspace version
  exactly. At baseline only `v0.1.0` matches. `v0.1.0-alpha.1` needs a version change
  first; inspect hard-coded Nix package/app versions as well as Cargo/lockfiles.
- [ ] Run the reusable build workflow at the intended commit and retain all three
  independent platform artifacts and logs. No successful GitHub build is claimed
  by this source review. A failed cross job must not hide another platform's
  downloadable CI artifact; publication must require all intended jobs to pass.
- [ ] Finish the existing dependency attribution/notices release gate. Linux's
  `packageLayout` includes several vendored/font licences, while baseline Windows
  and Mac packaging copies only EUPL-1.2. Passing `cargo deny` is not a complete
  attribution inventory. Inspect the exact final archives, including helper files.
- [ ] Run the final integrated formatting, Clippy, dependency/advisory, unit and
  private interoperability/confinement checks. CI package builds do not replace
  private NixOS/Windows VM evidence or native Mac testing. Record remaining native
  accessibility/audio/hardware gaps without calling them tested.
- [ ] Verify the signed manifest with the exact embedded public pair, verify the
  complete installation file sets and hashes, and exercise update installation
  with disposable profiles. Hybrid migration invalidates the old minisign fixture
  as proof for the new signature path.
- [ ] Review README/install/security/release notes against the final artifacts:
  early-development status; compatible TLS/Mumble voice; MLS experiments and
  historical-key limits; no completed RTC video/screen-sharing claim; Linux via
  Nix; Windows VM-only and Mac untested status; no OS code-signing claim.
- [ ] Decide how alpha clients find subsequent prereleases. The baseline default
  URL uses GitHub's `releases/latest/download`, whereas the release workflow marks
  versions containing `-` as prereleases. A first prerelease is not a stable
  latest release. Do not promise automatic alpha-to-alpha updates without a
  deliberate authenticated channel/discovery policy.

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
