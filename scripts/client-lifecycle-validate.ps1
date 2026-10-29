#requires -Version 5.1
<#
.SYNOPSIS
  Validate the real Windows client service on an elevated disposable machine.
.DESCRIPTION
  Refuses an existing SurfaceCoordinatorClient service or an existing workspace.
  Installs only beneath Workspace, talks only to a loopback coordinator, disables
  hotspot discovery before the service starts, and sends no Wi-Fi or power jobs.
  Always attempts uninstall/purge; report.json and evidence survive cleanup.

  Supplying a candidate also exercises coordinator and client signature rejection,
  a locked-backup replacement failure, and a successful signed service update.
  The candidate must report a different, newer version. Build the coordinator,
  baseline and candidate with the same disposable public update key. This script
  does not create keys, build binaries, or change the compiled trust key.

  IncludeMarkerRollback additionally injects a pending-update marker and baseline
  backup, stops only this script's coordinator, and proves rollback on start four
  after three unconfirmed starts. This is explicit marker/file fault injection,
  not proof of crash recovery or a failed real update completing automatically.

  IncludeConfirmationTimeoutRollback separately injects a pending marker and
  baseline backup, then leaves the candidate running with this coordinator off
  for the real 180-second production confirmation deadline. No timer is shortened.
.EXAMPLE
  .\scripts\client-lifecycle-validate.ps1 -DisposableTarget `
    -ExpectedComputerName SC-TEST-VM -Workspace C:\Validation\lifecycle-001 `
    -CoordinatorExe C:\Artifacts\coordinator.exe `
    -BaselineClientExe C:\Artifacts\baseline\client.exe `
    -CandidateClientExe C:\Artifacts\candidate\client.exe `
    -CandidateSignature C:\Artifacts\candidate\client.exe.sig `
    -UpdatePublicKey C:\Artifacts\test-update.pub `
    -ReleaseToolExe C:\Artifacts\release-tool.exe -IncludeMarkerRollback
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][switch]$DisposableTarget,
    [Parameter(Mandatory)][string]$ExpectedComputerName,
    [Parameter(Mandatory)][string]$Workspace,
    [Parameter(Mandatory)][string]$CoordinatorExe,
    [Parameter(Mandatory)][string]$BaselineClientExe,
    [string]$CandidateClientExe,
    [string]$CandidateSignature,
    [string]$UpdatePublicKey,
    [string]$ReleaseToolExe,
    [ValidateSet('windows-x86_64')][string]$Target = 'windows-x86_64',
    [ValidateRange(1024, 65535)][int]$Port = 18083,
    [switch]$IncludeMarkerRollback,
    [switch]$IncludeConfirmationTimeoutRollback
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$serviceName = 'SurfaceCoordinatorClient'
$checks = [System.Collections.Generic.List[object]]::new()
$cleanupErrors = [System.Collections.Generic.List[string]]::new()
$savedEnvironment = @{}
$coordinatorProcesses = [System.Collections.Generic.List[System.Diagnostics.Process]]::new()
$session = [Microsoft.PowerShell.Commands.WebRequestSession]::new()
$base = "http://127.0.0.1:$Port"
$startedAt = [DateTime]::UtcNow
$serviceTouched = $false
$workspaceCreated = $false
$passed = $false
$failure = $null
$deviceId = $null
$baselineVersion = $null
$candidateVersion = $null
$swapRestorationExercised = $false
$interactiveSessionExercised = $false
$brightnessSkipped = $false
$server = $null
$backupLock = $null
$run = $null
$installDir = $null
$stateDir = $null
$coordinatorDir = $null
$evidenceDir = $null

function Write-Json([string]$Path, $Value) {
    # Windows PowerShell's UTF-8 encoding includes a BOM, which serde rejects.
    [IO.File]::WriteAllText($Path, (ConvertTo-Json -InputObject $Value -Depth 12), [Text.UTF8Encoding]::new($false))
}

function Assert-Check([bool]$Condition, [string]$Description, [string]$Kind = 'real service') {
    $checks.Add([ordered]@{ description = $Description; passed = $Condition; kind = $Kind; at = [DateTime]::UtcNow.ToString('o') })
    if (-not $Condition) { throw "Failed: $Description" }
    Write-Host "PASS [$Kind] $Description"
}

function Wait-For([string]$Description, [scriptblock]$Condition, [int]$Seconds = 60) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 200
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out: $Description. Evidence: $evidenceDir"
}

function Assert-NoReparsePoint([string]$Path) {
    $current = [IO.Path]::GetFullPath($Path)
    while ($current) {
        if (Test-Path -LiteralPath $current) {
            $item = Get-Item -LiteralPath $current -Force
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Reparse points are not allowed in validation paths: $current"
            }
        }
        $current = Split-Path -Parent $current
    }
}

function Assert-OwnedPath([string]$Path) {
    $absolute = [IO.Path]::GetFullPath($Path)
    if (-not $workspaceCreated -or -not $absolute.StartsWith($run + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing a path outside this run: $Path"
    }
    Assert-NoReparsePoint $absolute
    # Also reject links inside recursive-delete targets, not just their ancestors.
    if (Test-Path -LiteralPath $absolute -PathType Container) {
        $links = @(Get-ChildItem -LiteralPath $absolute -Recurse -Force | Where-Object {
            ($_.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0
        })
        if ($links.Count) { throw "Refusing a validation directory containing a reparse point: $absolute" }
    }
}

function Get-TestService {
    Get-CimInstance -ClassName Win32_Service -Filter "Name='$serviceName'"
}

function Assert-OwnedService {
    $service = Get-TestService
    if ($null -ne $service) {
        $exePattern = [regex]::Escape((Join-Path $installDir 'client.exe'))
        $statePattern = [regex]::Escape($stateDir)
        # Rust canonicalization uses extended-length Windows paths in SCM.
        $commandLine = $service.PathName.Replace('\\?\', '')
        if ($commandLine -notmatch ('^\s*"?' + $exePattern + '"?\s+"?service"?\s+"?--state-dir"?\s+"?' + $statePattern + '"?\s*$')) {
            throw "Service no longer belongs to this run; refusing to touch it: $($service.PathName)"
        }
    }
    return $service
}

function Native-Argument([string]$Value) {
    # Standard Windows argv quoting, including trailing slashes and literal quotes.
    '"' + [regex]::Replace([regex]::Replace($Value, '(\\*)"', '$1$1\"'), '(\\+)$', '$1$1') + '"'
}

function Start-Logged([string]$Exe, [string[]]$Arguments, [string]$Name) {
    $quoted = @($Arguments | ForEach-Object { Native-Argument $_ })
    $process = Start-Process -FilePath $Exe -ArgumentList $quoted -WorkingDirectory $run -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput (Join-Path $evidenceDir "$Name.stdout.log") `
        -RedirectStandardError (Join-Path $evidenceDir "$Name.stderr.log")
    # Cache the native handle while the process is alive. Windows PowerShell 5
    # can otherwise lose ExitCode for a short-lived Start-Process invocation.
    $processHandle = $process.Handle
    return $process
}

