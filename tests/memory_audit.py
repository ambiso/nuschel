#!/usr/bin/env python3
"""Memory audit of the native desktop and its sandboxed workers (docs/memory-audit.md).

Runs the packaged desktop in a private headless GNOME Shell on this machine's
GPU, against a private loopback stock Murmur, a private null PulseAudio and a
private AT-SPI bus (disabled until the Settings scenario needs it). Nothing of
the user's session, devices, clipboard or profile is touched. Every process
of the desktop's tree (the desktop, each worker and its two bubblewrap
processes) is measured from /proc/<pid>/smaps_rollup and /proc/<pid>/smaps:

- RSS, PSS, USS (private clean + private dirty), shared clean/dirty,
  anonymous vs file-backed, swap;
- each mapping classified: the program executable, the GPU driver stack
  (Mesa, LLVM, libdrm, the Vulkan loader, libglvnd), window-system and other
  libraries, fonts, /dev/dri buffer mappings, glibc's main heap and its
  per-thread arenas, thread stacks (located from each thread's stack
  pointer), other anonymous memory;
- threads by name, and the GPU memory amdgpu/i915/xe report per DRM client
  in /proc/<pid>/fdinfo (VRAM, GTT, resident).

Scenarios (--scenarios, run in the order given):

  1  started, not connected (the server list)
  2  connected, idle in a channel
  3  2,000 messages received (code blocks, inline and display math), the
     timeline scrolled to the top and back
  4  six image messages in view
  6  a long idle period, sampled (--idle-minutes)
  5  AT-SPI enabled, Settings opened, the microphone test run once, Settings
     closed, AT-SPI disabled again (are the workers released?)

`--env K=V` adds environment variables to the desktop (experiments such as
MALLOC_ARENA_MAX=2) and `--taskset CPUS` limits its CPUs; `--heaptrack` runs
the desktop under heaptrack and ends it with SIGTERM at the end, so
heaptrack's "leaked" is the heap still live then; `--vkcube` measures a
minimal Vulkan window in the same shell, as a floor. A run stops cleanly when
the host's MemAvailable falls below --min-available-gib (8) or its swap use
exceeds --max-swap-gib (16): under pressure the kernel reclaims the measured
processes and the numbers describe the host, not the client. `report` turns
result folders into Markdown tables; `heap` attributes a heaptrack record's
live bytes (`heaptrack_print -t 0 --flamegraph-cost-type leaked -F STACKS`).

    TMPDIR=... NUSCHL_WL_TMP=... python3 tests/memory_audit.py run --package PKG \
        --murmur MURMUR --at-spi AT_SPI --pulse-bin PULSE/bin --gnome-shell SHELL \
        --output DIR [--scenarios 1,2,3,4,6,5] [--idle-minutes 15] [--env K=V ...] \
        [--taskset 0-3] [--heaptrack HEAPTRACK] [--vkcube VKCUBE]
    python3 tests/memory_audit.py report DIR...
    python3 tests/memory_audit.py heap STACKS
"""

import argparse
import base64
import json
import math
import os
import random
import re
import signal
import struct
import subprocess
import sys
import tempfile
import time
import uuid
import zlib
from collections import Counter, defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

KIB = 1024
ARENA = 64 << 20  # glibc HEAP_MAX_SIZE on 64-bit: non-main arena heaps are aligned to it
CHANNEL = 2  # Lobby/Development in local_murmur.CHANNELS
GPU_PACKAGES = re.compile(r"^(mesa|llvm|libdrm|vulkan-loader|libglvnd|spirv-tools|elfutils|libxshmfence|"
                          r"libva|directx|lm-sensors|libclc|libgbm)")
WINDOW_PACKAGES = re.compile(r"^(wayland|libxkbcommon|libx|libxcb|libdecor|xkeyboard|libxcursor|libxi-|"
                             r"libxrandr|libxext|libxfixes|libxrender)", re.IGNORECASE)
C_RUNTIME = re.compile(r"^(glibc|gcc-|libgcc|xgcc)")
FONT_FILE = re.compile(r"\.(ttf|otf|ttc|otc|pcf|pcf\.gz|woff2?)$", re.IGNORECASE)
ROLLUP = ("Rss", "Pss", "Pss_Dirty", "Pss_Anon", "Pss_File", "Pss_Shmem", "Shared_Clean", "Shared_Dirty",
          "Private_Clean", "Private_Dirty", "Anonymous", "Swap", "SwapPss")


# ----------------------------------------------------------------------------- /proc

def proc_read(pid, name):
    try:
        with open(f"/proc/{pid}/{name}", "rb") as handle:
            return handle.read().decode(errors="replace")
    except OSError:
        return None


def parents():
    result = {}
    for entry in os.listdir("/proc"):
        if entry.isdigit():
            stat = proc_read(entry, "stat")
            if stat:
                result[int(entry)] = int(stat[stat.rindex(")") + 2:].split()[1])
    return result


def descendants(root):
    children = defaultdict(list)
    for pid, ppid in parents().items():
        children[ppid].append(pid)
    order, stack = [], [root]
    while stack:
        pid = stack.pop()
        order.append(pid)
        stack.extend(sorted(children.get(pid, []), reverse=True))
    return order


def argv(pid):
    raw = proc_read(pid, "cmdline")
    return raw.split("\0")[:-1] if raw else []


def cpu_seconds(pids):
    ticks = os.sysconf("SC_CLK_TCK")
    total = 0
    for pid in pids:
        stat = proc_read(pid, "stat")
        if stat:
            fields = stat[stat.rindex(")") + 2:].split()
            total += int(fields[11]) + int(fields[12])
    return total / ticks


def worker_of(args):
    """The worker a bubblewrap command line confines (its `--ro-bind X /worker`)."""
    for index in range(len(args) - 2):
        if args[index] == "--ro-bind" and args[index + 2] == "/worker":
            return Path(args[index + 1]).name.removeprefix("nuschl-")
    return None


