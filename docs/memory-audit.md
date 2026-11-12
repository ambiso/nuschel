# Memory audit of the native client (2026-11-12)

Where the Linux desktop client's memory goes, measured, and what could be
trimmed. The goal is about 10 MB of RAM "if avoidable". The short answer is
that it is not avoidable for a GPU-rendered GUI on Mesa, but the client's own
(anonymous) memory in a long session can be roughly halved. This document holds measurements and analysis
only; no product code was changed.

## Summary

The tested package, the whole process tree (desktop, workers and their
bubblewrap processes), on a Radeon RX 7900 XTX with Mesa, in MiB:

| | PSS | anonymous | RSS |
|---|---:|---:|---:|
| started, not connected | 92 | 23 | 131 |
| idle in a channel | 75–92 | 27 | 137–142 |
| 2,000 messages loaded and scrolled | 94–110 | 44 | 155–160 |
| plus six image previews; unchanged after 15 min idle | 101–117 | 51 | 162–167 |
| after Settings and the microphone test | 115 | 62 | 186 |
| a minimal Vulkan window (vkcube), for comparison | 22–35 | 7 | 55 |

PSS moves by up to 17 MiB between identical runs, depending on which other
processes share the Mesa and LLVM pages. Anonymous memory moves by under 1 MiB.

- **The desktop is ~85 % of the tree.** Its file-backed part is GPU driver
  libraries (60 MiB RSS) and its own code (29 MiB, clean and reclaimable). Its
  private part, 22 → 53 MiB from idle to after Settings, is mostly
  live heap.
- **Nothing grows at idle**, but nothing is ever given back.
  - Every message stays at ~7 KiB once paged in (the open item in
    `architecture.md`). The largest single item is code-highlight styles kept
    twice, once per theme.
  - Images are held four times.
  - AT-SPI and Settings leave memory behind.
  - The crypto worker stays up after Settings.
- **The cheapest real saving is GPUI's GL probe.** wgpu also initialises a
  radeonsi GL context it never uses. Suppressing it in the environment saved
  4.4 MiB anon, 21 MiB RSS, 7 threads and 10 MiB VRAM, with identical
  rendering.
- **Not worth doing:** the malloc arena limit, the thread count, lazy fonts
  (60 KiB), emoji, syntect (not in the desktop) and stripping (disk only).
- **~10 MB in total is not achievable with a GPU renderer on Mesa.** A minimal
  Vulkan window already costs 5–7 MiB private plus 35–45 MiB of shared driver
  code. Nuschl's own memory at idle is ~14 MiB including workers. A realistic
  target is ~20 MiB anonymous / 60–70 MiB PSS that stays there with history,
  against 51 / 100+ today.


## What was measured

**Artifacts.** Primary: the tested package
`/nix/store/143xwr578zsjz6am7hgpv4d8515fp206-nuschl-0.1.0`
(source c1908e3c, NAR `sha256-z0OV1WkiPNv2XmzLc2jrHeBV9MH4k6FP5+OnRpBRAX8=`,
desktop binary sha256 `5e7933b9…77490`). For comparison:
`/nix/store/hlqyl1sga9g8bs3a1m4n4ssy2b95awnm-nuschl-0.1.0` built from main
1cbb5103 in this workspace (`nix build .#default` took 0.26 s: it was already in
the store), NAR `sha256-h8SKnznndYBICcpqdtRnqY7xFYxk8gp5sBJBJzt+KUQ=`. Source
references below are to 1cbb5103; c1908e3c differs from it only in 45 audio
commits (media worker, VAD, status/PipeWire).

**Kind of result.** These are private-runtime results on physical hardware:
an AMD Radeon RX 7900 XTX (Navi 31, `amdgpu`, RADV/radeonsi, LLVM 21.1.8,
Vulkan loader 1.4.363), 24 logical CPUs, 64 GiB RAM, NixOS. A reboot in the
middle of the audit also updated the host:

- before it, Mesa 26.2.6 (`2wmpx5il…-mesa-26.2.6`, `mr6cbi3r…-llvm-21.1.8-lib`)
  and kernel 7.3.1;
- after it, Mesa 26.3.0 (`x6c8ff2w…-mesa-26.3.0`, `pw5lk9rb…-llvm-21.1.8-lib`)
  and kernel 7.3.4.

The runs on either side agree within 1 MiB of anonymous memory. Nothing ran in a VM,
and other platforms (Windows, macOS, integrated GPUs) are not covered.

**Environment.** [`tests/memory_audit.py`](../tests/memory_audit.py) starts:

- a private headless GNOME Shell 50.4 on the real GPU, on its own Wayland
  socket and D-Bus;
- a private AT-SPI bus and registry, disabled (`IsEnabled` false) except in
  scenario 5;
- a private null PulseAudio (null sink and null source only);
- a private loopback stock Murmur 1.5.939, with message limits raised for the
  history scenario;
- a scripted `nuschl-cli` peer.

The desktop gets a fresh private profile with only the server and
`check_updates: false`; nothing leaves loopback. `TMPDIR`, `XDG_RUNTIME_DIR` and
the shell's runtime directory are under `~/.cache/claude-nuschl/ram-audit/`. The
window is 1280×820 logical pixels at scale 1.

**Metrics.** Every process in the desktop's tree is measured from
`/proc/<pid>/smaps_rollup` and `/proc/<pid>/smaps`. The tree is the desktop,
each worker and the two bubblewrap processes (`bwrap` and the PID-namespace
`bwrap-init`) per sandboxed worker.

