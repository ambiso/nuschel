# Dependency caches and cross-build verifier isolation

Crane's pinned `buildPackage` creates an implicit `buildDepsOnly` derivation with
an artifact-only `installPhase`. It nevertheless passes the final `postInstall`
attribute through. Our source-bound notice bundle therefore changed that
otherwise-unused dependency input after an application-only edit. The observed
Windows cache contained a 954 MiB archive and took about nine minutes to produce.

`flake.nix` now explicitly creates the cross dependency pass, removing only final
installation attributes (`preInstall`, `postInstall`, `installPhase` and
`installPhaseCommand`) and replacing its source with `dummySrc`. The cross pass
retains Crane's artifact-only install phase. Final products retain their complete
installation and notice hooks. Compilation/check hooks, real vendored sources
retained by `dummySrc`, toolchains, features, targets and linker arguments remain
dependency inputs. This is cache hygiene, not a migration of every Crane builder.

Run the regression against a committed revision:

```console
python3 tests/build_cache.py --revision REV --output /disk/new-proof-directory
```

It exports only tracked source using `git archive`, evaluates the real Windows
and aarch64 macOS package derivations, edits an application source comment, then
changes the production Windows target flags and a retained vendored dependency
source in the private snapshot. It requires unchanged dependency derivations and
changed final products for the application edit, a changed Windows dependency
derivation for the flag edit, and changed dependency derivations for the retained
dependency source. It also checks
that final installation hooks survive and dependency installation has no notice
hook. This evaluates derivations; it does not compile cross products or establish
native runtime compatibility.

The initial actual evaluation passed at `49bc0fec`; local evidence is
`~/.cache/nuschl-build-cache/equality/run-1/proof.json`.

## Separate cross verifier products

Windows verifier roots now use `nix/cross-updaters.nix` and the existing generated
crate2nix graph. Each product enables exactly `ed25519` or `mldsa44`; neither
selects `signer` or `runtime-fixture`. The Ed25519 graph selects dalek and excludes
ML-DSA; the ML-DSA graph selects ML-DSA and excludes dalek. The target-only
Windows Rust toolchain preserves the prior narrow dependency cache. A checked,
final-crate-only adapter corrects the pinned buildRustCrate `.exe` rename, and a
static CRT/pthread search argument preserves runtime independence. No old
Windows import archive workaround is included in these production engines.

Darwin retains the existing patched cargo-zigbuild, SDK 14.5 and deployment target
14.2. Four separate products cover each algorithm and x86_64/aarch64; each builds
one explicit binary with no default features. This is an intermediate step toward
the separately queued full crate2nix cross migration. The desktop Cargo roots no
longer enable either verifier feature. Final Windows/macOS slices copy the exact
separate products before checking all nine programs; macOS checks their minimum
OS before universal lipo and app signing. Existing complete notice scopes remain.

Build the engines independently:

```console
nix build .#windows-update-ed25519 .#windows-update-mldsa44 --cores 2 --max-jobs 2
nix build .#macos-x86_64-update-ed25519 .#macos-x86_64-update-mldsa44 .#macos-aarch64-update-ed25519 .#macos-aarch64-update-mldsa44 --cores 2 --max-jobs 2
python3 tests/build_cache_cross.py --artifacts --output /disk/new-cross-proof
```

The second regression inspects real production derivations and feature/provider
roots, exact final-helper copy inputs, and all nine required program assertions.
With `--artifacts`, it checks each output has exactly one executable, actual PE
imports have no MinGW runtime DLLs, all four Mach-O slices have the expected
architecture/minimum OS and no Nix-store dylib imports, and both universal helper
assemblies contain x86_64 and arm64. It does not execute macOS binaries.

On 2026-11-12 the Windows build reused dependencies and built four small leaves in
10 seconds; the four Darwin engines/dependency products built eight leaves in
95 seconds. Five-second monitoring observed at least 22,476,152,832 bytes free
in `/tmp`. Client scratch was disk-backed; the daemon build directory was not
reconfigured. Actual output hashes:

| Product | SHA-256 |
| --- | --- |
| Windows Ed25519 | `d36187a199ad9633e054378b21dcb764c5928eabdc73955e7499cce0eaef7cde` |
| Windows ML-DSA-44 | `2a26dcb4e44b67d7f702638d111be83c5725566869b473048520d48b6fa44e38` |
| Darwin x86_64 Ed25519 | `17cb115375f8731b0831035bb01feaae672db176ee2dc170bf5148d9af8398d3` |
| Darwin x86_64 ML-DSA-44 | `d14e440831dc3351bbf805e97fe515d57c0c696f3d1b5f47913845db19b1c5a1` |
| Darwin aarch64 Ed25519 | `79915318a803de25b82399071e21bc7366bf7ba2cec76f8496c3731792f8b658` |
| Darwin aarch64 ML-DSA-44 | `39197d7343f526659d1bcdd18ebe183a2d9e090751f7d0f4c6624d6aff6dabb0` |