def labels(pids):
    """A readable name per process: desktop, media-worker, bwrap(media-worker), …"""
    names, parent = {}, parents()
    for pid in pids:
        args = argv(pid)
        if not args:
            names[pid] = "?"
            continue
        base = Path(args[0]).name
        if base == "bwrap":
            inner = Path((argv(parent.get(pid, 0)) or [""])[0]).name == "bwrap"
            names[pid] = f"bwrap{'-init' if inner else ''}({worker_of(args)})"
        elif args[0] == "/worker":
            ancestor = parent.get(pid, 0)
            while ancestor and Path((argv(ancestor) or [""])[0]).name == "bwrap" and not worker_of(argv(ancestor)):
                ancestor = parent.get(ancestor, 0)
            names[pid] = worker_of(argv(ancestor)) or "worker"
        elif args[0].endswith("/libexec/nuschl/nuschl") or base == "nuschl":
            names[pid] = "desktop"
        else:
            names[pid] = base
    return names


def package(path):
    match = re.match(r"/nix/store/[0-9a-z]{32}-([^/]+)", path)
    return match.group(1) if match else None


def smaps(pid):
    text = proc_read(pid, "smaps")
    if text is None:
        return None
    mappings, current = [], None
    for line in text.splitlines():
        head = re.match(r"^([0-9a-f]+)-([0-9a-f]+) (\S+) \S+ \S+ \d+\s*(.*)$", line)
        if head:
            current = {"start": int(head.group(1), 16), "end": int(head.group(2), 16),
                       "perms": head.group(3), "path": head.group(4).strip(), "kb": {}}
            mappings.append(current)
        elif current is not None and line.endswith(" kB"):
            key, value = line.split(":", 1)
            current["kb"][key] = int(value.split()[0])
    return mappings


def stack_pointers(pid):
    """Each thread's stack pointer (from /proc/<pid>/task/<tid>/syscall) and its name."""
    result = []
    try:
        tids = os.listdir(f"/proc/{pid}/task")
    except OSError:
        return result
    for tid in tids:
        name = (proc_read(f"{pid}/task/{tid}", "comm") or "?").strip()
        fields = (proc_read(f"{pid}/task/{tid}", "syscall") or "").split()
        sp = None
        if len(fields) >= 9:
            sp = int(fields[7], 16)
        elif len(fields) == 3 and fields[0] == "-1":
            sp = int(fields[1], 16)
        result.append((tid, name, sp))
    return result


def classify(mapping, executable_names, stacks):
    path = mapping["path"]
    if mapping["start"] in stacks or path == "[stack]":
        return "thread stacks"
    if path.startswith("/dev/dri/") or "dmabuf" in path or path.startswith("/drm"):
        return "GPU buffer mappings (/dev/dri)"
    if path == "[heap]":
        return "malloc main heap (brk)"
    if path in ("[vdso]", "[vvar]", "[vsyscall]", "[vvar_vclock]", "[uprobes]"):
        return "kernel ([vdso] etc.)"
    if path == "":
        if mapping["perms"].startswith("rw") and mapping["start"] % ARENA == 0:
            return "malloc thread arenas"
        return "other anonymous"
    if path.startswith(("/memfd:", "/dev/shm/", "/SYSV")):
        return "memfd / shm"
    if FONT_FILE.search(path.replace(" (deleted)", "")):
        return "font files"
    if path in executable_names:
        return "program executable"
    name = package(path) or ""
    if GPU_PACKAGES.match(name):
        return "GPU driver stack libraries"
    if WINDOW_PACKAGES.match(name):
        return "window-system libraries"
    if C_RUNTIME.match(name):
        return "C runtime (glibc, libgcc, libstdc++)"
    if ".so" in path:
        return "other libraries"
    return "other files"


def drm_clients(pid):
    """GPU memory per DRM client from fdinfo, deduplicated by drm-client-id (KiB)."""
    clients = {}
    try:
        fds = os.listdir(f"/proc/{pid}/fd")
    except OSError:
        return clients
    for fd in fds:
        try:
            target = os.readlink(f"/proc/{pid}/fd/{fd}")
        except OSError:
            continue
        if not target.startswith("/dev/dri/"):
            continue
        info = proc_read(pid, f"fdinfo/{fd}") or ""
        fields = dict(line.split(":", 1) for line in info.splitlines() if ":" in line)
        client = fields.get("drm-client-id", "").strip() or f"fd{fd}"
        memory = {}
        for key, value in fields.items():
            parts = value.split()
            if key.startswith(("drm-", "amd-")) and len(parts) == 2 and parts[1] in ("KiB", "MiB", "GiB", "B"):
                scale = {"B": 1 / 1024, "KiB": 1, "MiB": 1024, "GiB": 1024 * 1024}[parts[1]]
                memory[key] = int(int(parts[0]) * scale)
        clients[client] = {"device": target, "driver": fields.get("drm-driver", "").strip(), "kib": memory}
    return clients


def measure_process(pid, name):
    rollup_text = proc_read(pid, "smaps_rollup")
    if rollup_text is None:
        return None
    rollup = {}
    for line in rollup_text.splitlines():
        if line.endswith(" kB"):
            key, value = line.split(":", 1)
            rollup[key] = int(value.split()[0])
    status = {}
    for line in (proc_read(pid, "status") or "").splitlines():
        key, _, value = line.partition(":")
        if key in ("VmPeak", "VmSize", "VmHWM", "VmRSS", "RssAnon", "RssFile", "RssShmem", "VmSwap", "VmStk"):
            status[key] = int(value.split()[0])
        elif key == "Threads":
            status[key] = int(value)
    threads = stack_pointers(pid)
    mappings = smaps(pid) or []
    stacks = set()
    for _, _, sp in threads:
        if sp is None:
            continue
        for mapping in mappings:
            if mapping["start"] <= sp < mapping["end"]:
                stacks.add(mapping["start"])
                break
    try:
        executable = os.readlink(f"/proc/{pid}/exe")
    except OSError:
        executable = ""
    executable_names = {executable, "/worker", executable.replace(" (deleted)", "")}
    categories = defaultdict(lambda: defaultdict(int))
    files = defaultdict(lambda: defaultdict(int))
    arenas = 0
    for mapping in mappings:
        category = classify(mapping, executable_names, stacks)
        if category == "malloc thread arenas":
            arenas += 1
        for key in ("Size", "Rss", "Pss", "Private_Clean", "Private_Dirty", "Shared_Clean", "Shared_Dirty", "Swap"):
            categories[category][key] += mapping["kb"].get(key, 0)
        categories[category]["mappings"] += 1
        if mapping["path"] and not mapping["path"].startswith("["):
            key = mapping["path"]
            for field in ("Rss", "Pss", "Private_Clean", "Private_Dirty", "Shared_Clean"):
                files[key][field] += mapping["kb"].get(field, 0)
            files[key]["category"] = category
    top_files = sorted(files.items(), key=lambda item: -item[1]["Pss"])[:60]
    return {
        "pid": pid, "name": name, "args": argv(pid)[:3], "rollup": rollup, "status": status,
        "uss": rollup.get("Private_Clean", 0) + rollup.get("Private_Dirty", 0),
        "threads": Counter(re.sub(r"[0-9]+$", "#", thread) for _, thread, _ in threads),
        "thread_count": len(threads), "stacks": len(stacks), "arenas": arenas,
        "categories": {key: dict(value) for key, value in categories.items()},
        "files": {key: dict(value) for key, value in top_files},
        "drm": drm_clients(pid),
    }