function Invoke-Native([string]$Exe, [string[]]$Arguments, [string]$Name, [int]$TimeoutSeconds = 90) {
    $process = Start-Logged $Exe $Arguments $Name
    try {
        if (-not $process.WaitForExit($TimeoutSeconds * 1000)) {
            $process.Kill()
            $process.WaitForExit()
            throw "$Name timed out after $TimeoutSeconds seconds"
        }
        $process.WaitForExit()
        $exitCode = $process.ExitCode
        if ($null -eq $exitCode) { throw "$Name exited, but Windows PowerShell did not expose its exit code; see evidence logs" }
        if ($exitCode -ne 0) { throw "$Name exited with code $exitCode; see evidence logs" }
        return Get-Content -LiteralPath (Join-Path $evidenceDir "$Name.stdout.log") -Raw
    } finally {
        $process.Dispose()
    }
}

function Start-TestCoordinator([string]$Name) {
    $process = Start-Logged $CoordinatorExe @('serve', '--data-dir', $coordinatorDir, '--bind', "127.0.0.1:$Port", '--auth', 'accounts', '--no-update-check', '--heartbeat-interval-secs', '2') $Name
    $coordinatorProcesses.Add($process)
    return $process
}

function Stop-OwnedCoordinator($Process) {
    # Keep the actual Process object to avoid stopping an unrelated reused PID.
    if ($null -ne $Process -and -not $Process.HasExited) {
        $Process.Kill()
        if (-not $Process.WaitForExit(10000)) { throw 'The test coordinator did not exit' }
    }
}

function Stop-TestService {
    $service = Assert-OwnedService
    if ($null -eq $service) { return }
    $process = if ($service.ProcessId -gt 0) { Get-Process -Id $service.ProcessId -ErrorAction SilentlyContinue } else { $null }
    if ($service.State -ne 'Stopped') { Stop-Service -Name $serviceName }
    Wait-For 'test service stopped' { (Assert-OwnedService).State -eq 'Stopped' }
    if ($null -ne $process) {
        try {
            # SCM can report Stopped just before the executable's file handle closes.
            Wait-For 'stopped service process exited' { $process.HasExited } 30
        } finally { $process.Dispose() }
    }
}

function Api([string]$Path, [string]$Method = 'Get', $Body = $null) {
    $arguments = @{ Uri = "$base/api/v1$Path"; Method = $Method; WebSession = $session; TimeoutSec = 10; UseBasicParsing = $true }
    if ($null -ne $Body) {
        $arguments.ContentType = 'application/json'
        $arguments.Body = ConvertTo-Json -InputObject $Body -Depth 8 -Compress
    }
    Invoke-RestMethod @arguments
}

function New-Job([string]$Kind, $Payload) {
    (Api '/jobs' 'Post' @{ kind = $Kind; payload = $Payload; targets = @{ type = 'devices'; device_ids = @($deviceId) } }).job
}

function Wait-Job([string]$Id, [string]$EvidenceName) {
    Wait-For "job $Id to finish" { (Api "/jobs/$Id").job.targets[0].state -in @('succeeded', 'failed', 'cancelled', 'expired') } 120
    $job = (Api "/jobs/$Id").job
    Write-Json (Join-Path $evidenceDir "$EvidenceName.json") $job
    return $job.targets[0]
}

function Wait-Device([string]$Version) {
    Wait-For "one online device on $Version" {
        try {
            $devices = @((Api '/devices').devices)
            $devices.Count -eq 1 -and $devices[0].online -and $devices[0].id -eq $deviceId -and $devices[0].client_version -eq $Version
        } catch { $false }
    } 120
}

function Assert-Script([string]$Name) {
    $token = 'lifecycle-' + [guid]::NewGuid().ToString('N')
    $body = "Write-Output '$token'; Write-Output ([Security.Principal.WindowsIdentity]::GetCurrent().User.Value)"
    $job = New-Job 'script.run' @{ shell = 'powershell'; body = $body; timeout_secs = 20; run_as = 'system' }
    $result = Wait-Job $job.id $Name
    Assert-Check ($result.state -eq 'succeeded' -and $result.output -match $token -and $result.output -match 'S-1-5-18') "$Name executes through the service as LocalSystem"
}

