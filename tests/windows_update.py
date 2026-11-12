#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""In-place update of the Windows build, in the VM from windows_vm.py.

    windows_update.py RESULT_WINDOWS --output DIR --secret-file FIXTURE_SECRET --public-file FIXTURE_PUBLIC --signer SIGNER

Installs RESULT_WINDOWS/bin/*.exe into a per-user folder in the guest. This
MUST be a separately built fixture source tree embedding the provided temporary
hybrid public pair in crates/update/update-trust.json. No adjacent key or runtime
trust override exists. Never use production private keys for this harness. Publishes a signed "9.9.9" through the harness (the same programs plus a
marker file), starts Nuschl with NUSCHL_UPDATE_URL pointing at it, clicks
"Update and restart" and "Restart now", and checks that the files were swapped,
the marker arrived, a new process runs and the old files are cleaned up.
"""
import argparse
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
import time
import uuid
from pathlib import Path

HERE = Path(__file__).resolve().parent
VM = [sys.executable, str(HERE / "windows_vm.py")]
REPO = HERE.parent
# The update bar's button in a 1280x800 guest with Nuschl maximised.
UPDATE_BUTTON = (1200, 52)
REQUIRED_PROGRAMS = {
    "nuschl.exe", "nuschl-content-worker.exe", "nuschl-crypto-worker.exe",
    "nuschl-math-worker.exe", "nuschl-media-worker.exe", "nuschl-storage-worker.exe",
    "nuschl-transfer-worker.exe", "nuschl-update-verify-ed25519.exe",
    "nuschl-update-verify-mldsa44.exe",
}

# The production updater has no harness credential. Serve only prefetched,
# synthetic signed update artifacts on the disposable guest's own loopback.
# Host files remain authenticated, and no credential enters updater URLs.
FILE_SERVER = r'''
$fixture = "$env:TEMP\nuschl-signed-update-fixture-__RUN__"
New-Item -ItemType Directory -Path $fixture | Out-Null
foreach ($entry in __FILES__) { Get-NuschlTestFile -Name $entry.remote -OutFile "$fixture\$($entry.local)" }
$fixtureJob = Start-Job -Name "nuschl-update-__RUN__" -ArgumentList $fixture,__PORT__ -ScriptBlock {
  param($root, $port)
  $listener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback, $port)
  $listener.Start()
  try {
    while ($true) {
      # A blocking AcceptTcpClient prevents PowerShell Stop-Job from finishing.
      if (-not $listener.Pending()) { Start-Sleep -Milliseconds 50; continue }
      $client = $listener.AcceptTcpClient()
      $client.ReceiveTimeout = 5000
      $client.SendTimeout = 5000
      try {
        $stream = $client.GetStream()
        # Read at most 8 KiB of ASCII HTTP headers without an unbounded ReadLine.
        $header = New-Object Text.StringBuilder
        while ($header.Length -lt 8192) {
          $b = $stream.ReadByte()
          if ($b -lt 0 -or $b -gt 127) { break }
          [void]$header.Append([char]$b)
          if ($header.ToString().EndsWith("`r`n`r`n")) { break }
        }
        $text = $header.ToString()
        $first = ($text -split "`r`n")[0]
        if (-not $text.EndsWith("`r`n`r`n") -or $first -cnotmatch '^GET /([A-Za-z0-9][A-Za-z0-9_.-]{0,127}) HTTP/1\.[01]$') {
          $bytes = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 400 Bad Request`r`nContent-Length: 0`r`nConnection: close`r`n`r`n")
          $stream.Write($bytes,0,$bytes.Length)
          continue
        }
        $file = Join-Path $root $Matches[1]
        if (-not [IO.File]::Exists($file)) {
          $bytes = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 404 Not Found`r`nContent-Length: 0`r`nConnection: close`r`n`r`n")
          $stream.Write($bytes,0,$bytes.Length)
          continue
        }
        $source = [IO.File]::OpenRead($file)
        try {
          $bytes = [Text.Encoding]::ASCII.GetBytes("HTTP/1.1 200 OK`r`nContent-Type: application/octet-stream`r`nContent-Length: $($source.Length)`r`nConnection: close`r`n`r`n")
          $stream.Write($bytes,0,$bytes.Length)
          $source.CopyTo($stream)
        } finally { $source.Dispose() }
      } catch { } finally { $client.Dispose() }
    }
  } finally { $listener.Stop() }
}
"FIXTURE_JOB_ID=$($fixtureJob.Id)"
Start-Sleep -Seconds 2
# Fail before starting Nuschl if the fixture did not bind or publish its files.
$probe = [Net.HttpWebRequest]::Create('http://127.0.0.1:__PORT__/update.json')
$probe.Proxy = $null
$probe.Timeout = 5000
$response = $probe.GetResponse()
$response.Dispose()
'''


def vm(*arguments, timeout=600):
    return subprocess.run([*VM, *arguments], check=True, capture_output=True, text=True, timeout=timeout).stdout


def guest(script, timeout=600):
    output = vm("run", script, "--timeout", str(timeout), timeout=timeout + 60)
    if "ERROR: control job failed:" in output:
        raise RuntimeError("Windows fixture job failed; see its bounded agent result")
    return output


def payload(build, public_file):
    """Reject missing helpers, operator tools or a non-fixture desktop first."""
    programs = sorted((build / "bin").glob("*.exe"))
    if {path.name for path in programs} != REQUIRED_PROGRAMS:
        raise ValueError("fixture needs the exact complete desktop/helper payload, without signer/test tools")
    if any(not path.is_file() or path.is_symlink() for path in programs):
        raise ValueError("fixture programs must be regular files")
    if public_file.is_symlink() or not public_file.is_file() or public_file.stat().st_size > 16384:
        raise ValueError("fixture public trust document must be a bounded regular file")
    public = public_file.read_bytes()
    trust = json.loads(public)
    if trust.get("format") != 2 or len(trust.get("keys", [])) != 1:
        raise ValueError("one temporary fixture public pair is required")
    # Necessary build sanity check, not a replacement for reviewed source/build
    # provenance: RELEASE_TRUST is an include_str constant in the desktop.
    if public not in (build / "bin/nuschl.exe").read_bytes():
        raise ValueError("desktop does not embed the exact supplied temporary trust document")
    notices = build / "share/licenses/nuschl"
    if notices.is_symlink() or not notices.is_dir() or not (notices / "THIRD-PARTY.json").is_file():
        raise ValueError("fixture must preserve the actual packaged notice payload")
    if any(path.is_symlink() for path in notices.rglob("*")):
        raise ValueError("fixture notices must not contain symlinks")
    return programs, notices


def owned_processes(run):
    # UUID is generated locally, never supplied by the build/artifacts.
    assert re.fullmatch(r"[0-9a-f]{32}", run)
    return r'''$ownedExe = "$env:LOCALAPPDATA\Programs\NuschlUpdateTest\__RUN__\nuschl.exe"
$owned = @(Get-CimInstance Win32_Process -Filter "Name='nuschl.exe'" | Where-Object { $_.ExecutablePath -eq $ownedExe })
'''.replace("__RUN__", run)


def copy_notices(source, destination):
    shutil.copytree(source, destination)
    # Nix output directories are read-only. Only this private staging copy
    # needs a writable directory for the synthetic delivery marker; retain all
    # original notice body bytes and never chmod the package/store source.
    for directory, _, _ in os.walk(destination):
        path = Path(directory)
        path.chmod(path.stat().st_mode | 0o300)


def stop_owned(run):
    return owned_processes(run) + "$owned | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }\n"


def focus_owned(run):
    guest(owned_processes(run) + r'''
if ($owned.Count -ne 1) { throw 'expected exactly one owned desktop process' }
Add-Type -AssemblyName UIAutomationClient
$condition = New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::ProcessIdProperty, [int]$owned[0].ProcessId)
$window = [Windows.Automation.AutomationElement]::RootElement.FindFirst([Windows.Automation.TreeScope]::Children, $condition)
if ($null -eq $window) { throw 'owned desktop window unavailable; no global click allowed' }
$window.SetFocus()
''')


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("build", type=Path, help="the .#windows result")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--signer", required=True)
    parser.add_argument("--secret-file", type=Path, required=True, help="temporary fixture pair only")
    parser.add_argument("--public-file", type=Path, required=True, help="same fixture pair embedded when building the guest app")
    args = parser.parse_args()
    if "NUSCHL_WINDOWS_VM_DIR" not in os.environ:
        parser.error("explicit NUSCHL_WINDOWS_VM_DIR for the owned installed guest is required")
    args.run = uuid.uuid4().hex
    args.fixture_job = None
    try:
        exercise_update(args)
    finally:
        try:
            cleanup = stop_owned(args.run)
            cleanup += f"Get-Job -Name 'nuschl-update-{args.run}' -ErrorAction SilentlyContinue | ForEach-Object {{ Stop-Job $_; Remove-Job $_ -Force }}\n"
            for relative in ("Programs/NuschlUpdateTest/", "NuschlUpdateProfiles/"):
                cleanup += f"Remove-Item -LiteralPath (Join-Path $env:LOCALAPPDATA '{relative}{args.run}') -Recurse -Force -ErrorAction SilentlyContinue\n"
            cleanup += f"Remove-Item -LiteralPath (Join-Path $env:TEMP 'nuschl-signed-update-fixture-{args.run}') -Recurse -Force -ErrorAction SilentlyContinue\n"
            guest(cleanup, timeout=30)
        except (subprocess.SubprocessError, OSError, RuntimeError):
            pass


def exercise_update(args):
    programs, notices = payload(args.build, args.public_file)
    args.output.mkdir(parents=True, exist_ok=True)
    subprocess.run([args.signer, "key-status", "--secret-file", str(args.secret_file),
                    "--public-file", str(args.public_file)], check=True, capture_output=True)
    port = int(guest("$listener = New-Object Net.Sockets.TcpListener([Net.IPAddress]::Loopback,0); $listener.Start(); $port = $listener.LocalEndpoint.Port; $listener.Stop(); $port").strip())
    if not 1024 <= port <= 65535:
        raise ValueError("unexpected guest loopback fixture port")
    with tempfile.TemporaryDirectory(prefix="nuschl-update-") as temporary:
        root = Path(temporary)
        staging = root / "release"
        copy_notices(notices, staging / "licenses")
        initial_files = []
        for program in programs:
            shutil.copyfile(program, staging / program.name)
            remote = f"install-{args.run}--{program.name}"
            vm("push", str(program), remote)
            initial_files.append((remote, program.name))
        for index, notice in enumerate(sorted(path for path in notices.rglob("*") if path.is_file())):
            relative = "licenses/" + notice.relative_to(notices).as_posix()
            if not re.fullmatch(r"[A-Za-z0-9_./-]+", relative):
                raise ValueError("unsupported fixture notice path")
            remote = f"install-{args.run}-notice-{index}"
            vm("push", str(notice), remote)
            initial_files.append((remote, relative))
        (staging / "licenses" / "UPDATE-TEST").write_text("delivered by the update test\n")
        published = root / "published"
        subprocess.run([sys.executable, str(REPO / "release" / "make-update.py"), "--version", "9.9.9",
                        "--base-url", f"http://127.0.0.1:{port}/", "--platform", f"windows-x86_64={staging}",
                        "--output", str(published), "--sign-key", str(args.secret_file),
                        "--public-key", str(args.public_file), "--signer", args.signer],
                       check=True, capture_output=True)
        expected_files = json.loads((published / "update.json").read_bytes())["platforms"]["windows-x86_64"]
        artifact_names = []
        for artifact in sorted(published.iterdir()):
            artifact_names.append(artifact.name)
            vm("push", str(artifact), args.run + "--" + artifact.name)
    names = "@(" + ",".join("@{remote='" + args.run + "--" + name + "';local='" + name + "'}" for name in artifact_names) + ")"
    output = guest(FILE_SERVER.replace("__FILES__", names).replace("__PORT__", str(port)).replace("__RUN__", args.run))
    match = re.search(r"^FIXTURE_JOB_ID=([0-9]+)\r?$", output, re.M)
    if not match:
        raise RuntimeError("fixture listener job identity absent")
    args.fixture_job = int(match[1])
    print(output, flush=True)
    print(guest(r'''
$dir = "$env:LOCALAPPDATA\Programs\NuschlUpdateTest\__RUN__"
$relativeProfile = 'profiles\profile µ'
$profile = Join-Path $dir $relativeProfile
New-Item -ItemType Directory -Force -Path $dir | Out-Null
foreach ($f in __INITIAL_FILES__) {
  $destination = Join-Path $dir $f.local
  New-Item -ItemType Directory -Force -Path (Split-Path -Parent $destination) | Out-Null
  Get-NuschlTestFile -Name $f.remote -OutFile $destination
}
$info = New-Object Diagnostics.ProcessStartInfo
$info.FileName = "$dir\nuschl.exe"
$info.Arguments = '--profile-dir "' + $relativeProfile + '"'
$info.WorkingDirectory = $dir
$info.UseShellExecute = $false
$info.CreateNoWindow = $true
$info.EnvironmentVariables['NUSCHL_UPDATE_URL'] = "http://127.0.0.1:__PORT__/"
$p = [Diagnostics.Process]::Start($info)
"started $($p.Id)"
'''.replace("__RUN__", args.run).replace("__PORT__", str(port)).replace("__INITIAL_FILES__", "@(" + ",".join("@{remote='" + remote + "';local='" + local + "'}" for remote, local in initial_files) + ")")), flush=True)
    time.sleep(8)
    # Focus only the window belonging to the unique owned install before the
    # existing fixture's display-level key/click operations.
    focus_owned(args.run)
    vm("key", "meta_l-up")  # maximise: the update bar's button sits at a known place
    time.sleep(25)  # the first check runs 20 s after start
    vm("screenshot", str(args.output / "update-offered.png"))
    focus_owned(args.run)
    vm("click", *map(str, UPDATE_BUTTON))
    time.sleep(25)
    vm("screenshot", str(args.output / "update-installed.png"))
    before = guest(owned_processes(args.run) + '($owned.ProcessId -join ",")')
    before_state = guest(owned_processes(args.run) + r'''
$profile = Join-Path (Split-Path -Parent $ownedExe) 'profiles\profile µ'
(Get-FileHash -LiteralPath (Join-Path $profile 'state\update-state.json') -Algorithm SHA256).Hash
''').strip()
    if not re.fullmatch(r"[0-9A-Fa-f]{64}", before_state):
        raise RuntimeError("initial owned update-state hash absent")
    focus_owned(args.run)
    vm("click", *map(str, UPDATE_BUTTON))  # "Restart now" replaces the update button
    time.sleep(15)
    vm("screenshot", str(args.output / "update-restarted.png"))
    report = guest(owned_processes(args.run) + r'''
$dir = "$env:LOCALAPPDATA\Programs\NuschlUpdateTest\__RUN__"
"marker: " + (Test-Path "$dir\licenses\UPDATE-TEST")
"old files left: " + (Test-Path "$dir\.nuschl-old")
"staging left: " + (Test-Path "$dir\.nuschl-update")
"process: " + ($owned.ProcessId -join ",")
$profile = Join-Path $dir 'profiles\profile µ'
$pattern = '(?:^|\s)--profile-dir\s+"' + [Regex]::Escape($profile) + '"(?:\s|$)'
if ($owned.Count -ne 1 -or $owned[0].CommandLine -cnotmatch $pattern) { throw 'restarted process lost the exact absolute owned profile' }
$state = Join-Path $profile 'state\update-state.json'
if ((Get-FileHash -LiteralPath $state -Algorithm SHA256).Hash -ne '__STATE_HASH__') { throw 'owned update-state changed or vanished across restart' }
'owned absolute Unicode profile and update-state continuity: verified'
foreach ($expected in __EXPECTED_FILES__) {
  $file = Join-Path $dir $expected.path
  if (-not (Test-Path -LiteralPath $file)) { throw 'installed manifest file missing' }
  if ((Get-FileHash -LiteralPath $file -Algorithm SHA256).Hash.ToLowerInvariant() -ne $expected.sha256) { throw 'installed manifest hash differs' }
}
'complete signed file hashes: verified'
'''.replace("__RUN__", args.run).replace("__STATE_HASH__", before_state).replace("__EXPECTED_FILES__", "@(" + ",".join("@{path='" + item["path"] + "';sha256='" + item["sha256"] + "'}" for item in expected_files) + ")"))
    print(report, flush=True)
    assert "marker: True" in report, report
    assert "staging left: False" in report, report
    assert "old files left: False" in report, "the restarted process did not clean up the old files"
    after = report.split("process:")[1].splitlines()[0].strip()
    previous_pids = set(before.strip().split(",")) - {""}
    current_pids = set(after.split(",")) - {""}
    assert previous_pids and current_pids and previous_pids.isdisjoint(current_pids), "Nuschl did not replace every owned process"
    assert "owned absolute Unicode profile and update-state continuity: verified" in report, "profile continuity was not proved"
    assert "complete signed file hashes: verified" in report, "installed payload was not checked completely"
    print("Windows in-place update: signed manifest verified, files swapped, restarted, old files removed.")


if __name__ == "__main__":
    main()
