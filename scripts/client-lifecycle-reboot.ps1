#requires -Version 5.1
<#
.SYNOPSIS
  Prepare or complete an isolated, real reboot/autostart check on SC-VALIDATION.
.DESCRIPTION
  Begin requires an elevated disposable SC-VALIDATION VM, no existing client
  service, and an explicitly versioned client executable. It installs beneath a fresh
  C:\Validation\client-lifecycle-reboot-<id> directory with hotspot discovery off
  and an unavailable loopback coordinator. Enrollment material is random and
  disposable. Checkpoint and reports contain no enrollment secret.

  A unique SYSTEM AtStartup task runs a protected copy of this script in Complete
  mode. Complete checks a real boot-time change, automatic service startup,
  identity and executable hash, then uninstalls/purges only its owned installation
  and removes its exact task. Evidence stays in the workspace and is copied as
  flat, per-run-prefixed files into an already-existing ResultsDirectory. Prepare
  that directory on the host before Begin; the guest never creates SMB folders.
  Allowed result roots are C:\Validation\results and \\host.lan\Data\results.

  Begin does NOT reboot unless -Reboot is explicitly supplied. Do not run it
  until other VM validation and screenshots have finished.
.EXAMPLE
  .\client-lifecycle-reboot.ps1 -Mode Begin -DisposableTarget `
    -BaselineClientExe C:\Artifacts\baseline\client.exe `
    -ResultsDirectory \\host.lan\Data\results -Reboot
