# Debian, Ubuntu and Arch packaging

This packaging lane is in development. No APT repository, PPA or AUR entry has
been published. The source recipes and binary artifacts must be verified in a
clean target distribution before they are advertised as supported installs.
A Nix-linked executable inside a `.deb` is not a portable Debian application.

## Shared runtime contract

Native packages install the real desktop executable and its eight helpers next
to each other in `/usr/lib/nuschl/`:

```
nuschl
nuschl-content-worker
nuschl-crypto-worker
nuschl-math-worker
nuschl-media-worker
nuschl-storage-worker
nuschl-transfer-worker
nuschl-update-verify-ed25519
nuschl-update-verify-mldsa44
```

`/usr/bin/nuschl` points to that real executable. Existing helper discovery uses
its directory, so a package must preserve the complete set; installing only the
UI is insufficient. URI registration remains `mumble://` and `nuschl://`, using
`org.nuschl.Nuschl.desktop` and the matching hicolor icon. Linux's in-app updater
is disabled; APT or pacman owns updates and installed files.

The Linux supervisor already exposes system library directories read-only inside
its worker sandbox. A genuine native build can use the target distribution's
libraries. A binary bundle must instead provide an audited runtime layout, with
no runtime dependency on `/nix/store`, including dynamically opened libraries and
plugins. Changing only an ELF interpreter or RPATH does not prove that property.
A helper executes as `/worker`, so an `$ORIGIN`-relative helper RPATH cannot assume
that its original install directory remains its origin. Any common runtime
adapter is coordinated with the Flatpak/portable Linux lane before use.

Keep the existing role-specific sandbox and fail-closed behavior. There is no
unconfined compatibility fallback, network-enabled codec worker, setuid wrapper,
or package maintainer script that changes global user-namespace or audio policy.
A distro that denies the required namespaces needs a separately reviewed,
least-privilege integration and an actual test; it is not fixed by disabling the
sandbox.

The installed license directory must contain the actual scoped dependency
inventory and all referenced original notices, including fonts and the two
verification engines. Root `LICENSE` alone is insufficient. Stage identity must
bind the source revision, lockfile and nine executables to the tested artifact.
The canonical source binding compares the staged notices and desktop assets with
the source helpers; recipes must not invent a separate weaker attribution policy.

## Source builds and compiler availability

The workspace declares Rust 1.95 or newer. Its pinned Nix toolchain is currently
1.99.1. The declared minimum is not itself proof that every current dependency
and source path was tested with exactly 1.95.

As checked on 12 November 2026:

- Debian 13's default Rust is 1.85.1. Its explicit `trixie-backports` Rust package
  is 1.96.1 on amd64, which clears the declared minimum; it still needs an actual
  package build and runtime test.
- Ubuntu 26.04's default Rust is 1.93.1. A compatible, explicitly declared build
  dependency/backport is required before claiming that suite can build the source
  package. A build script must not download rustup or a compiler during the build.
- Arch's official Rust package is 1:1.99.1-2. Arch source recipes should fetch
  locked Cargo dependencies in `prepare()` and build offline with `--frozen`.

