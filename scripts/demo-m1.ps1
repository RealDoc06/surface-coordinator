<#
.SYNOPSIS
  Milestone 1 demo: coordinator and client on localhost.

.DESCRIPTION
  Builds the workspace, starts a coordinator, enrolls this machine as a
  device, runs a PowerShell job on it through the HTTP API, shows that a
  client with a wrong enrollment key is rejected, and shows that a job
  queued while the client is offline is delivered when it reconnects.

  The enrollment key and the API token are generated on every run. All
  state goes to demo-data/ (ignored by git), which is wiped at the start.
#>
param(
    [int]$Port = 18080
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$data = Join-Path $root 'demo-data'
$coordinatorExe = Join-Path $root 'target\debug\coordinator.exe'
$clientExe = Join-Path $root 'target\debug\client.exe'
$processes = @()

function Step($text) { Write-Host "`n=== $text ===" -ForegroundColor Cyan }

function Start-Logged($exe, $arguments, $log) {
    # Environment variables set by the caller are inherited by the process.
    Start-Process -FilePath $exe -ArgumentList $arguments -NoNewWindow -PassThru `
        -RedirectStandardError $log -RedirectStandardOutput "$log.out"
}

function Wait-Until($what, [scriptblock]$condition, $seconds = 20) {
    $deadline = (Get-Date).AddSeconds($seconds)
    while (-not (& $condition)) {
        if ((Get-Date) -gt $deadline) { throw "timed out waiting for: $what" }
        Start-Sleep -Milliseconds 200
    }
}

function Show-Log($log, $pattern) {
    # Strip terminal colour codes from the log lines.
    Get-Content $log | Where-Object { $_ -match $pattern } |
        ForEach-Object { $_ -replace "$([char]27)\[[0-9;]*m", '' }
}

try {
    Step 'Build'
    cargo build --manifest-path (Join-Path $root 'Cargo.toml')
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }

    if (Test-Path $data) { Remove-Item -Recurse -Force $data }
    New-Item -ItemType Directory -Force $data | Out-Null
    $coordinatorData = Join-Path $data 'coordinator'

    Step 'Create an enrollment key'
    $created = & $coordinatorExe key create --data-dir $coordinatorData --label demo
    $keyId = ($created | Select-String 'key id:\s+(\S+)').Matches[0].Groups[1].Value
    $keySecret = ($created | Select-String 'key secret:\s+(\S+)').Matches[0].Groups[1].Value
    Write-Host "key id:     $keyId"
    Write-Host "key secret: (generated, $($keySecret.Length) hex characters, not shown)"

    Step 'Start the coordinator'
    $token = [guid]::NewGuid().ToString('N') + [guid]::NewGuid().ToString('N')
    $api = "http://127.0.0.1:$Port/api/v1"
    $headers = @{ Authorization = "Bearer $token" }
    $env:SC_API_TOKEN = $token
    $coordinatorLog = Join-Path $data 'coordinator.log'
    $processes += Start-Logged $coordinatorExe @('serve', '--bind', "127.0.0.1:$Port", '--data-dir', $coordinatorData) $coordinatorLog
    Remove-Item Env:SC_API_TOKEN
    Wait-Until 'coordinator to answer' { try { (Invoke-RestMethod "$api/health").status -eq 'ok' } catch { $false } }
    Invoke-RestMethod "$api/health" | ConvertTo-Json -Compress | Write-Host
    Write-Host 'devices before any client connects:' (Invoke-RestMethod "$api/devices" -Headers $headers | ConvertTo-Json -Compress)

    Step 'Start the client with the enrollment key'
    $env:SC_COORDINATOR_URL = "ws://127.0.0.1:$Port"
    $env:SC_KEY_ID = $keyId
    $env:SC_KEY_SECRET = $keySecret
    $clientState = Join-Path $data 'client'
    $clientLog = Join-Path $data 'client.log'
    $client = Start-Logged $clientExe @('--state-dir', $clientState) $clientLog
    $processes += $client
    Wait-Until 'device to come online' { (Invoke-RestMethod "$api/devices" -Headers $headers).devices | Where-Object online }
    $device = (Invoke-RestMethod "$api/devices" -Headers $headers).devices[0]
    $device | Select-Object id, hostname, os, client_version, online | Format-List | Out-String | Write-Host

    Step 'Run a PowerShell script on the device'
    $script = @'
Write-Output "hello from $env:COMPUTERNAME"
1..3 | ForEach-Object { Write-Output "line $_"; Start-Sleep -Milliseconds 300 }
[Console]::Error.WriteLine('this line went to stderr')
exit 3
'@
    $request = @{
        kind    = 'script.run'
        payload = @{ shell = 'powershell'; body = $script; timeout_secs = 60 }
        targets = @{ type = 'devices'; device_ids = @($device.id) }
    } | ConvertTo-Json -Depth 5
    $job = (Invoke-RestMethod "$api/jobs" -Method Post -Headers $headers -ContentType 'application/json' -Body $request).job
    Write-Host "created job $($job.id); polling while it runs:"
    do {
        Start-Sleep -Milliseconds 250
        $target = (Invoke-RestMethod "$api/jobs/$($job.id)" -Headers $headers).job.targets[0]
        Write-Host ("  state={0,-9} output so far: {1} bytes" -f $target.state, $target.output.Length)
    } while ($target.state -notin 'succeeded', 'failed', 'expired')
    Write-Host "final state: $($target.state)   exit code: $($target.exit_code)   reason: $($target.reason)"
    Write-Host '--- output ---'
    Write-Host $target.output.TrimEnd()
    Write-Host '--------------'

    Step 'A client with the wrong enrollment secret is rejected'
    $env:SC_KEY_SECRET = -join (1..64 | ForEach-Object { '{0:x}' -f (Get-Random -Maximum 16) })
    $wrongLog = Join-Path $data 'client-wrong-secret.log'
    $wrong = Start-Logged $clientExe @('--state-dir', (Join-Path $data 'client-wrong-secret')) $wrongLog
    $processes += $wrong
    Start-Sleep -Seconds 3
    Stop-Process -Id $wrong.Id -Force
    Write-Host 'client log:'
    Show-Log $wrongLog 'not connected' | Select-Object -First 2 | Write-Host
    Write-Host 'coordinator log:'
    Show-Log $coordinatorLog 'handshake failed' | Select-Object -First 2 | Write-Host

    Step 'A client with an unknown key id is rejected'
    $env:SC_KEY_ID = 'no-such-key'
    $unknownLog = Join-Path $data 'client-unknown-key.log'
    $unknown = Start-Logged $clientExe @('--state-dir', (Join-Path $data 'client-unknown-key')) $unknownLog
    $processes += $unknown
    Start-Sleep -Seconds 3
    Stop-Process -Id $unknown.Id -Force
    Write-Host 'client log:'
    Show-Log $unknownLog 'not connected' | Select-Object -First 2 | Write-Host
    Write-Host 'coordinator log:'
    Show-Log $coordinatorLog 'unknown enrollment key' | Select-Object -First 2 | Write-Host
    $devices = (Invoke-RestMethod "$api/devices" -Headers $headers).devices
    Write-Host "devices known to the coordinator: $(@($devices).Count) (only the enrolled one)"

    Step 'A job queued while the client is offline is delivered on reconnect'
    Stop-Process -Id $client.Id -Force
    Wait-Until 'device to go offline' { -not ((Invoke-RestMethod "$api/devices" -Headers $headers).devices | Where-Object online) }
    Write-Host 'client stopped; device online:' (Invoke-RestMethod "$api/devices" -Headers $headers).devices[0].online
    $request = @{
        kind    = 'script.run'
        payload = @{ shell = 'powershell'; body = 'Write-Output "queued job ran at $(Get-Date -Format o)"' }
        targets = @{ type = 'all' }
    } | ConvertTo-Json -Depth 5
    $queuedAt = Get-Date -Format o
    $job = (Invoke-RestMethod "$api/jobs" -Method Post -Headers $headers -ContentType 'application/json' -Body $request).job
    Start-Sleep -Seconds 2
    $target = (Invoke-RestMethod "$api/jobs/$($job.id)" -Headers $headers).job.targets[0]
    Write-Host "job $($job.id) created at $queuedAt; state two seconds later: $($target.state)"

    $env:SC_KEY_ID = $keyId
    $env:SC_KEY_SECRET = $keySecret
    $client = Start-Logged $clientExe @('--state-dir', $clientState) (Join-Path $data 'client-2.log')
    $processes += $client
    Write-Host 'client started again'
    Wait-Until 'queued job to finish' { (Invoke-RestMethod "$api/jobs/$($job.id)" -Headers $headers).job.targets[0].state -in 'succeeded', 'failed' }
    $target = (Invoke-RestMethod "$api/jobs/$($job.id)" -Headers $headers).job.targets[0]
    Write-Host "final state: $($target.state)   exit code: $($target.exit_code)"
    Write-Host "output: $($target.output.TrimEnd())"

    Step 'All jobs'
    (Invoke-RestMethod "$api/jobs" -Headers $headers).jobs |
        ForEach-Object { [pscustomobject]@{ job = $_.id; kind = $_.kind; state = $_.targets[0].state; exit_code = $_.targets[0].exit_code } } |
        Format-Table | Out-String | Write-Host
    Write-Host "Logs are in $data"
}
finally {
    foreach ($process in $processes) {
        if ($process -and -not $process.HasExited) { Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue }
    }
    Remove-Item Env:SC_API_TOKEN, Env:SC_COORDINATOR_URL, Env:SC_KEY_ID, Env:SC_KEY_SECRET -ErrorAction SilentlyContinue
}