function Assert-InteractiveJobs {
    $consoleUser = (Get-CimInstance -ClassName Win32_ComputerSystem).UserName
    if ([string]::IsNullOrWhiteSpace($consoleUser)) {
        $job = New-Job 'script.run' @{ shell = 'powershell'; body = "Write-Output 'no-user-session probe'"; timeout_secs = 20; run_as = 'user' }
        $result = Wait-Job $job.id 'interactive-user-unavailable'
        Assert-Check ($result.state -eq 'failed' -and $result.reason -eq 'no_user_session') 'No interactive login returns the precise no_user_session reason'
        return $false
    }

    $expectedSid = ([Security.Principal.NTAccount]::new($consoleUser)).Translate([Security.Principal.SecurityIdentifier]).Value
    $token = 'SC-Lifecycle-' + [guid]::NewGuid().ToString('N')
    $body = "Write-Output '$token'; Write-Output ('SID=' + [Security.Principal.WindowsIdentity]::GetCurrent().User.Value); Write-Output ('SESSION=' + [Diagnostics.Process]::GetCurrentProcess().SessionId)"
    $job = New-Job 'script.run' @{ shell = 'powershell'; body = $body; timeout_secs = 20; run_as = 'user' }
    $result = Wait-Job $job.id 'interactive-user-script'
    Assert-Check ($result.state -eq 'succeeded' -and $result.output -match $token -and $result.output -match ([regex]::Escape("SID=$expectedSid"))) 'User-scope PowerShell returns output under the logged-in user SID'
    if ($result.output -notmatch 'SESSION=(\d+)') { throw 'User script did not return its session id' }
    $consoleSessionId = [int]$Matches[1]
    Assert-Check ($consoleSessionId -gt 0 -and $expectedSid -ne 'S-1-5-18') 'User script runs in an interactive session instead of LocalSystem'

    $appPid = $null
    $appProcess = $null
    try {
        # The unique title identifies this process, and timeout bounds its life
        # even if validation is interrupted before exact-PID cleanup runs.
        $job = New-Job 'app.launch' @{ path = (Join-Path $env:SystemRoot 'System32\cmd.exe'); arguments = @('/d', '/q', '/c', "title $token & timeout /t 60 /nobreak >nul") }
        $result = Wait-Job $job.id 'interactive-app-launch'
        Assert-Check ($result.state -eq 'succeeded' -and $result.message -match '\(pid (\d+)\)') 'app.launch returns the launched application PID'
        if ($result.message -notmatch '\(pid (\d+)\)') { throw 'Application PID missing from the result' }
        $appPid = [int]$Matches[1]
        $app = Get-CimInstance -ClassName Win32_Process -Filter "ProcessId=$appPid"
        Assert-Check ($null -ne $app -and $app.Name -ieq 'cmd.exe' -and $app.CommandLine -match [regex]::Escape($token) -and $app.SessionId -eq $consoleSessionId) 'Launched cmd process has the unique test token and the user session'
        $appProcess = Get-Process -Id $appPid
        $owner = Invoke-CimMethod -InputObject $app -MethodName GetOwnerSid
        Assert-Check ($owner.ReturnValue -eq 0 -and $owner.Sid -eq $expectedSid) 'app.launch starts the application under the logged-in user SID'
        Write-Json (Join-Path $evidenceDir 'interactive-app-process.json') ($app | Select-Object ProcessId, ParentProcessId, Name, CommandLine, SessionId, CreationDate)
    } finally {
        # Resolve only the unique process launched by this test, never all cmd
        # processes or an entire session. Include its one known timeout child.
        $ownedApps = @(Get-CimInstance -ClassName Win32_Process -Filter "Name='cmd.exe'" | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($token) -and $_.SessionId -eq $consoleSessionId })
        foreach ($ownedApp in $ownedApps) {
            foreach ($child in @(Get-CimInstance -ClassName Win32_Process -Filter "ParentProcessId=$($ownedApp.ProcessId)" | Where-Object { $_.Name -ieq 'timeout.exe' -and $_.SessionId -eq $consoleSessionId })) {
                $process = Get-Process -Id $child.ProcessId -ErrorAction SilentlyContinue
                if ($null -ne $process) {
                    try { if ([Math]::Abs(($process.StartTime.ToUniversalTime() - $child.CreationDate.ToUniversalTime()).TotalMilliseconds) -lt 1) { $process.Kill(); $process.WaitForExit(5000) | Out-Null } }
                    finally { $process.Dispose() }
                }
            }
            $process = Get-Process -Id $ownedApp.ProcessId -ErrorAction SilentlyContinue
            if ($null -ne $process) {
                try { if ([Math]::Abs(($process.StartTime.ToUniversalTime() - $ownedApp.CreationDate.ToUniversalTime()).TotalMilliseconds) -lt 1) { $process.Kill(); $process.WaitForExit(5000) | Out-Null } }
                finally { $process.Dispose() }
            }
        }
        if ($null -ne $appProcess) {
            Assert-Check $appProcess.HasExited 'Test application cleanup stops the exact launched process' 'cleanup'
            $appProcess.Dispose()
        }
    }
    return $true
}

function Assert-SafeSettings {
    $zone = (Get-TimeZone).Id
    $job = New-Job 'setting.apply' @{ setting = 'time.zone'; value = $zone }
    $result = Wait-Job $job.id 'setting-current-timezone'
    Assert-Check ($result.state -eq 'succeeded' -and (Get-TimeZone).Id -eq $zone) 'setting.apply successfully reapplies the current timezone without changing it'

    $brightness = @(try { Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightness -ErrorAction Stop } catch { })
    $methods = @(try { Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightnessMethods -ErrorAction Stop } catch { })
    $levels = @($brightness | Select-Object -ExpandProperty CurrentBrightness -Unique)
    if ($methods.Count -gt 0 -and $levels.Count -ne 1) {
        Write-Warning 'Brightness was not changed: the existing display levels could not be preserved by one setting value.'
        return $false
    }
    $value = if ($levels.Count -eq 1) { [int]$levels[0] } else { 50 }
    $job = New-Job 'setting.apply' @{ setting = 'display.brightness'; value = $value }
    $result = Wait-Job $job.id 'setting-display-brightness'
    if ($methods.Count -eq 0) {
        Assert-Check ($result.state -eq 'failed' -and $result.reason -eq 'error' -and $result.message -match '^applying display\.brightness failed with exit code 3: this device has no built-in display whose brightness can be set$') 'VM without a brightness device reports the exact native hardware limitation'
    } else {
        $after = @(Get-CimInstance -Namespace root/WMI -ClassName WmiMonitorBrightness -ErrorAction Stop)
        Assert-Check ($result.state -eq 'succeeded' -and @($after | Where-Object { [int]$_.CurrentBrightness -ne $value }).Count -eq 0) 'setting.apply reapplies existing display brightness without changing it'
    }
    return $true
}

