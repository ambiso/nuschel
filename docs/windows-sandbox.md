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
2. grants the container read and execute on its own executable, and on each
   `--writable` directory the right to list it and add and delete entries in it
   plus Modify (inherit-only) on everything inside, leaving every other entry
   alone and writing nothing if the access is already there. Each path is opened
   once, without following junctions or links, and refused if it is one; the
   DACL is read and written through that handle, and checked after writing;
3. opens `--read-file` read-only, if given, and appends `--file-handle N`; the
   worker takes over a duplicate of that handle (`inherited_file`);
4. creates a job: kill on close, one active process, die on unhandled exception,
   the committed-memory limit (`--memory`), the CPU-time limit (`--cpu`) and the UI
   restrictions listed below;
5. creates the same executable, suspended, with only the worker arguments, in the
   AppContainer with the policy's capabilities, through an attribute list with
   `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, `PROC_THREAD_ATTRIBUTE_JOB_LIST`
   (so it is created inside its job, never outside it),
   `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` (the standard handles that are not
   consoles, and the offered file; nothing else),
   `PROC_THREAD_ATTRIBUTE_CHILD_PROCESS_POLICY` (no child processes),
   `PROC_THREAD_ATTRIBUTE_MITIGATION_POLICY` and, for a less privileged
   AppContainer (`--less-privileged`),
   `PROC_THREAD_ATTRIBUTE_ALL_APPLICATION_PACKAGES_POLICY`; resumes it;
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
| math | `nuschel.math` | less privileged | none | none | 512 MiB, and 120 CPU seconds over its life (it is restarted), as on Linux |
| storage | `nuschel.storage.<hash of the directory>` | less privileged | none | the history directory's contents (the directory itself: list, add, delete entries) | 1 GiB |
| crypto | `nuschel.crypto.<hash of the directory>` | less privileged | none | its `keys` directory's contents (likewise) | 1 GiB |
| media | `nuschel.media` | full | `microphone` | none | 1 GiB |
| transfer | `nuschel.transfer` | full | `internetClient`, `privateNetworkClientServer` | the offered file, as a read-only handle | 1 GiB |

Every container may also use its own folder (`%LOCALAPPDATA%\Packages\<profile>\AC`),
which is its working directory and `TEMP`, and read the system files every
AppContainer may read (the Windows directory, Program Files). A full AppContainer
also gets everything granted to "ALL APPLICATION PACKAGES"; a less privileged one
(LPAC) only what is granted to "ALL RESTRICTED APPLICATION PACKAGES" or to itself.

The history and keys directories are siblings in `%LOCALAPPDATA%\Nuschel\Nuschel\data`
(`Profile::history_dir`, `Profile::keys_dir`; crates/session/src/layout.rs), next to
the Mumble identity, which no worker is granted. Versions before that layout kept the
history directly in `data` and the keys in `%APPDATA%\Nuschel\Nuschel\config\nuschel-identity`;
the first start moves them, and the new directories get containers of their own
(their names hash the path). The grant an earlier version gave its storage container
on `data` itself stays, inherited by what is created there since; no current worker
runs in that container (SECURITY.md, "Where secrets live").

Decisions:

- **Less privileged AppContainers for content, math, storage and crypto.** The VM check
  showed that a full AppContainer can read the user's environment variables in the
  registry (`HKCU\Environment`, granted to "ALL APPLICATION PACKAGES"), where some
  people keep tokens. These four workers need nothing of what that group is
  granted, so they opt out of it, and they then cannot read that key either (nor
  start Winsock). Media and transfer stay full AppContainers: COM, which WASAPI
  needs, fails with "access denied" in a less privileged one (CPAL panics), and so
  does Winsock (`WSAStartup` fails with 10107). Making them less privileged would need
  capabilities such as `lpacCom` and testing the audio and network stacks under
  them.
- **Directory grants cannot be redirected.** The launch stage runs with the user's
  rights and writes DACLs, so a worker must not be able to steer it: a directory
  is opened without following reparse points and refused if it is a junction or
  link, and its DACL is read and written through that one handle. The container
  gets no DELETE, WRITE_DAC or attribute rights on the directory itself, so it
  cannot remove or move it. An older, broader grant (Modify on the directory
  itself) is revoked and replaced.
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

`nuschel-sandbox-windows` builds on `windows-sys` (Microsoft, MIT or Apache-2.0,
already in the dependency graph). It does not use the workspace lints, because
`forbid` cannot be lifted for single modules; it denies `unsafe_code` itself, and
`unsafe_op_in_unsafe_fn`, `missing_safety_doc`, Clippy's `undocumented_unsafe_blocks`
and `multiple_unsafe_ops_per_block` (one unsafe operation per block, each with a
`SAFETY:` comment), `as_conversions`, `ptr_as_ptr`, `ptr_cast_constness`,
`borrow_as_ptr` and the `cast_possible_*` lints are errors. `cargo-deny` lets only the
six worker crates depend on it. On other platforms it compiles to a constant.

## Safety argument

The crate is split so that unsafe code is small, local and testable:

| Module | Unsafe | What it holds |
| --- | --- | --- |
| `plan` | forbidden | Everything computed without Windows: NUL-terminated UTF-16 strings, the command line (at most 32,768 units), the environment block, job limits, the handle list, SID and ACL parsing (whether a DACL already grants access is decided on a copied byte slice) |
| `launch` | forbidden | The steps, generic over the API, so tests run them against a mock |
| `api` | one call per method | One trait method per Win32 function; `unsafe` exactly when the function reads or writes through caller pointers or takes a handle's ownership. Its `# Safety` section is Microsoft's documented contract (linked), with what ReactOS's implementation shows where it has one |
| `os` | one call per block | Every unsafe call to Windows, each with a `SAFETY:` comment, and the guards: `OsMemory` releases what Windows allocated with the function its documentation names (`LocalFree`, `CoTaskMemFree`, `FreeSid`), `Handle` closes once, `AttributeList` deletes once |
| `raw` | three functions | The only pointer reads that are not Win32 calls: copies of bytes, a NUL-terminated string, or an array out of memory Windows returned, into owned values |
| `windows` | one block | `File::from_raw_handle` for the duplicate of the inherited file |

