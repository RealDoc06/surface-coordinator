<#
.SYNOPSIS
  Verify a Docker upgrade and backup restore using isolated, real application data.
.DESCRIPTION
  Seed runs the previous image with a fresh volume, account and console client.
  Validate upgrades that volume, then restores its stopped-server backup into a
  separate volume and checks the previous image again. No service is installed
  and automatic hotspot discovery is disabled. Docker binds only to loopback.
  Run Validate after rebuilding the candidate image. Temporary credentials are
  encrypted for this Windows user; JSON reports contain no secrets.
.EXAMPLE
  .\scripts\deployment-validation.ps1 -Phase Seed
  .\scripts\deployment-validation.ps1 -Phase Validate -RunDirectory <printed-path>
#>
param(
    [ValidateSet('Seed', 'Validate')][string]$Phase = 'Seed',
    [string]$RunDirectory,
    [string]$PreviousImage = 'surface-coordinator:latest',
    [string]$CandidateImage = 'surface-coordinator:validation-0.2.3',
    [ValidateRange(1024, 65535)][int]$Port = 18088
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$root = Split-Path -Parent $PSScriptRoot
$evidenceRoot = [IO.Path]::GetFullPath((Join-Path $root 'demo-data\deployment-validation'))
$clientExe = [IO.Path]::GetFullPath((Join-Path $root 'target\release\client.exe'))
$session = [Microsoft.PowerShell.Commands.WebRequestSession]::new()
$checks = [System.Collections.Generic.List[string]]::new()
$containers = [System.Collections.Generic.List[string]]::new()
$clientProcess = $null
$state = $null
$success = $false
$savedEnvironment = @{}

foreach ($name in @('SC_ACCOUNT_PASSWORD', 'SC_KEY_SECRET', 'SC_KEY_ID', 'SC_COORDINATOR_URL', 'SC_API_TOKEN', 'SC_AUTH', 'SC_AUTO_UPDATE', 'SC_UI_BIND', 'SC_UI_DIR', 'SC_SECURE_COOKIES', 'SC_STATE_DIR')) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
    [Environment]::SetEnvironmentVariable($name, $null, 'Process')
}

function Docker {
    $result = & docker.exe @args 2>&1
    if ($LASTEXITCODE -ne 0) { throw "Docker failed: $($result -join [Environment]::NewLine)" }
    return $result
}

function Assert([bool]$condition, [string]$description) {
    if (-not $condition) { throw "Failed: $description" }
    $checks.Add($description)
    Write-Host "PASS $description"
}

function Wait-For([string]$description, [scriptblock]$condition, [int]$seconds = 45) {
    $deadline = [DateTime]::UtcNow.AddSeconds($seconds)
    do {
        if (& $condition) { return }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out: $description"
}

function Api([string]$path, [string]$method = 'Get', $body = $null) {
    $parameters = @{ Uri = "$base/api/v1$path"; Method = $method; WebSession = $session; TimeoutSec = 10 }
    if ($null -ne $body) {
        $parameters.ContentType = 'application/json'
        $parameters.Body = ConvertTo-Json -InputObject $body -Depth 8 -Compress
    }
    Invoke-RestMethod @parameters
}

function Protect([string]$value) { ConvertFrom-SecureString (ConvertTo-SecureString $value -AsPlainText -Force) }
function Unprotect([string]$value) { [PSCredential]::new('validation', (ConvertTo-SecureString $value)).GetNetworkCredential().Password }

function Save-State {
    $state | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $run 'state.json') -Encoding UTF8
}

function New-Volume([string]$name) {
    $existing = @(Docker volume ls --filter "name=$name" --format '{{.Name}}')
    if ($name -in $existing) { throw "Test volume $name already exists; refusing to reuse a restore destination." }
    Docker volume create --label "surface-coordinator.validation=$($state.run_id)" $name | Out-Null
}

function Assert-OwnedVolume([string]$name) {
    if ($name -notin @($state.volume, $state.rollback_volume) -or $name -notmatch '^sc-deployment-validation-[a-f0-9]{12}(-rollback)?$') {
        throw 'Refusing an unexpected Docker volume.'
    }
    $label = Docker volume inspect --format '{{index .Labels "surface-coordinator.validation"}}' $name
    if ($label -ne $state.run_id) { throw 'Docker volume ownership label does not match this validation run.' }
}