function Copy-ClientEvidence {
    if (Test-Path -LiteralPath $stateDir -PathType Container) {
        Assert-OwnedPath $stateDir
        # Do not retain config.json or the enrollment secret after purge.
        Get-ChildItem -LiteralPath $stateDir -File | Where-Object { $_.Name -like 'client.log*' -or $_.Name -like 'update-pending.json*' } | ForEach-Object {
            Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $evidenceDir $_.Name) -Force
        }
    }
}

try {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'This harness requires Windows' }
    if (-not $DisposableTarget -or $env:COMPUTERNAME -ine $ExpectedComputerName) {
        throw "Specify -DisposableTarget and the exact disposable target name. Execution host: $env:COMPUTERNAME"
    }
    $principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'Run from an elevated Administrator prompt on the disposable target' }
    if ($null -ne (Get-TestService)) { throw "An existing $serviceName service is present; this harness will not touch it" }
    if ($Workspace -notmatch '^[A-Za-z]:\\' -or $Workspace.Contains('..')) { throw 'Workspace must be a new absolute local directory without .. components' }
    $run = [IO.Path]::GetFullPath($Workspace).TrimEnd('\')
    if (Test-Path -LiteralPath $run) { throw "Workspace must not exist: $run" }
    $parent = Split-Path -Parent $run
    if (-not (Test-Path -LiteralPath $parent -PathType Container)) { throw "Create the dedicated parent directory first: $parent" }
    Assert-NoReparsePoint $run
    if (Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue) { throw "Port $Port is already in use" }

    $hasCandidate = -not [string]::IsNullOrWhiteSpace($CandidateClientExe)
    if (($IncludeMarkerRollback -or $IncludeConfirmationTimeoutRollback) -and -not $hasCandidate) { throw 'Rollback validation requires a signed candidate' }
    $artifactNames = @('CoordinatorExe', 'BaselineClientExe')
    if ($hasCandidate) { $artifactNames += @('CandidateClientExe', 'CandidateSignature', 'UpdatePublicKey', 'ReleaseToolExe') }
    elseif ($CandidateSignature -or $UpdatePublicKey -or $ReleaseToolExe) { throw 'Supply all candidate signing inputs together with CandidateClientExe' }
    foreach ($name in $artifactNames) {
        $value = Get-Variable -Name $name -ValueOnly
        if ([string]::IsNullOrWhiteSpace($value) -or -not (Test-Path -LiteralPath $value -PathType Leaf)) { throw "Missing artifact: $name" }
        $absolute = (Resolve-Path -LiteralPath $value).ProviderPath
        if ($absolute.StartsWith($run + '\', [StringComparison]::OrdinalIgnoreCase)) { throw 'Input artifacts must be outside the disposable workspace' }
        Set-Variable -Name $name -Value $absolute
    }

    Write-Host "Execution host and target: $env:COMPUTERNAME. Installing temporary $serviceName only beneath $run. Coordinator: $base."
    New-Item -ItemType Directory -Path $run | Out-Null
    $workspaceCreated = $true
    $installDir = Join-Path $run 'install'
    $stateDir = Join-Path $run 'client-state'
    $coordinatorDir = Join-Path $run 'coordinator-state'
    $evidenceDir = Join-Path $run 'evidence'
    New-Item -ItemType Directory -Path $evidenceDir | Out-Null
    foreach ($name in @('SC_ACCOUNT_PASSWORD', 'SC_KEY_SECRET', 'SC_KEY_ID', 'SC_COORDINATOR_URL', 'SC_API_TOKEN', 'SC_AUTH', 'SC_AUTO_UPDATE', 'SC_UI_BIND', 'SC_UI_DIR', 'SC_SECURE_COOKIES', 'SC_STATE_DIR', 'SC_DATA_DIR', 'SC_BIND', 'SC_NO_UPDATE_CHECK', 'SC_HEARTBEAT_INTERVAL_SECS')) {
        $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        [Environment]::SetEnvironmentVariable($name, $null, 'Process')
    }

    $helpText = Invoke-Native $BaselineClientExe @('install', '--help') 'install-help'
    if ($helpText -notmatch '--no-hotspot-discovery') { throw 'Baseline must support persisted install --no-hotspot-discovery before this harness can safely install it' }
    $baselineText = Invoke-Native $BaselineClientExe @('--version') 'baseline-version'
    if ($baselineText.Trim() -notmatch '^client (\d+\.\d+\.\d+)$') { throw "Unrecognized baseline version: $baselineText" }
    $baselineVersion = $Matches[1]
    $baselineHash = (Get-FileHash -LiteralPath $BaselineClientExe -Algorithm SHA256).Hash
    if ($hasCandidate) {
        $candidateText = Invoke-Native $CandidateClientExe @('--version') 'candidate-version'
        if ($candidateText.Trim() -notmatch '^client (\d+\.\d+\.\d+)$') { throw "Unrecognized candidate version: $candidateText" }
        $candidateVersion = $Matches[1]
        if ([version]$candidateVersion -le [version]$baselineVersion) { throw 'Candidate must report a newer version than baseline' }
        $candidateHash = (Get-FileHash -LiteralPath $CandidateClientExe -Algorithm SHA256).Hash
        Invoke-Native $ReleaseToolExe @('verify', '--file', $CandidateClientExe, '--version', $candidateVersion, '--target', $Target, '--signature', $CandidateSignature, '--public-key', $UpdatePublicKey) 'candidate-signature' | Out-Null
        Assert-Check $true 'Candidate signature verifies against the supplied disposable public key' 'artifact verification'
        Copy-Item -LiteralPath $UpdatePublicKey -Destination (Join-Path $evidenceDir 'update-public-key.pub')
        Copy-Item -LiteralPath $CandidateSignature -Destination (Join-Path $evidenceDir 'candidate.sig')
    }
    Write-Json (Join-Path $evidenceDir 'artifacts.json') @($artifactNames | ForEach-Object {
        $path = Get-Variable -Name $_ -ValueOnly
        [ordered]@{ role = $_; path = $path; sha256 = (Get-FileHash -LiteralPath $path -Algorithm SHA256).Hash }
    })

    $password = [guid]::NewGuid().ToString('N')
    $env:SC_ACCOUNT_PASSWORD = $password
    Invoke-Native $CoordinatorExe @('account', 'create', '--data-dir', $coordinatorDir, 'lifecycle') 'account-create' | Out-Null
    # Capture the enrollment secret in memory, never in retained evidence.
    $created = (& $CoordinatorExe key create --data-dir $coordinatorDir --label 'Disposable lifecycle validation' | Out-String)
    if ($LASTEXITCODE -ne 0) { throw 'Could not create isolated enrollment key' }
    if ($created -notmatch 'key id:\s+(\S+)') { throw 'Enrollment key id missing from command output' }
    $keyId = $Matches[1]
    if ($created -notmatch 'key secret:\s+(\S+)') { throw 'Enrollment key secret missing from command output' }
    $env:SC_KEY_SECRET = $Matches[1]
    $created = $null
    $server = Start-TestCoordinator 'coordinator'
    Wait-For 'coordinator health' { try { (Api '/health').status -eq 'ok' } catch { $false } }
    Api '/auth/login' 'Post' @{ username = 'lifecycle'; password = $password } | Out-Null
    $updates = Api '/updates'
    Assert-Check (-not $updates.auto_update -and -not $updates.checking) 'Only explicit test updates are enabled' 'isolation'

    # Recheck immediately before installation, then cleanup owns only matching paths.
    if ($null -ne (Get-TestService)) { throw 'A service appeared before installation; refusing to proceed' }
    Assert-OwnedPath $installDir
    Assert-OwnedPath $stateDir
    $serviceTouched = $true
    $installArguments = @('install', '--key-id', $keyId, '--coordinator-url', "ws://127.0.0.1:$Port", '--install-dir', $installDir, '--state-dir', $stateDir, '--no-hotspot-discovery')
    Invoke-Native $BaselineClientExe $installArguments 'install' | Out-Null
    Wait-For 'service running' { (Assert-OwnedService).State -eq 'Running' }
    $config = Get-Content -LiteralPath (Join-Path $stateDir 'config.json') -Raw | ConvertFrom-Json
    Assert-Check ($config.hotspot_discovery -eq $false) 'Installed configuration disables hotspot discovery before startup' 'isolation'
    $deviceId = $config.device_id
    $config = $null
    Wait-Device $baselineVersion
    $service = Assert-OwnedService
    Assert-Check ($service.StartName -eq 'LocalSystem' -and $service.StartMode -eq 'Auto') 'Service uses LocalSystem and automatic startup'
    Write-Json (Join-Path $evidenceDir 'service-installed.json') ($service | Select-Object Name, State, StartName, StartMode, PathName, ProcessId)
    Assert-Script 'installed-service-script'
    $firstPid = $service.ProcessId
    Assert-OwnedService | Out-Null
    Restart-Service -Name $serviceName
    Wait-For 'replacement service process' { $current = Assert-OwnedService; $current.State -eq 'Running' -and $current.ProcessId -ne $firstPid }
    Wait-Device $baselineVersion
    Assert-Check ((Get-Content -LiteralPath (Join-Path $stateDir 'config.json') -Raw | ConvertFrom-Json).device_id -eq $deviceId) 'Restart-Service preserves the device identity'
    Assert-Script 'restarted-service-script'

    Assert-OwnedService | Out-Null
    Invoke-Native $BaselineClientExe $installArguments 'reinstall' | Out-Null
    Wait-Device $baselineVersion
    $reinstalled = Get-Content -LiteralPath (Join-Path $stateDir 'config.json') -Raw | ConvertFrom-Json
    Assert-Check ($reinstalled.device_id -eq $deviceId -and $reinstalled.hotspot_discovery -eq $false -and (Get-FileHash -LiteralPath (Join-Path $installDir 'client.exe')).Hash -eq $baselineHash) 'Reinstallation preserves identity, isolated configuration, and baseline bytes'
    $reinstalled = $null
    Assert-Script 'reinstalled-service-script'
    $interactiveSessionExercised = Assert-InteractiveJobs
    $brightnessSkipped = -not (Assert-SafeSettings)

    if ($hasCandidate) {
        # Allow reads so the prior service can restart, but deny replacing config.
        # The installer swaps in the candidate first, then must restore baseline.
        $configPath = Join-Path $stateDir 'config.json'
        $configHash = (Get-FileHash -LiteralPath $configPath).Hash
        $configLock = [IO.File]::Open($configPath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::Read)
        $installFailed = $false
        try {
            Assert-OwnedService | Out-Null
            try { Invoke-Native $CandidateClientExe $installArguments 'installer-rollback' | Out-Null }
            catch { $installFailed = $true }
            $installError = Get-Content -LiteralPath (Join-Path $evidenceDir 'installer-rollback.stderr.log') -Raw
            Assert-Check ($installFailed -and $installError -match 'previous executable and configuration restored') 'Locked config forces installer rollback after candidate replacement' 'fault injection'
            Assert-Check ((Get-FileHash -LiteralPath (Join-Path $installDir 'client.exe')).Hash -eq $baselineHash -and (Get-FileHash -LiteralPath $configPath).Hash -eq $configHash) 'Failed reinstallation restores exact baseline bytes and leaves configuration intact' 'fault injection'
            Wait-Device $baselineVersion
            Assert-Script 'after-installer-rollback-script'
        } finally { $configLock.Dispose() }

        Assert-Check $updates.enabled 'Coordinator has a compiled update public key' 'artifact verification'
        $signature = (Get-Content -LiteralPath $CandidateSignature -Raw).Trim()
        $badSignature = [Convert]::ToBase64String([byte[]]::new(64))
        $uploadUri = "$base/api/v1/updates/upload?version=$candidateVersion&target=$Target"
        $refusedStatus = 0
        try {
            Invoke-RestMethod -Uri $uploadUri -Method Post -WebSession $session -InFile $CandidateClientExe -ContentType 'application/octet-stream' -Headers @{ 'X-Signature' = $badSignature } -TimeoutSec 60 -UseBasicParsing | Out-Null
        } catch {
            if ($_.Exception.PSObject.Properties['Response'] -and $null -ne $_.Exception.Response) {
                $refusedStatus = [int]$_.Exception.Response.StatusCode
            }
        }
        Assert-Check ($refusedStatus -eq 400 -and @((Api '/updates').cached).Count -eq 0) 'Coordinator refuses an invalid signature without caching the artifact' 'signature rejection'
        Invoke-RestMethod -Uri $uploadUri -Method Post -WebSession $session -InFile $CandidateClientExe -ContentType 'application/octet-stream' -Headers @{ 'X-Signature' = $signature } -TimeoutSec 60 -UseBasicParsing | Out-Null
        Assert-Check ((Api '/updates').latest.sha256 -ieq $candidateHash) 'Coordinator verifies and caches the exact signed candidate' 'artifact verification'

        $cachedSignature = Join-Path $coordinatorDir "updates\$candidateVersion\client-$Target.exe.sig"
        Assert-OwnedPath $cachedSignature
        $originalSignature = [IO.File]::ReadAllBytes($cachedSignature)
        try {
            [IO.File]::WriteAllText($cachedSignature, $badSignature, [Text.UTF8Encoding]::new($false))
            $job = New-Job 'client.update' @{ version = $candidateVersion }
            $result = Wait-Job $job.id 'client-bad-signature'
            Assert-Check ($result.state -eq 'failed' -and $result.reason -eq 'bad_signature') 'Client independently refuses a tampered cached signature' 'fault injection'
            Assert-Check ((Get-FileHash -LiteralPath (Join-Path $installDir 'client.exe')).Hash -eq $baselineHash) 'Signature refusal leaves the baseline executable intact' 'fault injection'
        } finally {
            [IO.File]::WriteAllBytes($cachedSignature, $originalSignature)
        }

        $oldExe = Join-Path $installDir 'client.old.exe'
        Assert-OwnedPath $oldExe
        $backupLock = [IO.File]::Open($oldExe, [IO.FileMode]::CreateNew, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None)
        try {
            $job = New-Job 'client.update' @{ version = $candidateVersion }
            $result = Wait-Job $job.id 'locked-backup-update'
            Assert-Check ($result.state -eq 'failed' -and $result.reason -ne 'bad_signature' -and $result.message -match '(sharing|used by another process|os error 32|access.*denied|os error 5)') 'Locked client.old.exe forces a native replacement failure' 'fault injection'
            Assert-Check ((Get-FileHash -LiteralPath (Join-Path $installDir 'client.exe')).Hash -eq $baselineHash -and -not (Test-Path -LiteralPath (Join-Path $stateDir 'update-pending.json'))) 'Failed replacement preserves baseline and leaves no pending update' 'fault injection'
            Assert-Script 'after-replacement-failure-script'
        } finally {
            $backupLock.Dispose()
            $backupLock = $null
            Assert-OwnedPath $oldExe
            Remove-Item -LiteralPath $oldExe -Force
        }

        # Permit download writes and the signed --version probe, but deny renaming
        # the downloaded file. swap() must put the running executable back after
        # first moving it to client.old.exe. Some Windows policies may block the
        # probe itself; that outcome is recorded as narrower preflight coverage.
        $downloadPath = Join-Path $installDir 'client.update.tmp'
        Assert-OwnedPath $downloadPath
        if (Test-Path -LiteralPath $downloadPath) { throw 'An unexpected update temporary file remains before rename fault injection' }
        $emptyDownload = [IO.File]::Open($downloadPath, [IO.FileMode]::CreateNew, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite)
        $emptyDownload.Dispose()
        $downloadLock = [IO.File]::Open($downloadPath, [IO.FileMode]::Open, [IO.FileAccess]::Read, [IO.FileShare]::ReadWrite)
        try {
            $job = New-Job 'client.update' @{ version = $candidateVersion }
            $result = Wait-Job $job.id 'locked-download-rename-update'
            Assert-Check ($result.state -eq 'failed') 'A download held without delete sharing refuses the update' 'fault injection'
            Assert-Check ((Get-FileHash -LiteralPath (Join-Path $installDir 'client.exe')).Hash -eq $baselineHash -and -not (Test-Path -LiteralPath $oldExe) -and -not (Test-Path -LiteralPath (Join-Path $stateDir 'update-pending.json'))) 'Rename fault leaves the exact baseline with no backup or pending marker' 'fault injection'
            $swapRestorationExercised = $result.message -match 'replacing the executable'
            if ($swapRestorationExercised) {
                Assert-Check $true 'Native executable swap reports a replacement failure and restores the baseline' 'fault injection'
            } else {
                Write-Warning 'Windows refused the locked download before the native swap. This run proves preflight refusal, not restoration after the first rename; see the job evidence.'
            }
            Assert-Script 'after-download-rename-failure-script'
        } finally {
            $downloadLock.Dispose()
            Assert-OwnedPath $downloadPath
            if (Test-Path -LiteralPath $downloadPath) { Remove-Item -LiteralPath $downloadPath -Force }
        }

        $preUpdatePid = (Assert-OwnedService).ProcessId
        $job = New-Job 'client.update' @{ version = $candidateVersion }
        $result = Wait-Job $job.id 'signed-update'
        Assert-Check ($result.state -eq 'succeeded') 'Signed update succeeds after the new service completes its handshake'
        Wait-Device $candidateVersion
        $installedExe = Join-Path $installDir 'client.exe'
        Assert-Check ((Get-FileHash -LiteralPath $installedExe).Hash -eq $candidateHash -and (Assert-OwnedService).ProcessId -ne $preUpdatePid) 'SCM restarts into the exact candidate executable'
        Wait-For 'update marker and backup cleanup' { -not (Test-Path -LiteralPath (Join-Path $stateDir 'update-pending.json')) -and -not (Test-Path -LiteralPath $oldExe) }
        Assert-Check ((Get-Content -LiteralPath (Join-Path $stateDir 'config.json') -Raw | ConvertFrom-Json).device_id -eq $deviceId) 'Signed update preserves the device identity'
        Assert-Script 'updated-service-script'

        if ($IncludeMarkerRollback) {
            Write-Host 'Injecting a pending marker and baseline backup; no failed real update is being simulated by altering its job record.'
            Stop-OwnedCoordinator $server
            Stop-TestService
            Assert-OwnedPath $oldExe
            Copy-Item -LiteralPath $BaselineClientExe -Destination $oldExe
            $markerPath = Join-Path $stateDir 'update-pending.json'
            Assert-OwnedPath $markerPath
            Write-Json $markerPath @{ job_target_id = ('fault-injection-' + [guid]::NewGuid().ToString('N')); previous_version = $baselineVersion; new_version = $candidateVersion; starts = 0; rolled_back = $false }
            foreach ($start in 1..3) {
                Assert-OwnedService | Out-Null
                Start-Service -Name $serviceName
                Wait-For "pending marker records unconfirmed start $start" {
                    try { (Get-Content -LiteralPath $markerPath -Raw | ConvertFrom-Json).starts -eq $start } catch { $false }
                }
                Assert-Check ((Get-FileHash -LiteralPath $installedExe).Hash -eq $candidateHash) "Candidate stays installed after unconfirmed start $start" 'marker fault injection'
                Copy-Item -LiteralPath $markerPath -Destination (Join-Path $evidenceDir "rollback-start-$start.json")
                Stop-TestService
            }
            Assert-OwnedService | Out-Null
            $markerBeforeFourthStart = Get-Content -LiteralPath $markerPath -Raw | ConvertFrom-Json
            try {
                Start-Service -Name $serviceName
            } catch {
                # The fourth candidate start restores baseline and exits with the
                # restart code before Start-Service can observe Running. Accept
                # that transient observer error only when the rollback itself is
                # proven; the wait below still requires SCM to start baseline.
                $startError = $_
                $rollbackMarker = $null
                $rollbackHash = $null
                $probeError = $null
                $expectedRollback = $false
                try {
                    Assert-OwnedService | Out-Null
                    $rollbackMarker = Get-Content -LiteralPath $markerPath -Raw | ConvertFrom-Json
                    $rollbackHash = (Get-FileHash -LiteralPath $installedExe).Hash
                    $expectedRollback = $rollbackHash -eq $baselineHash -and
                        $markerBeforeFourthStart.starts -eq 3 -and
                        $rollbackMarker.starts -eq 4 -and $rollbackMarker.rolled_back -eq $true -and
                        $rollbackMarker.previous_version -ceq $baselineVersion -and
                        $rollbackMarker.new_version -ceq $candidateVersion -and
                        $rollbackMarker.job_target_id -ceq $markerBeforeFourthStart.job_target_id
                } catch { $probeError = $_.Exception.Message }
                Write-Json (Join-Path $evidenceDir 'rollback-fourth-start-observer.json') @{
                    accepted_transient_start_error = $expectedRollback
                    start_error = $startError.Exception.Message
                    error_id = $startError.FullyQualifiedErrorId
                    installed_sha256 = $rollbackHash
                    marker = $rollbackMarker
                    verification_error = $probeError
                }
                if (-not $expectedRollback) { throw $startError }
                Write-Host 'Fourth startup restored baseline and exited for SCM restart; waiting for automatic recovery.'
            }
            Wait-For 'fourth startup restores baseline and SCM restarts it' {
                try {
                    (Get-FileHash -LiteralPath $installedExe).Hash -eq $baselineHash -and (Assert-OwnedService).State -eq 'Running' -and -not (Test-Path -LiteralPath $markerPath)
                } catch { $false }
            } 120
            Assert-Check $true 'Fourth startup restores the exact baseline after three unconfirmed starts' 'marker fault injection'
            $server = Start-TestCoordinator 'coordinator-after-rollback'
            Wait-For 'coordinator health after rollback' { try { (Api '/health').status -eq 'ok' } catch { $false } }
            Api '/auth/login' 'Post' @{ username = 'lifecycle'; password = $password } | Out-Null
            Wait-Device $baselineVersion
            Assert-Check ((Get-Content -LiteralPath (Join-Path $stateDir 'config.json') -Raw | ConvertFrom-Json).device_id -eq $deviceId) 'Injected rollback preserves the device identity' 'marker fault injection'
            Assert-Script 'rolled-back-service-script'
        }

        if ($IncludeConfirmationTimeoutRollback) {
            # The preceding three-start scenario ends on baseline, so restore the
            # candidate through the real signed update path before this scenario.
            if (((Api '/devices').devices[0]).client_version -ne $candidateVersion) {
                $job = New-Job 'client.update' @{ version = $candidateVersion }
                $result = Wait-Job $job.id 'signed-update-before-timeout'
                Assert-Check ($result.state -eq 'succeeded') 'Signed candidate is running before confirmation-deadline injection'
                Wait-Device $candidateVersion
            }
            Stop-OwnedCoordinator $server
            Stop-TestService
            Assert-OwnedPath $oldExe
            Copy-Item -LiteralPath $BaselineClientExe -Destination $oldExe
            $markerPath = Join-Path $stateDir 'update-pending.json'
            Assert-OwnedPath $markerPath
            Write-Json $markerPath @{ job_target_id = ('deadline-fault-' + [guid]::NewGuid().ToString('N')); previous_version = $baselineVersion; new_version = $candidateVersion; starts = 0; rolled_back = $false }
            $deadlineTimer = [Diagnostics.Stopwatch]::StartNew()
            $observed = @{ marker = $null; elapsed_seconds = $null; next_progress = 30 }
            Assert-OwnedService | Out-Null
            Start-Service -Name $serviceName
            Write-Host 'Waiting for the real 180-second confirmation deadline with the candidate running and the isolated coordinator stopped.'
            Wait-For 'production confirmation deadline restores baseline without manual restarts' {
                if ($deadlineTimer.Elapsed.TotalSeconds -ge $observed.next_progress) {
                    Write-Host ('Confirmation-deadline observation: {0:N0} seconds elapsed' -f $deadlineTimer.Elapsed.TotalSeconds)
                    $observed.next_progress += 30
                }
                try {
                    $marker = Get-Content -LiteralPath $markerPath -Raw | ConvertFrom-Json
                    if ($marker.rolled_back) {
                        $observed.marker = $marker
                        $observed.elapsed_seconds = $deadlineTimer.Elapsed.TotalSeconds
                        return $true
                    }
                } catch { }
                return $false
            } 240
            $deadlineTimer.Stop()
            Write-Json (Join-Path $evidenceDir 'confirmation-deadline-rollback.json') $observed
            Assert-Check ($observed.marker.starts -eq 1 -and $observed.marker.failure -match 'within 180 s' -and $observed.elapsed_seconds -ge 179) 'A single unconfirmed startup rolls back at the real 180-second deadline' 'deadline marker fault injection'
            Assert-Check ((Get-FileHash -LiteralPath $installedExe).Hash -eq $baselineHash) 'Confirmation timeout restores the exact baseline executable' 'deadline marker fault injection'
            Wait-For 'SCM starts baseline after the confirmation timeout' {
                (Assert-OwnedService).State -eq 'Running' -and -not (Test-Path -LiteralPath $markerPath)
            } 120
            $server = Start-TestCoordinator 'coordinator-after-confirmation-timeout'
            Wait-For 'coordinator health after confirmation timeout' { try { (Api '/health').status -eq 'ok' } catch { $false } }
            Api '/auth/login' 'Post' @{ username = 'lifecycle'; password = $password } | Out-Null
            Wait-Device $baselineVersion
            Assert-Check ((Get-Content -LiteralPath (Join-Path $stateDir 'config.json') -Raw | ConvertFrom-Json).device_id -eq $deviceId) 'Confirmation-deadline rollback preserves the device identity' 'deadline marker fault injection'
            Assert-Script 'confirmation-timeout-rolled-back-script'
        }
    }
    Write-Json (Join-Path $evidenceDir 'devices-final.json') (Api '/devices')
    $passed = $true
} catch {
    $failure = $_.Exception.Message
    Write-Warning $failure
} finally {
    if ($null -ne $backupLock) { $backupLock.Dispose() }
    if ($workspaceCreated) {
        if ($serviceTouched) {
            try {
                Assert-OwnedPath $installDir
                Assert-OwnedPath $stateDir
                $ownedService = Assert-OwnedService
                if ($null -ne $ownedService) { Stop-TestService }
                Copy-ClientEvidence
                Invoke-Native $BaselineClientExe @('uninstall', '--install-dir', $installDir, '--state-dir', $stateDir, '--purge') 'uninstall' | Out-Null
                Wait-For 'service removed' { $null -eq (Get-TestService) }
                Assert-Check (-not (Test-Path -LiteralPath $installDir) -and -not (Test-Path -LiteralPath $stateDir)) 'Uninstall removes the service and purges only the dedicated install/state directories' 'cleanup'
            } catch { $cleanupErrors.Add($_.Exception.Message) }
        }
        foreach ($process in $coordinatorProcesses) {
            try { Stop-OwnedCoordinator $process } catch { $cleanupErrors.Add($_.Exception.Message) }
        }
        try {
            if (Test-Path -LiteralPath $coordinatorDir) {
                Assert-OwnedPath $coordinatorDir
                Remove-Item -LiteralPath $coordinatorDir -Recurse -Force
            }
        } catch { $cleanupErrors.Add($_.Exception.Message) }
        $report = [ordered]@{
            host_name = $env:COMPUTERNAME; expected_target = $ExpectedComputerName; workspace = $run; coordinator_url = $base
            started_at = $startedAt.ToString('o'); finished_at = [DateTime]::UtcNow.ToString('o')
            passed = ($passed -and $cleanupErrors.Count -eq 0); failure = $failure; cleanup_errors = $cleanupErrors.ToArray()
            baseline_version = $baselineVersion; candidate_version = $candidateVersion; device_id = $deviceId
            checks = $checks.ToArray(); marker_rollback_requested = [bool]$IncludeMarkerRollback
            confirmation_deadline_rollback_requested = [bool]$IncludeConfirmationTimeoutRollback
            native_swap_restoration_exercised = $swapRestorationExercised
            interactive_session_exercised = $interactiveSessionExercised
            skipped = @(
                if (-not $CandidateClientExe) { 'Signed update, signature rejection, replacement failure: no candidate supplied.' }
                if (-not $IncludeMarkerRollback) { 'Marker-driven rollback: IncludeMarkerRollback was not requested.' }
                if (-not $IncludeConfirmationTimeoutRollback) { 'Production confirmation-deadline rollback: IncludeConfirmationTimeoutRollback was not requested.' }
                if ($CandidateClientExe -and -not $swapRestorationExercised) { 'Restoration after the first executable rename was not exercised; the locked download was refused earlier.' }
                if (-not $interactiveSessionExercised) { 'Interactive user script and application launch: no interactive console login was available.' }
                if ($brightnessSkipped) { 'Brightness apply: current display levels could not be preserved safely by one value.' }
            )
            limitations = @(
                'Coverage is limited to a disposable Windows service and localhost coordinator; no Surface Wi-Fi adapter or fleet performance is tested.',
                'A locked backup tests replacement refusal before the executable swap; it is not a completed-update rollback.',
                'Marker rollback scenarios inject files after a successful update. They cover startup-count and confirmation-deadline recovery when requested, not automatic restart of a crashing or corrupt executable.',
                'This harness does not reboot the machine. Automatic boot startup is configured and inspected, not validated across a reboot.'
            )
        }
        Write-Json (Join-Path $run 'report.json') $report
        Write-Host "Report: $(Join-Path $run 'report.json')"
        Write-Host "Evidence: $evidenceDir"
    }
    foreach ($name in $savedEnvironment.Keys) { [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process') }
}

if (-not $passed -or $cleanupErrors.Count -gt 0) {
    if ($cleanupErrors.Count) { Write-Warning ('Cleanup needs attention on the disposable target: ' + ($cleanupErrors -join '; ')) }
    throw "Client lifecycle validation failed. $failure"
}
