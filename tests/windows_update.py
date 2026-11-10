#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""In-place update of the Windows build, in the VM from windows_vm.py.

    windows_update.py RESULT_WINDOWS --output DIR [--rsign PATH]

Installs RESULT_WINDOWS/bin/*.exe into a per-user folder in the guest, with a
throwaway minisign key as `update-key.pub` beside it (the release key is not
used). Publishes a signed "9.9.9" through the harness (the same programs plus a
marker file), starts Nuschel with NUSCHEL_UPDATE_URL pointing at it, clicks
"Update and restart" and "Restart now", and checks that the files were swapped,
the marker arrived, a new process runs and the old files are cleaned up.
"""
import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
VM = [sys.executable, str(HERE / "windows_vm.py")]
REPO = HERE.parent
# The update bar's button in a 1280x800 guest with Nuschel maximised.
UPDATE_BUTTON = (1200, 52)


def vm(*arguments, timeout=600):
    return subprocess.run([*VM, *arguments], check=True, capture_output=True, text=True, timeout=timeout).stdout


def guest(script, timeout=600):
    return vm("run", script, "--timeout", str(timeout), timeout=timeout + 60)


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("build", type=Path, help="the .#windows result")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rsign", default=shutil.which("rsign") or "rsign")
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    files = Path(os.environ.get("NUSCHEL_WINDOWS_VM_DIR", Path.home() / "vms" / "nuschel-windows")) / "files"
    with tempfile.TemporaryDirectory(prefix="nuschel-update-") as temporary:
        root = Path(temporary)
        subprocess.run([args.rsign, "generate", "-W", "-p", str(root / "test.pub"), "-s", str(root / "test.secret"),
                        "-c", "Nuschel update test key"], check=True, capture_output=True)
        staging = root / "release"
        (staging / "licenses").mkdir(parents=True)
        for program in sorted((args.build / "bin").glob("*.exe")):
            shutil.copyfile(program, staging / program.name)
            vm("push", str(program), f"install--{program.name}")
        (staging / "licenses" / "UPDATE-TEST").write_text("delivered by the update test\n")
        subprocess.run([sys.executable, str(REPO / "release" / "make-update.py"), "--version", "9.9.9",
                        "--base-url", "http://10.0.2.100/files/", "--platform", f"windows-x86_64={staging}",
                        "--output", str(files), "--sign-key", str(root / "test.secret"), "--rsign", args.rsign],
                       check=True, capture_output=True)
        vm("push", str(root / "test.pub"), "update-key.pub")
    print(guest(r'''
$dir = "$env:LOCALAPPDATA\Programs\NuschelUpdateTest"
Get-Process nuschel -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $dir | Out-Null
foreach ($f in "nuschel.exe","nuschel-content-worker.exe","nuschel-media-worker.exe","nuschel-storage-worker.exe") {
  Invoke-WebRequest -UseBasicParsing "http://10.0.2.100/files/install--$f" -OutFile "$dir\$f"
}
Invoke-WebRequest -UseBasicParsing "http://10.0.2.100/files/update-key.pub" -OutFile "$dir\update-key.pub"
$env:NUSCHEL_UPDATE_URL = "http://10.0.2.100/files/"
$p = Start-Process "$dir\nuschel.exe" -ArgumentList "--profile-dir","C:\nuschel-update-profile" -PassThru
"started $($p.Id)"
'''), flush=True)
    time.sleep(8)
    vm("key", "meta_l-up")  # maximise: the update bar's button sits at a known place
    time.sleep(25)  # the first check runs 20 s after start
    vm("screenshot", str(args.output / "update-offered.png"))
    vm("click", *map(str, UPDATE_BUTTON))
    time.sleep(25)
    vm("screenshot", str(args.output / "update-installed.png"))
    before = guest('(Get-Process nuschel).Id')
    vm("click", *map(str, UPDATE_BUTTON))  # "Restart now" replaces the update button
    time.sleep(15)
    vm("screenshot", str(args.output / "update-restarted.png"))
    report = guest(r'''
$dir = "$env:LOCALAPPDATA\Programs\NuschelUpdateTest"
"marker: " + (Test-Path "$dir\licenses\UPDATE-TEST")
"old files left: " + (Test-Path "$dir\.nuschel-old")
"staging left: " + (Test-Path "$dir\.nuschel-update")
"process: " + ((Get-Process nuschel -ErrorAction SilentlyContinue).Id -join ",")
''')
    print(report, flush=True)
    assert "marker: True" in report, report
    assert "staging left: False" in report, report
    assert "old files left: False" in report, "the restarted process did not clean up the old files"
    assert before.strip() and before.strip() not in report.split("process:")[1], "Nuschel did not restart"
    print("Windows in-place update: signed manifest verified, files swapped, restarted, old files removed.")


if __name__ == "__main__":
    main()
