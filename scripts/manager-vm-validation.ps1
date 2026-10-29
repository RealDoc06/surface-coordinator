<#
.SYNOPSIS
  Prepares and controls manager updater fixtures inside SC-VALIDATION only.
.DESCRIPTION
  Run Setup through the disposable VM's SYSTEM queue runner after copying this
  script to \\host.lan\Data\artifacts\manager\manager-vm-validation.ps1 and both
  packaged fixtures to artifacts\manager\0.2.1 and artifacts\manager\0.2.2.

  Setup copies fixtures locally, serves them on 127.0.0.1:18789, installs 0.2.1
  silently through an interactive Validator task, then opens the isolated app.
  The production app identity is never installed. Validator must be logged in.

  Manifest switches the fixture to valid, tamper, wrongversion or invalidinstaller without a
  server restart. Reinstall restores a chosen fixture without deleting app data.
  Status records the actual installed version and copies logs
  to artifacts\manager\evidence. No keys, production services or Wi-Fi are used.
.EXAMPLE
  & '\\host.lan\Data\artifacts\manager\manager-vm-validation.ps1' -Action Setup
.EXAMPLE
  & 'C:\Validation\manager\manager-vm-validation.ps1' -Action Manifest -Scenario tamper
.EXAMPLE
  & 'C:\Validation\manager\manager-vm-validation.ps1' -Action Status
#>
[CmdletBinding()]
param(
    [ValidateSet('Setup', 'Serve', 'Install', 'Launch', 'Reinstall', 'Manifest', 'Status')][string]$Action = 'Setup',
    [ValidateSet('valid', 'tamper', 'wrongversion', 'invalidinstaller')][string]$Scenario = 'valid',
    [ValidateSet('0.2.1', '0.2.2')][string]$InstallVersion = '0.2.1'
)

$ErrorActionPreference = 'Stop'
if ($env:COMPUTERNAME -ne 'SC-VALIDATION') {
    throw "This helper may run only inside disposable guest SC-VALIDATION, not $env:COMPUTERNAME."
}
$work = 'C:\Validation\manager'
$share = '\\host.lan\Data\artifacts\manager'
$localScript = Join-Path $work 'manager-vm-validation.ps1'
$serverTask = 'SurfaceCoordinatorManagerValidationServer'
$interactiveTask = 'SurfaceCoordinatorManagerValidationInteractive'
$productName = 'Surface Coordinator Manager Update Test'
$port = 18789
$base = "http://127.0.0.1:$port"
$utf8 = [Text.UTF8Encoding]::new($false)
$validator = Get-LocalUser -Name Validator
$profile = Get-CimInstance Win32_UserProfile | Where-Object SID -eq $validator.SID.Value
if (-not $profile -or -not $profile.LocalPath) { throw 'Validator must log in once before this fixture can run.' }
# Tauri's currentUser NSIS template uses $LOCALAPPDATA\${PRODUCTNAME}.
$installedExe = Join-Path $profile.LocalPath "AppData\Local\$productName\manager.exe"

function Write-Json([string]$Path, $Value) {
    $temporaryPath = "$Path.pending"
    [IO.File]::WriteAllText($temporaryPath, ($Value | ConvertTo-Json -Depth 10) + "`n", $utf8)
    Move-Item -LiteralPath $temporaryPath -Destination $Path -Force
}

function Installer-Name([string]$Version) { "surface-coordinator-manager-$Version-windows-x86_64-setup.exe" }

function Select-Manifest([string]$Name) {
    $manifest = Join-Path $work "manifests\$Name.json"
    if (-not (Test-Path -LiteralPath $manifest -PathType Leaf)) { throw "Fixture manifest missing: $manifest" }
    $temporaryManifest = Join-Path $work 'active-manifest.pending'
    Copy-Item -LiteralPath $manifest -Destination $temporaryManifest -Force
    Move-Item -LiteralPath $temporaryManifest -Destination (Join-Path $work 'active-manifest.json') -Force
    Write-Json (Join-Path $work 'scenario.json') @{ scenario = $Name; utc = [DateTime]::UtcNow.ToString('o') }
    Write-Host "Updater fixture now serves $Name at $base/latest.json"
}

