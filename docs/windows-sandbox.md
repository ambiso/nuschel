# Windows worker sandbox

Nuschel's workers (content, storage, crypto, media, transfer) run in AppContainers
on Windows, each in a job object of its own. This page records how, why, what was
verified in a Windows 11 VM, and what remains. SECURITY.md has the summary.

## How a worker starts

Windows has no `exec`, so a helper cannot set up confinement and then become the
worker, as bwrap and `sandbox-exec` do. Each worker executable is therefore its own
launch stage. The supervisor (`crates/ipc/src/host.rs`) starts it as

    nuschel-<kind>-worker.exe --nuschel-sandbox --profile NAME [policy] -- [worker arguments]

with an empty environment apart from `SystemRoot`, `WINDIR`, `TEMP` and `TMP`, no
console window, and inside the supervisor's kill-on-close job (`host::contain`, as
before). The first thing every worker's `main` does is
`nuschel_sandbox_windows::launch_if_requested()`. With the flag, that code:

1. creates the AppContainer profile `NAME`, or derives its SID if it exists;
2. grants the container read and execute on its own executable, and Modify
   (inherited) on each `--writable` directory, leaving every other entry alone and
   writing nothing if the access is already there;
3. opens `--read-file` read-only, if given, and appends `--file-handle N`;
4. creates a job: kill on close, one active process, die on unhandled exception,
   the committed-memory limit (`--memory`), the CPU-time limit (`--cpu`) and the UI
   restrictions listed below;
