# SPDX-License-Identifier: EUPL-1.2
# Test agent in the user's desktop session: fetch a PowerShell script from the
# host harness (reachable only through QEMU's guest forwarding), run it, and
# post the output back. GUI programs it starts appear on the desktop.
$ErrorActionPreference = 'Continue'
$ProgressPreference = 'SilentlyContinue'
$host_url = 'http://10.0.2.100'
while ($true) {
    try {
        $job = Invoke-RestMethod -Uri "$host_url/next" -TimeoutSec 60
        if ($job -and $job.id) {
            $output = try {
                $block = [ScriptBlock]::Create($job.script)
                & $block 2>&1 | Out-String -Width 400
            } catch {
                "ERROR: " + ($_ | Out-String)
            }
            $body = @{ id = $job.id; output = $output } | ConvertTo-Json -Compress
            Invoke-RestMethod -Uri "$host_url/result" -Method Post -Body ([Text.Encoding]::UTF8.GetBytes($body)) -ContentType 'application/json' -TimeoutSec 60 | Out-Null
        }
    } catch {
        Start-Sleep -Seconds 2
    }
}
