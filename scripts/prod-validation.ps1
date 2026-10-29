<#
.SYNOPSIS
  Exercise release binaries against isolated localhost state, without installing a service.
.DESCRIPTION
  Checks authentication, real PowerShell execution, cancellation, offline delivery,
  and recovery of a running job across a coordinator restart. Records local timings.
  Uses a fresh directory for every run and never changes Wi-Fi or power settings.
  With -KeepRunning, leaves the test app open for browser inspection; access.json
  contains its temporary account and process IDs. No production data is used.
#>
param(
    [ValidateRange(1024, 65535)][int]$Port = 18081,
    [ValidateSet('debug', 'release')][string]$Profile = 'release',
    [switch]$KeepRunning
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$run = Join-Path $root ('demo-data\prod-validation\' + (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N').Substring(0, 6))
$coordinatorExe = Join-Path $root "target\$Profile\coordinator.exe"
$clientExe = Join-Path $root "target\$Profile\client.exe"
$base = "http://127.0.0.1:$Port"
$processes = [System.Collections.Generic.List[System.Diagnostics.Process]]::new()
$checks = [System.Collections.Generic.List[string]]::new()
$timings = [System.Collections.Generic.List[double]]::new()
$dispatchTimings = [System.Collections.Generic.List[long]]::new()
$savedEnvironment = @{}
$success = $false
$session = [Microsoft.PowerShell.Commands.WebRequestSession]::new()

foreach ($name in @('SC_ACCOUNT_PASSWORD', 'SC_KEY_SECRET', 'SC_KEY_ID', 'SC_COORDINATOR_URL', 'SC_API_TOKEN', 'SC_AUTH', 'SC_AUTO_UPDATE', 'SC_UI_BIND', 'SC_UI_DIR', 'SC_SECURE_COOKIES', 'SC_STATE_DIR')) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
}