5. creates the same executable, suspended, with only the worker arguments, in the
   AppContainer with the policy's capabilities, through an attribute list with
   `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
   (the three standard handles and the offered file, nothing else),
   `PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY` (no child processes),
   `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` and, for a less privileged
   AppContainer (`--less-privileged`),
   `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY`; assigns it to the
   job; resumes it;
6. waits, and exits with the worker's exit code.

The launch stage reads no untrusted input: the worker's pipes go straight to the
confined process. When the supervisor kills it (a timeout, a mute change, Nuschel
exiting), its job handle closes and the job ends the worker.

The policy is plain data (`nuschel_ipc::windows_sandbox::Policy`), encoded on the
command line by the supervisor and parsed by the launch stage. Its encoding, the
command-line quoting and the per-worker policies are unit-tested on every platform.

## Policies

| Worker | AppContainer profile | Kind | Capabilities | Files | Committed memory |
| --- | --- | --- | --- | --- | --- |
| content | `nuschel.content` | less privileged | none | none | 512 MiB, plus the request's CPU seconds |
| storage | `nuschel.storage.<hash of the directory>` | less privileged | none | the history directory | 1 GiB |
| crypto | `nuschel.crypto.<hash of the directory>` | less privileged | none | its `nuschel-identity` directory | 1 GiB |
| media | `nuschel.media` | full | `microphone` | none | 1 GiB |
| transfer | `nuschel.transfer` | full | `internetClient`, `privateNetworkClientServer` | the offered file, as a read-only handle | 1 GiB |

Every container may also use its own folder (`%LOCALAPPDATA%\Packages\<profile>\AC`),
which is its working directory and `TEMP`, and read the system files every
AppContainer may read (the Windows directory, Program Files). A full AppContainer
also gets everything granted to "ALL APPLICATION PACKAGES"; a less privileged one
(LPAC) only what is granted to "ALL RESTRICTED APPLICATION PACKAGES" or to itself.

Decisions:

- **Less privileged AppContainers for content, storage and crypto.** The VM check
  showed that a full AppContainer can read the user's environment variables in the
  registry (`HKCU\Environment`, granted to "ALL APPLICATION PACKAGES"), where some
  people keep tokens. These three workers need nothing of what that group is
  granted, so they opt out of it, and they then cannot read that key either (nor
  start Winsock). Media and transfer stay full AppContainers: COM, which WASAPI
  needs, fails with "access denied" in a less privileged one (CPAL panics), and so
  does Winsock (`WSAStartup` fails with 10107). Making them less privileged would need
  capabilities such as `lpacCom` and testing the audio and network stacks under
  them.
- **One profile per worker kind**, created once and reused. Storage and crypto get
  one per directory instead, so the workers of two Nuschel profiles (for example
  `--profile-dir`) can never open each other's history or keys: a grant belongs to a
  container, not to a process.
- **Transfer gets `internetClient` and `privateNetworkClientServer`**: outgoing
  connections to the rendezvous, the relay and peers, and connections both ways on
  the local network, so that two Nuschel users on one LAN can use magic-wormhole's
  direct transit instead of a public relay (the Linux and macOS sandboxes allow the
  LAN too). It does not get `internetClientServer`: nothing on the Internet can
  connect to it. No AppContainer can reach loopback, so the transfer worker cannot
  reach local services either.
- **The offered file goes by handle.** The launch stage opens it with the user's
  rights and passes the handle; the file's permissions never change. The worker
  reads it at explicit offsets (`FileExt::seek_read`), so pickups running at once do
  not share a file position; Linux and macOS open `--file` once and read it the same
  way.
- **Media runs in an AppContainer.** WASAPI capture and playback through CPAL work
  there with the `microphone` capability (verified below), so the restricted-token
  fallback was not needed and is not implemented. Windows' microphone privacy setting
  still applies: the media worker is listed under "Let desktop apps access your
  microphone".
- **No console** (`DETACHED_PROCESS`): a hidden console (`CREATE_NO_WINDOW`) needs
  a `conhost.exe` child, which the child-process policy refuses, and the worker then
  fails to start with `STATUS_DLL_INIT_FAILED`.
- **Environment**: `SystemRoot`, `windir`, `TEMP` and `TMP` (the container's) and
  `LOCALAPPDATA`; without `LOCALAPPDATA`, `CreateProcessW` refuses an AppContainer
  with `ERROR_ENVVAR_NOT_FOUND`.
- **UI restrictions** of the job: no clipboard (read or write), no global atoms, no
  USER handles of processes outside the job, no desktop creation or switching, no
  display or system parameter changes, no logoff or shutdown.
- **Mitigations**: no extension-point DLLs (AppInit and the like), no non-system
  fonts, no images from remote shares or with a low mandatory label.
- **Failure**: if any step fails (no AppContainer support, a policy forbidding
  them, a directory it cannot grant), the launch stage prints the reason on stderr
  and exits with 125 without starting the worker. That is what Linux does without
  bwrap: the worker is unavailable, never unconfined. Windows 10 1607 or newer is
  required (the child-process policy and `DeriveCapabilitySidsFromName`).

## Why an own crate with unsafe code

Nuschel's own crates forbid `unsafe`; SECURITY.md allows a small supervisor adapter
for OS sandbox APIs, preferring safe wrappers. No maintained safe wrapper does what
is needed here, so `crates/sandbox-windows` is that adapter. The crates considered
(November 2026):

| Crate | What it covers | Why not |
| --- | --- | --- |
| `rappct` 0.13.3 (MIT) | AppContainer profiles, capabilities, ACLs, launch with job limits | Eleven releases within three days in October 2025 and none since, by one author. Its launcher creates its own pipes or inherits all handles; it cannot hand over existing handles, so every frame would have to be relayed. Its launch code opts out of `unsafe_op_in_unsafe_fn` and `undocumented_unsafe_blocks`, reads test-only environment variables (`RAPPCT_TEST_*`) on the production path, and has no UI restrictions or child-process policy |
| `win-custody` 0.1.2 (MIT or Apache-2.0) | Explicit handle lists, suspended start into a job, tokens, SDDL | Careful unsafe code, but no AppContainer and explicitly "not a sandbox". The security capabilities must sit in the same attribute list as the handle list, so it cannot be combined with another crate. One month old, one author |
| `win32job` 2.0.3 (already used) | Job objects | No process memory, active-process or UI limits in its API, no process creation |
| `heel`, `crabpot-windows`, `cageforge-windows`, `sandboxrs-windows`, `zagens-windows-sandbox` | Sandboxes for AI agents' shell commands | 6,000 to 41,000 lines with firewall rules, PTYs and policy engines, far more unsafe code than this needs, and fast-moving (`heel` released four versions on 2026-11-11) |
| `std` | Process creation | `CommandExt::raw_attribute` is still unstable (Rust 1.99), and `Command` inherits every inheritable handle |

`nuschel-sandbox-windows` is about 900 lines (with formatting and comments) over
`windows-sys` (Microsoft, MIT or Apache-2.0, already in the dependency graph). It
does not use the workspace lints, because `forbid` cannot be lifted for one module;
it denies `unsafe_code` itself and allows it only in `src/win32.rs`, with
`unsafe_op_in_unsafe_fn` denied and Clippy's `undocumented_unsafe_blocks` and
`multiple_unsafe_ops_per_block` as errors: one unsafe operation per block, each with
a `SAFETY:` comment. Memory Windows allocates is copied into Rust values and freed
where it was received, handles become `OwnedHandle` at once, and the attribute
list borrows what Windows keeps pointers to for the list's lifetime. `cargo-deny`
lets only the five worker crates depend on it. On other platforms it compiles to a
constant.

## Verified in the Windows VM

Windows 11 Enterprise 25H2 (build 26200) in `tests/windows_vm.py`, two vCPUs, a QEMU
HDA sound card without a host backend, no network except forwards into host
loopback. `crates/sandbox-windows/tests/confinement.rs` (see tests/README.md) starts
itself through `nuschel_ipc::host`'s real commands as each worker kind and probes
from the inside. Result on 2026-11-11, every worker kind:

| Attempt | content | storage | crypto | media | transfer |
| --- | --- | --- | --- | --- | --- |
| Token is an AppContainer | yes | yes | yes | yes | yes |
| Read a file in Documents | denied | denied | denied | denied | denied |
| List the user's profile | denied | denied | denied | denied | denied |
| Write to the profile, `C:\Users\Public`, the user's `TEMP` | denied | denied | denied | denied | denied |
| Write in its own directory | n/a | allowed | allowed | n/a | n/a |
| TCP connection to 10.0.2.100:80 | denied (Winsock does not start) | denied (same) | denied (same) | denied (WSAEACCES) | allowed |
| Start `cmd.exe` | denied (blocked) | denied | denied | denied | denied |
| Open the supervisor's process to query or terminate | denied | denied | denied | denied | denied |
| Commit more memory than its limit | denied | denied | denied | denied | denied |
| Read the offered file by handle | n/a | n/a | n/a | n/a | allowed |
| Read `HKCU\Environment` (the user's environment variables) | denied | denied | denied | allowed | allowed |
| Read `HKCU\...\CurrentVersion\Run` (autostart) | denied | denied | denied | denied | denied |
| Create a key under `HKCU\Software` | denied | denied | denied | denied | denied |
| Read text the test put on the clipboard | denied | denied | denied | denied | denied |

`EnumProcesses` from inside returns two process ids (not the system's list), and the
launch stage refuses a policy it cannot apply (exit 125). The real workers in the
same sandboxes: content decoded a PNG; storage appended and read back history;
crypto created an identity; media opened the default WASAPI input and output (on
the VM's virtual sound card, which delivers silence); transfer got a code from a
magic-wormhole mailbox server on host loopback (through a guest forward), sent the
offered file to the reference CLI on the host through the transit relay, and
received a file from it.

The real desktop then ran against a stock Murmur on host loopback: an image and
legacy HTML from another client rendered, a message typed in the VM was sent and
history was written in the granted directory (`history.sqlite3` and its WAL), and
unmuting replaced the media worker with one that captured (Windows showed the
microphone in use). Inspecting the processes showed each worker as a normal-token
launch stage with an AppContainer child (media with one capability). After the
switch to less privileged containers the desktop was restarted: the previous
session's history (with its image) came back from the storage worker and rendered,
and a new image message rendered. The debug build reported "Playback stuttered"
after unmuting; the release build from `nix build .#windows` (taken before the
less-privileged change; media is unchanged by it) did not, so that is the
unoptimised build, not the sandbox.

