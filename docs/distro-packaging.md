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
The common staging/notice contract is still being coordinated; recipes must not
invent a separate weaker attribution policy.

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

Still pending in this lane: a finalized common runtime stage, a real `.deb`,
source-package/PPA artifacts, an AUR recipe and generated metadata, clean Debian,
Ubuntu and Arch install/runtime tests, actual confined helper denial probes,
URI/icon integration, removal/upgrade behavior and package-manager update proof.
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
