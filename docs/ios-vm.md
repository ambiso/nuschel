# Linux-hosted iOS feasibility experiment

iOS is the first mobile implementation track. The proposed environment is a macOS
guest on this Linux host, followed by Xcode and an Intel iOS Simulator. A developer
does not need to own a Mac or iPhone to run these host checks. The environment is
**not yet proven suitable for SwiftUI or Simulator**. `tests/ios_vm.py` separates
observations from that eventual proof; it does not install software, pull images,
boot macOS, accept credentials, or implement the mobile client.

Latest local checkpoint (2026-11-12): the user supplied `Xcode_16.2.xip` in the
repository root, excluded by `.gitignore`. It is 2,969,019,672 bytes; its locally
measured SHA-256 is
`0e367d06eb7c334ea143bada5e4422f56688aabff571bedf0d2ad9434b7290de`.
The confined checks below now verify its Apple Software Update signature,
certificate chain/current CRLs, signed payload checksums and complete compressed
payload stream. Actual bundle plists declare **Xcode 16.2, build 16C5032a**, and
the iPhoneSimulator **18.2 SDK**. Nothing was extracted, installed or executed.
The SHA-256 remains a local reproducibility fingerprint, not a separately
Apple-published SHA-256 checksum. Native Apple `xip` acceptance remains untested.

The later experiment also acquired recovery media and reached a Recovery GUI.
Retained private-VM console screenshots show `sw_vers` reporting macOS
14.6.1/23G93 and `uname -m` reporting x86_64. The recovery evidence records Apple
signed-chunklist verification; images and logs are under `~/vms/nuschl-ios` and
`~/.cache/nuschl-ios-vm`. This proves neither an installed macOS guest nor
Metal, Xcode, Simulator, SwiftUI or XCTest. The owned container currently reports
`exited` after the controlled read-only resume below; it stopped cleanly with
exit 0. No installed OS was created.

The corrected status helper accepts Podman's expanded dropped-capability list
only with the matching, bounded local OCI configuration proving empty capability
sets and `noNewPrivileges: true`; it still rejects added capabilities and unsafe
mount/device/port profiles. The stopped owned container passes configuration
checks. The controlled resume also passed all five live capability/NNP checks.
After stopping, `live_effective_verified` returns **false** again, correctly
distinguishing current state from retained historical live evidence.
The launch plan still explicitly requests `--cap-drop ALL` and no new privileges.

The existing Windows harness (`tests/windows_vm.py`) establishes useful patterns:
dedicated disk state, private test accounts, bounded resources, and loopback-only
host forwards. Its unattended Windows installer/PowerShell agent cannot install
macOS or establish that Simulator graphics work. The new harness uses a separate
directory and strict SSH host-key verification instead of reusing that agent.

## Pinned inputs and candidate guest