def measure_tree(root, label):
    pids = descendants(root)
    names = labels(pids)
    processes = [result for pid in pids if (result := measure_process(pid, names[pid]))]
    total = defaultdict(int)
    for process in processes:
        for key in ROLLUP:
            total[key] += process["rollup"].get(key, 0)
        total["USS"] += process["uss"]
        total["threads"] += process["thread_count"]
        for client in process["drm"].values():
            for key, value in client["kib"].items():
                total[f"gpu:{key}"] += value
    return {"label": label, "time": time.time(), "processes": processes, "total": dict(total),
            "meminfo": meminfo()}


def meminfo():
    values = {}
    for line in (Path("/proc/meminfo").read_text()).splitlines():
        key, _, value = line.partition(":")
        values[key] = int(value.split()[0])
    return {key: values.get(key) for key in ("MemTotal", "MemFree", "MemAvailable", "SwapTotal", "SwapFree")}


def brief(sample):
    total = sample["total"]
    workers = sorted(p["name"] for p in sample["processes"] if p["name"] != "desktop" and not p["name"].startswith("bwrap"))
    return (f"{sample['label']}: tree PSS {total.get('Pss', 0) / KIB:.1f} MiB (+{total.get('SwapPss', 0) / KIB:.1f} swapped), "
            f"anon+swap {(total.get('Anonymous', 0) + total.get('Swap', 0)) / KIB:.1f} MiB, USS {total.get('USS', 0) / KIB:.1f} MiB, "
            f"RSS {total.get('Rss', 0) / KIB:.1f} MiB, {len(sample['processes'])} processes "
            f"({', '.join(workers) or 'no workers'}), {total.get('threads', 0)} threads")


# ----------------------------------------------------------------------------- content

WORDS = ("voice channel server latency packet opus codec jitter buffer echo noise gate whisper shout mute "
         "deafen certificate history message timeline sandbox worker render glyph atlas texture font emoji "
         "link preview image formula code block review merge release build test cache memory profile "
         "the a of and to in is that it for on with as was at by this be from or have an they which").split()
CODE = {
    "rust": ["fn main() {", "    let mut total = 0u64;", "    for value in 1..=100 {", "        total += value * value;",
             "    }", "    let names: Vec<String> = vec![\"a\".into(), \"b\".into()];",
             "    println!(\"{total} {:?}\", names);", "    match total % 3 {", "        0 => {}",
             "        _ => unreachable!(),", "    }", "}"],
    "python": ["import json", "def load(path):", "    with open(path) as handle:", "        return json.load(handle)",
               "class Worker:", "    def __init__(self, name):", "        self.name = name",
               "    def run(self, items):", "        return [item * 2 for item in items if item]", "print(load('x'))"],
    "javascript": ["const cache = new Map();", "export async function fetchJson(url) {",
                   "  if (cache.has(url)) return cache.get(url);", "  const response = await fetch(url);",
                   "  const body = await response.json();", "  cache.set(url, body);", "  return body;", "}"],
    "c": ["#include <stdio.h>", "int main(int argc, char **argv) {", "    for (int i = 0; i < argc; i++)",
          "        printf(\"%s\\n\", argv[i]);", "    return 0;", "}"],
}
FORMULAS = [r"x^{2} + y^{2} = r^{2}", r"e^{i\pi} + 1 = 0", r"\frac{a + b}{c - d}", r"\int_0^1 x^{2}\,dx = \frac{1}{3}",
            r"\sqrt{a^2 + b^2}", r"\nabla \cdot \mathbf{E} = \frac{\rho}{\varepsilon_0}", r"\binom{n}{k}",
            r"\lim_{n \to \infty} \left(1 + \frac{1}{n}\right)^n = e"]


def formula(rng, index):
    # About 120 distinct formulas: the desktop caches 512.
    variant = index % 15
    base = FORMULAS[index % len(FORMULAS)]
    return base if variant == 0 else rf"{base} + \sum_{{i=1}}^{{{variant}}} i^{{{variant % 4 + 1}}}"


