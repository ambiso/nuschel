# TODO audit and work ownership

Reviewed on 2026-11-11 against the verified rename checkpoint `f0116b77` and the
changes recorded below. Every section of the then 1,246-line `TODO.md` was read.
This is a source, test and prior-evidence audit; it is not a claim that every
platform or test scenario was run again. Historical verification counts remain
attached to their original checkpoints.

The [detailed source ledger](reviews/2026-11-11-todo-audit.json) records all 60 open
desktop/client/audio items in the assigned range, 22 checked/prose corrections,
20 server/security findings and 45 prospective conflict tracks. Its line references
use the original checkpoint, so they remain interpretable after TODO edits. Findings
describe the pre-fix state; completed work below supersedes those findings explicitly.

## Conflict check before delegation

All three workspaces started at `f0116b77`. Ownership was assigned before work
started. Separate directories alone do not establish independence: shared manifests,
protocols, schemas and APIs stayed with the integrator.

| Track | Exclusive ownership | Dependencies and conflict boundary |
| --- | --- | --- |
| iOS environment tooling | `tests/ios_vm.py`, `tests/ios_vm_test.py`, `docs/ios-vm.md` | New host-side tooling only; no SDK, Cargo, Nix, database or production API changes |
| Desktop/audio audit | Read-only desktop, client, media and integration-test review | Findings returned to the integrator; no edits, builds, host audio or camera access |
| Server/security audit, then store locking | Read-only server/security review, then crypto-worker `store.rs` and necessary crypto test lifetimes | Locking is local to the crypto store; no wire, schema, dependency or SDK change; no concurrent crypto build |
| Integrator | `TODO.md`, this report, release workflow, shared documentation and task wiring | Applies findings and integrates commits sequentially; reviews the other tracks before merging |

The store-lock implementation was authorized only after its file and lifecycle
dependencies were identified. The other agents were notified of shared-file
ownership. Agents commit with `jj`; the integrator retains one linear sequence.

## Coverage of the review

| TODO area | Result and evidence |
| --- | --- |
| Execution order and user decisions | Rename checkpoint already pushed. iOS remains first; Linux-hosted guest/graphics proof precedes reliance on Simulator. Linked device sessions are required before concurrent desktop/phone auto-connect. Recovery, sync, Android, browser completion and video remain later. |
| Reports and UI | Code copy, selection/context copy, sanitized links and search pagination exist. Image copying remains separate from Open/Save image. Evidence: desktop `view.rs`, `document.rs`, `selection.rs`, `menus.rs`; content parser and storage search tests. Existing bug reports without a demonstrated fix remain open. |
| Audio and stock protocol | UDP crypt setup, fallback, jitter/PLC and default VAD/RNNoise/Sonora are implemented. Stale introductory descriptions are removed. Evidence: session connection/UDP code, media worker capture/playback, private-server protocol and voice suites. |
| Isolation and secrets | Windows math uses AppContainer confinement. Rename retirement removes earlier Windows profile grants; real old-Windows-profile migration remains untested. Crypto-store exclusion was missing and is handled separately. macOS media deny-list and Linux Pulse socket exposure remain open. |
| Performance and resource limits | Timeline layout is virtualized and decoding is in the content worker. Active timeline retention remains unbounded while scrolled back. Shared document storage avoids whole-message copying; visible highlight work and user lookup remain. Existing availability/resource-limit review is retained. |
| Accessibility and integration matrix | AT-SPI coverage is not an Orca pass. Stock GUI/client interoperability does not establish a two-stock-client baseline. Tested restart, loss/jitter and worker-kill scenarios are separated from network-only drops, explicit reordering, hot device/format changes, complete graph snapshots and worker stalls. |
| Native desktop targets | Both Darwin architectures and universal app cross-builds are verified; native Apple audio/Seatbelt/screen-reader tests remain open. Windows VM tooling and scripted updater checks exist; the full unattended desktop suite and UDP path remain open. |
| Server and storage | Compatibility server, persistent SQL history, operator CLI and Murmur import exist. Requested E2EE default is not the current compatible default; enabling it awaits protected media. Existing history, moderation and rate-limit gaps remain visible. |
| Identity, transparency and MLS | Preserve experimental status, strict plaintext/voice rejection, and missing historical roster/order proofs. Distinguish worker linking primitives from server mailbox/UI and simultaneous authenticated device sessions. Witness deployment and recovery/sync remain separate work. |
| Dependencies and release | Correct advisory prose to match scoped exceptions. Release helper tools now follow `flake.lock` and Nix invocations use two cores. Workflow existence does not prove signing-secret setup or a published release. No release was published during this audit. |
| SDK and other clients | SDK already models encrypted conversations and send/open requests. Identity/trust UI, mobile crypto and I/O adapters, bindings and the actual iOS app remain open. TUI and browser foundations do not constitute complete mobile/browser clients. |
| Files, video, recovery and preferences | Retain reviewed designs and user choices. No new transport, recovery provider, secret synchronization or encrypted-media behavior is claimed. Relay-only must continue to omit peer discovery and IP advertisement. |

