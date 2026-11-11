# SPDX-License-Identifier: EUPL-1.2
# First logon in the disposable test VM: keep the session awake and unlocked,
# install the agent for every logon, and start it now.
$ErrorActionPreference = 'Continue'
powercfg /change monitor-timeout-ac 0
powercfg /change standby-timeout-ac 0
powercfg /change hibernate-timeout-ac 0
reg add 'HKCU\Control Panel\Desktop' /v ScreenSaveActive /t REG_SZ /d 0 /f
reg add 'HKLM\SOFTWARE\Policies\Microsoft\Windows\Personalization' /v NoLockScreen /t REG_DWORD /d 1 /f
# No Windows Update or first-run noise in a VM that has no internet anyway.
reg add 'HKLM\SOFTWARE\Policies\Microsoft\Windows\WindowsUpdate\AU' /v NoAutoUpdate /t REG_DWORD /d 1 /f
reg add 'HKLM\SOFTWARE\Policies\Microsoft\Edge' /v HideFirstRunExperience /t REG_DWORD /d 1 /f
$agent = 'C:\nuschl-agent'
New-Item -ItemType Directory -Force -Path $agent | Out-Null
$cd = (Get-Volume -FileSystemLabel NUSCHLVM).DriveLetter + ':\'
Copy-Item ($cd + 'agent.ps1') $agent -Force
$run = "powershell -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File $agent\agent.ps1"
reg add 'HKCU\Software\Microsoft\Windows\CurrentVersion\Run' /v NuschlAgent /t REG_SZ /d $run /f
Start-Process powershell -ArgumentList '-NoProfile','-WindowStyle','Hidden','-ExecutionPolicy','Bypass','-File',"$agent\agent.ps1"