function Start-Coordinator([string]$image, [string]$volume, [string]$stage) {
    Assert-OwnedVolume $volume
    $name = "$($state.volume)-$stage"
    $containers.Add($name)
    Docker run -d --name $name --label "surface-coordinator.validation=$($state.run_id)" `
        -p "127.0.0.1:$($state.port):8080" -v "${volume}:/data" `
        -e SC_SECURE_COOKIES=false -e SC_NO_UPDATE_CHECK=true -e SC_AUTO_UPDATE=false `
        $image serve --auth accounts --heartbeat-interval-secs 2 | Out-Null
    Wait-For "$stage coordinator health" { try { (Api '/health').status -eq 'ok' } catch { $false } }
    return $name
}

function Stop-Coordinator([string]$name, [string]$stage) {
    Docker stop --time 10 $name | Out-Null
    Docker logs $name | Set-Content -LiteralPath (Join-Path $run "$stage.log") -Encoding UTF8
    Docker rm $name | Out-Null
    [void]$containers.Remove($name)
}

function Snapshot([string]$volume, [string]$filename) {
    Assert-OwnedVolume $volume
    Docker run --rm --user 0 --entrypoint tar -v "${volume}:/data:ro" -v "${run}:/evidence" `
        $state.previous_image_id -C /data -czf "/evidence/$filename" . | Out-Null
    Assert ((Get-Item -LiteralPath (Join-Path $run $filename)).Length -gt 0) "$filename contains a stopped coordinator data snapshot"
}

function Start-Client([string]$stage) {
    $env:SC_KEY_SECRET = Unprotect $state.key_secret_encrypted
    $arguments = @('run', '--no-hotspot-discovery', '--state-dir', (Join-Path $run 'client'), '--coordinator-url', "ws://127.0.0.1:$($state.port)", '--key-id', $state.key_id)
    $quoted = $arguments | ForEach-Object { '"' + $_ + '"' }
    $script:clientProcess = Start-Process -FilePath $clientExe -ArgumentList $quoted -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput (Join-Path $run "$stage-client.out.log") -RedirectStandardError (Join-Path $run "$stage-client.log")
    Remove-Item Env:SC_KEY_SECRET
    Wait-For "$stage real client online" { @((Api '/devices').devices | Where-Object online).Count -eq 1 }
}

function Stop-Client {
    if ($null -ne $script:clientProcess -and -not $script:clientProcess.HasExited) {
        Stop-Process -Id $script:clientProcess.Id -Force
        $script:clientProcess.WaitForExit()
    }
    $script:clientProcess = $null
}

function Run-Job([string]$message) {
    $job = (Api '/jobs' 'Post' @{
        kind = 'script.run'; payload = @{ shell = 'powershell'; body = "Write-Output '$message'"; timeout_secs = 30 }
        targets = @{ type = 'devices'; device_ids = @($state.device_id) }
    }).job
    Wait-For 'real PowerShell result' { (Api "/jobs/$($job.id)").job.targets[0].state -in @('succeeded', 'failed', 'expired') }
    $result = (Api "/jobs/$($job.id)").job.targets[0]
    Assert ($result.state -eq 'succeeded' -and $result.exit_code -eq 0 -and $result.output.Contains($message)) "$message executes through the real client"
    return $job.id
}

function Restore-Session {
    $script:session = [Microsoft.PowerShell.Commands.WebRequestSession]::new()
    $cookie = [Net.Cookie]::new('sc_session', (Unprotect $state.session_encrypted), '/', '127.0.0.1')
    $session.Cookies.Add($cookie)
}

function Verify-PreservedData([string]$stage) {
    Assert ((Api '/session').authenticated) "$stage preserves the existing authenticated session"
    Assert (@((Api '/accounts').accounts | Where-Object username -eq 'deployment-check').Count -eq 1) "$stage preserves the operator account"
    Assert (@((Api '/enrollment-keys').keys | Where-Object id -eq $state.key_id).Count -eq 1) "$stage preserves the enrollment key"
    $group = @((Api '/groups').groups | Where-Object id -eq $state.group_id)
    Assert ($group.Count -eq 1 -and $state.device_id -in $group[0].device_ids) "$stage preserves group membership"
    $scriptEntry = @((Api '/scripts').scripts | Where-Object id -eq $state.script_id)
    Assert ($scriptEntry.Count -eq 1 -and $scriptEntry[0].body -eq "Write-Output 'Saved deployment script'") "$stage preserves the script library"
    $oldJob = (Api "/jobs/$($state.seed_job_id)").job
    Assert ($oldJob.targets[0].state -eq 'succeeded' -and $oldJob.targets[0].output.Contains('Before upgrade')) "$stage preserves completed job output and history"
    Api '/auth/login' 'Post' @{ username = 'deployment-check'; password = (Unprotect $state.password_encrypted) } | Out-Null
    Assert ((Api '/session').authenticated) "$stage accepts a fresh password login"
}