function Start-Logged([string]$exe, [string[]]$arguments, [string]$name) {
    $quoted = $arguments | ForEach-Object { '"' + $_ + '"' }
    $process = Start-Process -FilePath $exe -ArgumentList $quoted -WindowStyle Hidden -PassThru `
        -RedirectStandardError (Join-Path $run "$name.log") -RedirectStandardOutput (Join-Path $run "$name.out.log")
    $processes.Add($process)
    return $process
}

function Wait-For([string]$description, [scriptblock]$condition, [int]$seconds = 40) {
    $deadline = [DateTime]::UtcNow.AddSeconds($seconds)
    do {
        if (& $condition) { return }
        Start-Sleep -Milliseconds 50
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out: $description. Logs: $run"
}

function Api([string]$path, [string]$method = 'Get', $body = $null) {
    $arguments = @{ Uri = "$base/api/v1$path"; Method = $method; WebSession = $session; TimeoutSec = 10 }
    if ($null -ne $body) {
        $arguments.ContentType = 'application/json'
        $arguments.Body = ConvertTo-Json -InputObject $body -Depth 8 -Compress
    }
    Invoke-RestMethod @arguments
}

function Start-Coordinator([string]$name) {
    Start-Logged $coordinatorExe @('serve', '--data-dir', (Join-Path $run 'coordinator'), '--bind', "127.0.0.1:$Port", '--auth', 'accounts', '--no-update-check', '--heartbeat-interval-secs', '10') $name
}

function Start-Client([string]$name) {
    Start-Logged $clientExe @('run', '--no-hotspot-discovery', '--state-dir', (Join-Path $run 'client'), '--coordinator-url', "ws://127.0.0.1:$Port", '--key-id', $keyId) $name
}

function New-Script([string]$body) {
    (Api '/jobs' 'Post' @{
        kind = 'script.run'
        payload = @{ shell = 'powershell'; body = $body; timeout_secs = 60 }
        targets = @{ type = 'devices'; device_ids = @($deviceId) }
    }).job
}

function Target([string]$jobId) { (Api "/jobs/$jobId").job.targets[0] }
function Wait-Target([string]$jobId) {
    Wait-For 'job to complete' { (Target $jobId).state -in @('succeeded', 'failed', 'cancelled', 'expired') }
    Target $jobId
}
function Assert([bool]$condition, [string]$description) {
    if (-not $condition) { throw "Failed: $description. Logs: $run" }
    $checks.Add($description)
    Write-Host "PASS $description"
}

try {
    if (-not (Test-Path -LiteralPath $coordinatorExe) -or -not (Test-Path -LiteralPath $clientExe)) {
        throw "Build the UI, then cargo build --locked --$Profile (omit --debug for debug)."
    }
    if (Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue) {
        throw "Port $Port is already in use; choose an unused test port."
    }
    New-Item -ItemType Directory -Path $run -Force | Out-Null
    # Run immutable copies so browser review cannot lock the build outputs.
    $binaryDirectory = Join-Path $run 'bin'
    New-Item -ItemType Directory -Path $binaryDirectory | Out-Null
    Copy-Item -LiteralPath $coordinatorExe -Destination (Join-Path $binaryDirectory 'coordinator.exe')
    Copy-Item -LiteralPath $clientExe -Destination (Join-Path $binaryDirectory 'client.exe')
    $coordinatorExe = Join-Path $binaryDirectory 'coordinator.exe'
    $clientExe = Join-Path $binaryDirectory 'client.exe'
    $password = [guid]::NewGuid().ToString('N')
    $env:SC_ACCOUNT_PASSWORD = $password
    & $coordinatorExe account create --data-dir (Join-Path $run 'coordinator') operator | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'Could not create the isolated account.' }
    $created = & $coordinatorExe key create --data-dir (Join-Path $run 'coordinator') --label 'Local validation'
    if ($LASTEXITCODE -ne 0) { throw 'Could not create the isolated enrollment key.' }
    $keyId = ($created | Select-String 'key id:\s+(\S+)').Matches[0].Groups[1].Value
    $env:SC_KEY_SECRET = ($created | Select-String 'key secret:\s+(\S+)').Matches[0].Groups[1].Value
    $server = Start-Coordinator 'coordinator'
    Wait-For 'coordinator health' { try { (Api '/health').status -eq 'ok' } catch { $false } }
    $status = 0
    try { Api '/devices' | Out-Null } catch { $status = [int]$_.Exception.Response.StatusCode }
    Assert ($status -eq 401) 'Unauthenticated device access is refused'
    Api '/auth/login' 'Post' @{ username = 'operator'; password = $password } | Out-Null
    $client = Start-Client 'client'
    Wait-For 'client online' { @((Api '/devices').devices | Where-Object online).Count -eq 1 }
    $deviceId = (Api '/devices').devices[0].id
    Assert (-not [string]::IsNullOrEmpty($deviceId)) 'Real Windows client enrolls and connects'

    $job = New-Script "Write-Output 'Surface Coordinator ready'; Write-Output ('Device: ' + `$env:COMPUTERNAME); Write-Output 'Health check: passed'"
    $result = Wait-Target $job.id
    $screenshotJob = $job.id
    Assert ($result.state -eq 'succeeded' -and $result.output -match 'Health check: passed') 'Real PowerShell output and successful result arrive'
    1..5 | ForEach-Object {
        $timer = [Diagnostics.Stopwatch]::StartNew()
        $job = New-Script "Write-Output 'latency check $_'"
        $result = Wait-Target $job.id
        $timer.Stop()
        if ($result.state -ne 'succeeded') { throw 'Latency check failed.' }
        $timings.Add([Math]::Round($timer.Elapsed.TotalMilliseconds, 1))
        $dispatchTimings.Add([long]$result.started_at - [long]$result.created_at)
    }

    $job = New-Script "Write-Output 'waiting for cancellation'; Start-Sleep -Seconds 30"
    Wait-For 'job running' { (Target $job.id).state -eq 'running' }
    Api "/jobs/$($job.id)/cancel" 'Post' | Out-Null
    $result = Wait-Target $job.id
    Assert ($result.state -eq 'failed' -and $result.reason -eq 'cancelled') 'Running script cancels'

    Stop-Process -Id $client.Id -Force
    Wait-For 'client offline' { -not (Api '/devices').devices[0].online }
    $job = New-Script "Write-Output 'Delivered after reconnect'"
    Assert ((Target $job.id).state -eq 'queued') 'Offline job remains queued'
    $client = Start-Client 'client-reconnected'
    $result = Wait-Target $job.id
    Assert ($result.state -eq 'succeeded' -and $result.output -match 'Delivered after reconnect') 'Queued job executes after reconnect with the same device ID'

    $job = New-Script "Write-Output 'Before coordinator restart'; Start-Sleep -Seconds 4; Write-Output 'Recovered after coordinator restart'"
    Wait-For 'script output before restart' { (Target $job.id).output -match 'Before coordinator restart' }
    Stop-Process -Id $server.Id -Force
    Start-Sleep -Milliseconds 300
    $server = Start-Coordinator 'coordinator-restarted'
    Wait-For 'restarted coordinator health' { try { (Api '/health').status -eq 'ok' } catch { $false } }
    $result = Wait-Target $job.id
    Assert ($result.state -eq 'succeeded' -and $result.output -match 'Recovered after coordinator restart') 'Running job survives coordinator restart and replays its output/result'

    $apiTimings = @(1..40 | ForEach-Object {
        $timer = [Diagnostics.Stopwatch]::StartNew()
        Api '/devices' | Out-Null
        $timer.Stop()
        [Math]::Round($timer.Elapsed.TotalMilliseconds, 2)
    }) | Sort-Object
    $version = (Api '/health').version
    $report = [ordered]@{
        host_name = $env:COMPUTERNAME; url = $base; profile = $Profile; version = $version
        checked_at = [DateTime]::UtcNow.ToString('o'); checks = $checks.ToArray()
        coordinator_sha256 = (Get-FileHash -LiteralPath $coordinatorExe -Algorithm SHA256).Hash
        client_sha256 = (Get-FileHash -LiteralPath $clientExe -Algorithm SHA256).Hash
        script_completion_ms = $timings.ToArray()
        dispatch_ack_ms = $dispatchTimings.ToArray()
        devices_api_median_ms = $apiTimings[20]; devices_api_p95_ms = $apiTimings[37]
        limitations = @('One real Windows console client on localhost; no Surface Wi-Fi adapter or service identity exercised.', 'Timings include PowerShell HTTP polling, and are not fleet/network benchmarks.')
    }
    $report | ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $run 'report.json') -Encoding utf8
    if ($KeepRunning) {
        @{ url = $base; username = 'operator'; password = $password; coordinator_pid = $server.Id; client_pid = $client.Id; job_id = $screenshotJob } |
            ConvertTo-Json | Set-Content -LiteralPath (Join-Path $run 'access.json') -Encoding utf8
    }
    $success = $true
    Write-Host "Report: $(Join-Path $run 'report.json')"
    if ($KeepRunning) { Write-Host "Test app: $base (temporary account and process IDs in access.json)" }
} finally {
    foreach ($name in $savedEnvironment.Keys) {
        if ($null -eq $savedEnvironment[$name]) {
            Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
        } else {
            [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process')
        }
    }
    if (-not $KeepRunning -or -not $success) {
        foreach ($process in $processes) {
            if (-not $process.HasExited) { Stop-Process -Id $process.Id -Force -ErrorAction SilentlyContinue }
        }
    }
}
