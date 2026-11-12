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
That fingerprint does **not** verify Apple's signature, the archive's declared
version, payload integrity or completeness. Those checks still precede use.

The later experiment also acquired recovery media and reached a Recovery GUI.
Retained private-VM console screenshots show `sw_vers` reporting macOS
14.6.1/23G93 and `uname -m` reporting x86_64. The recovery evidence records Apple
signed-chunklist verification; images and logs are under `~/vms/nuschl-ios` and
`~/.cache/nuschl-ios-vm`. This proves neither an installed macOS guest nor
Metal, Xcode, Simulator, SwiftUI or XCTest. The owned container currently reports
`exited`; it has not been restarted for this inspection.

The existing status helper reports a capability-profile mismatch because it
expects a literal `all` in `CapDrop`. Podman instead reports an explicit dropped
capability list here; the retained OCI configuration has an empty capabilities
object and `noNewPrivileges: true`. Correct and test that diagnostic before
using it as a launch gate. Inspection of a stopped configuration is not a runtime
effective-capability proof.

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
| Candidate macOS | Sonoma **14.5**, x86_64 | An exact guest-version check, not a downloaded or booted guest |
| Candidate Xcode | **16.2**, with iOS SDK **18.2** | A compatible version selection; actual build number/archive hash must still be recorded |
| Simulator runtime | iOS **18.2**, identifier `com.apple.CoreSimulator.SimRuntime.iOS-18-2` | Must be installed and available independently of the SDK |
| Rust simulator target | **`x86_64-apple-ios`** | Intel Simulator; `aarch64-apple-ios` is a physical-device target |

Apple lists Xcode 16.2 as supporting macOS Sonoma 14.5 through Sequoia 15.x and
including the iOS 18.2 SDK. This older, fixed combination is an initial experiment,
not a recommendation for eventual store distribution.
[Apple's Xcode requirements](https://developer.apple.com/xcode/system-requirements)
describe supported combinations; they do not validate a QEMU guest.

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