| Input | Selection | What this establishes |
| --- | --- | --- |
| Docker-OSX source | [`aa05a2c9a06aabfc8542c1c373ad1ad0af74ede3`](https://github.com/sickcodes/Docker-OSX/tree/aa05a2c9a06aabfc8542c1c373ad1ad0af74ede3) | A fixed upstream reference for review |
| Docker-OSX amd64 image | `docker.io/sickcodes/docker-osx@sha256:3a3c82c79bc4e73531f819ccdfa4053b3084efd7c1f645678dbf8b4b3a24369c` | Frozen image bytes; source-to-image provenance has **not** been verified |
| Candidate installed macOS | Sonoma **14.8.9 / 23J631**, x86_64 | Explicitly repinned from 14.5 using current Apple metadata; Recovery 14.6.1 is not installed-OS evidence |
| Candidate Xcode | **16.2 / 16C5032a**, with iOS SDK **18.2** | Authenticated archive plists; no guest installation or execution |
| Simulator runtime | iOS **18.2**, identifier `com.apple.CoreSimulator.SimRuntime.iOS-18-2` | Must be installed and available independently of the SDK |
| Rust simulator target | **`x86_64-apple-ios`** | Intel Simulator; `aarch64-apple-ios` is a physical-device target |

Apple lists Xcode 16.2 as supporting macOS Sonoma 14.5 through Sequoia 15.x and
including the iOS 18.2 SDK. This older, fixed combination is an initial experiment,
not a recommendation for eventual store distribution.
[Apple's Xcode requirements](https://developer.apple.com/xcode/system-requirements)
describe supported combinations; they do not validate a QEMU guest.

## Confined archive inspection

`tests/ios_xip.py` supports only the observed XAR v1/RSA/SHA-1
`Metadata`+`Content` profile. The legacy signature authenticates the compressed
TOC checksum; its signed TOC binds the two payload SHA-1 checksums. All three
are checked, together with the complete file's local SHA-256. This is not a new
archive format or a claim of modern SHA-256 signature strength.

The trust anchor is independently retrieved from
[Apple PKI's Apple Inc. Root](https://www.apple.com/appleca/AppleIncRootCertificate.cer),
DER SHA-256
`b0b1730ecbc7ff4505142c49f1295e6eda6bcaed7e2c68c5be91b5a11001f024`.
An embedded self-signed certificate alone is never trusted. In addition to normal
chain and current-date checks, **both** the leaf and intermediate must carry
the production Apple Software Update EKU `1.2.840.113635.100.4.1`, with exactly
three certificates ending at that root. This follows
[Apple's `SecPolicyCreateAppleSWUpdateSigning` policy](https://github.com/apple-oss-distributions/Security/blob/main/OSX/sec/Security/SecPolicy.c)
and [Software Update certificate practice statement](https://images.apple.com/certificateauthority/pdf/Apple_Software_Update_CPS_v3.0.pdf).
Generic Apple-issued Developer ID/code-signing certificates do not satisfy this
policy. The observed leaf is `Software Update`, SHA-256
`e074d204ac2498e9dc904a7bc7ced8464119b79d05668028920583b1e896ebb4`.
The [Apple XAR implementation](https://github.com/apple-oss-distributions/xar/blob/main/xar/lib/signature.c)
describes the checksum/signature binding.

Supply current DER CRLs explicitly; the worker never fetches URLs from archive
data. On this run, the certificate's `http://crl.apple.com/softwareupdateca.crl`
and `http://crl.apple.com/root.crl` distribution points supplied valid signed CRLs.
The Software Update CRL is valid November 11–25, 2026; the root CRL is valid
August 25, 2026–January 7, 2027. Their signatures, issuers, validity intervals and
certificate serials passed `openssl verify -crl_check_all`. HTTP transport does
not confer trust; cryptographic verification is mandatory. The HTTPS accelerator
URLs returned 404, while Apple's linked `https://www.apple.com/appleca/root.crl`
served a correctly signed **expired 2025 CRL**. That input fails the helper;
the original failure is retained. Never bypass expiry or use an arbitrary old
verification date to make it pass. Re-fetch and recheck before later use.

```sh
python3 tests/ios_xip.py /absolute/path/Xcode_16.2.xip \
  --apple-root /disk/cache/apple-root.der \
  --root-crl /disk/cache/root.crl --software-crl /disk/cache/softwareupdateca.crl \
  --inspect-payload
```

The Linux wrapper requires Bubblewrap, `prlimit` and OpenSSL. It mounts only the
archive, supplied public trust material, helper and read-only Nix store. It
unshares networking, drops capabilities, sets no new privileges, clears the
environment and exposes no host home, VM disk, clipboard, audio or GPU. Native
XML/compression/crypto processing runs inside this boundary, not the host
coordinator. Temporary writes contain only small public certificate material.
Address space is capped at 256 MiB per process; output at 128 KiB; files at
1 MiB. Authentication/hash inspection has CPU 30 seconds per process and a
45-second whole-process-group deadline. Full payload inspection uses CPU
180 seconds per process and a 240-second whole-group deadline. Timeout/error
kills the owned process group. No archive path is extracted or followed.

Observed results:

| Check | Actual evidence |
| --- | --- |
| Complete signature/hash inspection | 4.65 seconds; zero effective/bounding capabilities, `NoNewPrivs=1` |
| Metadata | Archive format `Version=1`, declared file data 12,619,879,303 bytes; this is not an Xcode version |
| Complete payload inspection | 62.27 seconds, 755 checked independent XZ chunks, 12,651,889,664 expanded CPIO bytes |
| Archive layout | 141,981 entries, complete trailer and 457 zero-padding bytes; no extraction |
| Xcode Info/version plists | `com.apple.dt.Xcode`, short version 16.2, bundle version 23507; `ProductBuildVersion=16C5032a` (`DTXcodeBuild=16C5032`) |
| Simulator SDK | `SDKSettings.plist` reports `iphonesimulator18.2`, version 18.2; `simctl` and `xcodebuild` paths exist |

SDK presence is **not** an installed Simulator runtime, working Simulator,
SwiftUI rendering, Xcode startup or native Apple archive-policy proof.
Parser limits include 1 MiB TOC, bounded certificates/CRLs, 16 MiB PBZX chunks,
80 MiB LZMA decoder memory, 64 KiB output chunks, finite expanded-byte/entry
budgets and bounded selected plists. Unknown formats fail rather than falling
back to an unbounded extractor. Only original source bytes are hashed; no
repair, truncation or version inference changes the supplied archive.

Regression invocation (the source archive stays untouched; tamper tests require
disk-backed CoW copies):

```sh
TMPDIR=/disk/cache NUSCHL_XIP_ARCHIVE=/absolute/path/Xcode_16.2.xip \
NUSCHL_APPLE_ROOT=/disk/cache/apple-root.der NUSCHL_XIP_PAYLOAD=1 \
NUSCHL_ROOT_CRL=/disk/cache/root.crl NUSCHL_SOFTWARE_CRL=/disk/cache/softwareupdateca.crl \
NUSCHL_STALE_ROOT_CRL=/disk/cache/expired-root.crl \
  python3 tests/ios_xip_test.py -v
```

Public evidence/logs are retained under
`~/.cache/nuschl-ios-feasibility-next/`: `status.json`,
`xip-authentication.json`, `xip-payload.json`, `xip-revocation.json`,
`crl-retrieval.json`, the original `crl-verification.json` failure and
`xip-final-tests.log`. The private container-inspect JSON is mode 0600 and must
not be published because container environment fields may contain sensitive data.
The capability suite now has 20 host tests; all **18 archive tests passed** with the
explicit local-media inputs (73.69 seconds). The archive suite covers real signature,
payload, truncation, untrusted-root, stale/signed CRL and corrupt-CRL failures,
plus malformed/bounded parser cases. Without explicit supplied-archive/CRL
environment variables, those real-media tests are skipped, not counted as proof.

Docker-OSX documents headless operation and Xcode use, while GPU acceleration is
still on its TODO list. Its default virtual display therefore does not establish
Metal, Simulator rendering, or XCTest compatibility. The mutable `naked` and
`sonoma` tags returned `MANIFEST_UNKNOWN` during this assessment; `latest` resolved
to the immutable digest above. Neither mutable tags nor downloaded third-party
macOS disks are automatic inputs to this harness.
[Pinned upstream documentation](https://github.com/sickcodes/Docker-OSX/blob/aa05a2c9a06aabfc8542c1c373ad1ad0af74ede3/README.md)

## Workflow

Keep all images, firmware state and logs on disk. The default is
`~/vms/nuschl-ios`; `--state PATH` or `NUSCHL_IOS_VM_DIR` selects another dedicated
directory. Home/filesystem roots, mount-option delimiters, symlinks escaping the
media directory, and Linux RAM-backed filesystems are refused. These commands do
not create the state directory:

```sh
python3 tests/ios_vm.py preflight
python3 tests/ios_vm.py metadata
python3 tests/ios_vm.py status
python3 tests/ios_vm.py plan
python3 tests/ios_vm.py probe
```

`preflight` checks Linux/x86_64, real KVM API/version and VM creation, available
memory, local container runtime/storage, and free disk. Its default budget is
**256 GiB** for VM/image/workspace growth, with **64 GiB** left for the host;
at least **16 GiB currently available RAM** is required. A host-preflight pass
does not imply that a guest, Xcode or Simulator exists.

`metadata` fetches only a digest-verified registry manifest and config, each capped
at 1 MiB. It downloads no image layers. The selected image reports
**1,241,891,179 compressed layer bytes** (about 1.16 GiB), amd64, created
2025-11-11. Unpacking, recovery media, macOS, Xcode, simulator runtimes, DerivedData
and test results consume additional disk. Check both container storage and VM
storage before any explicit acquisition; no global installation or daemon changes
are needed by these checks.

`container-probe --image FULL_IMMUTABLE_IMAGE_ID_OR_DIGEST` optionally checks KVM
inside an **already cached amd64 image containing Python 3**. It never pulls.
The probe has no network or host file mounts, drops capabilities, disables privilege
escalation, and receives only `/dev/kvm`; limits are one CPU, 128 MiB and 32 PIDs.
It cleans up only its own randomly named container. It proves container-to-KVM
access, not a macOS boot. An arm64 cached image is deliberately refused.

`plan` prints a JSON argv and shell-quoted launch command for review. Missing
`macos.qcow2` and `BaseSystem.qcow2` are explicit blockers. Only these two files
may be bound into the container; neither may escape the dedicated directory.
The recovery image is read-only. The plan forces KVM without TCG fallback, six
vCPUs, 12 GB guest RAM, a 14 GB container ceiling, dropped capabilities and no new
privileges. It publishes SSH and VNC only on `127.0.0.1:50922` and
`127.0.0.1:5999`. It exposes no host microphone, sound device, camera, USB device,
X11/Wayland socket, SSH agent, home directory, container socket or source tree.

The plan **does not execute**. Before using it:

1. Acquire the selected Apple guest/recovery/Xcode/runtime materials, record their
   versions, build numbers, origin URLs and SHA-256 hashes, and verify those hashes.
   See the latest local checkpoint above for the supplied archive and recovery
   assets; the original host-only assessment predated them. The harness does not request
   personal Apple accounts, accept passwords, or treat public links as hash pins.
2. Explicitly acquire the pinned Linux container image within the assessed budget.
   Inspect its embedded `/home/arch/OSX-KVM/Launch.sh` and firmware/bootloader inputs.
   The source reference alone is not provenance for the registry image. Confirm
   image CPU features, permissions and media paths against the actual artifact.
3. Review the printed command. It bypasses upstream's default command, including
   automatic downloads, `sudo/chown` and serial generation, then executes the
   image's reviewed `Launch.sh`. That script and its virtual graphics remain
   untested here. The runtime's private container network has outbound access;
   loopback publishing is not an egress firewall. Do not use host networking or
   mount additional personal directories to make installation convenient.
4. Use only a disposable guest test account with no synced identity or personal
   credentials. Configure its SSH public key and verify the SSH host fingerprint
   from the guest console. Store its dedicated private key as
   `STATE/guest_ed25519` and verified host entry as `STATE/known_hosts`, both owned
   by the invoking user and mode 0600. For port 50922, the known-host entry names
   `[127.0.0.1]:50922`. There is no automatic `ssh-keyscan` trust acceptance.

Loopback VNC is not encrypted/authenticated by this plan: other users on the host
may reach it. The guest must remain disposable and contain no credentials.
KVM and QEMU are substantial native-code/kernel capabilities; this is a constrained
test setup, not a claim that a container or VM eliminates those risks. It is not a
new exception to the application's memory-safety policy or the iOS sandbox review.

`status` inspects only `nuschl-ios-feasibility`; it does not change it. Non-loopback
published ports, unexpected host bind mounts/devices, added capabilities,
privileged mode and host networking are reported as boundary problems. An absent
or inaccessible container is distinguished from a proven guest.
For the owned local Podman container, capability normalization reads only the
exact container's regular, bounded OCI configuration beneath Podman's independently
reported storage root. A running status additionally reads all five capability
sets and `NoNewPrivs` from `/proc`, binding the observation to a stable PID/start
identity across two runtime inspections. Stopped configuration evidence cannot
stand in for that live check.

`probe` uses strict, noninteractive SSH to the dedicated account at **127.0.0.1
only**. User SSH configuration, agent access, password prompts and forwarding are
disabled. It reads architecture, macOS/Xcode/SDK versions, available simulator
runtimes/devices and display metadata. Every process has a wall-time and output
ceiling; the guest commands do not boot devices, accept licenses, install tools,
change settings or build projects. Runtime listings are not rendering evidence.
Exit status 2 denotes an error, missing material or boundary failure; status 0
only means the requested inspection completed. Check the explicit proof fields.

## Evidence and remaining gates

Initial host assessment on 2026-11-11, before the later Recovery experiment:

- Linux x86_64, QEMU 11.1.2, rootless Podman 6.1.4; host KVM API 12 and
  `KVM_CREATE_VM` succeeded.
- The confined cached-container KVM probe also succeeded, with no network/mounts
  and no image download. Cached probe ID:
  `sha256:44c08198793074dcc111e82cc55999b01eb52a0883ab3844a7d68fb97cef6602`.
- VM and container image storage are btrfs, not RAM-backed `/tmp`; the host had
  approximately 685 GiB free and 39 GiB available RAM at assessment time.
- Registry metadata was verified. Docker-OSX layers, Apple recovery/guest,
  Xcode and Simulator were **not downloaded**. No guest exists; the read-only
  guest probe fails explicitly because its dedicated SSH identity is absent.

The environment cannot be called viable until the following **actual guest
evidence** is retained, with exact versions/builds, commands, exit codes and logs:

1. The selected macOS guest boots and reports x86_64; Xcode, iOS SDK and installed
   runtime match the recorded profile, with installer/media hashes retained.
2. A newly created, dedicated iOS Simulator device completes boot. Record graphics
   capability and relevant failures; a WindowServer desktop or listed runtime is
   insufficient, and no GPU support is inferred from the host GPU.
3. A minimal SwiftUI feasibility app builds for **x86_64 Simulator**, installs,
   launches, and renders a captured, inspected frame containing its expected text.
4. An automated **XCTest UI test** finds that app's named controls, taps one,
   observes a visible state change, and passes. Retain its `.xcresult` and screenshot.
5. The actual Rust SDK's `x86_64-apple-ios` library links into that simulator app
   and a binding call returns a checked result. No SDK API is implemented here.

Do not lower a failed graphics/SwiftUI/XCTest gate to merely “macOS boots.” Record
the failing command and determine another Linux-hosted configuration before
committing to this environment. Physical-device microphone, echo cancellation,
Bluetooth, background/locked-phone behavior, battery and VoiceOver checks remain
separate; simulator success would not prove them or require owning Apple hardware
for the initial SDK/UI work.

Host boundary regression checks (no Apple guest or downloads required):

```sh
TMPDIR=/path/on/disk python3 tests/ios_vm_test.py -v
```

## Controlled read-only resume (2026-11-12)

Only the existing container was resumed, 12:45:09–12:53:52 +01:00, after disk-backed
CoW backups of both host qcow2 files and copies of firmware/OpenCore state.
Preflight: 629.3 GB disk free, 39.7 GB available RAM, all loopback ports free.
A five-second resource guard with an exact container ID/start-time lease and
10-minute ceiling observed minima of 628.37 GB disk free and 35.66 GB available
RAM. The container stopped cleanly, exit 0; its guard and one-shot report receivers
are terminal. No new VM, installation, partitioning, license acceptance or login.
Both host qcow2 hashes are unchanged from their backups.

Actual live init/QEMU PID 696334 had all five capability masks zero and
`NoNewPrivs=1`, with stable PID/start identity. QMP reports QEMU 10.1.2, KVM enabled;
`AUDIO_DRIVER=none`. No host audio/GPU/USB/display/credentials were added.

| Actual drive | Persistence / guest mapping |
| --- | --- |
| MacHDD `/image` | Persistent writable plain qcow2 → `~/vms/nuschl-ios/macos.qcow2`; no backing/overlay. Guest **disk1**, blank 80 GiB (85.9 GB), no filesystem and not currently installable |
| OpenCoreBoot | Guest disk0, 402.7 MB EFI/Linux; `snapshot=on`, original OpenCore backing |
| InstallMedia | Guest disk2, 3.2 GB Recovery, mounted read-only HFS+; `snapshot=on`, read-only host BaseSystem backing |
| OVMF_CODE / OVMF_VARS | Code read-only; variables writable in retained container layer, backed up separately |

No global `-snapshot` flag exists. OpenCore/Recovery temporary overlays
`/var/tmp/vl.7HSJY3` and `/var/tmp/vl.4MNJY3` are auto-unlinked while open.
Their actual fd13/15 sizes were about 196.6 KB each (200,704 allocated bytes);
initial pathname stat correctly failed ENOENT. Target-disk writes would persist;
Recovery/OpenCore overlay writes would not. No boot options were changed.

Recovery reports 14.6.1/23G93 x86_64, no APFS containers, and a visible
1920×1080 GUI. `ioreg` shows `IONDRVFramebuffer` and no `IOAccelerator` entries;
`system_profiler` is absent. This does not prove Metal or installed-OS graphics.
`/Install macOS Sonoma.app` has assistant version **19.6.02**, not an OS-version
promise; it contains no SharedSupport/InstallInfo payload, and Recovery has no
`softwareupdate` utility. An initial dropped keyboard character broke one find
probe; the slower repeat and the original failure are retained.

A bounded read-only official Apple catalog fetch identifies product **140-88558**.
[Its distribution metadata](https://swdist.apple.com/content/downloads/13/30/140-88558-A_TN3FIE7J63/p61cqj3ktmcqilz3q64ls4ekp1os9c8qbp/140-88558.English.dist)
explicitly declares **Sonoma 14.8.9 / 23J631**, making that the new exact candidate.
Its 10,317 bytes have SHA-256
`d38a7d1f71bfc39723000a13375bd824c9dfac41818cabeed6acc816e0422edd`.
The Apple catalog was 509,149 compressed / 7,051,224 decoded bytes, SHA-256
`818e1dd43f59c226f27426468875281f7cf1870cbd69b90665db000d3e7a8f76`.
Only tiny metadata was fetched; no installer package was downloaded.

The proposed [InstallAssistant.pkg](https://swcdn.apple.com/content/downloads/13/30/140-88558-A_TN3FIE7J63/p61cqj3ktmcqilz3q64ls4ekp1os9c8qbp/InstallAssistant.pkg)
is **13,654,790,295 bytes**, expected catalog SHA-1
`10632733f9b4ab1397fffc98b53e6790e4c2c1b0`. Its `.pkm` declares
`com.apple.pkg.InstallAssistant.macOSSonoma`. Whole-package SHA-256, appropriate
Apple package-signing policy and payload validation are still required after a
separately bounded acquisition. Metadata hashes do not replace artifact checks.
The next stages remain acquisition/authentication, then a reviewed target-disk/APFS
and installation plan. The exact checker rejects Recovery 14.6.1 even with
otherwise matching Xcode/SDK/runtime listings; no version check was relaxed.

All receipts are under `~/.cache/nuschl-ios-feasibility-next/`: `live-status.json`,
`pre-resume-backup.json`, `qmp-initial.json`, `drive-persistence.json`,
`guest-report-response*.jsonl`, `recovery-ready.png`,
`recovery-version-disk-proof.png`, `post-resume-state.json`,
`post-resume-disk-hashes.json`, `resume-guard.log`, `catalog-retrieval-2.json`,
`sonoma-catalog-candidates.json`, `sonoma-metadata-retrieval.json`, and
`sonoma-installer-metadata.json`. Backups are in the 0700 `pre-resume-backup/`
subdirectory. No owned guest, receiver, guard or build remains running.
