# Native Windows hybrid update proof

On 2026-11-12, the non-distributed `nuschl-update-runtime-fixture` passed in the
existing private Windows 11 Enterprise Evaluation 25H2 VM, build 26200. QEMU
11.1.2 used two vCPUs, 6 GiB RAM, restricted user networking with only the
authenticated harness forward, and a synthetic HDA backend with no host audio.
No guest reinstall, production trust enrollment, production seed, remote service
or release operation was involved. Keys were generated only in a guest temporary
directory; the embedded production public store remains empty.

The actual fixed-algorithm Ed25519 and ML-DSA-44 EXEs were compiled separately
from the locked graph with Rust 1.99.1. Imports contain Windows system DLLs/API
sets; no MinGW/libgcc/libstdc++/libwinpthread DLL was staged or discovered through
ambient PATH. The fixture's own signing-enabled products are not substitutes
for these two individually featured verifier builds.

The native run covers valid signatures, either corrupted component, raw-byte
changes, a wrong pair, no trust, legacy/downgrade input, expiry and duplicate
fields. It also covers the desktop's actual Tokio `spawn_blocking` context.
Test-only native adversaries call the production bootstrap first, then run through
`verify_hybrid` and its real LPAC launcher, Job containment and strict reply check.
Each role must return the correct request-bound forged reply after denied private
file read/write attempts, while a corrupted sibling still prevents acceptance.
Oversized replies and stalls reject in both roles. The case's total less-than-ten-
second assertion includes a bounded one-second image-release retry after Windows
Job termination. Deleting an unmapped image is supplementary cleanup evidence;
this fixture does not inspect every Job/token limit from inside the container.

Network evidence is specifically initialization denial/unavailability: the pinned
Windows standard library panics when LPAC's WSAStartup returns 10107, before a
socket is created. The test catches only that exact observed assertion and still
requires a correct bound reply afterward. Any other panic or a successful
connection fails. This is not a socket-level denial result or a crashed-child
pass. The two observed profile SIDs differ; that is supplementary identity
evidence alongside the real launcher calls, not a token-attestation mechanism.

Finally, a dedicated guest-loopback HTTP fixture provides the signed manifest,
signature and synthetic program. The native library remembers its version,
downloads and checks it, applies the installation, and cleans up. This does not
launch or restart the desktop application; that separate proof is described below.
The native source and binary hashes are in
[the reviewed proof record](reviews/2026-11-12-windows-hybrid.json).

To reproduce, first boot an explicitly selected existing disposable guest using
the [authenticated harness](windows-vm.md). Never select an absent default guest
or attach personal profiles. Build the test products without a desktop rebuild:

```sh
nix build --impure --file tests/windows_hybrid.nix ed ml fixture \
  --cores 2 --max-jobs 2 --no-link --print-out-paths
NUSCHL_WINDOWS_VM_DIR=/path/to/existing-installed-guest \
TMPDIR=/path/to/owned-disk-scratch python3 tests/windows_hybrid.py \
  --ed25519 /ED_OUTPUT/bin/nuschl-update-verify-ed25519.exe \
  --mldsa44 /ML_OUTPUT/bin/nuschl-update-verify-mldsa44.exe \
  --fixture /FIXTURE_OUTPUT/bin/nuschl-update-runtime-fixture.exe \
  --output /path/to/owned-proof-output
```

The test-only Nix expression retains three narrowly scoped pinned-builder
adaptations: static MinGW pthread search, the Windows executable suffix in the
final bin rename, and the exact already-locked windows-targets 0.52.6 import
archive. It does not change the repository's production builders or dependency
graph. The initial cold cross graph had 188 small derivations; subsequent source
rebuilds used three cached leaf products. Five-second scratch monitoring kept
at least 22.47 GB free in `/tmp`; the client TMPDIR was disk-backed, while Nix's
daemon scratch retained its default location. All build failures were retained.

## GUI flow and profile regression

The GUI harness first received complete-payload and ownership corrections: all
nine programs and actual packaged notices are required, run IDs scope paths,
file offers and listener jobs, and only matching executable paths may be stopped.
Six host cases and a real guest test using two same-name managed stand-ins at
different paths pass; the foreign-path sentinel survives. The additional host
case preserves Nix's read-only source notices while making only the copied staging
directories writable for a synthetic delivery marker. No desktop or update runs
in that preflight. Child-only environment configuration avoids leaving the harness
agent pointed at a fixture update server.

An exact tracked `4382aa2f` source archive, with only a temporary public trust
document added outside the repository, produced the first GUI fixture. Its two
official-feature helper EXEs passed the full native adversarial suite, and the GUI
applied/restarted and checked every file. Review caught a real production bug:
relaunch discarded `--profile-dir` and opened the guest's default profile. That run
does not establish owned-profile continuity and remains retained as failure
evidence. The private guest's default data was not deleted to hide the problem.