def sentence(rng, words=12):
    text = " ".join(rng.choice(WORDS) for _ in range(rng.randint(words // 2, words * 2)))
    return text[0].upper() + text[1:] + "."


def message(rng, index):
    kind = index % 20
    if kind in (3, 13):
        language = list(CODE)[index % len(CODE)]
        lines = CODE[language][: rng.randint(4, len(CODE[language]))]
        return f"{sentence(rng, 6)}\n```{language}\n" + "\n".join(lines) + "\n```"
    if kind in (7, 17):
        return f"{sentence(rng, 8)} Inline: ${formula(rng, index)}$ and {sentence(rng, 4)}"
    if kind == 11:
        return f"{sentence(rng, 6)}\n$${formula(rng, index)}$$"
    if kind in (5, 15):
        return sentence(rng, 4) + "\n" + "\n".join(f"- {sentence(rng, 5)}" for _ in range(rng.randint(2, 4)))
    if kind == 9:
        return f"**{sentence(rng, 3)}** *{sentence(rng, 5)}* `inline code` [a link](https://example.test/{index})"
    return " ".join(sentence(rng) for _ in range(rng.randint(1, 3)))


def png(width, height, seed):
    """A picture (gradients and banded waves) that compresses to stock Mumble's 128 KiB, standard library only."""
    phase = random.Random(seed).random() * 6
    rows = []
    for y in range(height):
        row = bytearray([0])
        for x in range(width):
            band = int(4 + 3 * math.sin(phase + x / 61 + y / 47)) * 32
            row += bytes((x * 255 // width & 0xF0, y * 255 // height & 0xF0, band & 255))
        rows.append(bytes(row))

    def chunk(kind, data):
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(b"".join(rows), 9)) + chunk(b"IEND", b""))


IMAGES = [(1280, 720), (800, 600), (640, 480), (1024, 768), (480, 360), (960, 540)]


# ----------------------------------------------------------------------------- the run

class Audit:
    def __init__(self, args, root):
        self.args, self.root = args, root
        self.out = args.output
        self.helpers = []
        self.app = None
        self.desktop_pid = None
        self.shell = None
        self.bot = None
        self.murmur = None
        self.samples = []
        self.notes = {"env": dict(kv.split("=", 1) for kv in args.env), "package": str(args.package)}

    # -- infrastructure
    def spawn(self, command, env, log):
        process = subprocess.Popen(command, env=env, stdout=(self.out / log).open("a"), stderr=subprocess.STDOUT,
                                   start_new_session=True)
        self.helpers.append(process)
        return process

    def wrapper_environment(self):
        """The variables the package's `bin/nuschl` wrapper sets (to run the real binary under heaptrack)."""
        wrapper = Path(self.args.package) / "bin/nuschl"
        lines = wrapper.read_text().splitlines()
        body = "\n".join(line for line in lines[1:] if not line.startswith("exec "))
        output = subprocess.run(["bash", "-e", "-c", body + "\nprintf '%s\\0%s' \"$LD_LIBRARY_PATH\" \"$PATH\""],
                                capture_output=True, text=True, check=True, env={"PATH": os.environ["PATH"]}).stdout
        library_path, path = output.split("\0")
        return {"LD_LIBRARY_PATH": library_path, "PATH": path}

    def start_infrastructure(self):
        from gnome_session import GnomeSession
        from local_murmur import Murmur, eventually, free_port
        from mumble_client import Client, certificate_pin

        binaries = Path(self.args.package) / "bin"
        self.port = free_port()
        (self.root / "murmur").mkdir()
        self.murmur = Murmur(self.args.murmur, self.root / "murmur", self.port)
        self.murmur.ini.write_text(self.murmur.ini.read_text() + "messagelimit=100000\nmessageburst=100000\n"
                                   "textmessagelength=0\nimagemessagelength=0\n")
        self.murmur.seed()
        self.murmur.start()
        self.shell = GnomeSession(self.args.gnome_shell, 1600, 1000, log=(self.out / "gnome.log").open("w"))
        self.shell.start()
        self.env = self.shell.app_environment()
        self.env.pop("NO_AT_BRIDGE", None)
        self.start_accessibility_bus()
        pulse = self.shell.runtime / "pulse"
        pulse.mkdir(mode=0o700, exist_ok=True)
        script = pulse / "default.pa"
        script.write_text(
            f"load-module module-native-protocol-unix socket={pulse / 'native'} auth-anonymous=1\n"
            "load-module module-null-sink sink_name=speakers rate=48000\n"
            "load-module module-null-sink sink_name=microphone rate=48000\n"
            "set-default-sink speakers\nset-default-source microphone.monitor\n")
        self.spawn([str(self.args.pulse_bin / "pulseaudio"), "--daemonize=no", "--use-pid-file=no",
                    "--exit-idle-time=-1", "-n", f"--file={script}"],
                   self.shell.environment() | {"PULSE_RUNTIME_PATH": str(pulse), "PULSE_STATE_PATH": str(pulse / "state"),
                                               "DBUS_SESSION_BUS_ADDRESS": f"unix:path={self.root}/no-pulse-bus"},
                   "pulse.log")
        eventually(lambda: (pulse / "native").exists(), "private null audio missing")
        self.env |= {"RUST_LOG": "nuschl=info", "PULSE_SERVER": f"unix:{pulse}/native",
                     "NUSCHL_AUDIO_BACKEND": "pulseaudio", "PULSE_SINK": "speakers",
                     "PULSE_SOURCE": "microphone.monitor", "PIPEWIRE_REMOTE": str(self.root / "no-pipewire")}
        self.env |= self.notes["env"]
        self.pin = certificate_pin(binaries, "127.0.0.1", self.port, self.root / "probe")
        self.bot = Client(binaries, "127.0.0.1", self.port, self.pin, "Mark Bot", self.root / "bot",
                          ["--message-limit", "100000", "--message-burst", "100000"])
        self.bot.send(kind="join", channel=CHANNEL)
        self.bot.wait(lambda e: e["kind"] == "user" and e["user"]["session"] == self.bot.session
                      and e["user"]["channel"] == CHANNEL)
        self.profile = self.root / "profile"
        (self.profile / "config").mkdir(parents=True)
        (self.profile / "config/servers.json").write_text(json.dumps([{
            "id": str(uuid.uuid4()), "name": "Memory audit", "host": "127.0.0.1", "port": self.port,
            "username": "Andi", "certificate_sha256": self.pin}]))
        # Defaults, except no update check: this run never leaves loopback.
        (self.profile / "config/settings.json").write_text(json.dumps({"check_updates": False}))

    def start_accessibility_bus(self):
        """A private AT-SPI bus and registry, disabled (IsEnabled false) until scenario 5."""
        import shutil
        from accessibility import Bus, Node
        from local_murmur import eventually

        busctl = shutil.which("busctl") or "/run/current-system/sw/bin/busctl"
        self.session_bus = Bus(self.env["DBUS_SESSION_BUS_ADDRESS"], busctl)
        self.spawn([str(self.args.at_spi / "libexec/at-spi-bus-launcher")], self.env | {"GSETTINGS_BACKEND": "memory"},
                   "bus-launcher.log")

        def address():
            try:
                return self.session_bus("call", "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress")[0]
            except RuntimeError:
                return None
        a11y = Bus(eventually(address, "the accessibility bus did not start", timeout=15), busctl)
        self.set_accessibility(False)
        self.spawn([str(self.args.at_spi / "libexec/at-spi2-registryd")], self.env, "registry.log")
        self.registry = Node(a11y, "org.a11y.atspi.Registry", "/org/a11y/atspi/accessible/root")

        def ready():
            try:
                self.registry.children()
                return True
            except RuntimeError:
                return False
        eventually(ready, "the accessibility registry did not start", timeout=15)

    def set_accessibility(self, enabled):
        self.session_bus("set-property", "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Status", "IsEnabled", "b",
                         "true" if enabled else "false")

    def launch(self, url, log):
        from local_murmur import eventually
        package = Path(self.args.package)
        command = [str(package / "bin/nuschl"), "--profile-dir", str(self.profile)] + ([url] if url else [])
        env = dict(self.env)
        if self.args.heaptrack:
            env |= self.wrapper_environment()
            self.heaptrack_file = self.out / "desktop.heaptrack"
            command = [str(self.args.heaptrack / "bin/heaptrack"), "--record-only", "-o", str(self.heaptrack_file),
                       str(package / "libexec/nuschl/nuschl")] + command[1:]
        if self.args.taskset:
            # Fewer CPUs visible: GPUI's background pool (one thread per CPU) and Mesa's queues shrink.
            command = ["taskset", "-c", self.args.taskset] + command
            self.notes["taskset"] = self.args.taskset
        self.launched_at = time.monotonic()
        self.app = subprocess.Popen(command, env=env, stdout=(self.out / log).open("a"), stderr=subprocess.STDOUT,
                                    start_new_session=True)
        eventually(lambda: self.shell.window("Nuschl"), "native window missing", timeout=90)
        self.window_at = time.monotonic() - self.launched_at
        self.shell.place("Nuschl", 20, 20, 1280, 820)
        if self.args.heaptrack:
            real = str(package / "libexec/nuschl/nuschl")
            self.desktop_pid = eventually(lambda: next((pid for pid in descendants(self.app.pid)
                                                        if (argv(pid) or [""])[0] == real), None),
                                          "desktop under heaptrack not found")
        else:
            self.desktop_pid = self.app.pid

    def stop_desktop(self):
        from local_murmur import eventually
        if not self.app or self.app.poll() is not None:
            return
        if self.args.heaptrack:
            # Only the desktop: heaptrack then finishes writing its record.
            os.kill(self.desktop_pid, signal.SIGTERM)
            try:
                self.app.wait(timeout=600)
            except subprocess.TimeoutExpired:
                os.killpg(self.app.pid, signal.SIGKILL)
        else:
            os.killpg(self.app.pid, signal.SIGTERM)
            try:
                self.app.wait(timeout=15)
            except subprocess.TimeoutExpired:
                os.killpg(self.app.pid, signal.SIGKILL)
                self.app.wait(timeout=5)
        eventually(lambda: self.shell.window("Nuschl") is None, "window stayed", timeout=30)

    def settle(self, minimum=10, maximum=120, quiet=0.1):
        """Wait until the desktop is idle: under `quiet` CPU seconds of its own over 3 s (a media
        worker in a channel runs audio all the time) and no one-shot content worker running."""
        time.sleep(minimum)
        deadline = time.monotonic() + maximum
        while time.monotonic() < deadline:
            self.guard()
            before = cpu_seconds([self.desktop_pid])
            time.sleep(3)
            busy = cpu_seconds([self.desktop_pid]) - before
            content = any(name == "content-worker" for name in labels(descendants(self.desktop_pid)).values())
            if busy < quiet and not content:
                return
        print("warning: the desktop did not settle", flush=True)

    def cpu_rates(self, seconds=5):
        """CPU use per process over `seconds`, in percent of one core."""
        pids = descendants(self.desktop_pid)
        names = labels(pids)
        before = {pid: cpu_seconds([pid]) for pid in pids}
        time.sleep(seconds)
        return {f"{names[pid]}[{pid}]": round(100 * (cpu_seconds([pid]) - before[pid]) / seconds, 1) for pid in pids}

    def guard(self):
        """Stop the run (cleanly, keeping what was measured) when the host runs short of memory:
        under pressure the kernel reclaims this tree and the numbers measure the host, not the app."""
        host = meminfo()
        available = host["MemAvailable"] / (1024 * 1024)
        swapped = (host["SwapTotal"] - host["SwapFree"]) / (1024 * 1024)
        if available < self.args.min_available_gib or swapped > self.args.max_swap_gib:
            raise RuntimeError(f"host MemAvailable {available:.1f} GiB (minimum {self.args.min_available_gib}), "
                               f"swap used {swapped:.1f} GiB (maximum {self.args.max_swap_gib}): stopping")

    def sample(self, label, repeat=3):
        """Measure the tree `repeat` times, 2 s apart; the last is kept, the spread recorded."""
        self.guard()
        totals = []
        for index in range(repeat):
            if index:
                time.sleep(2)
            sample = measure_tree(self.desktop_pid, label)
            totals.append(sample["total"].get("Pss", 0))
        sample["pss_spread_kib"] = [min(totals), max(totals)]
        sample["cpu_percent"] = self.cpu_rates()
        self.samples.append(sample)
        (self.out / f"{re.sub(r'[^a-z0-9]+', '-', label.lower()).strip('-')}.json").write_text(json.dumps(sample, indent=1))
        print(brief(sample), flush=True)
        return sample

    def screenshot(self, name):
        try:
            time.sleep(0.5)
            window = self.shell.window("Nuschl")
            self.shell.screenshot(self.out / name, (window["x"], window["y"], window["width"], window["height"]))
        except Exception as error:  # a missing screenshot must not lose the measurements
            print(f"screenshot {name} failed: {error}", flush=True)

    # -- scenarios
    def scenario_1(self):
        self.launch(None, "desktop-1.log")
        self.settle(minimum=20)
        self.screenshot("1-server-list.png")
        self.sample("1 started, not connected")
        self.stop_desktop()

    def connect(self):
        self.launch(f"mumble://Andi@127.0.0.1:{self.port}/Lobby/Development", "desktop.log")
        user = self.bot.wait(lambda e: e["kind"] == "user" and e["user"]["name"] == "Andi"
                             and e["user"]["channel"] == CHANNEL, timeout=90)["user"]
        self.notes["connected_after_s"] = round(time.monotonic() - self.launched_at, 1)
        self.desktop_session = user["session"]

    def scenario_2(self):
        if not self.desktop_pid or self.app.poll() is not None:
            self.connect()
        self.settle(minimum=20)
        self.screenshot("2-connected.png")
        self.sample("2 connected, idle in a channel")

    def scenario_3(self):
        if not self.desktop_pid or self.app.poll() is not None:
            self.connect()
            self.settle()
        rng = random.Random(1)
        started = time.monotonic()
        for index in range(self.args.messages):
            if index % 200 == 0:
                self.guard()
            self.bot.send(kind="text", channel=CHANNEL, body=f"#{index} " + message(rng, index))
            if index % 50 == 49:
                time.sleep(0.25)
        self.notes["messages_sent_s"] = round(time.monotonic() - started, 1)
        self.settle(minimum=15, maximum=300)
        self.notes["messages_processed_s"] = round(time.monotonic() - started, 1)
        self.screenshot("3a-received.png")
        self.sample("3a history received (bottom)", repeat=1)
        window = self.shell.window("Nuschl")
        x, y = window["x"] + int(window["width"] * 0.62), window["y"] + int(window["height"] * 0.45)
        for direction in (-1, 1):
            for round_ in range(self.args.scroll_rounds):
                self.shell.wheel(x, y, direction * 60)
                time.sleep(0.35)
                if direction < 0 and round_ == self.args.scroll_rounds - 1:
                    self.settle(minimum=3, maximum=60)
                    self.screenshot("3b-scrolled-top.png")
                    self.sample("3b history scrolled to the top", repeat=1)
        self.settle(minimum=10, maximum=120)
        self.screenshot("3c-scrolled-back.png")
        self.sample("3 long history loaded and scrolled")

    def scenario_4(self):
        if not self.desktop_pid or self.app.poll() is not None:
            self.connect()
            self.settle()
        sizes = []
        for index, (width, height) in enumerate(IMAGES):
            data = png(width, height, index)
            sizes.append(len(data))
            self.bot.send(kind="html", channel=CHANNEL, html=(
                f"<p>Screenshot {index + 1} ({width}×{height})</p>"
                f"<img src=\"data:image/png;base64,{base64.b64encode(data).decode()}\"/>"))
            time.sleep(1.0)
        self.notes["image_png_bytes"] = sizes
        self.settle(minimum=15)
        self.screenshot("4-images.png")
        self.sample("4 images in view")
        # The images scrolled through once.
        window = self.shell.window("Nuschl")
        x, y = window["x"] + int(window["width"] * 0.62), window["y"] + int(window["height"] * 0.45)
        self.shell.wheel(x, y, -25)
        time.sleep(1)
        self.shell.wheel(x, y, 40)
        self.settle(minimum=5)
        self.sample("4b images scrolled past and back")

    def scenario_6(self):
        if not self.desktop_pid or self.app.poll() is not None:
            self.connect()
            self.settle()
        timeline = []
        end = time.monotonic() + self.args.idle_minutes * 60
        start = time.monotonic()
        while time.monotonic() < end:
            self.guard()
            sample = measure_tree(self.desktop_pid, "idle")
            desktop = next((p for p in sample["processes"] if p["name"] == "desktop"), {"rollup": {}, "thread_count": 0})
            timeline.append({"t": round(time.monotonic() - start), "pss": sample["total"].get("Pss", 0),
                             "uss": sample["total"].get("USS", 0), "swap_pss": sample["total"].get("SwapPss", 0),
                             "anon_plus_swap": sample["total"].get("Anonymous", 0) + sample["total"].get("Swap", 0),
                             "desktop_anon_plus_swap": desktop["rollup"].get("Anonymous", 0) + desktop["rollup"].get("Swap", 0),
                             "desktop_rss": desktop["rollup"].get("Rss", 0),
                             "desktop_pss": desktop["rollup"].get("Pss", 0),
                             "desktop_private_dirty": desktop["rollup"].get("Private_Dirty", 0),
                             "threads": sample["total"].get("threads", 0), "processes": len(sample["processes"]),
                             "cpu_s": round(cpu_seconds(descendants(self.desktop_pid)), 2),
                             "host_available_kib": sample["meminfo"]["MemAvailable"],
                             "host_swap_free_kib": sample["meminfo"]["SwapFree"]})
            (self.out / "idle-timeline.json").write_text(json.dumps(timeline, indent=1))
            time.sleep(self.args.idle_interval)
        self.notes["idle_minutes"] = self.args.idle_minutes
        self.screenshot("6-after-idle.png")
        self.sample("6 after a long idle period")

    def scenario_5(self):
        from accessibility import items
        from local_murmur import eventually
        if not self.desktop_pid or self.app.poll() is not None:
            self.connect()
            self.settle()
        self.sample("5a before Settings (accessibility off)", repeat=1)
        self.set_accessibility(True)
        nuschl = eventually(lambda: next((n for n in self.registry.children() if n.name.lower() == "nuschl"), None),
                            "the desktop did not register with AT-SPI", timeout=60)
        self.settle(minimum=10)
        self.sample("5b accessibility on, before Settings")

        def button(name):
            def find():
                return next((node for node, role, label, _ in items(nuschl) if role == "push button" and label == name),
                            None)
            return eventually(find, f"no {name!r} button", timeout=30)
        button("Settings").press()
        self.settle(minimum=5)
        self.screenshot("5-settings.png")
        self.sample("5c Settings open (devices listed)")
        button("Test microphone").press()
        time.sleep(10)
        self.screenshot("5-mic-test.png")
        self.sample("5d microphone test running")
        button("Stop test").press()
        time.sleep(2)
        button("Done").press()
        eventually(lambda: not any(label == "Done" for _, _, label, _ in items(nuschl)), "Settings did not close",
                   timeout=20)
        self.settle(minimum=15)
        self.sample("5e Settings closed")
        self.set_accessibility(False)
        self.settle(minimum=15)
        self.sample("5f accessibility off again")

    def vkcube(self):
        """A minimal Vulkan window (vkcube) in the same shell, for the floor."""
        process = subprocess.Popen([str(self.args.vkcube / "bin/vkcube"), "--wsi", "wayland"], env=self.env,
                                   stdout=(self.out / "vkcube.log").open("w"), stderr=subprocess.STDOUT,
                                   start_new_session=True)
        try:
            time.sleep(15)
            self.desktop_pid = process.pid
            self.sample("floor: vkcube (Vulkan, RADV, Wayland)")
        finally:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait(timeout=10)
            self.desktop_pid = None

    def run(self):
        try:
            self.start_infrastructure()
            if self.args.vkcube:
                self.vkcube()
            for scenario in self.args.scenarios.split(","):
                started = time.monotonic()
                getattr(self, f"scenario_{scenario.strip()}")()
                self.notes[f"scenario_{scenario}_s"] = round(time.monotonic() - started)
            self.notes["window_after_s"] = round(getattr(self, "window_at", 0), 1)
        finally:
            try:
                self.stop_desktop()
            except Exception as error:
                print(f"stopping the desktop: {error}", flush=True)
            for process in self.helpers:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
            if self.bot:
                try:
                    self.bot.close()
                except AssertionError:
                    pass
            if self.shell:
                self.shell.stop()
            if self.murmur:
                self.murmur.stop()
            (self.out / "notes.json").write_text(json.dumps(self.notes, indent=1))
        if self.args.heaptrack and getattr(self, "heaptrack_file", None):
            record = next(iter(sorted(self.out.glob("desktop.heaptrack*"))), None)
            if record:
                # An address-space cap, so the analysis cannot push the host into swap.
                printed = subprocess.run(["prlimit", "--as=12884901888", "--",
                                          str(self.args.heaptrack / "bin/heaptrack_print"), "-f", str(record),
                                          "--print-leaks", "1", "--print-peaks", "1", "--print-allocators", "1",
                                          "--print-temporary", "0", "--disable-builtin-suppressions", "-n", "40", "-s", "8"],
                                         capture_output=True, text=True, timeout=1800)
                (self.out / "heaptrack.txt").write_text(printed.stdout + printed.stderr)


# ----------------------------------------------------------------------------- report

def mib(kib):
    return f"{kib / KIB:.1f}"


def report(folders):
    samples = []
    for folder in folders:
        for path in sorted(Path(folder).glob("*.json")):
            if path.name == "notes.json" or path.name.startswith("idle-timeline"):
                continue
            sample = json.loads(path.read_text())
            sample["folder"] = Path(folder).name
            samples.append(sample)
    samples.sort(key=lambda sample: sample["time"])
    lines = ["## Scenario totals (whole process tree)", "",
             "| run | scenario | PSS | swap PSS | PSS+swap | anon+swap | USS | private dirty | RSS | processes | threads | GPU VRAM | GPU GTT |",
             "|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
    for sample in samples:
        total = sample["total"]
        lines.append(f"| {sample['folder']} | {sample['label']} | {mib(total.get('Pss', 0))} | {mib(total.get('SwapPss', 0))} | "
                     f"{mib(total.get('Pss', 0) + total.get('SwapPss', 0))} | {mib(total.get('Anonymous', 0) + total.get('Swap', 0))} | "
                     f"{mib(total.get('USS', 0))} | {mib(total.get('Private_Dirty', 0))} | {mib(total.get('Rss', 0))} | "
                     f"{len(sample['processes'])} | {total.get('threads', 0)} | "
                     f"{mib(total.get('gpu:drm-memory-vram', 0))} | {mib(total.get('gpu:drm-memory-gtt', 0))} |")
    lines += ["", "## Per process (MiB)", ""]
    for sample in samples:
        lines += [f"### {sample['folder']}: {sample['label']}", "",
                  "| process | RSS | PSS | USS | priv. dirty | priv. clean | shared clean | shared dirty | anon | swap | file PSS | "
                  "threads | arenas | VmHWM | VRAM | GTT |",
                  "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|"]
        for process in sample["processes"]:
            r = process["rollup"]
            vram = sum(c["kib"].get("drm-memory-vram", 0) for c in process["drm"].values())
            gtt = sum(c["kib"].get("drm-memory-gtt", 0) for c in process["drm"].values())
            lines.append(f"| {process['name']} | {mib(r.get('Rss', 0))} | {mib(r.get('Pss', 0))} | {mib(process['uss'])} | "
                         f"{mib(r.get('Private_Dirty', 0))} | {mib(r.get('Private_Clean', 0))} | "
                         f"{mib(r.get('Shared_Clean', 0))} | {mib(r.get('Shared_Dirty', 0))} | {mib(r.get('Anonymous', 0))} | "
                         f"{mib(r.get('Swap', 0))} | {mib(r.get('Pss_File', 0))} | {process['thread_count']} | {process['arenas']} | "
                         f"{mib(process['status'].get('VmHWM', 0))} | {mib(vram)} | {mib(gtt)} |")
        lines.append("")
        for process in sample["processes"]:
            if process["name"] != "desktop":
                continue
            lines += [f"Desktop by category ({sample['label']}):", "",
                      "| category | mappings | RSS | PSS | private dirty | private clean | shared clean | swap |",
                      "|---|---:|---:|---:|---:|---:|---:|---:|"]
            for name, values in sorted(process["categories"].items(), key=lambda item: -item[1].get("Pss", 0)):
                lines.append(f"| {name} | {values.get('mappings', 0)} | {mib(values.get('Rss', 0))} | {mib(values.get('Pss', 0))} | "
                             f"{mib(values.get('Private_Dirty', 0))} | {mib(values.get('Private_Clean', 0))} | "
                             f"{mib(values.get('Shared_Clean', 0))} | {mib(values.get('Swap', 0))} |")
            lines += ["", "Threads: " + ", ".join(f"{name} ×{count}" if count > 1 else name
                                                 for name, count in sorted(process["threads"].items())), ""]
    return "\n".join(lines)


# ----------------------------------------------------------------------------- heap attribution

# Driver frames are C names matched at the start of the frame (no substrings).
DRIVER_FRAMES = [
    ("GPU driver: radeonsi/GL context (EGL, gallium)",
     re.compile(r"^(egl[A-Z]|dri_|dri2_|driCreate|st_|si_|_mesa_|glthread|_glapi|pipe_|u_upload|tc_|threaded_|"
                r"amdgpu_winsys|amdgpu_cs|radeon_drm|gbm_|ac_llvm|lp_|llvmpipe|draw_)")),
    ("GPU driver: RADV/Vulkan", re.compile(r"^(radv_|vk_|aco|ac_|nir_|wsi_|Radv|vk[A-Z])")),
    ("GPU driver: LLVM", re.compile(r"^(llvm::|LLVM)")),
    ("GPU driver: Vulkan loader", re.compile(r"^(loader_|terminator_|vkCreateInstance|vkEnumerate)")),
]
LIBRARY_FRAMES = [
    ("AccessKit", re.compile(r"accesskit")),
    ("fontdb", re.compile(r"fontdb::")),
    ("cosmic-text / harfrust / swash", re.compile(r"cosmic_text::|harfrust::|swash::|skrifa::|read_fonts::")),
    ("GPUI text system", re.compile(r"gpui::text_system|gpui_wgpu::cosmic_text_system")),
    ("GPUI element arena", re.compile(r"gpui::arena")),
    ("GPUI renderer (wgpu atlas, scene, buffers)", re.compile(r"gpui_wgpu::|WgpuRenderer|WgpuAtlas|gpui::scene")),
    ("wgpu / naga", re.compile(r"\bwgpu|naga::|gpu_allocator")),
    ("image decoding", re.compile(r"\bimage::|\bpng::|zune_")),
    ("SVG (usvg/resvg)", re.compile(r"usvg::|resvg::|tiny_skia")),
    ("zbus / D-Bus", re.compile(r"zbus::|zvariant::")),
    ("tokio", re.compile(r"tokio::")),
    ("Wayland / xkbcommon", re.compile(r"wayland_client|smithay|xkbcommon|xkb_")),
    ("GPUI (other)", re.compile(r"\bgpui::|gpui_kit::|gpui_linux::")),
]
NUSCHL_FRAME = re.compile(r"(nuschl[a-z_]*::[A-Za-z0-9_]+(?:::[A-Z][A-Za-z0-9_]*)?(?:>?::[a-z_][a-z0-9_]*)?)")
PLUMBING = ("<alloc::", "alloc::", "<hashbrown", "hashbrown::", "core::", "<core::", "__", "malloc", "calloc",
            "realloc", "operator new")


def heap_owner(frames):
    """Driver C frames first, then the leaf-most Nuschl frame (not `main`), then the leaf-most library frame."""
    for name, pattern in DRIVER_FRAMES:
        if any(pattern.match(frame) for frame in frames):
            return name, None
    for frame in reversed(frames):
        match = NUSCHL_FRAME.search(frame)
        if match and not match.group(1).startswith("nuschl::main"):
            return "Nuschl", match.group(1).replace(">", "")
    for frame in reversed(frames):
        for name, pattern in LIBRARY_FRAMES:
            if pattern.search(frame):
                return name, None
    return "other / unresolved", None


def heap(path):
    totals, details = defaultdict(int), defaultdict(lambda: defaultdict(int))
    grand = 0
    for line in open(path, errors="replace"):
        stack, _, cost = line.rstrip("\n").rpartition(" ")
        if not cost.isdigit():
            continue
        cost = int(cost)
        grand += cost
        frames = [frame for frame in stack.split(";") if frame]
        name, detail = heap_owner(frames)
        totals[name] += cost
        if detail is None:
            detail = next((frame for frame in reversed(frames) if not frame.startswith(PLUMBING)),
                          frames[-1] if frames else "?")
        details[name][detail[:120]] += cost
    lines = [f"live at the end: {grand / 1048576:.2f} MiB"]
    for name, cost in sorted(totals.items(), key=lambda item: -item[1]):
        lines.append(f"{cost / 1048576:7.2f} MiB  {name}")
        for detail, value in sorted(details[name].items(), key=lambda item: -item[1])[:40]:
            if value >= 16 * 1024:
                lines.append(f"           {value / 1048576:6.2f}  {detail}")
    return "\n".join(lines)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    commands = parser.add_subparsers(dest="command", required=True)
    run = commands.add_parser("run")
    run.add_argument("--package", type=Path, required=True, help="the nuschl package (nix build .#default)")
    run.add_argument("--murmur", type=Path, required=True, help="mumble-server executable")
    run.add_argument("--at-spi", type=Path, required=True, help="at-spi2-core package")
    run.add_argument("--pulse-bin", type=Path, required=True, help="PulseAudio's bin folder")
    run.add_argument("--gnome-shell", type=Path, required=True)
    run.add_argument("--output", type=Path, required=True)
    run.add_argument("--scenarios", default="1,2,3,4,6,5")
    run.add_argument("--messages", type=int, default=2000)
    run.add_argument("--scroll-rounds", type=int, default=40)
    run.add_argument("--idle-minutes", type=float, default=15)
    run.add_argument("--idle-interval", type=float, default=30)
    run.add_argument("--env", action="append", default=[], help="K=V for the desktop (an experiment)")
    run.add_argument("--min-available-gib", type=float, default=8,
                     help="stop when the host's MemAvailable falls below this (default 8)")
    run.add_argument("--max-swap-gib", type=float, default=16,
                     help="stop when the host's swap use rises above this (default 16)")
    run.add_argument("--taskset", help="CPU list for the desktop (taskset -c), an experiment")
    run.add_argument("--heaptrack", type=Path, help="heaptrack package: run the desktop under it")
    run.add_argument("--vkcube", type=Path, help="vulkan-tools package: measure vkcube first, as a floor")
    show = commands.add_parser("report")
    show.add_argument("folders", nargs="+", type=Path)
    stacks = commands.add_parser("heap")
    stacks.add_argument("stacks", type=Path, help="heaptrack_print -t 0 --flamegraph-cost-type leaked -F output")
    args = parser.parse_args()
    if args.command == "report":
        print(report(args.folders))
        return
    if args.command == "heap":
        print(heap(args.stacks))
        return
    for name in ("TMPDIR", "NUSCHL_WL_TMP"):
        if not os.environ.get(name) or os.environ[name].startswith("/tmp"):
            parser.error(f"set {name} to a scratch folder on disk (not /tmp)")
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschl-ram-") as temporary:
        Audit(args, Path(temporary)).run()


if __name__ == "__main__":
    main()