Sources: [Debian stable Rust](https://packages.debian.org/trixie/rustc),
[Debian backports Rust](https://packages.debian.org/trixie-backports/rustc),
[Ubuntu Rust](https://packages.ubuntu.com/resolute/rustc),
[Arch Rust](https://archlinux.org/packages/extra/x86_64/rust/),
[Arch Rust packaging](https://wiki.archlinux.org/title/Rust_package_guidelines).

All source builds retain `Cargo.lock` and the patched `vendor/` sources. Offline
Debian/Launchpad builds additionally need all registry/git sources prepared in
the source upload. Dependency fetching belongs to an explicit preparation step,
not `debian/rules`. Build each update verifier with only its own feature; do not
enable the signing-tool feature in distributed client helpers.

## Local packages and publication

A binary `.deb` and a genuine source package serve different purposes. Local
binary packaging must verify its runtime stage and preserve all helpers and
notices. A source package must compile those programs from the included source
and declared build dependencies; repacking binaries does not make it suitable
for a PPA.

[Launchpad PPAs](https://ubuntu.com/docs/launchpad/user/reference/packaging/ppas/ppa/)
accept source packages only and reject prebuilt binary uploads. The changelog's
`Distribution` selects the Ubuntu series. Prepare `.dsc`, source archives and
`_source.changes` locally, test them in an appropriate clean builder, and only
then sign/upload with the operator's explicitly selected account and key.
Publication is a separate coordinated action; packaging scripts must not upload
implicitly. See [Launchpad's build instructions](https://ubuntu.com/docs/launchpad/user/how-to/packaging/build-deb-packages-in-launchpad/).

Debian has no directly equivalent central per-user PPA service. A separately
hosted APT repository can publish signed `InRelease`/`Release.gpg` metadata.
Clients must use a repository-specific `Signed-By` keyring in a deb822 `.sources`
entry, rather than granting that key global APT authority. Hosting, key enrollment
and uploads remain explicit operator actions. See
[Debian's third-party repository guidance](https://wiki.debian.org/DebianRepository/UseThirdParty).

An AUR submission contains a reviewed `PKGBUILD` and generated `.SRCINFO`, not a
binary upload. A source variant builds against Arch libraries; a `-bin` variant
must explicitly identify and checksum the genuine portable release runtime it
repackages. It cannot silently depend on a user's Nix installation. Regenerate
metadata with `makepkg --printsrcinfo` and test in a clean Arch root before an
operator publishes it. See [PKGBUILD](https://man.archlinux.org/man/PKGBUILD.5.en)
and [makepkg](https://man.archlinux.org/man/makepkg.8.en).

## Required evidence

The amd64 prototype now produces a nine-program FHS stage, a real `.deb`, a
checksummed FHS source archive for the binary recipe, a `nuschl-bin` PKGBUILD and
`.SRCINFO` generated by `makepkg`. The recipe also packages those exact bytes
with `makepkg` inside Nix. That build uses `--nodeps` because Nix is not an Arch
installation; it does not prove that Arch's dependency resolver/install succeeds.
An adjacent local archive is used until the operator selects a real immutable
release URL. The local package maintainer identity is deliberately a validation
fixture, not publication metadata.

The actual prototype opens a GPUI window with target distro fonts and CPU-only
Vulkan in a private Xvfb/DBus namespace with no Nix store or GPU/audio devices.
The production content supervisor also passes rendering and filesystem/network/
environment denial canaries there. This does not establish distro user-namespace
or Ubuntu AppArmor compatibility.

Still pending: a clean source-package rebuild and compatible compiler enrollment,
clean Debian/Ubuntu/Arch package-manager install/runtime tests, physical-platform
support, interactive URI/icon integration, removal/upgrade behavior and
package-manager update proof. Media's public PipeWire/ALSA configuration mounts
are integrated, with scoped startup checks against an owned virtual sound server;
physical input/output quality and target-distribution installation remain unproved.
No external repository/account action is authorized merely by building these
local artifacts. A successful archive extraction or package metadata check alone
does not prove any runtime or sandbox behavior.

## Native prototype checkpoint

The Debian 13 amd64 prototype relinks only the nine final executable leaves,
using the cached pinned Rust dependencies and the signed target sysroot. It
bundles no glibc or native shared-library closure. Per-program link receipts
record the actual target native inputs and binary hashes; the stage rejects Nix
shared-library fallbacks, store interpreters/RPATHs and requirements above glibc
2.41. These link checks do not inspect every runtime `dlopen` or configuration
lookup.

`packaging/linux/runtime-smoke.py` runs the staged programs against only the
target distro's `/usr` in a new offline namespace. It checks the version command,
synthetic Markdown/PNG, formula coverage, Opus encode/decode, an empty crypto
identity request, SQLite migration/query, transfer EOF, and rejection of invalid
public test signatures. It also checks absence of the Nix store, home, host audio
and GPU devices. Every process has bounded time/output and is reaped. This is a
loader and worker-RPC check, not a GUI render, audio-device, transfer, signature
acceptance or installation proof.

Actual distro media startup must additionally verify the target PipeWire/ALSA
public configuration and plugin paths under the production worker sandbox.
A library found via `DT_NEEDED` is insufficient evidence for those resources.
No home directory or blanket `/etc` exposure is an acceptable workaround.

A measured Debian PipeWire startup failure is resolved in the test namespace by
read-only `/usr/share/pipewire`: the same actual worker then opens its route
against an owned virtual sound server. Debian puts `client.conf` in
`pipewire-bin`, not the client library package, so the binary recipe explicitly
requires it and the module package. ALSA likewise needs `/usr/share/alsa`; without
it the worker cannot load `alsa.conf`, and with it configuration loads (the
private namespace deliberately has no physical card). These are public,
role-specific data paths; no whole `/etc` or home grant is proposed.

## Builder interfaces and local validation artifacts

The flake exposes these local validation artifacts on `x86_64-linux`:

```sh
nix build --cores 2 --max-jobs 2 .#deb
nix build --cores 2 --max-jobs 2 .#arch-bin
nix build --cores 2 --max-jobs 2 .#deb-source .#deb-source-check
nix build --cores 2 --max-jobs 2 \
  .#checks.x86_64-linux.distro-metadata \
  .#checks.x86_64-linux.distro-source-assets
```

`linux-native-stage` exposes the shared FHS installation tree. These outputs do
not publish or install anything. `deb-source-check` checks source assembly and
offline Cargo metadata; it does not compile the source package. Default flake
builds use unpublished local versions, as described below. Other platforms omit
these Linux artifact names; the existing default and cross builders are unchanged.

The underlying library interfaces are:

- `nix/portable-linux.nix` takes pinned `pkgs`, `toolchain`, application
  `projectRoot`, exact `revision`, actual `nativeNotices`, desktop integration and
  optional concrete AppStream XML. `stage` contains `/usr`; `programs` exposes the
  nine final leaves. Application and packaging-builder hashes are separate when
  staging a frozen application branch before packaging is merged.
- `packaging/debian/package.nix` takes that audited stage plus an explicit version,
  maintainer and source epoch. It emits the `.deb`, validation/control metadata
  and its checksum. The local fixture maintainer must be replaced before release.
- `packaging/arch/package.nix` emits the deterministic FHS archive, local `-bin`
  recipe, actual `makepkg --printsrcinfo` output and package. No signing-tool
  feature, Nix runtime closure, self-updater or external upload is introduced.
- `packaging/debian/source.nix` takes a **tracked frozen source export**, the exact
  cached Cargo vendor, actual notice and desktop inputs, source revision and
  explicit package/maintainer/suite metadata. It emits genuine unsigned `.dsc`,
  orig/Debian tarballs and `_source.changes`; its `debian/rules` compiles all nine
  executables offline, with verifier features built separately. Original icons
  and required corresponding-source archives are explicit quilt binary inputs;
  no prebuilt application/helper is used as source.

The source upload redistributes more than a runtime package. `SOURCE-NOTICES.json`
inventories every registry source, including build/dev and unselected-platform
packages, original notice bytes and exact-revision curated supplements. Missing
source grants remain explicit publication blockers. A runtime notice pass does
not certify the larger source archive, or legal compatibility/completeness.
The local source package has **not** passed a clean distro source rebuild.

`packaging/debian/apt-repository.nix` creates an unsigned local APT pool, Packages
indexes and Release metadata from the exact `.deb`. Its explicit publication
epoch creates a seven-day Valid-Until; refresh metadata before publication rather
than extending an old signature indefinitely. It does not load a key or contact
an account/server. The operator must first close the install/runtime/source and
notice gates, then sign Release into InRelease and Release.gpg using an explicitly
selected **outside-Nix** keyring/key. Never put a private key in a derivation or
Nix store. Publish the exact pool/index/signature files together. Client deb822
configuration uses the operator's actual HTTPS repository URI, suite and a
repository-specific `Signed-By: /usr/share/keyrings/nuschl-archive.gpg`.

For a PPA, inspect the locally generated source files, supply the compatible
compiler through the declared Launchpad build dependencies, and test that clean
source build. Standard `debsign -k<explicit-key> *_source.changes` and `dput
<explicit-PPA> *_source.changes` are separate operator actions, after review and
account/key enrollment. No packaging build invokes them. Launchpad's resulting
binaries are an external rebuild; a signed APT repository can distribute the
**exact Nix-built binaries** instead. These provenance paths must not be conflated.

## Version ordering

A short Git SHA identifies source but is not a version counter. Earlier local
prototype artifacts named only `+git<sha>` are validation fixtures and must not
be uploaded as a development update sequence.

`packaging/linux/version.py` reads the exact frozen commit from complete Git
history (rejecting shallow history and ignoring replace refs). Tagged release
mode requires the exact `v<workspace-version>` tag target and uses that release
version. Development mode emits, for example,
`0.1.0+git<count>.<commit-UTC-timestamp>.g<shortSHA>` for Debian and
`0.1.0.r<count>.<commit-UTC-timestamp>.g<shortSHA>` for Arch. Count is ordered
before timestamp/SHA; even a descendant with an earlier committer clock sorts
newer. No current clock is used. The coordinator supplies this deterministic
public metadata as explicit inputs to the Nix assemblers; no Rust relink is
needed to change package metadata.

Official development updates must descend from the previous published source;
pass its revision and package versions to the tool. It validates ancestry and
actual `dpkg --compare-versions`/`vercmp` ordering, rejecting rollback or reuse.
Unrelated branches have no implicit release ordering. A new tagged release must
also sort above the previously published development version; increment the
workspace release version rather than relabeling a newer development build with
an older release number. Actual package revision changes remain explicit.

## Pure exported-source interface

The pure entrypoints are `packaging/linux/version.nix` (`mkVersion`) and
`packaging/linux/default.nix` (`mkPackages`). The latter takes `pkgs`, `toolchain`,
`craneLib`, actual unfiltered `sourceInfo`, optional `versionMetadata`,
`nativeNotices`, `desktopFiles`, `appstreamXML` (or explicit `null` until the shared
metadata is present), and an explicit maintainer for publication. It takes the
application tree **only** from `sourceInfo.outPath`. It returns `versions`,
`nativeStage`, `deb`, `archBin`, `debSource`, `debSourceCheck`, `metadataTests`, and
`assetBinding`. The coordinator registers the flake attrs; this library never
moves a bookmark or edits a lockfile.

Notice/desktop inputs are not accepted merely by name: a bounded binding check
compares them with the canonical source helpers, requires the actual Cargo.lock,
and records their hashes plus the actual source NarHash. Non-null AppStream must
match the tracked shared XML. The final stage checks the installed bytes against
that attestation. This is not a legal or runtime certification.

A path/GitHub export need not contain `.git`, `revCount`, or an authenticated
commit timestamp. Missing official metadata therefore gives deterministic,
explicitly unpublished local versions: Debian `base~local.n<source-identity>` and
Arch `baselocal.n<source-identity>`, with normalized epoch 1. They sort below the
corresponding tagged release; arbitrary local snapshots have no promised update
ordering. Unknown Git revision stays `null`. The identity comes from the actual
unfiltered source NarHash, not a guessed revision or path-copy modification time.
`aptRepository` is absent for these local builds.

For official version ordering, prepare a **separate public sidecar outside the
source**; embedding its own source hash would be circular. Schema 1 has exactly
`schema`, `kind`, `base_version`, `source_nar_hash`, `source_revision`, `sequence`,
`commit_epoch`, `commit_stamp`, `tag`, and `package_revision`. The pure function
checks types/bounds, version/tag policy, the actual NarHash, and the revision when
sourceInfo provides it. The exporter supplies the fixed commit date, never a
current clock; pure Nix does not authenticate a date or Git tag from a path mtime.

`packaging/linux/export-version.py --repo <owned-repository> --revision <full-sha>
--exported-source <immutable-sourceInfo.outPath> --mode development --output
<new-outside-source.json>` verifies the full tracked Git membership, bytes,
executable/link modes and source NarHash before writing the sidecar. It refuses
untracked/missing content, escape links, mutable exports, mismatched NarHash and
checkpoint overwrite. Release mode additionally requires the exact workspace
version's local tag target; signature/enrollment authority remains with the
release operator. Prior published source/package arguments enforce ancestry and
actual distro ordering. No private key or account is used by this tool.

The release script can pass that fixed sourceInfo and JSON into the **same pure
lib builder**, without Git or a new flake input inside evaluation/derivations.
An official sidecar makes `aptRepository` available as an unsigned preparation
artifact. That convenience fixture uses the source commit epoch, so its seven-day
validity can already have expired. For publication, assemble fresh indexes with
`packaging/debian/apt-repository.nix` and an explicit current `publicationEpoch`,
then sign outside Nix. This does not close source-license, clean-build,
install/runtime, signing or publication gates. `versions.publicationReady`
deliberately remains false.