The separate fixed source `3f1657d5` changes only `apps/desktop/src/update.rs` and
one initialization hook in `main.rs`. It remembers the typed explicit profile as
an absolute native path and supplies it through `Command` arguments. It does not
replay one-shot URL, offer or autostart arguments. Default-profile behavior stays
unchanged. All 82 native desktop unit tests pass, including actual profile
normalization, spaces/Unicode and Unix non-UTF8 argument preservation.

The strengthened `963f8382` harness starts the app with the **relative**
`profiles\profile µ` under its unique owned install. After signed installation
and the real Restart button, it verifies the new process's exact **absolute**
profile argument and an unchanged SHA-256 of that profile's persisted update
state. PIDs change from 508 to 3604 with no old PID retained, all 16 signed installed
files match their manifest hashes, the marker arrives, and staging/retired files
are removed. Owned UIA focus and the actual button geometry pass. The three
screenshots show the offer, installed state and clean restarted application.
The fixed build's eight helper/worker EXEs are byte-identical to the original
official-feature products that passed the full native suite; only the desktop
EXE changes. The [GUI proof record](reviews/2026-11-12-windows-gui-update.json)
retains exact package/source, snapshot, cache, import and binary hashes.

The dependency phase took 575 seconds and produced a 954 MiB CargoArtifacts
archive. The fixed-source desktop/native-unit build took 370 seconds. Its private
scratch override reuses those validated artifacts only after complete dependency
environment comparison showed identical dummy source, toolchain, features and
link inputs; only the source-specific notice postInstall/output paths differed.
The new notice postInstall remains intact. No production builder was changed.
Both phases used two cores/jobs and the five-second 8 GiB scratch guard; at least
22.47 GB remained free. Original and fixed source snapshots are distinct and
immutable. The private seeds stayed in a sibling mode-0600 key file, never in
either source archive, Nix store, guest or public proof. Only temporary public
trust entered the fixture source. The app had no configured server/voice session.

At that original GUI snapshot, the official cross builder enabled both verifier
Cargo features in one workspace build. Its entry points are fixed to their respective algorithm, but
these individually featured test products do not prove feature-graph or binary
equivalence for every eventual official archive. Exact packaged-helper/import and
archive/runtime validation remains a release gate after later changes, including
the separately integrated audio fixes outside this snapshot. Physical Windows
hardware, native macOS Seatbelt/updater execution, production enrollment and an
actual release remain separate. The old minisign GUI run is not used as evidence
for this current hybrid path.


## Frozen merged cross-distribution revalidation

Source `c26529d3` freezes merged `0c1452be` (the same tree later named
`39ea7132`). The real production Windows package and both-architecture universal
macOS app built from this source. Windows all-nine/import/source-notice checks
pass. The updated builder produces independent Ed25519 and ML-DSA feature roots;
its actual packaged helper bytes match the separate products.

A fresh private fixture copied that entire tracked tree and changed only
`crates/update/update-trust.json` to one temporary public pair. Private seeds stayed
in a sibling mode-0600 file outside both source snapshots, store and guest.
Production and fixture select the exact same dependency derivation/artifact, with
no manual override. Production took 835 seconds; the public-only final fixture
305 seconds. All eight fixture helper/worker EXEs are byte-identical to production;
only the desktop EXE contains the temporary public pair.

The existing, explicitly selected Windows guest passed the full native adversarial
LPAC suite using those exact packaged helpers, then the actual GUI update flow.
The process changes from PID 6020 to 1480, all 16 signed file hashes match, the
marker arrives and staging/retired files disappear. A relative space/Unicode
profile reappears as the same absolute profile after restart, with identical
persisted update-state SHA. The captured raw update manifest/signature also pass
ordinary host verification with the same temporary public pair. The clean restarted
screenshot was visually inspected. Cleanup leaves zero `nuschl.exe` processes and
the owned VM shuts down cleanly (QEMU exit 0). There was no configured voice server
or host device access.

The real source-bound tar/ZIP/update producer checks also pass for both platforms,
with 15 Windows and 18 Mac installation files. On Mac the independent pre-sign
helper copies and universal bytes match; final ad-hoc signing legitimately changes
signature bytes. Pinned rcodesign's generic verifier exposes its documented empty-
CMS bug. Independent test-only CodeDirectory code-page/Info.plist/CodeResources
checks and real tampering regressions provide scoped integrity evidence; native
Apple tooling, Seatbelt, media and self-update remain untested. No production
signature policy was changed.

[The current structured record](reviews/2026-11-12-cross-distributions.json) retains
exact products, archive/hash data, source authority, public-only cache provenance
and the original failing Mac-tool diagnostic. These local frozen-source results
are not first remote CI/release evidence, physical Windows proof, native Mac
acceptance or production trust enrollment.

Production trust update (2026-11-12): the dedicated public pair is now enrolled
in `crates/update/update-trust.json`. Statements above about absent enrollment
describe the recorded historical work, not the current source trust store.
This does not establish CI secret availability, a remote release or new platform proof.