`tests/windows_hybrid.py` passed against those exact new Windows products and the
previously validated, non-distributed runtime fixture
`p09yx6j5gd4q0vv6rjm9wfd3qnmkgzby`. Temporary keys were generated only inside its
private guest directory. Real production LPAC launches passed strict two-worker
AND/corruption/raw-byte/pair/expiry/downgrade/duplicate-field checks, the actual
Tokio blocking caller, forged-child/sibling rejection, file/write probes, output
and total deadline/reaping checks, and a private HTTP install/cleanup. Distinct
LPAC SIDs are supplementary identity evidence. The narrowly handled Winsock
initialization error 10107 establishes network initialization unavailable under
LPAC; it is not evidence of a successful socket-level denial probe. The existing,
explicitly selected Windows guest used restricted forwarding and no audio
backend; it was shut down cleanly (QEMU exit 0) after this run.

Local evidence under `~/.cache/nuschl-build-cache/`:

- `cross-artifacts-2/proof.json`: all six produced products and universal helper checks.
- `native/proof.json`, `native/guest.log`: exact Windows native fixture and limits.
- `windows-build-status.json`, `macos-build-status.json`: costs and scratch monitoring.
- `equality/run-2/proof.json`: source/flag invariance after separate-product wiring.
- `equality/run-3/proof.json`: also proves retained vendored source still invalidates both dependency caches (`a346bf08`).

## Complete frozen distribution proof

The separate products are now integrated into complete distributions at frozen
`c26529d3` (merged `0c1452be`, same Git tree as `39ea7132`). The Windows product
build passed in 835 seconds and both Mac slices/universal app in 795 seconds.
Actual source-bound release tar/ZIP and update preparation checks pass: Windows
15 installation files, Mac 18, with all nine programs and full notice bytes.
Windows helpers are byte-identical to separate products. Both Mac slices preserve
their exact helper bytes and the universal pre-sign helper equals actual lipo;
final signing changes signature metadata as expected. Bundle version/identifier,
minOS14.2 and system-only dynamic imports were inspected.

The GUI fixture changes only the public trust JSON in a separate source copy.
Its dependency derivation is exactly the production one, with no cache override:
`n4mvfqv4ny9zhrnfgwkxsxvmlbia3hln`, a 956 MiB artifact produced once. The fixture
final/library/notice leaves took 305 seconds. All eight worker/helper EXEs remain
byte-identical to production; only the desktop embeds the temporary public pair.
Native LPAC and actual GUI signed installation/restart pass, including all 16
installed hashes and absolute space/Unicode profile/update-state continuity.
The guest is shut down cleanly; keys remain outside source/store/guest.

Run `tests/cross_distributions.py` with an exact full-revision local Git flake,
a matching tracked-source snapshot and a new disk output directory. It compares
every exported path/byte/executable bit to that Git archive before loading the
source authority. Seven actual Git-tampering regressions cover this binding.
See [the structured proof](reviews/2026-11-12-cross-distributions.json).

The pinned rcodesign0.29 generic verifier fails on its own empty ad-hoc CMS wrapper
and warns that its verification command is buggy. The original failure remains
in the evidence. `tests/cross_macho.py` therefore supplies explicitly test-only,
bounded inspection of the exact emitted format, using Apple's published
[Mach-O loader](https://github.com/apple-oss-distributions/xnu/blob/main/EXTERNAL_HEADERS/mach-o/loader.h),
[CodeDirectory format](https://github.com/apple-oss-distributions/xnu/blob/main/osfmk/kern/cs_blobs.h)
and [special-slot semantics](https://github.com/apple-oss-distributions/Security/blob/main/OSX/libsecurity_codesigning/lib/codedirectory.h).
It requires both architectures, SHA-256 code-page coverage, strict ADHOC/no CMS
identity, and correct main Info.plist/CodeResources/requirements slots. Eleven
real produced-byte tampering cases pass. Unknown forms fail closed. This is
additional scoped integrity evidence, not Apple's codesign/Gatekeeper verification
or authenticated publisher identity; production signer/publisher rules did not
change. Native Apple tooling/runtime and first external CI/release remain open.

Unreferenced docs/reviews JSON still changes final cross product/notice paths,
although the real dependency derivations stay cached. Narrowing final source
filtering without losing actual embedded assets/policy inputs remains in TODO.
The broader per-crate migration and production trust enrollment are also separate
open work. No remote publication or host device access occurred.

Production trust update (2026-11-12): the dedicated public pair is now enrolled
in `crates/update/update-trust.json`. Statements above about absent enrollment
describe the recorded historical work, not the current source trust store.
This does not establish CI secret availability, a remote release or new platform proof.