## What remains

- Workers share the user's desktop. The job's UI restrictions keep them away from
  other processes' windows and the clipboard, but a worker could still create
  windows of its own; there is no separate desktop and no win32k lockdown (`ole32`
  and `userenv` load `user32`). Arbitrary Code Guard is not enabled either.
- Media and transfer, as full AppContainers, can read what "ALL APPLICATION
  PACKAGES" may read, including the user's environment variables
  (`HKCU\Environment`). Beyond the keys and folders in the table above, what that
  group and "ALL RESTRICTED APPLICATION PACKAGES" may read (system folders,
  registry) was not audited for Nuschel.
- The media worker can use whatever the Windows audio service offers it, as on Linux
  with an audio socket; capture is subject to Windows' microphone privacy setting.
- The transfer worker may connect anywhere on the Internet and on the LAN, as on the
  other platforms (the peer's transit hints decide where).
- The launch stage is started by `std`'s process API and so inherits every
  inheritable handle of the desktop process; it is trusted code and passes none of
  them on.
- AppContainer profiles stay registered when a Nuschel profile is deleted, and the
  read/execute entries on the worker executables stay (they are written again after
  an update replaces the files). An installation on a network share cannot start
  workers (remote images are refused).
- Only tested in a VM, without a physical audio device. The checks used a cargo
  cross build with the flake's toolchain and MinGW; the `.#windows` package was
  built and run (desktop, media) before the less-privileged change only.