try {
    if ($env:OS -ne 'Windows_NT') { throw 'Run this validation on Windows with Docker Desktop in Linux-container mode.' }
    if (-not (Test-Path -LiteralPath $clientExe)) { throw 'Build target\release\client.exe first.' }
    if ((Docker info --format '{{.OSType}}') -ne 'linux') { throw 'Docker must be using Linux containers.' }
    if ($Phase -eq 'Seed') {
        if ($RunDirectory) { throw 'Seed always creates a new evidence directory; omit RunDirectory.' }
        if (Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue) { throw "Port $Port is in use." }
        $id = [guid]::NewGuid().ToString('N').Substring(0, 12)
        $run = Join-Path $evidenceRoot ((Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + $id)
        New-Item -ItemType Directory -Path $run -Force | Out-Null
        $state = [ordered]@{
            run_id = $id; host_name = $env:COMPUTERNAME; port = $Port
            volume = "sc-deployment-validation-$id"; rollback_volume = "sc-deployment-validation-$id-rollback"
            previous_image = $PreviousImage; previous_image_id = [string](Docker image inspect --format '{{.Id}}' $PreviousImage)
            previous_version = [string](Docker run --rm $PreviousImage --version)
            seeded_at = [DateTime]::UtcNow.ToString('o'); password_encrypted = Protect ([guid]::NewGuid().ToString('N'))
        }
        $base = "http://127.0.0.1:$($state.port)"
        Save-State
        New-Volume $state.volume
        $env:SC_ACCOUNT_PASSWORD = Unprotect $state.password_encrypted
        Docker run --rm -v "$($state.volume):/data" -e SC_ACCOUNT_PASSWORD $state.previous_image_id account create deployment-check | Out-Null
        Remove-Item Env:SC_ACCOUNT_PASSWORD
        $server = Start-Coordinator $state.previous_image_id $state.volume 'previous'
        Api '/auth/login' 'Post' @{ username = 'deployment-check'; password = (Unprotect $state.password_encrypted) } | Out-Null
        $key = (Api '/enrollment-keys' 'Post' @{ label = 'Isolated deployment check' }).key
        $state.key_id = $key.id
        $state.key_secret_encrypted = Protect $key.secret
        $state.session_encrypted = Protect $session.Cookies.GetCookies([uri]$base)['sc_session'].Value
        Start-Client 'previous'
        $state.device_id = (Api '/devices').devices[0].id
        Assert (-not [string]::IsNullOrEmpty($state.device_id)) 'Previous Linux image accepts the real Windows console client'
        $state.group_id = (Api '/groups' 'Post' @{ name = 'Deployment validation' }).group.id
        Api "/groups/$($state.group_id)/members" 'Put' @{ device_ids = @($state.device_id) } | Out-Null
        $state.script_id = (Api '/scripts' 'Post' @{ name = 'Deployment validation'; shell = 'powershell'; body = "Write-Output 'Saved deployment script'" }).script.id
        $state.seed_job_id = Run-Job 'Before upgrade'
        Verify-PreservedData 'Previous image'
        Stop-Client
        Stop-Coordinator $server 'previous'
        Snapshot $state.volume 'pre-upgrade.tar.gz'
        $state.backup_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $run 'pre-upgrade.tar.gz')).Hash
        $state.seed_checks = $checks.ToArray()
        Save-State
        $success = $true
        Write-Host "Seed ready. Candidate image can now be rebuilt. RunDirectory: $run"
    } else {
        if (-not $RunDirectory) { throw 'Validate requires the RunDirectory printed by Seed.' }
        $run = (Resolve-Path -LiteralPath $RunDirectory).Path
        if (-not $run.StartsWith($evidenceRoot + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Evidence directory must be inside demo-data\deployment-validation.' }
        $state = Get-Content -LiteralPath (Join-Path $run 'state.json') -Raw | ConvertFrom-Json
        $base = "http://127.0.0.1:$($state.port)"
        if (Get-NetTCPConnection -State Listen -LocalPort $state.port -ErrorAction SilentlyContinue) { throw "Port $($state.port) is in use." }
        Assert ($state.backup_sha256 -eq (Get-FileHash -Algorithm SHA256 -LiteralPath (Join-Path $run 'pre-upgrade.tar.gz')).Hash) 'Stopped-server backup checksum is unchanged'
        $candidateId = [string](Docker image inspect --format '{{.Id}}' $CandidateImage)
        $candidateVersion = [string](Docker run --rm $candidateId --version)
        Restore-Session
        $server = Start-Coordinator $candidateId $state.volume 'candidate'
        Verify-PreservedData 'Upgraded image'
        $historyMetadata = (Api "/jobs/$($state.seed_job_id)?include_output=false").job
        Assert ('output' -notin $historyMetadata.targets[0].PSObject.Properties.Name) 'Upgraded image can read historical job metadata without loading output'
        $historyOutput = Api "/jobs/$($state.seed_job_id)/targets/$($historyMetadata.targets[0].id)/output"
        Assert ($historyOutput.output.Contains('Before upgrade') -and -not $historyOutput.output_truncated) 'Upgraded image preserves historical output through the selected-target endpoint'
        Start-Client 'candidate'
        Assert ((Api '/devices').devices[0].id -eq $state.device_id) 'Upgraded image reconnects the existing device identity'
        $candidateJob = Run-Job 'After upgrade'
        Stop-Client
        Stop-Coordinator $server 'candidate'
        Snapshot $state.volume 'post-upgrade.tar.gz'
        New-Volume $state.rollback_volume
        Assert-OwnedVolume $state.rollback_volume
        Docker run --rm --user 0 --entrypoint tar -v "$($state.rollback_volume):/data" -v "${run}:/evidence:ro" `
            $state.previous_image_id -C /data -xzf /evidence/pre-upgrade.tar.gz | Out-Null
        Restore-Session
        $server = Start-Coordinator $state.previous_image_id $state.rollback_volume 'rollback'
        Verify-PreservedData 'Restored previous image'
        Assert (@((Api '/jobs').jobs | Where-Object id -eq $candidateJob).Count -eq 0) 'Rollback restores the backup rather than retaining post-upgrade mutations'
        Start-Client 'rollback'
        $rollbackJob = Run-Job 'After backup restore'
        Stop-Client
        Stop-Coordinator $server 'rollback'
        Snapshot $state.rollback_volume 'post-rollback.tar.gz'
        $report = [ordered]@{
            host_name = $env:COMPUTERNAME; completed_at = [DateTime]::UtcNow.ToString('o'); passed = $true
            url = $base; previous_image = $state.previous_image; previous_image_id = $state.previous_image_id; previous_version = $state.previous_version
            candidate_image = $CandidateImage; candidate_image_id = $candidateId; candidate_version = $candidateVersion
            client_version = [string](& $clientExe --version); client_sha256 = (Get-FileHash -Algorithm SHA256 -LiteralPath $clientExe).Hash
            backup_sha256 = $state.backup_sha256; seed_job_id = $state.seed_job_id; candidate_job_id = $candidateJob; rollback_job_id = $rollbackJob
            checks = @($state.seed_checks) + $checks.ToArray()
            limitations = @('Local Linux Docker with one real Windows console client; no live database, Surface hardware, service installation, proxy or Wi-Fi changes.', 'Rollback uses the complete pre-upgrade snapshot together with its pinned previous image, so changes since that snapshot are intentionally absent.')
        }
        foreach ($volume in @($state.volume, $state.rollback_volume)) {
            Assert-OwnedVolume $volume
            Docker volume rm $volume | Out-Null
        }
        $report.cleanup = @{ test_containers_removed = ($containers.Count -eq 0); test_volumes_removed = $true; console_client_stopped = ($null -eq $clientProcess) }
        $report | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath (Join-Path $run 'report.json') -Encoding UTF8
        $success = $true
        Write-Host "Deployment validation passed. Report: $(Join-Path $run 'report.json')"
    }
} finally {
    Stop-Client
    foreach ($name in $containers.ToArray()) {
        try { Stop-Coordinator $name "failed-$Phase" } catch { Write-Warning "Could not clean test container ${name}: $($_.Exception.Message)" }
    }
    foreach ($name in $savedEnvironment.Keys) { [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process') }
    if (-not $success -and $null -ne $state) {
        [ordered]@{ host_name = $env:COMPUTERNAME; phase = $Phase; passed = $false; checked_at = [DateTime]::UtcNow.ToString('o'); checks = $checks.ToArray() } |
            ConvertTo-Json -Depth 6 | Set-Content -LiteralPath (Join-Path $run "failure-$Phase.json") -Encoding UTF8
        Write-Warning "Validation failed; scoped volumes and evidence remain for diagnosis at $run"
    }
}