#>
[CmdletBinding(DefaultParameterSetName = 'Begin')]
param(
    [Parameter(Mandatory)][switch]$DisposableTarget,
    [ValidateSet('Begin', 'Complete')][string]$Mode = 'Begin',
    [Parameter(Mandatory, ParameterSetName = 'Begin')][string]$BaselineClientExe,
    [Parameter(ParameterSetName = 'Begin')][string]$ResultsDirectory = 'C:\Validation\results',
    [Parameter(ParameterSetName = 'Begin')][ValidatePattern('^\d+\.\d+\.\d+$')][string]$BaselineVersion = '90.0.1',
    [Parameter(ParameterSetName = 'Begin')][ValidatePattern('^[A-Fa-f0-9]{64}$')][string]$ExpectedSha256,
    [Parameter(ParameterSetName = 'Begin')][ValidateRange(1024, 65535)][int]$Port = 18085,
    [Parameter(ParameterSetName = 'Begin')][switch]$Reboot,
    [Parameter(Mandatory, ParameterSetName = 'Complete')][string]$CheckpointPath
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$serviceName = 'SurfaceCoordinatorClient'
$validationRoot = 'C:\Validation'
$powershellExe = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$savedSecret = [Environment]::GetEnvironmentVariable('SC_KEY_SECRET', 'Process')
$checks = [Collections.Generic.List[object]]::new()
$cleanupErrors = [Collections.Generic.List[string]]::new()
$checkpoint = $null
$workspace = $null
$installDir = $null
$stateDir = $null
$evidence = $null
$sourceExe = $null
$taskName = $null
$taskArguments = $null
$serviceTouched = $false
$taskTouched = $false
$prepared = $false
$verified = $false
$failure = $null
$rebootRequested = $false

function Write-Json([string]$Path, $Value) {
    [IO.File]::WriteAllText($Path, (ConvertTo-Json -InputObject $Value -Depth 8), [Text.UTF8Encoding]::new($false))
}

function Assert-Check([bool]$Condition, [string]$Description) {
    $checks.Add([ordered]@{ description = $Description; passed = $Condition; at = [DateTime]::UtcNow.ToString('o') })
    if (-not $Condition) { throw "Failed: $Description" }
    Write-Host "PASS $Description"
}

function No-Reparse([string]$Path) {
    $current = [IO.Path]::GetFullPath($Path)
    while ($current) {
        if (Test-Path -LiteralPath $current) {
            if (((Get-Item -LiteralPath $current -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Refusing a reparse point: $current" }
        }
        $current = Split-Path -Parent $current
    }
}

function Owned-Directory([string]$Path) {
    $absolute = [IO.Path]::GetFullPath($Path)
    if (-not $absolute.StartsWith($workspace + '\', [StringComparison]::OrdinalIgnoreCase)) { throw "Path escaped this reboot workspace: $Path" }
    No-Reparse $absolute
    if (Test-Path -LiteralPath $absolute -PathType Container) {
        foreach ($item in Get-ChildItem -LiteralPath $absolute -Recurse -Force) {
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw "Refusing a nested reparse point: $($item.FullName)" }
        }
    }
}

function Get-OwnedService {
    $service = Get-CimInstance Win32_Service -Filter "Name='$serviceName'"
    if ($null -ne $service) {
        $pattern = '^\s*"?' + [regex]::Escape((Join-Path $installDir 'client.exe')) + '"?\s+"?service"?\s+"?--state-dir"?\s+"?' + [regex]::Escape($stateDir) + '"?\s*$'
        if ($service.PathName.Replace('\\?\', '') -notmatch $pattern) { throw 'The client service belongs to another installation; refusing to touch it' }
    }
    return $service
}

function Get-OwnedTask {
    $task = Get-ScheduledTask -TaskPath '\' -TaskName $taskName -ErrorAction SilentlyContinue
    if ($null -ne $task) {
        $taskUser = $task.Principal.UserId
        $taskSid = if ($taskUser -match '^S-1-') { ([Security.Principal.SecurityIdentifier]::new($taskUser)).Value } else { ([Security.Principal.NTAccount]::new($taskUser)).Translate([Security.Principal.SecurityIdentifier]).Value }
        if (@($task.Actions).Count -ne 1 -or $task.Actions[0].Execute -ine $powershellExe -or $task.Actions[0].Arguments -cne $taskArguments -or $taskSid -ne 'S-1-5-18') {
            throw 'The startup task no longer belongs to this validation; refusing to unregister it'
        }
    }
    return $task
}

function Wait-For([string]$Description, [scriptblock]$Condition, [int]$Seconds = 120) {
    $deadline = [DateTime]::UtcNow.AddSeconds($Seconds)
    do {
        if (& $Condition) { return }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out: $Description"
}

function Quote-Argument([string]$Value) {
    '"' + [regex]::Replace([regex]::Replace($Value, '(\\*)"', '$1$1\"'), '(\\+)$', '$1$1') + '"'
}

function Invoke-Client([string[]]$Arguments, [string]$Name) {
    $quoted = @($Arguments | ForEach-Object { Quote-Argument $_ })
    $output = Join-Path $evidence "$Name.stdout.log"
    $process = Start-Process -FilePath $sourceExe -ArgumentList $quoted -WindowStyle Hidden -WorkingDirectory $workspace -PassThru `
        -RedirectStandardOutput $output -RedirectStandardError (Join-Path $evidence "$Name.stderr.log")
    # Cache the handle before a fast child exits so PowerShell 5 retains ExitCode.
    $processHandle = $process.Handle
    try {
        if (-not $process.WaitForExit(90000)) { $process.Kill(); $process.WaitForExit(); throw "$Name timed out" }
        $process.WaitForExit()
        $exitCode = $process.ExitCode
        if ($exitCode -ne 0) { throw "$Name failed with exit code $exitCode; inspect evidence" }
        return Get-Content -LiteralPath $output -Raw
    } finally { $process.Dispose() }
}

function Validate-ResultsPath([string]$Path) {
    if ($Path -notmatch '^(?:[A-Za-z]:\\|\\\\)') { throw 'ResultsDirectory must be an absolute local or UNC directory' }
    $absolute = [IO.Path]::GetFullPath($Path).TrimEnd('\')
    $allowed = $false
    foreach ($root in @('C:\Validation\results', '\\host.lan\Data\results')) {
        if ($absolute -ieq $root -or $absolute.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)) { $allowed = $true }
    }
    if (-not $allowed) { throw 'ResultsDirectory must be beneath C:\Validation\results or \\host.lan\Data\results' }
    return $absolute
}

function Secure-Workspace {
    $acl = [Security.AccessControl.DirectorySecurity]::new()
    $acl.SetAccessRuleProtection($true, $false)
    foreach ($sid in @('S-1-5-18', 'S-1-5-32-544')) {
        $identity = [Security.Principal.SecurityIdentifier]::new($sid)
        $rule = [Security.AccessControl.FileSystemAccessRule]::new($identity, 'FullControl', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
        $acl.AddAccessRule($rule)
    }
    $acl.SetOwner([Security.Principal.SecurityIdentifier]::new('S-1-5-32-544'))
    Set-Acl -LiteralPath $workspace -AclObject $acl
}

try {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT -or -not $DisposableTarget -or $env:COMPUTERNAME -ine 'SC-VALIDATION') {
        throw "This helper is restricted to -DisposableTarget on SC-VALIDATION. Execution host: $env:COMPUTERNAME"
    }
    $principal = [Security.Principal.WindowsPrincipal]::new([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) { throw 'This helper requires an elevated Administrator or SYSTEM token' }
    if (($Mode -eq 'Begin') -ne ($PSCmdlet.ParameterSetName -eq 'Begin')) { throw 'Use Begin with BaselineClientExe, or Complete with CheckpointPath' }
    if (-not (Test-Path -LiteralPath $validationRoot -PathType Container)) { throw 'Prepare the protected C:\Validation directory first' }
    No-Reparse $validationRoot

    if ($Mode -eq 'Begin') {
        if ($null -ne (Get-CimInstance Win32_Service -Filter "Name='$serviceName'")) { throw 'An existing client service is present; run this probe only after main validation has cleaned up' }
        if (Get-NetTCPConnection -State Listen -LocalPort $Port -ErrorAction SilentlyContinue) { throw "Loopback port $Port must be unused" }
        if (-not (Test-Path -LiteralPath $BaselineClientExe -PathType Leaf)) { throw 'Client executable is missing' }
        if ($ExpectedSha256 -and (Get-FileHash -LiteralPath $BaselineClientExe -Algorithm SHA256).Hash -ine $ExpectedSha256) { throw 'Client executable does not match the expected SHA256' }
        $results = Validate-ResultsPath $ResultsDirectory
        if (-not (Test-Path -LiteralPath $results -PathType Container)) { throw "Precreate ResultsDirectory on the host before Begin: $results" }
        No-Reparse $results
        $id = [guid]::NewGuid().ToString('N')
        $workspace = Join-Path $validationRoot "client-lifecycle-reboot-$id"
        if (Test-Path -LiteralPath $workspace) { throw 'The generated reboot workspace unexpectedly exists' }
        New-Item -ItemType Directory -Path $workspace | Out-Null
        Secure-Workspace
        $installDir = Join-Path $workspace 'install'
        $stateDir = Join-Path $workspace 'client-state'
        $evidence = Join-Path $workspace 'evidence'
        New-Item -ItemType Directory -Path $evidence | Out-Null
        $sourceExe = Join-Path $workspace 'baseline-source.exe'
        Copy-Item -LiteralPath $BaselineClientExe -Destination $sourceExe
        $snapshotScript = Join-Path $workspace 'reboot-probe.ps1'
        Copy-Item -LiteralPath $PSCommandPath -Destination $snapshotScript
        $CheckpointPath = Join-Path $workspace 'checkpoint.json'
        $taskName = "SC-Lifecycle-Reboot-$id"
        $taskArguments = (@('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $snapshotScript, '-Mode', 'Complete', '-DisposableTarget', '-CheckpointPath', $CheckpointPath) | ForEach-Object { Quote-Argument $_ }) -join ' '
        $reported = (Invoke-Client @('--version') 'baseline-version').Trim()
        Assert-Check ($reported -eq "client $BaselineVersion") 'Client reports the explicitly requested version'
        if ($ExpectedSha256) { Assert-Check ((Get-FileHash -LiteralPath $sourceExe -Algorithm SHA256).Hash -ieq $ExpectedSha256) 'Copied client matches the expected SHA256' }
        Assert-Check ((Invoke-Client @('install', '--help') 'install-help') -match '--no-hotspot-discovery') 'Client supports persisted discovery disable'
        Write-Host "Execution target: SC-VALIDATION. Preparing temporary service and startup task only beneath $workspace."
        $env:SC_KEY_SECRET = [guid]::NewGuid().ToString('N') + [guid]::NewGuid().ToString('N')
        $keyId = 'reboot-fixture-' + [guid]::NewGuid().ToString('N')
        Owned-Directory $installDir
        Owned-Directory $stateDir
        if ($null -ne (Get-CimInstance Win32_Service -Filter "Name='$serviceName'")) { throw 'A client service appeared before installation' }
        $serviceTouched = $true
        Invoke-Client @('install', '--key-id', $keyId, '--coordinator-url', "ws://127.0.0.1:$Port", '--install-dir', $installDir, '--state-dir', $stateDir, '--no-hotspot-discovery') 'install' | Out-Null
        Wait-For 'baseline service running before reboot' { (Get-OwnedService).State -eq 'Running' }
        $config = Get-Content -LiteralPath (Join-Path $stateDir 'config.json') -Raw | ConvertFrom-Json
        Assert-Check ($config.hotspot_discovery -eq $false) 'Hotspot discovery is disabled before reboot'
        $checkpoint = [ordered]@{
            fixture_only = $true; host_name = 'SC-VALIDATION'; workspace = $workspace; task_name = $taskName; task_arguments = $taskArguments
            prepared_at = [DateTime]::UtcNow.ToString('o'); boot_before = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime().ToString('o')
            device_id = $config.device_id; baseline_version = $BaselineVersion; baseline_sha256 = (Get-FileHash -LiteralPath $sourceExe).Hash
            script_sha256 = (Get-FileHash -LiteralPath $snapshotScript).Hash; results_directory = $results; port = $Port
        }
        $config = $null
        Write-Json $CheckpointPath $checkpoint
        $trigger = New-ScheduledTaskTrigger -AtStartup
        $trigger.Delay = 'PT10S'
        $action = New-ScheduledTaskAction -Execute $powershellExe -Argument $taskArguments
        $taskPrincipal = New-ScheduledTaskPrincipal -UserId 'SYSTEM' -LogonType ServiceAccount -RunLevel Highest
        $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit (New-TimeSpan -Minutes 10) -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries
        if ($null -ne (Get-ScheduledTask -TaskName $taskName -ErrorAction SilentlyContinue)) { throw 'The unique startup task name already exists' }
        $taskTouched = $true
        Register-ScheduledTask -TaskPath '\' -TaskName $taskName -Trigger $trigger -Action $action -Principal $taskPrincipal -Settings $settings | Out-Null
        Get-OwnedTask | Out-Null
        $prepared = $true
        $rebootRequested = [bool]$Reboot
        Write-Json (Join-Path $workspace 'report.json') @{ phase = 'awaiting_reboot'; host_name = $env:COMPUTERNAME; workspace = $workspace; checks = $checks.ToArray(); reboot_requested = $rebootRequested }
        Write-Host "Reboot probe prepared. Checkpoint: $CheckpointPath"
        if (-not $Reboot) { Write-Host 'No reboot requested. The unique startup task will complete validation after the next deliberate VM reboot.' }
    } else {
        $CheckpointPath = [IO.Path]::GetFullPath($CheckpointPath)
        if ($CheckpointPath -notmatch '^C:\\Validation\\client-lifecycle-reboot-([0-9a-f]{32})\\checkpoint\.json$') { throw 'CheckpointPath is not an owned reboot probe path' }
        $id = $Matches[1]
        $workspace = Split-Path -Parent $CheckpointPath
        No-Reparse $CheckpointPath
        $installDir = Join-Path $workspace 'install'
        $stateDir = Join-Path $workspace 'client-state'
        $evidence = Join-Path $workspace 'evidence'
        $sourceExe = Join-Path $workspace 'baseline-source.exe'
        $snapshotScript = Join-Path $workspace 'reboot-probe.ps1'
        $checkpoint = Get-Content -LiteralPath $CheckpointPath -Raw | ConvertFrom-Json
        if (-not $checkpoint.fixture_only -or $checkpoint.host_name -cne 'SC-VALIDATION' -or $checkpoint.workspace -ine $workspace -or $checkpoint.task_name -cne "SC-Lifecycle-Reboot-$id") { throw 'Checkpoint does not match this isolated workspace' }
        $taskName = $checkpoint.task_name
        $taskArguments = (@('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', $snapshotScript, '-Mode', 'Complete', '-DisposableTarget', '-CheckpointPath', $CheckpointPath) | ForEach-Object { Quote-Argument $_ }) -join ' '
        if ($taskArguments -cne $checkpoint.task_arguments) { throw 'Checkpoint startup action was changed' }
        $results = Validate-ResultsPath $checkpoint.results_directory
        Assert-Check ((Get-FileHash -LiteralPath $PSCommandPath).Hash -eq $checkpoint.script_sha256 -and (Get-FileHash -LiteralPath $sourceExe).Hash -eq $checkpoint.baseline_sha256) 'Protected helper and baseline match their pre-reboot hashes'
        Get-OwnedTask | Out-Null
        $taskTouched = $true
        Get-OwnedService | Out-Null
        $serviceTouched = $true
        $bootAfter = (Get-CimInstance Win32_OperatingSystem).LastBootUpTime.ToUniversalTime()
        Assert-Check ($bootAfter -gt [DateTime]::Parse($checkpoint.boot_before).ToUniversalTime()) 'Windows completed a real reboot after the recorded checkpoint'
        Wait-For 'automatic service startup after reboot' { (Get-OwnedService).State -eq 'Running' }
        $service = Get-OwnedService
        Assert-Check ($service.StartMode -eq 'Auto' -and $service.StartName -eq 'LocalSystem') 'SCM automatically started the LocalSystem service after reboot'
        $process = Get-Process -Id $service.ProcessId
        Assert-Check ($process.StartTime.ToUniversalTime() -ge $bootAfter) 'Service process was created during the new boot'
        $process.Dispose()
        $config = Get-Content -LiteralPath (Join-Path $stateDir 'config.json') -Raw | ConvertFrom-Json
        Assert-Check ($config.device_id -ceq $checkpoint.device_id -and $config.hotspot_discovery -eq $false) 'Reboot preserves device identity and disabled hotspot discovery'
        $config = $null
        Assert-Check ((Get-FileHash -LiteralPath (Join-Path $installDir 'client.exe')).Hash -eq $checkpoint.baseline_sha256) 'Automatically started executable matches the exact installed baseline'
        Write-Json (Join-Path $evidence 'service-after-reboot.json') ($service | Select-Object Name, State, StartMode, StartName, PathName, ProcessId)
        Write-Json (Join-Path $evidence 'boot-times.json') @{ before = $checkpoint.boot_before; after = $bootAfter.ToString('o') }
        $verified = $true
    }
} catch {
    $failure = $_.Exception.Message
    Write-Warning $failure
} finally {
    [Environment]::SetEnvironmentVariable('SC_KEY_SECRET', $savedSecret, 'Process')
    # A successful Begin intentionally leaves its service/task for the next boot.
    if (-not $prepared) {
        if ($serviceTouched) {
            try {
                Owned-Directory $installDir
                Owned-Directory $stateDir
                $service = Get-OwnedService
                if ($null -ne $service -and $service.State -ne 'Stopped') {
                    $serviceProcess = Get-Process -Id $service.ProcessId -ErrorAction SilentlyContinue
                    Stop-Service -Name $serviceName
                    Wait-For 'service stopped for cleanup' { (Get-OwnedService).State -eq 'Stopped' } 60
                    if ($null -ne $serviceProcess) {
                        try { Wait-For 'service executable released' { $serviceProcess.HasExited } 30 }
                        finally { $serviceProcess.Dispose() }
                    }
                }
                if (Test-Path -LiteralPath $stateDir) {
                    Get-ChildItem -LiteralPath $stateDir -File -Filter 'client.log*' | ForEach-Object { Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $evidence $_.Name) -Force }
                }
                Invoke-Client @('uninstall', '--install-dir', $installDir, '--state-dir', $stateDir, '--purge') 'uninstall' | Out-Null
                Wait-For 'service deletion' { $null -eq (Get-CimInstance Win32_Service -Filter "Name='$serviceName'") } 60
                Assert-Check (-not (Test-Path -LiteralPath $installDir) -and -not (Test-Path -LiteralPath $stateDir)) 'Uninstall purges only the dedicated installation and state'
            } catch { $cleanupErrors.Add($_.Exception.Message) }
        }
        if ($taskTouched) {
            try {
                if ($null -ne (Get-OwnedTask)) { Unregister-ScheduledTask -TaskPath '\' -TaskName $taskName -Confirm:$false }
                Assert-Check ($null -eq (Get-ScheduledTask -TaskPath '\' -TaskName $taskName -ErrorAction SilentlyContinue)) 'The exact validation startup task is removed'
            } catch { $cleanupErrors.Add($_.Exception.Message) }
        }
        if ($workspace -and (Test-Path -LiteralPath $workspace -PathType Container)) {
            $reportPath = Join-Path $workspace 'report.json'
            $report = [ordered]@{
                phase = 'complete'; passed = ($verified -and $cleanupErrors.Count -eq 0); host_name = $env:COMPUTERNAME
                workspace = $workspace; completed_at = [DateTime]::UtcNow.ToString('o'); failure = $failure; checks = $checks.ToArray(); cleanup_errors = $cleanupErrors.ToArray()
                limitation = 'Proves Windows boot and automatic service startup with unchanged identity/binary. No coordinator enrollment or network movement is tested.'
            }
            Write-Json $reportPath $report
            Write-Host "Reboot validation report: $reportPath"
            if ($null -ne $checkpoint) {
                try {
                    $results = Validate-ResultsPath $checkpoint.results_directory
                    $destination = $results
                    $prefix = Split-Path -Leaf $workspace
                    Wait-For 'result destination available' {
                        try {
                            No-Reparse $destination
                            if (-not (Test-Path -LiteralPath $destination -PathType Container)) { return $false }
                            Copy-Item -LiteralPath $reportPath -Destination (Join-Path $destination "$prefix-report.json") -Force
                            Copy-Item -LiteralPath $CheckpointPath -Destination (Join-Path $destination "$prefix-checkpoint.json") -Force
                            Get-ChildItem -LiteralPath $evidence -File | ForEach-Object {
                                Copy-Item -LiteralPath $_.FullName -Destination (Join-Path $destination "$prefix-evidence-$($_.Name)") -Force
                            }
                            return $true
                        } catch { return $false }
                    } 60
                    Write-Host "Copied reboot evidence to $destination (file prefix $prefix)"
                } catch { Write-Warning "Could not copy results; local evidence is retained at $workspace. $($_.Exception.Message)" }
            }
        }
    }
}

if ($prepared) {
    if ($rebootRequested) {
        Write-Host 'Explicit -Reboot requested. Restarting only the disposable SC-VALIDATION VM now.'
        Restart-Computer -Force
    }
} elseif (-not $verified -or $cleanupErrors.Count -gt 0) {
    throw "Reboot lifecycle validation failed. $failure"
}
