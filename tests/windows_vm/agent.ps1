# SPDX-License-Identifier: EUPL-1.2
# Trusted control agent for a disposable, single-user test guest. Credentials
# arrive on private read-only media, never through a URL or process argument.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
[Net.ServicePointManager]::Expect100Continue = $false
$script:AgentRevision = (Get-FileHash -Algorithm SHA256 -LiteralPath $PSCommandPath).Hash.ToLowerInvariant()

function Invoke-NuschlControl {
    param([string]$Path, [byte[]]$Body, [string]$OutFile)
    $cd = (Get-Volume -FileSystemLabel NUSCHLVM -ErrorAction Stop).DriveLetter
    $token = [IO.File]::ReadAllText("${cd}:\control.token").Trim()
    if ($token -cnotmatch '^[0-9a-f]{64}$') { throw 'Invalid harness credential media' }
    $request = [Net.HttpWebRequest]::Create("http://10.0.2.100$Path")
    # No proxy auto-discovery, redirects, cookies or implicit credentials.
    $request.Proxy = $null
    $request.AllowAutoRedirect = $false
    $request.KeepAlive = $false
    $request.Timeout = 60000
    $request.ReadWriteTimeout = 60000
    $request.Headers['Authorization'] = "Bearer $token"
    $request.Headers['X-Nuschl-Agent'] = $script:AgentRevision
    if ($null -ne $Body) {
        $request.Method = 'POST'
        $request.ContentType = 'application/json; charset=utf-8'
        $request.ContentLength = $Body.Length
        $stream = $request.GetRequestStream()
        try { $stream.Write($Body, 0, $Body.Length) } finally { $stream.Dispose() }
    }
    $response = $request.GetResponse()
    try {
        if ($OutFile) {
            $file = [IO.File]::Create($OutFile)
            try { $response.GetResponseStream().CopyTo($file) } finally { $file.Dispose() }
        } else {
            $reader = New-Object IO.StreamReader($response.GetResponseStream(), [Text.Encoding]::UTF8)
            try { $reader.ReadToEnd() } finally { $reader.Dispose() }
        }
    } finally { $response.Dispose() }
}

function Get-NuschlTestFile {
    param([Parameter(Mandatory=$true)][string]$Name, [Parameter(Mandatory=$true)][string]$OutFile)
    if ($Name -cnotmatch '^[A-Za-z0-9][A-Za-z0-9_.-]{0,127}$') { throw 'Invalid harness file name' }
    Invoke-NuschlControl -Path "/files/$Name" -OutFile $OutFile
}

while ($true) {
    try {
        $job = Invoke-NuschlControl -Path '/next' | ConvertFrom-Json
        if ($job -and $job.id) {
            if ($job.id -cnotmatch '^[0-9a-f]{32}$' -or $job.script -isnot [string] -or
                [Text.Encoding]::UTF8.GetByteCount($job.script) -gt 262144) { throw 'Invalid control job' }
            $output = New-Object Text.StringBuilder
            try {
                $block = [ScriptBlock]::Create($job.script)
                & $block 2>&1 | Out-String -Stream -Width 400 | ForEach-Object {
                    # Bound the result while streaming; JSON escaping remains <2 MiB.
                    $left = 131072 - $output.Length
                    if ($left -gt 0) { [void]$output.AppendLine($_.Substring(0, [Math]::Min($_.Length, $left))) }
                }
            } catch {
                [void]$output.AppendLine('ERROR: control job failed: ' + $_.Exception.GetType().Name)
            }
            $text = $output.ToString()
            if ($text.Length -gt 131072) { $text = $text.Substring(0, 131072) }
            # Avoid splitting a UTF-16 surrogate pair at the cap.
            if ($text.Length -and [char]::IsHighSurrogate($text[$text.Length - 1])) { $text = $text.Substring(0, $text.Length - 1) }
            $body = @{ id = $job.id; output = $text } | ConvertTo-Json -Compress
            Invoke-NuschlControl -Path '/result' -Body ([Text.Encoding]::UTF8.GetBytes($body)) | Out-Null
        }
    } catch {
        # Deliberately do not log request headers, credentials or exception URLs.
        Start-Sleep -Seconds 2
    }
}