function Run-Interactive([string]$Operation) {
    $taskAction = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoLogo -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File $localScript -Action $Operation -InstallVersion $InstallVersion"
    $principal = New-ScheduledTaskPrincipal -UserId $validator.SID.Value -LogonType Interactive -RunLevel Limited
    $executionLimit = if ($Operation -eq 'Launch') { [TimeSpan]::Zero } else { New-TimeSpan -Minutes 5 }
    $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit $executionLimit -MultipleInstances IgnoreNew
    Register-ScheduledTask -TaskName $interactiveTask -Action $taskAction -Principal $principal -Settings $settings -Force | Out-Null
    $statusPath = Join-Path $work "$($Operation.ToLowerInvariant())-status.json"
    if (Test-Path -LiteralPath $statusPath) { Remove-Item -LiteralPath $statusPath -Force }
    Start-ScheduledTask -TaskName $interactiveTask
    $deadline = [DateTime]::UtcNow.AddMinutes(4)
    do {
        if (Test-Path -LiteralPath $statusPath -PathType Leaf) {
            $status = Get-Content -LiteralPath $statusPath -Raw | ConvertFrom-Json
            if (-not $status.success) { throw "$Operation failed in Validator's session: $($status.error)" }
            $status | ConvertTo-Json -Depth 5
            # The interactive task writes status just before exiting. Wait for
            # that exit before replacing its action with the next operation.
            while ($Operation -eq 'Install' -and (Get-ScheduledTask -TaskName $interactiveTask).State -eq 'Running' -and [DateTime]::UtcNow -lt $deadline) {
                Start-Sleep -Milliseconds 200
            }
            return
        }
        Start-Sleep -Milliseconds 500
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for $Operation. Validator must be logged in; inspect the $interactiveTask scheduled task."
}

switch ($Action) {
    'Setup' {
        if ([Security.Principal.WindowsIdentity]::GetCurrent().User.Value -ne 'S-1-5-18') {
            throw 'Run Setup through the SYSTEM queue runner.'
        }
        $loggedIn = (Get-CimInstance Win32_ComputerSystem).UserName
        if ($loggedIn -ne "$env:COMPUTERNAME\Validator") { throw "Log Validator into the guest console first. Current interactive user: $loggedIn" }
        New-Item -ItemType Directory -Path (Join-Path $work 'manifests') -Force | Out-Null
        $acl = Get-Acl -LiteralPath $work
        $rule = [Security.AccessControl.FileSystemAccessRule]::new($validator.SID, 'Modify', 'ContainerInherit,ObjectInherit', 'None', 'Allow')
        $acl.SetAccessRule($rule)
        Set-Acl -LiteralPath $work -AclObject $acl
        if ([IO.Path]::GetFullPath($PSCommandPath) -ne $localScript) { Copy-Item -LiteralPath $PSCommandPath -Destination $localScript -Force }
        foreach ($version in @('0.2.1', '0.2.2')) {
            $source = Join-Path $share $version
            $destination = Join-Path $work $version
            New-Item -ItemType Directory -Path $destination -Force | Out-Null
            $installerName = Installer-Name $version
            foreach ($name in @($installerName, "$installerName.sig", 'latest.json', 'manager-SHA256SUMS.txt')) {
                Copy-Item -LiteralPath (Join-Path $source $name) -Destination (Join-Path $destination $name) -Force
            }
            foreach ($line in Get-Content -LiteralPath (Join-Path $destination 'manager-SHA256SUMS.txt')) {
                if ($line -notmatch '^([0-9a-fA-F]{64})  ([^\\/]+)$') { throw 'Malformed fixture checksum entry.' }
                $expectedHash = $Matches[1]
                $name = $Matches[2]
                $actualHash = (Get-FileHash -LiteralPath (Join-Path $destination $name) -Algorithm SHA256).Hash
                if ($actualHash -ne $expectedHash) { throw "Fixture checksum mismatch: $version/$name" }
            }
            $manifest = Get-Content -LiteralPath (Join-Path $destination 'latest.json') -Raw | ConvertFrom-Json
            $signature = [IO.File]::ReadAllText((Join-Path $destination "$installerName.sig")).Trim()
            if ($manifest.version -ne $version -or $manifest.platforms.'windows-x86_64'.signature -ne $signature) {
                throw "Fixture version or signature manifest mismatch: $version"
            }
            $installerInfo = (Get-Item -LiteralPath (Join-Path $destination $installerName)).VersionInfo
            if ($installerInfo.ProductName -ne $productName -or $installerInfo.ProductVersion -ne $version) {
                throw "Refusing to install a fixture without the isolated product identity/version: $version"
            }
        }
        $newInstaller = Installer-Name '0.2.2'
        $newManifestPath = Join-Path $work '0.2.2\latest.json'
        $valid = Get-Content -LiteralPath $newManifestPath -Raw | ConvertFrom-Json
        $valid.platforms.'windows-x86_64'.url = "$base/$newInstaller"
        Write-Json (Join-Path $work 'manifests\valid.json') $valid
        $tamper = Get-Content -LiteralPath (Join-Path $work 'manifests\valid.json') -Raw | ConvertFrom-Json
        $tamper.platforms.'windows-x86_64'.url = "$base/tampered-$newInstaller"
        Write-Json (Join-Path $work 'manifests\tamper.json') $tamper
        $tamperedFile = Join-Path $work "tampered-$newInstaller"
        Copy-Item -LiteralPath (Join-Path $work "0.2.2\$newInstaller") -Destination $tamperedFile -Force
        $stream = [IO.File]::Open($tamperedFile, [IO.FileMode]::Append, [IO.FileAccess]::Write)
        try { $stream.WriteByte(0x21) } finally { $stream.Dispose() }
        $wrongVersion = Get-Content -LiteralPath (Join-Path $work 'manifests\valid.json') -Raw | ConvertFrom-Json
        $wrongVersion.version = '0.2.3'
        Write-Json (Join-Path $work 'manifests\wrongversion.json') $wrongVersion
        $invalidSource = Join-Path $share 'negative-fixtures\invalid-signed-setup.exe'
        if (Test-Path -LiteralPath $invalidSource -PathType Leaf) {
            Copy-Item -LiteralPath $invalidSource -Destination (Join-Path $work 'invalid-signed-setup.exe') -Force
            Copy-Item -LiteralPath "$invalidSource.sig" -Destination (Join-Path $work 'invalid-signed-setup.exe.sig') -Force
            $invalidInstaller = Get-Content -LiteralPath (Join-Path $work 'manifests\valid.json') -Raw | ConvertFrom-Json
            $invalidInstaller.platforms.'windows-x86_64'.url = "$base/invalid-signed-setup.exe"
            $invalidInstaller.platforms.'windows-x86_64'.signature = [IO.File]::ReadAllText((Join-Path $work 'invalid-signed-setup.exe.sig')).Trim()
            Write-Json (Join-Path $work 'manifests\invalidinstaller.json') $invalidInstaller
        }
        Select-Manifest $Scenario

        $existingTask = Get-ScheduledTask -TaskName $serverTask -ErrorAction SilentlyContinue
        if ($existingTask) { Stop-ScheduledTask -TaskName $serverTask }
        $taskAction = New-ScheduledTaskAction -Execute 'powershell.exe' -Argument "-NoLogo -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File $localScript -Action Serve"
        $principal = New-ScheduledTaskPrincipal -UserId SYSTEM -LogonType ServiceAccount -RunLevel Highest
        $settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit ([TimeSpan]::Zero) -MultipleInstances IgnoreNew
        Register-ScheduledTask -TaskName $serverTask -Action $taskAction -Principal $principal -Settings $settings -Force | Out-Null
        Start-ScheduledTask -TaskName $serverTask
        $ready = $false
        $deadline = [DateTime]::UtcNow.AddSeconds(20)
        do {
            try { $ready = (Invoke-RestMethod "$base/status" -TimeoutSec 2).hostname -eq 'SC-VALIDATION' } catch {}
            if (-not $ready) { Start-Sleep -Milliseconds 300 }
        } while (-not $ready -and [DateTime]::UtcNow -lt $deadline)
        if (-not $ready) { throw "Updater fixture server did not start. Inspect $work\server-error.txt." }
        # These tests use loopback only. A program-specific block keeps other
        # networks closed and avoids a first-listen Windows Security prompt.
        $firewallRule = 'SurfaceCoordinatorManagerValidationBlock'
        if (-not (Get-NetFirewallRule -Name $firewallRule -ErrorAction SilentlyContinue)) {
            New-NetFirewallRule -Name $firewallRule -DisplayName 'Disposable Manager fixture: block external inbound' -Direction Inbound -Action Block -Profile Any -Program $installedExe | Out-Null
        }
        Run-Interactive 'Install'
        Run-Interactive 'Launch'
        & $localScript -Action Status
    }
    'Serve' {
        $listener = [Net.HttpListener]::new()
        $listener.Prefixes.Add("$base/")
        $newInstaller = Installer-Name '0.2.2'
        $routes = @{
            '/latest.json' = (Join-Path $work 'active-manifest.json')
            "/$newInstaller" = (Join-Path $work "0.2.2\$newInstaller")
            "/tampered-$newInstaller" = (Join-Path $work "tampered-$newInstaller")
        }
        $invalidInstallerFile = Join-Path $work 'invalid-signed-setup.exe'
        if (Test-Path -LiteralPath $invalidInstallerFile -PathType Leaf) { $routes['/invalid-signed-setup.exe'] = $invalidInstallerFile }
        try {
            $listener.Start()
            Write-Json (Join-Path $work 'server-status.json') @{ hostname = $env:COMPUTERNAME; pid = $PID; endpoint = "$base/latest.json"; started = [DateTime]::UtcNow.ToString('o') }
            while ($listener.IsListening) {
                $context = $listener.GetContext()
                $code = 200
                $bytes = @()
                try {
                    $path = $context.Request.Url.AbsolutePath
                    if ($context.Request.HttpMethod -notin @('GET', 'HEAD')) { $code = 405 }
                    elseif ($path -eq '/status') {
                        $bytes = $utf8.GetBytes((@{ hostname = $env:COMPUTERNAME; scenario = (Get-Content -LiteralPath (Join-Path $work 'scenario.json') -Raw | ConvertFrom-Json).scenario } | ConvertTo-Json))
                    } elseif ($routes.ContainsKey($path)) { $bytes = [IO.File]::ReadAllBytes($routes[$path]) }
                    else { $code = 404 }
                    $context.Response.StatusCode = $code
                    $context.Response.ContentType = if ($path -in @('/latest.json', '/status')) { 'application/json' } else { 'application/octet-stream' }
                    $context.Response.Headers.Add('Cache-Control', 'no-store')
                    $context.Response.ContentLength64 = $bytes.Length
                    if ($context.Request.HttpMethod -ne 'HEAD' -and $bytes.Length) { $context.Response.OutputStream.Write($bytes, 0, $bytes.Length) }
                } catch {
                    $code = 500
                    [IO.File]::AppendAllText((Join-Path $work 'server-error.txt'), "$([DateTime]::UtcNow.ToString('o')) $($_.Exception.Message)`n", $utf8)
                } finally {
                    $context.Response.Close()
                    $entry = @{ utc = [DateTime]::UtcNow.ToString('o'); path = $context.Request.Url.AbsolutePath; status = $code; bytes = $bytes.Length } | ConvertTo-Json -Compress
                    [IO.File]::AppendAllText((Join-Path $work 'requests.jsonl'), "$entry`n", $utf8)
                }
            }
        } catch {
            [IO.File]::AppendAllText((Join-Path $work 'server-error.txt'), "$([DateTime]::UtcNow.ToString('o')) $($_.Exception.Message)`n", $utf8)
            throw
        } finally { $listener.Close() }
    }
    { $_ -in 'Install', 'Launch' } {
        if ([Security.Principal.WindowsIdentity]::GetCurrent().User.Value -ne $validator.SID.Value) {
            throw "$Action must run as interactive Validator, using the task created by Setup."
        }
        $status = @{ action = $Action; identity = [Security.Principal.WindowsIdentity]::GetCurrent().Name; started = [DateTime]::UtcNow.ToString('o'); success = $false; error = $null }
        try {
            if ($Action -eq 'Install') {
                $installer = Join-Path $work "$InstallVersion\$(Installer-Name $InstallVersion)"
                $process = Start-Process -FilePath $installer -ArgumentList '/S' -WindowStyle Hidden -PassThru
                if (-not $process.WaitForExit(180000)) { throw 'The silent fixture installer did not finish within three minutes.' }
                $status.exit_code = $process.ExitCode
                if ($process.ExitCode -ne 0) { throw "The fixture installer returned $($process.ExitCode)." }
                if (-not (Test-Path -LiteralPath $installedExe -PathType Leaf)) { throw 'The current-user fixture executable was not installed.' }
                $status.installed_version = (Get-Item -LiteralPath $installedExe).VersionInfo.ProductVersion
                if ($status.installed_version -ne $InstallVersion) { throw "Expected installed version $InstallVersion, got $($status.installed_version)." }
            } else {
                $running = @(Get-CimInstance Win32_Process -Filter "Name='manager.exe'" | Where-Object ExecutablePath -eq $installedExe)
                if ($running.Count) { $status.pid = $running[0].ProcessId }
                else {
                    # This is the visible UI under test; helper processes stay hidden.
                    $process = Start-Process -FilePath $installedExe -WindowStyle Normal -PassThru
                    Start-Sleep -Seconds 2
                    if ($process.HasExited) { throw "The fixture manager exited with $($process.ExitCode)." }
                    $status.pid = $process.Id
                }
            }
            $status.installed_exe = $installedExe
            $status.success = $true
        } catch { $status.error = $_.Exception.Message; throw }
        finally {
            $status.completed = [DateTime]::UtcNow.ToString('o')
            Write-Json (Join-Path $work "$($Action.ToLowerInvariant())-status.json") $status
        }
    }
    'Manifest' { Select-Manifest $Scenario }
    'Reinstall' {
        if ([Security.Principal.WindowsIdentity]::GetCurrent().User.Value -ne 'S-1-5-18') {
            throw 'Run Reinstall through the SYSTEM queue runner.'
        }
        Run-Interactive 'Install'
        Run-Interactive 'Launch'
        & $localScript -Action Status
    }
    'Status' {
        $version = if (Test-Path -LiteralPath $installedExe -PathType Leaf) { (Get-Item -LiteralPath $installedExe).VersionInfo.ProductVersion } else { $null }
        $processes = @(Get-CimInstance Win32_Process -Filter "Name='manager.exe'" | Where-Object ExecutablePath -eq $installedExe | Select-Object ProcessId, ParentProcessId, SessionId)
        $status = @{ hostname = $env:COMPUTERNAME; utc = [DateTime]::UtcNow.ToString('o'); installed_exe = $installedExe; installed_version = $version; processes = $processes; scenario = (Get-Content -LiteralPath (Join-Path $work 'scenario.json') -Raw | ConvertFrom-Json).scenario }
        Write-Json (Join-Path $work 'status.json') $status
        $evidence = Join-Path $share 'evidence'
        New-Item -ItemType Directory -Path $evidence -Force | Out-Null
        foreach ($name in @('status.json', 'install-status.json', 'launch-status.json', 'server-status.json', 'server-error.txt', 'scenario.json', 'requests.jsonl')) {
            $path = Join-Path $work $name
            if (Test-Path -LiteralPath $path -PathType Leaf) { Copy-Item -LiteralPath $path -Destination (Join-Path $evidence $name) -Force }
        }
        $status | ConvertTo-Json -Depth 5
    }
}
