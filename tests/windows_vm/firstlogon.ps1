# SPDX-License-Identifier: EUPL-1.2
# Also invoked through QMP to upgrade an already installed disposable guest.
param([switch]$RefreshOnly)
$ErrorActionPreference = 'Stop'
if (-not $RefreshOnly) {
    powercfg /change monitor-timeout-ac 0
    powercfg /change standby-timeout-ac 0
    powercfg /change hibernate-timeout-ac 0
    reg add 'HKCU\Control Panel\Desktop' /v ScreenSaveActive /t REG_SZ /d 0 /f
    reg add 'HKLM\SOFTWARE\Policies\Microsoft\Windows\Personalization' /v NoLockScreen /t REG_DWORD /d 1 /f
    reg add 'HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' /v NoAutoUpdate /t REG_DWORD /d 1 /f
    reg add 'HKLM\SOFTWARE\Policies\Microsoft\Edge' /v HideFirstRunExperience /t REG_DWORD /d 1 /f
}
$agent = 'C:\nuschl-agent'
$cd = (Get-Volume -FileSystemLabel NUSCHLVM -ErrorAction Stop).DriveLetter + ':\'
# Stop only this harness's old/new agent, never unrelated PowerShell processes.
Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe'" | Where-Object {
    $_.ProcessId -ne $PID -and $_.CommandLine -match '(?i)-File\s+"?C:\\(?:nuschel|nuschl)-agent\\agent\.ps1(?:"|\s|$)'
} | ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
New-Item -ItemType Directory -Force -Path $agent | Out-Null
Copy-Item ($cd + 'agent.ps1') "$agent\agent.ps1" -Force
$runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
Remove-ItemProperty -Path $runKey -Name NuschelAgent -ErrorAction SilentlyContinue
$run = "powershell -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File $agent\agent.ps1"
Set-ItemProperty -Path $runKey -Name NuschlAgent -Value $run
Start-Process powershell -ArgumentList '-NoProfile','-WindowStyle','Hidden','-ExecutionPolicy','Bypass','-File',"$agent\agent.ps1"