The arguments the comments make, in short:

- **Strings** passed to Windows are `WideCString`s: exactly one NUL, at the end,
  immutable, borrowed across the call.
- **Out-parameters** are fresh locals (`&raw mut`), set to null first and read or
  taken into a guard only after the function reports success; Microsoft's pages
  do not say whether they are written on failure (ReactOS mostly writes null or
  nothing).
- **Ownership**: a guard is created right after a successful call, before
  anything that can fail, so every allocation and handle is released exactly once
  on every path, after `GetLastError` has been read.
- **Lifetimes**: UpdateProcThreadAttribute keeps `lpValue` until the list is
  deleted, and CreateProcessW reads it then. `AttributeList<'v>` borrows every
  value for `'v` (and `Capabilities<'s>` the SIDs and the capability array it
  points to), has a `Drop` that deletes the list, and so the borrow checker
  rejects any value that would move or be dropped before the list is deleted.
  The list's buffer is a never-resized, 16-byte-aligned `Vec`.
- **Aliasing**: Windows only reads the SIDs, ACLs and values passed to it, which
  are behind shared borrows; the one buffer it may write, the command line, is an
  exclusive borrow, and out-parameters are locals nothing else references.
- **Conversions** are `try_from`, `cast_signed`/`cast_unsigned` on bit patterns, or
  `without_provenance` for handle values (never dereferenced); no `as`.
