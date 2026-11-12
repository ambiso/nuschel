#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""Run actual hybrid/LPAC adversarial and loopback-install proof in an OWNED VM.

Boot the explicitly selected, existing disposable guest with windows_vm.py first.
This script never boots/reinstalls a guest, enrolls trust or imports private keys.
It runs the non-distributed runtime-fixture with two separately built engines.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import uuid

HERE = Path(__file__).resolve().parent
EXPECTED = [
    "PASS: actual desktop Tokio spawn_blocking context retains strict two-worker acceptance",
    "PASS: two real confined engines, either-component corruption, raw bytes, wrong pair, absent trust, legacy/downgrade, expiry, duplicate fields",
    "PASS: actual sandbox file/write/network probes, mandatory AND despite one forged answer, bounded output and timeout/reaping",
    "PASS: private HTTP hybrid manifest -> remembered version -> checked download -> atomic install -> cleanup",
]


def vm(*arguments):
    return subprocess.run([sys.executable, str(HERE / "windows_vm.py"), *arguments],
                          check=True, capture_output=True, text=True, timeout=300).stdout


def executable(path):
    path = Path(path).resolve(strict=True)
    if not path.is_file() or path.stat().st_size > 128 * 1024**2:
        raise ValueError("missing or oversized fixture executable")
    with path.open("rb") as source:
        if source.read(2) != b"MZ":
            raise ValueError("expected a native Windows PE executable")
    return path


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--ed25519", type=executable, required=True)
    parser.add_argument("--mldsa44", type=executable, required=True)
    parser.add_argument("--fixture", type=executable, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if "NUSCHL_WINDOWS_VM_DIR" not in os.environ:
        parser.error("explicit NUSCHL_WINDOWS_VM_DIR is required; no default guest is selected")
    state = Path(os.environ["NUSCHL_WINDOWS_VM_DIR"]).resolve(strict=True)
    if not (state / "installed").is_file():
        parser.error("only an existing installed disposable guest may be used")
    status = json.loads(vm("status"))
    args.output.mkdir(parents=True, exist_ok=True)
    run = uuid.uuid4().hex
    binaries = [args.ed25519, args.mldsa44, args.fixture]
    names = [f"hybrid-{run}-{index}.exe" for index in range(3)]
    for path, name in zip(binaries, names):
        vm("push", str(path), name)
    # Names, run ID and hashes are locally generated ASCII. No source path or
    # private material is interpolated into PowerShell.
    script = r'''
$ErrorActionPreference = 'Stop'
$dir = Join-Path $env:TEMP 'nuschl-hybrid-__RUN__'
New-Item -ItemType Directory -Path $dir | Out-Null
try {
  Get-NuschlTestFile -Name '__ED__' -OutFile "$dir\nuschl-update-verify-ed25519.exe"
  Get-NuschlTestFile -Name '__ML__' -OutFile "$dir\nuschl-update-verify-mldsa44.exe"
  Get-NuschlTestFile -Name '__FIXTURE__' -OutFile "$dir\nuschl-update-runtime-fixture.exe"
  $program = "$dir\nuschl-update-runtime-fixture.exe"
  $arguments = @('"' + "$dir\nuschl-update-verify-ed25519.exe" + '"', '"' + "$dir\nuschl-update-verify-mldsa44.exe" + '"')
  $info = New-Object Diagnostics.ProcessStartInfo
  $info.FileName = $program
  $info.Arguments = $arguments -join ' '
  $info.UseShellExecute = $false
  $info.CreateNoWindow = $true
  $info.RedirectStandardOutput = $true
  $info.RedirectStandardError = $true
  $p = New-Object Diagnostics.Process
  $p.StartInfo = $info
  [void]$p.Start()
  $stdout = $p.StandardOutput.ReadToEndAsync()
  $stderr = $p.StandardError.ReadToEndAsync()
  if (-not $p.WaitForExit(180000)) {
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    throw 'native hybrid fixture exceeded 180 second outer deadline'
  }
  $code = $p.ExitCode
  $stdout.Result
  $stderr.Result
  "FIXTURE_EXIT_CODE=$code"
  if ($code -ne 0) { throw "native hybrid fixture rejected ($code)" }
} finally {
  Remove-Item -LiteralPath $dir -Recurse -Force -ErrorAction SilentlyContinue
}
'''
    for key, value in {"__RUN__": run, "__ED__": names[0], "__ML__": names[1], "__FIXTURE__": names[2]}.items():
        script = script.replace(key, value)
    output = vm("run", script, "--timeout", "240")
    (args.output / "guest.log").write_text(output)
    if "ERROR: control job failed:" in output or "FIXTURE_EXIT_CODE=0" not in output:
        raise RuntimeError("native fixture failed; inspect bounded guest.log")
    for expected in EXPECTED:
        if expected not in output:
            raise RuntimeError("missing actual native proof stage: " + expected)
    report = {"guest_state": str(state), "guest_status": status, "stages": EXPECTED,
              "keys": "generated only inside a temporary guest directory; no production trust enrollment",
              "binaries": [{"path": str(path), "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}
                           for path in binaries],
              "limits": ["Windows VM, not physical hardware or macOS", "No desktop UI restart proof from this fixture", "LPAC uses production profiles and both fixed algorithm launchers"]}
    (args.output / "proof.json").write_text(json.dumps(report, indent=2) + "\n")
    print("Native Windows hybrid verifier, LPAC fault boundaries and loopback install passed.")


if __name__ == "__main__":
    main()