Cross-checking the active RTC dependency graph confirmed AWS-LC remains selected
through `str0m-rust-crypto` → `dimpl`'s `rcgen` feature. Selecting RustCrypto does
not close that TODO. Separately, sealed crypto files can currently be written above
their 16 MiB read cap; the restart-availability risk is recorded as an open store
design task, not silently folded into the small ownership-lock fix.

## Completed work from this pass

- Release helper tools follow the repository's pinned nixpkgs even after changing
  directory; Nix release invocations use two cores. `actionlint` passed. No signing
  secret was inspected or release published.
- The iOS tooling has 14 passing host regression tests. Host and confined cached
  container KVM creation, disk/RAM preflight and immutable registry metadata checks
  passed. The [guest evidence gates](ios-vm.md) remain open: no Apple media, macOS
  boot, SwiftUI rendering, XCTest or Rust library linkage was demonstrated.
- Crypto-store exclusion now acquires a persistent OS lock before any key/state
  access and holds it for the store lifetime. All 49 crypto-worker tests passed,
  including cross-process contention and crash release, key-access ordering,
  independent directories and link/MLS restarts. An independent source review found
  no concrete issue. Native Windows/macOS locking behavior still needs runtime
  validation. This supersedes the pre-fix missing-lock finding in the source ledger.
- The scoped crypto build used Nix per-crate derivations, added approximately
  312 MiB of apparent outputs and left zero scratch data; it created no Cargo target.
  Nix ran with two cores and two jobs. No new dependency or unsafe block was added.
- SDK, Windows migration, fuzzing and architecture prose now describe the existing
  implementation and its limits. Nix option formatting and whitespace checks passed.

## Next dependency path

1. Linux KVM/container preflight → supplied macOS guest and pinned Xcode → graphics,
   SwiftUI rendering and XCTest proof in iOS Simulator. Host KVM success alone does
   not close the guest milestone.
2. Small UniFFI API and iOS build → SwiftUI text/history UI and reviewed iOS audio
   adapter in parallel. Keep interfaces stable before assigning those two tracks.
3. Voice/chat/reconnect against private legacy Mumble and compatible Nuschl servers.
4. Background audio, device routes, accessibility and physical-device validation.
   These are distinct from simulator proof and can involve external testers.
5. Authenticated per-device server sessions before linked simultaneous logins or
   synced auto-connect. Recovery/vault, production MLS history and video retain their
   own prerequisites and do not block the first client with a separate device account.

Before assigning additional production work, review shared `native.rs`, media
supervisor, protocol/IPC types, worker startup and generated Cargo/Nix files. UI and
audio tracks cannot independently redesign these same boundaries. Keep one owner
for shared API changes, then delegate callers against an agreed interface.
