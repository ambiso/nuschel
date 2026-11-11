#!/usr/bin/env python3
# SPDX-License-Identifier: EUPL-1.2
"""In-place update of the Windows build, in the VM from windows_vm.py.

    windows_update.py RESULT_WINDOWS --output DIR [--rsign PATH]

Installs RESULT_WINDOWS/bin/*.exe into a per-user folder in the guest, with a
throwaway minisign key as `update-key.pub` beside it (the release key is not
used). Publishes a signed "9.9.9" through the harness (the same programs plus a
marker file), starts Nuschl with NUSCHL_UPDATE_URL pointing at it, clicks
"Update and restart" and "Restart now", and checks that the files were swapped,
the marker arrived, a new process runs and the old files are cleaned up.
"""
import argparse
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
VM = [sys.executable, str(HERE / "windows_vm.py")]
REPO = HERE.parent
# The update bar's button in a 1280x800 guest with Nuschl maximised.
UPDATE_BUTTON = (1200, 52)
UPDATE_PORT = 18765

# The production updater has no harness credential. Serve only prefetched,
# synthetic signed update artifacts on the disposable guest's own loopback.
# Host files remain authenticated, and no credential enters updater URLs.
FILE_SERVER = r'''
if ($global:NuschlUpdateFixture) {
  Stop-Job $global:NuschlUpdateFixture -ErrorAction SilentlyContinue
  Remove-Job $global:NuschlUpdateFixture -Force -ErrorAction SilentlyContinue
}
$fixture = "$env:TEMP\nuschl-signed-update-fixture"
Remove-Item -LiteralPath $fixture -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Path $fixture | Out-Null
foreach ($name in __FILES__) { Get-NuschlTestFile -Name $name -OutFile "$fixture\$name" }
$global:NuschlUpdateFixture = Start-Job -ArgumentList $fixture,__PORT__ -ScriptBlock {
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


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("build", type=Path, help="the .#windows result")
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rsign", default=shutil.which("rsign") or "rsign")
    args = parser.parse_args()
    try:
        exercise_update(args)
    finally:
        try:
            guest("if ($global:NuschlUpdateFixture) { Stop-Job $global:NuschlUpdateFixture; Remove-Job $global:NuschlUpdateFixture -Force; $global:NuschlUpdateFixture = $null }", timeout=30)
        except (subprocess.SubprocessError, OSError, RuntimeError):
            pass


def exercise_update(args):
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="nuschl-update-") as temporary:
        root = Path(temporary)
        subprocess.run([args.rsign, "generate", "-W", "-p", str(root / "test.pub"), "-s", str(root / "test.secret"),
                        "-c", "Nuschl update test key"], check=True, capture_output=True)
        staging = root / "release"
        (staging / "licenses").mkdir(parents=True)
        for program in sorted((args.build / "bin").glob("*.exe")):
            shutil.copyfile(program, staging / program.name)
            vm("push", str(program), f"install--{program.name}")
        (staging / "licenses" / "UPDATE-TEST").write_text("delivered by the update test\n")
        published = root / "published"
        subprocess.run([sys.executable, str(REPO / "release" / "make-update.py"), "--version", "9.9.9",
                        "--base-url", f"http://127.0.0.1:{UPDATE_PORT}/", "--platform", f"windows-x86_64={staging}",
                        "--output", str(published), "--sign-key", str(root / "test.secret"), "--rsign", args.rsign],
                       check=True, capture_output=True)
        artifact_names = []
        for artifact in sorted(published.iterdir()):
            artifact_names.append(artifact.name)
            vm("push", str(artifact), artifact.name)
        vm("push", str(root / "test.pub"), "update-key.pub")
    names = "@(" + ",".join("'" + name + "'" for name in artifact_names) + ")"
    print(guest(FILE_SERVER.replace("__FILES__", names).replace("__PORT__", str(UPDATE_PORT))), flush=True)
    print(guest(r'''
$dir = "$env:LOCALAPPDATA\Programs\NuschlUpdateTest"
Get-Process nuschl -ErrorAction SilentlyContinue | Stop-Process -Force
Remove-Item -Recurse -Force $dir -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path $dir | Out-Null
foreach ($f in "nuschl.exe","nuschl-content-worker.exe","nuschl-media-worker.exe","nuschl-storage-worker.exe") {
  Get-NuschlTestFile -Name "install--$f" -OutFile "$dir\$f"
}
Get-NuschlTestFile -Name "update-key.pub" -OutFile "$dir\update-key.pub"
$env:NUSCHL_UPDATE_URL = "http://127.0.0.1:18765/"
$p = Start-Process "$dir\nuschl.exe" -ArgumentList "--profile-dir","C:\nuschl-update-profile" -PassThru
"started $($p.Id)"
'''), flush=True)
    time.sleep(8)
    vm("key", "meta_l-up")  # maximise: the update bar's button sits at a known place
    time.sleep(25)  # the first check runs 20 s after start
    vm("screenshot", str(args.output / "update-offered.png"))
    vm("click", *map(str, UPDATE_BUTTON))
    time.sleep(25)
    vm("screenshot", str(args.output / "update-installed.png"))
    before = guest('(Get-Process nuschl).Id')
    vm("click", *map(str, UPDATE_BUTTON))  # "Restart now" replaces the update button
    time.sleep(15)
    vm("screenshot", str(args.output / "update-restarted.png"))
    report = guest(r'''
$dir = "$env:LOCALAPPDATA\Programs\NuschlUpdateTest"
"marker: " + (Test-Path "$dir\licenses\UPDATE-TEST")
"old files left: " + (Test-Path "$dir\.nuschl-old")
"staging left: " + (Test-Path "$dir\.nuschl-update")
"process: " + ((Get-Process nuschl -ErrorAction SilentlyContinue).Id -join ",")
''')
    print(report, flush=True)
    assert "marker: True" in report, report
    assert "staging left: False" in report, report
    assert "old files left: False" in report, "the restarted process did not clean up the old files"
    assert before.strip() and before.strip() not in report.split("process:")[1], "Nuschl did not restart"
    print("Windows in-place update: signed manifest verified, files swapped, restarted, old files removed.")


if __name__ == "__main__":
    main()
