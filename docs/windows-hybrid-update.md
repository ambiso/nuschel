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
launch or restart the desktop application. The source and binary hashes are in
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

The following remain open: actual desktop GUI update/restart using a separately
built temporary-public-trust source, final Windows distribution archive proof,
physical Windows hardware, and native macOS Seatbelt/updater proof. Before the
GUI harness may run, it must install both verifier EXEs with every required helper
and use unique owned install/profile directories and PIDs. Its current fixed
four-file initial copy and `Get-Process nuschl | Stop-Process` are unsafe for that
test scope and have not been used by this proof. The old minisign GUI run does
not renew evidence for the current hybrid updater.