- **RSS**: resident pages. Shared pages are counted in full by every process.
- **PSS**: each resident page divided by the number of processes mapping it.
  The tree's summed PSS is the conventional "RAM used" number. It depends on
  what *else* runs: pages of Mesa, LLVM or glibc that another process on the
  host also maps count only half (or less). Here the same Mesa/LLVM pages were
  also mapped by the private GNOME Shell and by other GPU clients on the host.
  Between otherwise identical runs, tree PSS varied by up to ~17 MiB at
  constant RSS and private dirty.
- **USS**: private clean + private dirty. Pages no other process maps.
- **Private dirty / anonymous (+ swap)**: memory only this process wrote.
  Heap, stacks, relocated data and driver state. It cannot be dropped without
  swapping, and it does not depend on co-residents. It varied by under 1 MiB
  between runs, so comparisons below use it and RSS.
- **File-backed clean**: executable code, read-only data and fonts. The kernel
  can drop and re-read these pages at any time. They count in RSS and PSS, but
  are not a hard cost.

Each mapping is classified: the executable, the GPU driver stack (Mesa, LLVM,
libdrm, the Vulkan loader, libglvnd), window-system and other libraries, fonts,
`/dev/dri` buffer mappings, glibc's main heap, glibc's per-thread arenas
(anonymous mappings aligned to `HEAP_MAX_SIZE`, 64 MiB), thread stacks (located
from each thread's stack pointer in `/proc/<pid>/task/<tid>/syscall`) and other
anonymous memory. GPU memory comes from the DRM client statistics in
`/proc/<pid>/fdinfo/<fd>` (`drm-memory-vram`, `drm-memory-gtt`, …), which
amdgpu exposes per process without root. `/sys/kernel/debug/dri` is not
readable without root and was not used, and wgpu's memory report is not exposed
by GPUI.

**Scenarios.**

1. Started, not connected (the server list).
2. Connected to the private Murmur, idle in a channel (joined muted; the media
   worker plays the channel).
3. 2,000 messages received from the peer (every tenth a fenced code block,
   every tenth inline math, every twentieth display math, plus lists and
   inline Markdown; about 120 distinct formulas), then the timeline scrolled
   to the top (message #0) and back with the mouse wheel.
4. Six image messages (`<img src="data:image/png…">`, 480×360 to 1280×720,
   13–45 KB each, within stock Mumble's 128 KiB) in view, then scrolled past
   and back.
5. AT-SPI enabled, Settings opened, the microphone test started on the null
   source, stopped, and Settings closed. Then AT-SPI was disabled again.
6. 15 minutes idle, sampled every 30 s.

Each state is sampled after the desktop's own CPU use falls below 0.1 s per
3 s and no one-shot content worker is running. Three samples are taken 2 s
apart and the last is kept.

## Results

### Per scenario (whole process tree)

Tested package, MiB. "anon+swap" is anonymous memory including any swapped out,
which is the robust "our memory" number. **PSS is the conventional total,
counting shared pages fractionally.**

There are two runs. *Primary* (before the reboot, Mesa 26.2.6, kernel 7.3.1)
covered scenarios 1–4. *Rerun* (after the reboot, Mesa 26.3.0, kernel 7.3.4)
repeated 2–4 and covered 6 and 5. The private dirty and anon+swap columns of
the two agree within 0.6 MiB. Their PSS differs by up to 17 MiB, because a
different set of host processes shared the Mesa pages at the time.

| scenario | run | PSS | anon+swap | USS | private dirty | RSS | processes | threads | VRAM | GTT |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| floor: vkcube, a minimal Vulkan window (RADV, Wayland) | primary | 35.1 | 7.1 | 23.0 | 7.1 | 54.9 | 1 | 6 | 15.8 | 2.3 |
| 1 started, not connected | primary | **92.3** | 23.4 | 72.1 | 23.2 | 130.6 | 4 | 57 | 111.2 | 4.0 |
| 2 connected, idle in a channel | primary | **92.1** | 26.5 | 71.7 | 26.1 | 136.9 | 7 | 57 | 111.1 | 4.0 |
| 2 connected, idle in a channel | rerun | **74.8** | 27.1 | 62.0 | 26.6 | 141.9 | 7 | 64 | 111.1 | 4.0 |
| 3 2,000 messages received (math worker still up) | rerun | 104.8 | 42.2 | 90.4 | 41.6 | 176.3 | 10 | 68 | 111.2 | 4.0 |
| 3 … scrolled to the top | rerun | 109.5 | 46.5 | 94.7 | 45.7 | 180.9 | 10 | 68 | 111.4 | 4.0 |
| 3 … and back, settled (math worker exited) | primary | **109.7** | 43.7 | 89.2 | 43.2 | 154.7 | 7 | 57 | 111.5 | 4.0 |
| 3 … and back, settled (math worker exited) | rerun | **94.1** | 44.2 | 79.7 | 43.7 | 159.8 | 7 | 64 | 111.5 | 4.0 |
| 4 six images in view | primary | **116.7** | 50.6 | 96.1 | 50.2 | 161.6 | 7 | 57 | 111.5 | 4.0 |
| 4 six images in view | rerun | **100.9** | 51.1 | 86.6 | 50.6 | 166.7 | 7 | 64 | 111.5 | 4.0 |
| 6 after 15 minutes idle | rerun | **100.7** | 51.1 | 86.5 | 50.6 | 166.7 | 7 | 64 | 111.5 | 4.0 |
| 5a before Settings | rerun | 101.1 | 51.1 | 86.5 | 50.6 | 166.7 | 7 | 64 | 111.5 | 4.0 |
| 5b AT-SPI enabled (screen reader present) | rerun | 106.7 | 56.2 | 92.3 | 55.7 | 172.5 | 7 | 64 | 111.5 | 4.0 |
| 5c Settings open | rerun | 117.5 | 64.4 | 99.8 | 63.4 | 198.8 | 13 | 76 | 111.5 | 4.0 |
| 5d microphone test running | rerun | **121.7** | 67.7 | 103.6 | 66.7 | 202.6 | 13 | 79 | 111.5 | 4.0 |
| 5e Settings closed | rerun | **115.1** | 61.6 | 100.3 | 60.9 | 186.2 | 10 | 69 | 111.5 | 4.0 |
| 5f AT-SPI disabled again | rerun | 115.0 | 61.6 | 100.3 | 60.9 | 186.2 | 10 | 69 | 111.5 | 4.0 |

No sample had anything in swap. (The pre-reboot idle run did; see below.)

What the scenarios show:

- **Startup to idle in a channel.** ~23–27 MiB of anonymous memory, 75–92 MiB
  PSS and 131–142 MiB RSS. Connecting adds the media worker and its two
  bubblewrap processes: +3 MiB anon, +7 MiB PSS.
- **Long history.** Receiving and scrolling 2,000 messages adds ~17 MiB of
  anonymous memory for good. The desktop holds +14.7 MiB of it (main heap
  +9–10, arenas +3, other anonymous +2) and the storage worker +2.6. The
  timeline reached message #0, so all 2,000 were laid out at some point. The
  math worker (15 MiB PSS) comes and goes with its 60 s idle exit.
- **Images.** Six previews add ~7 MiB of anonymous memory: decoded pixels kept
  CPU-side, plus conversion on GPUI's background threads. VRAM did not change
  visibly (sub-allocated).
- **Long idle.** Nothing grows: anon+swap 51.1 → 51.06 MiB over 15 minutes,
  RSS and thread count constant. CPU at idle is ~0.2 % of a core for the
  desktop and 2–4.5 % for the media worker (muted, in a channel).
- **Settings and the microphone test.**
  - Enabling AT-SPI adds 5 MiB (AccessKit's tree over the long timeline).
  - Opening Settings starts *two* more sandboxed workers: a preview media
    worker, then a fresh one when the test starts, plus the crypto worker for
    the identity panel. Each comes with two bubblewrap processes: 13 processes,
    +17 MiB PSS at the peak.
  - Closing Settings **releases the preview media worker**, but the **crypto
    worker stays** until exit (2.6 MiB PSS plus 0.4 for its bubblewrap pair).
  - The desktop keeps the 9.7 MiB of anonymous memory it grew while AT-SPI and
    Settings were active.
    - AccessKit's tree (+5 MiB) stays after AT-SPI is disabled.
    - Settings leaves about 2 MiB live and 3 MiB that glibc keeps (see the heap
      profile).

### Per process (rerun, scenario 4: history and images, idle)

| process | RSS | PSS | USS | private dirty | private clean | shared clean | threads |
|---|---:|---:|---:|---:|---:|---:|---:|
| desktop | 138.5 | 85.7 | 72.4 | 43.3 | 29.1 | 66.1 | 52 |
| storage worker | 10.6 | 7.3 | 6.9 | 3.5 | 3.4 | 3.7 | 3 |
| media worker | 9.9 | 7.2 | 6.5 | 3.6 | 2.9 | 3.4 | 5 |
| 2 × bwrap + 2 × bwrap-init | 7.8 | 0.7 | 0.4 | 0.4 | 0.0 | 7.0 | 4 |

The desktop is ~85 % of the tree by any measure. Its 43 MiB of private dirty
memory at this point:

| desktop, private dirty | MiB |
|---|---:|
| malloc main heap | 21.8 |
| malloc thread arenas (22) | 8.4 |
| GPU driver library relocations (`.data.rel.ro`, GOT) | 5.9 |
| other anonymous mappings (large allocations, driver mappings) | 4.3 |
| thread stacks (52 threads) | 1.6 |
| the executable's relocated data | 1.2 |

Its file-backed part is 95 MiB RSS: 29 MiB of its own executable (private) and
60–64 MiB of driver libraries (shared).

### The pre-reboot idle run

The first 15-minute idle run was cut short when the host froze due to an
unrelated out-of-memory process (user-confirmed); it rebooted at about 18:48.
Its first 150 s were flat, like the rerun. After that the host reclaimed hard:
the idle desktop's RSS went 136 → 105 → 61 → 36 → 11.5 MiB in three minutes
(swap-out and eviction of its own executable pages), and it kept running.
That says nothing about growth, but shows how much of the RSS is reclaimable
when the system needs it. The partial timeline is kept as
`idle-timeline-interrupted-by-host-crash.json`.

## Environment-only experiments

Each experiment changes only the desktop's environment or CPU affinity, with
no code change, and repeats scenario 2 (and 3). Tree totals in MiB, compared
with the rerun.

| experiment | what it tests | scen. 2 anon+swap | scen. 2 RSS | scen. 2 PSS | VRAM / GTT | threads | scen. 3 anon+swap | scen. 3 RSS |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| none (rerun) | | 27.1 | 141.9 | 74.8 | 111.1 / 4.0 | 64 | 44.2 | 159.8 |
| `VK_LOADER_DRIVERS_SELECT=*radeon*` | load only RADV's ICD | 25.7 | 135.1 | 72.7 | 111.1 / 4.0 | 64 | | |
| `__EGL_VENDOR_LIBRARY_FILENAMES=<none>` | no EGL vendor, so wgpu's GL backend finds no adapter and radeonsi is never loaded | 22.8 | 120.5 | 70.8 | **100.9 / 2.0** | 57 | | |
| both of the above | Vulkan through RADV only | **21.4** | **113.3** | 66.8 | 100.9 / 2.0 | 57 | | |
| `MALLOC_ARENA_MAX=2` | one extra arena instead of 23 | 26.7 | 141.9 | 76.6 | 111.1 / 4.0 | 64 | 43.8 | 159.6 |
| `taskset -c 0-3` | 4 GPUI background threads instead of 24 | 26.5 | 141.5 | 76.0 | 111.1 / 4.0 | 44 | 43.6 | 159.2 |
| all four | | **20.2** | **111.9** | 65.3 | 100.9 / 2.0 | 37 | **37.2** | **129.5** |

- **The GPU backends are the only env-level lever, and a real one.**
  - Not probing GL removes `libgallium` (11 MiB RSS) and its 7 threads, and
    saves 4.4 MiB of the desktop's anonymous memory. radeonsi's context
    and heap go with it, along with 10 MiB of VRAM and 2 MiB of GTT.
  - Loading only RADV's ICD saves 6–7 MiB RSS and 1.3 MiB of relocations.
  - Together: −5.7 MiB anon, −28.6 MiB RSS, −8 MiB PSS.
  - Rendering was unchanged in screenshots, including math, code and images.
- **Arena count and thread count do not matter.** One arena moves the same
  bytes into the main heap. Twenty fewer threads save 0.6 MiB.
- **No environment variable touches the main contributors.** That is the
  heap, which grows with history, images and Settings, and the executable.


## Attribution

### The GPU driver stack

What the desktop maps from the driver stack in scenario 2. This is the primary
run, Mesa 26.2.6 (MiB; RSS counts shared pages in full, PSS shares them):

| file | RSS | PSS | private dirty | why it is loaded |
|---|---:|---:|---:|---|
| `libLLVM.so.21.1` | 32.6 | 16.0 | 2.07 | RADV itself links it (`NEEDED`), as do lavapipe and `libgallium` |
| `libgallium-26.2.6.so` | 10.8 | 5.2 | 1.57 | radeonsi, for wgpu's GL backend (EGL) |
| `libvulkan_radeon.so` | 5.3 | 4.1 | 0.41 | RADV, the driver actually used |
| `libvulkan_lvp.so` | 2.9 | 2.8 | 0.37 | lavapipe ICD, loaded by the Vulkan loader, unused |
| `libvulkan_nouveau.so` | 2.6 | 2.6 | 0.48 | ICD for NVIDIA, unused |
| `libvulkan_asahi.so` | 2.2 | 2.2 | 0.46 | ICD for Apple GPUs, unused |
| `libvulkan_dzn.so` | 1.6 | 1.5 | 0.31 | ICD for D3D12, unused |
| loader, glvnd, EGL, libdrm, … | 2.2 | 1.0 | 0.25 | |
| **total** | **60.2** | **35.6** | **5.9** | |

Two things load more than the one driver in use:

- **Every installed Vulkan ICD.** The Vulkan loader opens every ICD in
  `/run/opengl-driver/share/vulkan/icd.d` (13 on NixOS) to enumerate devices,
  and keeps those that load: lavapipe, nouveau, asahi and dzn beside RADV.
  vkcube does exactly the same (see the floor), so this is the loader's
  behaviour on this distribution, not Nuschl's. libLLVM, the largest single
  mapping, stays even with RADV alone: NixOS's RADV links it.
- **A second, GL, backend.** GPUI creates its wgpu instance with
  `Backends::VULKAN | Backends::GL` and enumerates all adapters
  (`vendor/gpui-pre-wgpu/src/wgpu_context.rs`). wgpu's GL backend creates an EGL
  display and context at instance creation. That brings up radeonsi in
  `libgallium` with six Mesa threads (`gdrv`, `gl`, `sh`, `sh_opt`, `cs`,
  `traceq`) and GPU memory, and they stay for the life of the process, though the
  Vulkan adapter is the one used.

The private dirty part (5.9 MiB) is relocation of these libraries' data
(`.data.rel.ro`/GOT), the same in every process that loads them.

### GPU memory

`fdinfo` reports one DRM client for the desktop: libdrm_amdgpu shares one device
between RADV and radeonsi. It holds about **111 MiB of VRAM and 4 MiB of GTT**
in every scenario, from the server list onwards. Of the VRAM, 24 MiB is shared
with the compositor (the swapchain images) and 16.5 MiB is marked purgeable.
History and images did not change it measurably (+0.4 MiB), because wgpu's
gpu-allocator sub-allocates atlas textures from blocks it already holds. vkcube
holds 15.8 MiB of VRAM.

On this discrete GPU none of that is system RAM, and none of it shows in RSS:
the CPU-mapped `/dev/dri` ranges had no resident pages. On an integrated GPU or
APU, "VRAM" is carved from system RAM and GTT is system RAM. There these
allocations would be the single largest cost, roughly the whole budget several
times over. Known contributors from GPUI's renderer
(`vendor/gpui-pre-wgpu/src/wgpu_renderer.rs`):

- a window-sized 4× MSAA texture for paths (1280×820×4 B×4 = 16 MiB);
- a window-sized intermediate texture (4 MiB);
- the swapchain images;
- 1024² atlas textures per kind (monochrome R8, subpixel and polychrome RGBA);
- a 2 MiB instance buffer;
- the GL context's own allocations.

GPUI already asks wgpu for `MemoryHints::MemoryUsage`.

### Binaries

From `readelf -S` of the tested package (MiB). The release profile is
`lto = "thin"`, `codegen-units = 1`, `strip = "debuginfo"`: no `.debug_*`
sections; symbols are kept.

| binary | file | `.text` | `.rodata` | unwind (`.eh_frame*`, `.gcc_except_table`) | `.rela.dyn` | RELRO + data (private dirty once relocated) | `.symtab` + `.strtab` |
|---|---:|---:|---:|---:|---:|---:|---:|
| desktop (`libexec/nuschl/nuschl`) | 50.2 | 30.4 | 2.7 | 4.9 | 1.5 | 1.28 | 9.3 |
| media worker | 5.5 | 3.6 | 0.3 | 0.4 | 0.2 | 0.15 | 0.9 |
| content worker | 5.8 | 2.6 | 0.9 | 0.3 | 0.6 | 0.53 | 0.8 |
| crypto worker | 8.0 | 5.7 | 0.2 | 0.6 | 0.1 | 0.09 | 1.3 |
| storage worker | 7.2 | 4.0 | 0.7 | 0.7 | 0.2 | 0.14 | 1.5 |
| transfer worker | 5.9 | 2.7 | 1.3 | 0.5 | 0.3 | 0.23 | 1.0 |
| math worker | 40.1 | 18.1 | 10.1 | 2.5 | 1.9 | 1.50 | 6.0 |

- **Stripping** the symbol tables would save 9.3 MiB of disk for the desktop
  (21 MiB over all binaries) and **nothing in RAM**: `.symtab`/`.strtab` are in
  no loadable segment.
- **The desktop's code is nearly all resident.** 29.3 of 30.4 MiB of `.text` is
  resident in every scenario, private to the desktop (no other process maps
  the file). That is not 29 MiB of code run: page-fault readahead and
  fault-around map whole neighbourhoods of the cached file. It is clean and
  file-backed, and the kernel drops it under pressure: when the host ran out of
  memory (below) the idle desktop's executable pages went to almost zero and it
  kept running. It still counts in RSS and PSS, so a smaller binary shows up
  directly in those numbers.
- **The math worker** carries Typst, its fonts (most of the 10 MiB `.rodata`)
  and syntect (Typst's raw blocks). Its 12.9 MiB of resident code and data
  exist only while it runs.
- **Syntect and html5ever** are only in the content and math workers: the
  desktop links no syntax highlighter.

### Threads, stacks and arenas

The desktop runs 45–52 threads:

| threads | owner | notes |
|---|---|---|
| `Worker-0` … `Worker-23` (24) | GPUI's background executor (`vendor/gpui-pre-linux/src/linux/dispatcher.rs`) | one per logical CPU (`available_parallelism`), at least 2 |
| `Timer`, main thread, `nuschl` | GPUI | |
| `tokio-rt-worker` ×2, `blocking-*` | the desktop's tokio runtime | already `worker_threads(2)` |
| `zbus::Connection` ×4, `async-io`, `smol-*` | zbus D-Bus connections | zbus users in the graph: AccessKit, ashpd (portals), notify-rust, oo7 (Secret Service), the desktop |
| `nuschl:gdrv`, `gl`, `sh`, `sh_opt`, `cs`, `traceq` | Mesa radeonsi | only because of the GL backend probe |
| `nuschl:disk$` ×3–9 | Mesa shader disk cache | |

Stacks are cheap: the 14 identified thread-stack mappings hold 1.6 MiB
resident. Rust's 2 MiB default stacks are reserved address space, not memory.

**glibc's malloc arenas** are not a lever either. Each thread that allocates
under contention gets its own arena, and 22–23 exist. They hold 0.6–1 MiB at
start and grow with work done on GPUI's background threads: 3.8–4.1 MiB after
the history and 7.5 MiB after the images. With `MALLOC_ARENA_MAX=2` the same
bytes simply move into the main heap. The desktop's anonymous memory after the
history was 36.1 MiB with the limit against 36.6 MiB without, which is within
run-to-run noise.

### Workers

| worker | when it runs | measured (PSS / RSS / private dirty, MiB) |
|---|---|---|
| storage (SQLite history) | from start to exit | 4.4 / 7.9 / 0.8 idle; 7.1 / 10.6 / 3.4 after 2,000 messages |
| media (audio, Opus, RNNoise) | while connected to voice, even muted; one for all servers | 6.7 / 9.8 / 3.6, and 2–4.5 % of a core continuously while muted in a channel |
| media, Settings preview | while Settings is open: a second worker, replaced by a third when the microphone test starts | 5–8 each, released when Settings closes |
| crypto (keystore, MLS, KT) | from first use (opening Settings asks for the identity) to exit; no idle exit | 2.5 / 4.6 / 0.3 |
| math (Typst) | from the first formula; exits after 60 s idle (`apps/desktop/src/math.rs`) | 15.4 / 17.7 / 2.5 |
| content (Markdown, HTML, highlighting, image decode) | one-shot per batch of up to 64 messages, exits after replying | peak RSS 20 MiB for 64 messages, 22 MiB for one 1280×720 image, for 0.02–0.2 s |
| transfer (magic-wormhole) | per file transfer | not exercised |

Every sandboxed worker adds two bubblewrap processes (the outer `bwrap` and
`bwrap-init`, PID 1 of the new PID namespace) costing 0.2 MiB PSS each, mostly
shared libc pages.

The media worker's models are small: RNNoise weights are in its 0.26 MiB
`.rodata`. Its resident memory is buffers and code.

The process boundaries are a security requirement (`AGENTS.md`, `SECURITY.md`).
None of the opportunities below merges a worker into the desktop. The only
levers are fewer simultaneous instances and shorter lifetimes.

### The desktop's heap (heaptrack)

The desktop was run under heaptrack 1.5 through scenarios 2, 3 and 4 and
ended with SIGTERM, so heaptrack's "leaked" means **still allocated at that
moment**. That moment is the state after the history and the images, idle.
Heaptrack counted 7.9 million allocations in 189 s, a peak heap of 30.9 MB and
**28.4 MiB live at the end**: almost everything allocated stays allocated.

The same state without heaptrack held 30–34 MiB of malloc-managed memory:
main heap 21.8 MiB, arenas 8.4 MiB, and part of 4.3 MiB of other anonymous
mappings (glibc serves allocations above 128 KiB with `mmap`). Against 28.4 MiB
live, glibc's free-but-unreturned memory is at most a few MiB (≤ 15 %). The heap is live data, so neither `MALLOC_ARENA_MAX` nor
another allocator would shrink it much.

Stacks attributed by the frames they contain: driver C functions first, then
the nearest Nuschl frame, then the nearest known library frame. MiB live at
the end:

| owner | MiB | main items |
|---|---:|---|
| **Nuschl: message documents and their view state** | **13.2** | `document::Highlights::new` 4.72; `nuschl_core::ContentBlock` (deserialised documents) 3.41; `document::Document` 0.80; `layout::Focusable` 0.72; `document::text_block` 0.66; `selection::BlockText` 0.57; `client::timeline::Message` 0.52; `presentation::MessageUi` 0.49; `ipc::ContentResponse` 0.39; `CodeRun` 0.21; `selection::SelectionArea` 0.20; `scrolling::Row` 0.18; `state::local_time` 0.16 |
| Nuschl: images | 2.0 | `document::render_image`: the decoded pixels of six previews |
| Nuschl: formulas | 0.6 | `math::Formula::image`, `MathImage::decode` |
| Nuschl: other (typing line, desktop state) | 0.9 | `typing::LiveStatus` 0.66 |
| GPU driver: RADV | 3.4 | `radv_create_cmd_buffer` 1.86, CS and command streams 0.66, device and pipelines |
| GPU driver: radeonsi GL context | 2.6 | `st_create_context` 0.85, llvmpipe screen 0.27, `_mesa_alloc_shared_state` 0.26, threaded context 0.21 |
| GPUI element arena | 2.0 | `gpui::arena::Arena::new`: two fixed 1 MiB arenas |
| wgpu / naga | 1.4 | shader IR (`naga::front::wgsl`, validator) kept after pipeline creation |
| GPUI renderer, text system, windows | 1.3 | glyph paint lists 0.66, bounds tree, text system |
| Wayland, xkbcommon | 0.4 | keymap |
| zbus | 0.17 | |
| AccessKit (AT-SPI off) | 0.08 | |
| **fontdb: all 263 system faces** | **0.06** | the "load every system font" suspect is negligible here |
| cosmic-text / swash / harfrust | 0.06 | shaping caches |
| tokio | 0.05 | |

What this rules out, and what it points at:

- **Fonts, emoji, syntax highlighting, tokio and the allocator are not where
  the memory is.**
  - cosmic-text's `FontSystem::new()` does load every system face, but here
    that is 263 faces and 60 KiB of metadata. Only the three faces in use are
    mapped (0.8 MiB of file-backed pages).
  - The emoji table is 40 KB of `.rodata`.
  - The desktop links no syntax highlighter: highlighting happens in the
    content worker.
- **Per-message memory is.** About **7 KiB per retained message** in the
  desktop: the decoded document, two highlight arrays, layout, selection and
  focus state.
- **The single biggest item is the highlight table.** `document::Highlights`
  keeps, per text block, an `Arc<[(Range<usize>, HighlightStyle)]>` for the
  dark palette and another for the light one when they differ, as they do for
  code. A GPUI `HighlightStyle` is about 120 bytes (seven `Option`s including
  colours, underline and strikethrough), so each coloured span costs ~136 bytes,
  twice.
- **History paged in stays in.** Live messages are trimmed to
  `MAX_TIMELINE = 1000` (`crates/client/src/client.rs`). Pages loaded while
  scrolling up are merged without a bound, so after scrolling to message #0 all
  2,000 messages and their desktop documents stay resident. That is the open
  item in [architecture.md](architecture.md) ("Bounding retained message memory
  while paging an active timeline is still open"), measured.
- **Images are held four times.** The worker's `ContentBlock::Image` carries
  the downscaled pixels as a base64 `String` (4/3 of the RGBA size) *and* the
  original `data:` URI (`crates/core/src/lib.rs`). The client keeps both with
  the message. `render_image` decodes the base64 into a `RenderImage`, which
  stays CPU-side for as long as the message exists
  (`apps/desktop/src/images.rs` releases only unused ones). The fourth copy is
  in the GPU atlas.

A second heaptrack run covered scenario 2, then scenario 5 without the long
history. Its heap-over-time series (`heaptrack_print -M`) shows live heap:

- 11.8 MiB idle in a channel;
- 12.1 MiB with AT-SPI enabled. AccessKit's tree is small without a long
  timeline, and costs about 2.5 KiB per message with one (+5 MiB at 2,000);
- 14.9 MiB with Settings open;
- 14.0 MiB after Settings was closed and AT-SPI disabled.

So Settings leaves about 2 MiB of **live** allocations behind. The remaining
~3 MiB of the residue seen in RSS is glibc keeping freed memory.

## Opportunities, ranked

The savings below are for the tree, measured where the effect was tested and
estimated from the heap profile otherwise. "anon" is private dirty, anonymous
memory: the hard cost. "RSS" includes clean file pages, which the kernel can
reclaim. PSS falls by between the two, depending on what else maps the same
libraries. None of these crosses a process boundary, merges workers or touches
a security rule.

| # | change | saving (tree) | effort | risk |
|---:|---|---|---|---|
| 1 | **Don't initialise wgpu's GL backend when Vulkan works.** GPUI creates its instance with `VULKAN \| GL` and enumerates every adapter (`vendor/gpui-pre-wgpu/src/wgpu_context.rs`). Create a Vulkan-only instance first, and a GL one only if no Vulkan adapter is usable. | **measured: −4.4 MiB anon, −21 MiB RSS, −7 threads, −10 MiB VRAM, −2 MiB GTT**; PSS −4 to −8 | small: a patch in the already-patched vendored `gpui-pre-wgpu` | low–medium: the GL fallback must keep working where Vulkan is missing (old GPUs, some VMs); worth a VM run |
| 2 | **Bound retained history.** Evict desktop documents and client messages for pages far from the viewport after paging back down, and re-read them from the storage worker. This is the open item in `architecture.md`. AccessKit needs only a light entry per offscreen message. | ~7 KiB per message beyond the bound: **−7 to −10 MiB anon at 2,000 messages**, unbounded otherwise; storage worker −1–2 | medium | medium: scroll anchoring, selection, search jumps, and the requirement that offscreen messages stay exposed to screen readers |
| 3 | **Compact highlight storage.** Store a small style class (`u8`) with a `Range<u32>` per span and resolve it to a `HighlightStyle` for the current theme while painting, instead of two arrays of ~136-byte entries (`apps/desktop/src/document.rs`). | **−4 MiB anon at 2,000 messages** (−4.4 of the 4.7 MiB), ~2 KiB per message | small–medium, desktop only | low |
| 4 | **Keep one copy of an image.** Drop the base64 RGBA string from the stored `ContentBlock::Image` once it is decoded, or keep only the `RenderImage`, and for offscreen messages re-decode on demand. | ~4/3 of the decoded preview size per image: **−2 to −3 MiB for six previews**, scaling with images in history | small–medium | low; the original `data:` URI stays for the viewer and for saving |
| 5 | **Return transient heap to the system.** After Settings, AT-SPI or a large history page, glibc keeps the freed memory (~3 MiB measured after Settings). Options: an allocator that purges (a dependency review question under `SECURITY.md`), or glibc tunables such as `GLIBC_TUNABLES=glibc.malloc.trim_threshold=…` in the package wrapper (not measured here). | ~3 MiB anon after peaks | small (wrapper) to medium (allocator) | low to medium |
| 6 | **Let the crypto worker exit when idle,** as the math worker does after 60 s. It starts on first use (opening Settings asks for the identity) and stays until exit (`crates/session/src/identity.rs`). | −3 MiB PSS (worker plus its bubblewrap pair) after Settings or identity use | small | low: a cold start on the next request |
| 7 | **One preview media worker in Settings.** Opening Settings starts one, and starting the microphone test replaces it with another. | −5 to −8 MiB PSS while Settings is open; nothing at rest | small | low |
| 8 | **Load only the Vulkan driver in use.** The Vulkan loader keeps every ICD that loads (lavapipe, nouveau, asahi and dzn here). Setting `VK_LOADER_DRIVERS_SELECT` (or `VK_DRIVER_FILES`) helps, but the package cannot know the user's GPU, and changing the environment inside the process would need `unsafe` (`set_var`). This is better left as a documented packager/user knob. vkcube pays the same. | measured: −1.3 MiB anon, −6.8 MiB RSS | small as documentation; awkward as code | medium: a wrong selection means no GPU |
| 9 | **Smaller desktop executable.** 29 MiB of its 30 MiB `.text` is resident (private, clean). Candidates: `opt-level = "s"` for cold crates, dropping features the desktop does not use (it links `image` codecs, and wgpu's GL backend if #1 drops GL), `panic = "abort"` where nothing relies on unwinding (removes most of 2 MiB `.gcc_except_table`). | RSS/PSS only, roughly −5 to −10 MiB; reclaimable pages, no anon | medium | low–medium |
| 10 | **SQLite page cache in the storage worker.** Its private dirty memory goes 0.8 → 3.4 MiB with history. A smaller `cache_size` would cap it. | −1 to −2 MiB anon | small | low (more reads on search/paging) |

Things that look like fruit but are not:

- **`MALLOC_ARENA_MAX` / fewer arenas**: measured zero; the bytes move to the
  main heap.
- **Fewer GPUI background threads**: measured −0.6 MiB for 20 fewer threads.
- **Fewer tokio threads**: it already has 2.
- **Loading fonts lazily**: 60 KiB for all 263 faces here. It matters only on
  systems with thousands of fonts.
- **Emoji data**: 40 KB of `.rodata`.
- **Syntect grammars**: not in the desktop; they live in the one-shot content
  worker, which exits after each batch.
- **Stripping binaries**: disk only.
- **Dropping idle workers**: the math worker already exits after 60 s and the
  content worker after every batch. The storage and media workers are in use
  whenever their feature is. Only the crypto worker (#6) stays idle.
- **GPUI's two 1 MiB element arenas, wgpu's 1.4 MiB of retained shader IR,
  RADV's 1.9 MiB of command buffers**: upstream, small.

#1 alone takes the idle tree from 27.1 to 22.8 MiB of anonymous memory
(measured). For the 2,000-message state (~44 MiB), #1 plus #2 and #3 would
give roughly 28–32 MiB. #2 and #3 overlap: a bounded history also bounds its
highlights. #4 removes most of the images' 7 MiB. #8–#10 and the allocator
would add another 2–5 MiB.

## The floor: is ~10 MB achievable?

**Not as PSS or RSS of a GPU-rendered GUI on Mesa.** Measured on the same
machine, in the same shell, with the same loader and drivers:

- **vkcube**, the smallest possible Vulkan window, is 54.9 MiB RSS and 22–35
  MiB PSS, of which 7.1 MiB is private dirty. Its driver libraries alone are
  45 MiB resident: RADV, LLVM, the loader and the unused ICDs. The private
  part is mostly relocated driver data (RADV and LLVM: ~2.5 MiB) and driver
  heap.
- **Nuschl's desktop with only RADV** (experiment "both") still maps 38 MiB of
  driver libraries (RADV 12, LLVM 25, which NixOS's RADV links) and has 3.4
  MiB of RADV heap and ~2.5 MiB of driver relocations.

So any GPU client on this stack starts at roughly **5–7 MiB of private memory
plus 35–45 MiB of shared driver code**. In PSS that code counts in full when
nothing else on the system uses the same Mesa build, and fractionally when the
compositor or other GPU applications share it. The GPU memory (100–111 MiB of
VRAM here) is not system RAM on a discrete GPU, but on an integrated GPU or APU
it is, and it would then exceed every other item in this report.

What is **Nuschl's own** memory at idle in a channel (tree anon 27.1 MiB,
rerun):

| | MiB |
|---|---:|
| driver-owned (GL context 2.6 + RADV heap 3.4 + library relocations 5.5) | ~11.5 |
| the executable's relocated data | 1.2 |
| desktop heap, arenas, stacks (GPUI, Nuschl state) | ~9 |
| workers: media 3.6, storage 0.8, four bubblewrap processes 0.8 | ~5.2 |

"About 10 MiB of Nuschl's own memory at idle" is therefore roughly where the
client already is: ~14 MiB including the workers, ~9 MiB for the desktop. The
history (+7 KiB per message), images, AT-SPI and Settings are what grow it.
Fixes #2–#7 keep that growth bounded. Fix #1 removes the part of the driver
cost that is avoidable.

A realistic target for this architecture is about **20 MiB of anonymous
memory and 60–70 MiB PSS at idle** (measured with the environment-only GPU
settings: 20.2–21.4 MiB anon, 65–67 MiB PSS), staying there with a long history and
images, instead of 51 MiB anon / ~100 MiB PSS after 2,000 messages and six
images today. Getting to ~10 MB *total* would mean not using the GPU at all: a
CPU renderer drawing into shared-memory Wayland buffers avoids the driver
stack entirely. GPUI has no such backend, so that is an architectural change,
not low-hanging fruit.

## Comparison with main 1cbb5103

The package built from main 1cbb5103 in this workspace
(`hlqyl1sga9g8bs3a1m4n4ssy2b95awnm`) behaves the same. It was measured after the
reboot (Mesa 26.3.0), in MiB:

- scenario 1: anon 22.9, RSS 128.0, PSS 68.8;
- scenario 2: anon 26.3, RSS 135.0, PSS 72.8. Its media worker had the same
  3.6 MiB anon / 9.9 MiB RSS and 2.2 % CPU as the tested package's.

The 45 audio commits between them do not change memory measurably.

## Reproducing

```sh
nix build --no-link --print-out-paths --cores 2 --max-jobs 2 --inputs-from . \
    nixpkgs#gnome-shell nixpkgs#at-spi2-core nixpkgs#pulseaudio nixpkgs#murmur \
    nixpkgs#heaptrack nixpkgs#vulkan-tools nixpkgs#imagemagick
TMPDIR=<disk scratch> NUSCHL_WL_TMP=<short disk scratch> python3 tests/memory_audit.py run \
    --package <nuschl package> --murmur <murmur>/bin/mumble-server --at-spi <at-spi2-core> \
    --pulse-bin <pulseaudio>/bin --gnome-shell <gnome-shell>/bin/gnome-shell --output <dir> \
    [--scenarios 1,2,3,4,6,5] [--vkcube <vulkan-tools>] [--env K=V] [--taskset 0-3] \
    [--heaptrack <heaptrack>]
python3 tests/memory_audit.py report <dir>...
heaptrack_print -f <dir>/desktop.heaptrack.zst -t 0 --flamegraph-cost-type leaked -F <stacks>
python3 tests/memory_audit.py heap <stacks>
```

The run stops cleanly when the host's available memory falls below 8 GiB or
its swap use exceeds 16 GiB (`--min-available-gib`, `--max-swap-gib`). Under
memory pressure the kernel reclaims the measured processes, and the numbers
then describe the host rather than the client.

**What this does not cover:**

- integrated GPUs and other drivers (NVIDIA, Intel);
- X11, Windows and macOS;
- HiDPI scale factors, where window-sized GPU buffers grow with the square of
  the scale;
- a nuschld server with server-side history;
- file transfers;
- several servers at once.

The heap profile covers the desktop only. The workers' heaps were inferred from
smaps.