- **Handles from outside** (the standard handles, the offered file) are only
  borrowed; only handles Windows returned to this process are closed. On the
  worker side the handle value comes from the command line, so nothing can prove
  this process does not already own it (the current directory's handle, say): it
  is checked (an open disk file, not a directory, not a standard handle) and then
  only duplicated; the File owns the duplicate, and the inherited handle is never
  closed.
- **The worker is never outside its job**: it is created in it
  (`PROC_THREAD_ATTRIBUTE_JOB_LIST`), suspended, and terminated if it cannot be
  resumed.

## Miri

Miri cannot run Win32 calls, so the unit tests run the real launch steps against
`mock.rs`, a stand-in for Windows that follows the documented contracts (and
ReactOS where it has an implementation):

- it allocates what each function returns with that function's allocator and
  panics on a release with the wrong function, a double release, or a leak (left
  over when the mock is dropped); handles likewise;
- it reads every pointer as far as Windows would (strings to their NUL, a SID's
  whole length, an ACL's `AclSize`, a whole `STARTUPINFOEXW`) and writes every
  out-parameter, the attribute list buffer and the command line;
- like Windows it keeps the attribute list's value pointers and dereferences them
  only in CreateProcessW, so a value moved or dropped too early is a
  use-after-free under Miri;
- it rejects what Windows would reject or misuse: a handle list with
  `bInheritHandles` false or the reverse, a handle that is not inheritable or
  listed twice, a standard handle missing from the list, an unsorted environment,
  a command line of 32,767 units or more, unknown handles;
- after every successful call it leaves an odd last-error value, so reading
  `GetLastError` after anything but a failure shows;
- it can fail the n-th call of every fallible function, and a test fails each of
  them in turn (69 cases, with and without an existing profile), checking that the
  launch fails (except the tolerated grant on the executable), that a worker that
  cannot be resumed is terminated, and that nothing leaks or is released twice.

`just miri` runs these tests under Miri for `x86_64-unknown-linux-gnu` and for
`x86_64-pc-windows-gnu` (where the Windows-only modules are compiled too), with
Stacked Borrows, and also with Tree Borrows and strict provenance. A deliberate bug
(passing a copy of the handle list that is dropped before CreateProcessW) is reported
by Miri as a use-after-free.

What Miri does not cover: the Windows side of each call. The mock's ACL model is
simple (GRANT_ACCESS appends an entry, REVOKE_ACCESS removes the trustee's; no
merging, ordering or propagation), and it has no junctions or races. Whether Windows really
behaves as documented (what it reads, keeps or writes) is taken from the
documentation and ReactOS and checked only indirectly, by the VM test below; and the
Win32 implementation of the trait (one call per method) and `windows.rs` are compiled
but never executed under Miri.

## Independent review

A separate reviewer, given the code, Microsoft's pages and the ReactOS tree but not
these conclusions, read the crate for soundness. Its findings and what changed:

| Finding | Severity | Fix |
| --- | --- | --- |
| `inherited_file` was a safe function that could make a second owner of a handle named on the command line (the current directory's handle is a disk file too) | soundness | It now duplicates the handle and wraps only the duplicate; the inherited handle is never closed. Directories are refused |
| DACL grants went by path: a worker could replace its emptied directory with a junction, and the next launch (as the user) would grant the container Modify on the target and propagate it; the separate read and write were a race | security | Paths are opened once with `FILE_FLAG_OPEN_REPARSE_POINT` and refused if they are reparse points; `GetSecurityInfo`/`SetSecurityInfo` on that handle; no DELETE or attribute rights on the directory itself; older broader grants revoked |
| The worker existed, suspended, outside its job until `AssignProcessToJobObject`; the comment that kill-on-close covered a failed termination was wrong | security (minor) | `PROC_THREAD_ATTRIBUTE_JOB_LIST` creates it inside the job |
| A launch stage started from a terminal would pass console handles into the container | security (minor) | Console standard handles are not passed on |
| The command-line limit was 32,768 units; CreateProcessW's is 32,767 including the NUL | contract | Fixed |
| The claim that the safe API methods are fine for any handle value held only for memory safety (`-1` is the current process) | contract | The claim is narrowed; -1 is never passed |
| `STARTF_USESTDHANDLES` with `bInheritHandles` false when nothing is inherited | contract | Kept on purpose and documented: with DETACHED_PROCESS it stops CreateProcessW from copying this process's standard handles into the child |
| `--profile`, `--memory`, `--cpu` could repeat (the last one won) | robustness | Repeats are refused |
| The failure sweep never reached the "profile exists" path; the mock did not catch late `GetLastError` reads, inheritance without a handle list, unknown handles, or the length limit | test gaps | All added (above) |
| The executable's DACL is read and written by concurrent launches (lost updates), and gains one entry per container | robustness | Left: a lost grant fails closed and is written again on the next launch |
| The FNV-1a profile hash could collide; the container folder and TEMP persist between runs | design notes | Left (paths are not chosen by an attacker); listed below |

The reviewer found nothing wrong with the attribute list's lifetimes, the
allocation and release pairs, handle closing, error conventions, `raw.rs`,
`append_argument`, the environment block, the mitigation constants or the handle
list.

## Verified in the Windows VM

Windows 11 Enterprise 25H2 (build 26200) in `tests/windows_vm.py`, two vCPUs, a QEMU
HDA sound card without a host backend, no network except forwards into host
loopback. `crates/sandbox-windows/tests/confinement.rs` (see tests/README.md) starts
itself through `nuschel_ipc::host`'s real commands as each worker kind and probes
from the inside. Result on 2026-11-11, every worker kind:

| Attempt | content | storage | crypto | media | transfer |
| --- | --- | --- | --- | --- | --- |
| Token is an AppContainer | yes | yes | yes | yes | yes |
| Its job, read from inside: one process, memory limit, all 8 UI restrictions | yes (512 MiB) | yes (1 GiB) | yes (1 GiB) | yes (1 GiB) | yes (1 GiB) |
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
launch stage refuses a policy it cannot apply (exit 125). With `--nested` the test
runs inside a job of its own, so each worker starts in that job and its own job, UI
limits and all, has to nest; that works on Windows 11 (the AssignProcessToJobObject
page says a job with UI limits cannot be nested), which matters because the
desktop's kill-on-close job usually holds the launch stage before it creates the
worker. The same holds with the job attached at creation (`JOB_LIST`), checked
again after the review fixes.

The math worker (added later) passed the same probes in its less privileged
AppContainer (`--only math`): every attempt in the table denied, `HKCU\Environment`
too and Winsock not starting, its job showing one process, 512 MiB, 120 CPU seconds
and all eight UI restrictions; the real worker rendered a formula. Each probe also
reads its job's CPU limit: 5 seconds for content (its request budget), 120 for math,
none for the others.

After the review fixes, in the VM: every check above passed again, nested and not,
including a file the host received from the transfer worker (read through the
duplicated handle) and one it sent. A storage worker given a junction to
Documents as its directory refused to start (exit 125, "a junction or link, which
is not followed") and Documents' DACL was unchanged. The desktop's history
directory, which an earlier build had granted Modify on itself, now shows only
`(RX,WD,AD,DC)` on the directory and `(OI)(CI)(IO)(M)` for its contents for the
storage container. The real workers in the
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
  read/execute entries on the worker executables stay (one per container; they are
  written again after an update replaces the files, and concurrent launches can
  lose one, which fails closed until the next launch). An installation on a network
  share cannot start workers (remote images are refused).
- A container's folder and TEMP persist between runs, so a compromised content
  worker could leave files for the next one.
- Storage and crypto profile names use FNV-1a over the path; a collision would
  merge two containers' grants (the paths are not chosen by an attacker).
- When a grant is written, Windows propagates it into the directory's existing
  contents by name; this happens only when the grant is missing (normally before
  the worker first ran).
- Only tested in a VM, without a physical audio device. The checks used a cargo
  cross build with the flake's toolchain and MinGW; the `.#windows` package was
  built and run (desktop, media) before the less-privileged change only.
