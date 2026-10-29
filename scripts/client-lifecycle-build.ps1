#requires -Version 5.1
<#
.SYNOPSIS
  Build signed, disposable client lifecycle fixtures from a source snapshot.
.DESCRIPTION
  Copies tracked and nonignored untracked Rust workspace files, including local
  edits, into a new demo-data/client-lifecycle-fixtures directory. All version and
  public-key changes occur in that copy. Uses target/client-lifecycle-fixtures,
  never target/release. A cache lock prevents concurrent fixture builds colliding.

  The random private signing seed exists only in process memory and briefly in
  the release-tool child environment. It is never written or printed. These
  high-version, disposable-key executables are TEST FIXTURES, not release builds.
  No services, coordinator servers, Wi-Fi settings, or production data are touched.
.EXAMPLE
  .\scripts\client-lifecycle-build.ps1 -ReleaseToolExe .\target\release\release-tool.exe
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][string]$ReleaseToolExe,
    [string]$FixtureDirectory,
    [ValidatePattern('^\d+\.\d+\.\d+$')][string]$BaselineVersion = '90.0.1',
    [ValidatePattern('^\d+\.\d+\.\d+$')][string]$CandidateVersion = '90.0.2',
    [ValidateRange(1, 120)][int]$BuildTimeoutMinutes = 30
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$root = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot)).TrimEnd('\')
$fixtureRoot = Join-Path $root 'demo-data\client-lifecycle-fixtures'
$targetDir = Join-Path $root 'target\client-lifecycle-fixtures'
$targetTriple = 'x86_64-pc-windows-msvc'
$updateTarget = 'windows-x86_64'
$cacheLock = $null
$privateKey = $null
$seed = $null
$rng = $null
$previousSigningKey = [Environment]::GetEnvironmentVariable('SC_UPDATE_SIGNING_KEY', 'Process')
$previousTargetDir = [Environment]::GetEnvironmentVariable('CARGO_TARGET_DIR', 'Process')
$previousRustFlags = @{}
$startedAt = [DateTime]::UtcNow

function Write-Utf8([string]$Path, [string]$Text) {
    [IO.File]::WriteAllText($Path, $Text, [Text.UTF8Encoding]::new($false))
}

function Assert-NoReparsePoint([string]$Path) {
    $current = [IO.Path]::GetFullPath($Path)
    while ($current) {
        if (Test-Path -LiteralPath $current) {
            if (((Get-Item -LiteralPath $current -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
                throw "Refusing a reparse point in fixture paths: $current"
            }
        }
        $current = Split-Path -Parent $current
    }
}

function Native-Argument([string]$Value) {
    '"' + [regex]::Replace([regex]::Replace($Value, '(\\*)"', '$1$1\"'), '(\\+)$', '$1$1') + '"'
}

function Invoke-Logged([string]$Exe, [string[]]$Arguments, [string]$Name) {
    $quoted = @($Arguments | ForEach-Object { Native-Argument $_ })
    $stdout = Join-Path $logs "$Name.stdout.log"
    $stderr = Join-Path $logs "$Name.stderr.log"
    Write-Host "$Name running on $env:COMPUTERNAME; log: $stderr"
    $process = Start-Process -FilePath $Exe -ArgumentList $quoted -WorkingDirectory $source -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    # Retain the native handle before a short-lived child can exit (PowerShell 5).
    $processHandle = $process.Handle
    try {
        $deadline = [DateTime]::UtcNow.AddMinutes($BuildTimeoutMinutes)
        while (-not $process.WaitForExit(1000)) {
            if ([DateTime]::UtcNow -ge $deadline) {
                $process.Kill()
                $process.WaitForExit()
                throw "$Name exceeded $BuildTimeoutMinutes minutes; see $stderr"
            }
        }
        $process.WaitForExit()
        $exitCode = $process.ExitCode
        if ($exitCode -ne 0) { throw "$Name failed with exit code $exitCode; see $stderr" }
        return Get-Content -LiteralPath $stdout -Raw
    } finally { $process.Dispose() }
}

function Set-FixtureVersion([string]$Version) {
    $manifestPath = Join-Path $source 'Cargo.toml'
    $manifest = [IO.File]::ReadAllText($manifestPath)
    $versionPattern = [regex]::new('(?m)^(version\s*=\s*")[^"]+("\s*)$')
    if ($versionPattern.Matches($manifest).Count -ne 1) { throw 'Expected exactly one workspace version in copied Cargo.toml' }
    Write-Utf8 $manifestPath ($versionPattern.Replace($manifest, { param($match) $match.Groups[1].Value + $Version + $match.Groups[2].Value }))

    # Change only local workspace package versions, keeping all registry pins and
    # checksums intact so both fixture builds still run with --locked.
    $lockPath = Join-Path $source 'Cargo.lock'
    $lockText = [IO.File]::ReadAllText($lockPath)
    foreach ($package in $workspacePackages) {
        $pattern = [regex]::new('(?m)(^name = "' + [regex]::Escape($package) + '"\r?\nversion = ")[^"]+(")')
        if ($pattern.Matches($lockText).Count -ne 1) { throw "Expected one copied lockfile entry for local package $package" }
        $lockText = $pattern.Replace($lockText, { param($match) $match.Groups[1].Value + $Version + $match.Groups[2].Value })
    }
    Write-Utf8 $lockPath $lockText
}

try {
    if ([Environment]::OSVersion.Platform -ne [PlatformID]::Win32NT) { throw 'Build these Windows service fixtures on Windows' }
    if ([version]$CandidateVersion -le [version]$BaselineVersion) { throw 'CandidateVersion must be newer than BaselineVersion' }
    if (-not (Test-Path -LiteralPath $ReleaseToolExe -PathType Leaf)) { throw "release-tool.exe is missing: $ReleaseToolExe" }
    $ReleaseToolExe = (Resolve-Path -LiteralPath $ReleaseToolExe).ProviderPath
    $cargoExe = (Get-Command cargo.exe -ErrorAction Stop).Source
    if ([string]::IsNullOrWhiteSpace($FixtureDirectory)) {
        $FixtureDirectory = Join-Path $fixtureRoot ((Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
    }
    $fixture = [IO.Path]::GetFullPath($FixtureDirectory).TrimEnd('\')
    if (-not $fixture.StartsWith($fixtureRoot + '\', [StringComparison]::OrdinalIgnoreCase)) { throw "FixtureDirectory must be a new directory beneath $fixtureRoot" }
    if (Test-Path -LiteralPath $fixture) { throw "FixtureDirectory already exists: $fixture" }
    Assert-NoReparsePoint $fixture
    Assert-NoReparsePoint $targetDir
    $gitRoot = (& git -C $root rev-parse --show-toplevel | Out-String).Trim()
    if ($LASTEXITCODE -ne 0 -or [IO.Path]::GetFullPath($gitRoot).TrimEnd('\') -ine $root) { throw 'The script must belong to the expected source repository' }
    & git -C $root check-ignore --quiet -- $fixture
    if ($LASTEXITCODE -ne 0) { throw 'Fixture output must be ignored by Git' }

    Write-Host "Execution host: $env:COMPUTERNAME. Source snapshot: $fixture. Fixture cache: $targetDir."
    New-Item -ItemType Directory -Path $fixture -Force | Out-Null
    New-Item -ItemType Directory -Path $targetDir -Force | Out-Null
    try { $cacheLock = [IO.File]::Open((Join-Path $targetDir 'fixture-build.lock'), [IO.FileMode]::OpenOrCreate, [IO.FileAccess]::ReadWrite, [IO.FileShare]::None) }
    catch { throw 'Another fixture builder owns the isolated target cache; wait for it to finish' }
    $source = Join-Path $fixture 'source'
    $artifacts = Join-Path $fixture 'artifacts'
    $logs = Join-Path $fixture 'logs'
    foreach ($directory in @($source, $artifacts, $logs, (Join-Path $artifacts 'baseline'), (Join-Path $artifacts 'candidate'))) {
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
    }
    [Environment]::SetEnvironmentVariable('SC_UPDATE_SIGNING_KEY', $null, 'Process')
    $env:CARGO_TARGET_DIR = $targetDir
    # The copied repository config controls static CRT linkage. Inherited
    # per-process flags must not silently override the standalone build policy.
    foreach ($name in @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS')) {
        $previousRustFlags[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        Remove-Item -LiteralPath "Env:\$name" -ErrorAction SilentlyContinue
    }

    $relativeFiles = @(& git -C $root -c core.quotepath=false ls-files --cached --others --exclude-standard -- crates Cargo.toml Cargo.lock .cargo rust-toolchain rust-toolchain.toml | Sort-Object -Unique)
    if ($LASTEXITCODE -ne 0) { throw 'Cannot list the working source snapshot' }
    $snapshotFiles = [System.Collections.Generic.List[object]]::new()
    foreach ($relative in $relativeFiles) {
        $original = [IO.Path]::GetFullPath((Join-Path $root $relative))
        if (-not $original.StartsWith($root + '\', [StringComparison]::OrdinalIgnoreCase)) { throw "Source path escaped the repository: $relative" }
        if (-not (Test-Path -LiteralPath $original -PathType Leaf)) { continue }
        Assert-NoReparsePoint $original
        $destination = Join-Path $source $relative
        New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
        Copy-Item -LiteralPath $original -Destination $destination
        $snapshotFiles.Add([ordered]@{ path = $relative; sha256 = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash })
    }
    if (-not (Test-Path -LiteralPath (Join-Path $source 'Cargo.lock'))) { throw 'Snapshot must include the existing Cargo.lock' }
    Write-Utf8 (Join-Path $fixture 'source-snapshot.json') (ConvertTo-Json -InputObject $snapshotFiles.ToArray() -Depth 4)

    $uiDist = Join-Path $root 'ui\dist'
    $uiCopied = Test-Path -LiteralPath (Join-Path $uiDist 'index.html') -PathType Leaf
    if ($uiCopied) {
        Assert-NoReparsePoint $uiDist
        foreach ($item in Get-ChildItem -LiteralPath $uiDist -Recurse -Force) {
            if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) { throw 'Built UI contains a reparse point' }
            if ($item.PSIsContainer) { continue }
            $relative = $item.FullName.Substring($uiDist.Length).TrimStart('\')
            $destination = Join-Path (Join-Path $source 'ui\dist') $relative
            New-Item -ItemType Directory -Path (Split-Path -Parent $destination) -Force | Out-Null
            Copy-Item -LiteralPath $item.FullName -Destination $destination
        }
    }
    $workspacePackages = @(Get-ChildItem -LiteralPath (Join-Path $source 'crates') -Directory | ForEach-Object {
        $manifestPath = Join-Path $_.FullName 'Cargo.toml'
        if (Test-Path -LiteralPath $manifestPath) {
            $manifest = [IO.File]::ReadAllText($manifestPath)
            if ($manifest -match '(?m)^version\.workspace\s*=\s*true\s*$' -and $manifest -match '(?m)^name\s*=\s*"([^"]+)"') { $Matches[1] }
        }
    })
    if ($workspacePackages.Count -eq 0) { throw 'No workspace packages found in copied source' }

    $seed = [byte[]]::new(32)
    $rng = [Security.Cryptography.RandomNumberGenerator]::Create()
    $rng.GetBytes($seed)
    $privateKey = [Convert]::ToBase64String($seed)
    [Array]::Clear($seed, 0, $seed.Length)
    try {
        $env:SC_UPDATE_SIGNING_KEY = $privateKey
        $publicKey = (Invoke-Logged $ReleaseToolExe @('public-key') 'public-key').Trim()
    } finally { [Environment]::SetEnvironmentVariable('SC_UPDATE_SIGNING_KEY', $null, 'Process') }
    if ([Convert]::FromBase64String($publicKey).Length -ne 32) { throw 'release-tool returned an invalid public key' }
    $keyDir = Join-Path $source 'release-keys'
    New-Item -ItemType Directory -Path $keyDir | Out-Null
    $publicKeyPath = Join-Path $keyDir 'client-update.pub'
    Write-Utf8 $publicKeyPath ($publicKey + "`n")
    Copy-Item -LiteralPath $publicKeyPath -Destination (Join-Path $artifacts 'test-update.pub')
    Copy-Item -LiteralPath $ReleaseToolExe -Destination (Join-Path $artifacts 'release-tool.exe')

    Set-FixtureVersion $BaselineVersion
    Invoke-Logged $cargoExe @('build', '--locked', '--release', '--target', $targetTriple, '--target-dir', $targetDir, '-j', '2', '-p', 'coordinator', '-p', 'client') 'build-baseline' | Out-Null
    $built = Join-Path $targetDir "$targetTriple\release"
    Copy-Item -LiteralPath (Join-Path $built 'client.exe') -Destination (Join-Path $artifacts 'baseline\client.exe')
    Copy-Item -LiteralPath (Join-Path $built 'coordinator.exe') -Destination (Join-Path $artifacts 'coordinator.exe')

    Set-FixtureVersion $CandidateVersion
    Invoke-Logged $cargoExe @('build', '--locked', '--release', '--target', $targetTriple, '--target-dir', $targetDir, '-j', '2', '-p', 'client') 'build-candidate' | Out-Null
    $candidatePath = Join-Path $artifacts 'candidate\client.exe'
    Copy-Item -LiteralPath (Join-Path $built 'client.exe') -Destination $candidatePath
    $signaturePath = $candidatePath + '.sig'
    try {
        $env:SC_UPDATE_SIGNING_KEY = $privateKey
        Invoke-Logged $ReleaseToolExe @('sign', '--file', $candidatePath, '--version', $CandidateVersion, '--target', $updateTarget, '--out', $signaturePath) 'sign-candidate' | Out-Null
    } finally { [Environment]::SetEnvironmentVariable('SC_UPDATE_SIGNING_KEY', $null, 'Process') }
    Invoke-Logged $ReleaseToolExe @('verify', '--file', $candidatePath, '--version', $CandidateVersion, '--target', $updateTarget, '--signature', $signaturePath, '--public-key', $publicKeyPath) 'verify-candidate' | Out-Null

    $baselinePath = Join-Path $artifacts 'baseline\client.exe'
    $reportedBaseline = (Invoke-Logged $baselinePath @('--version') 'baseline-version').Trim()
    $reportedCandidate = (Invoke-Logged $candidatePath @('--version') 'candidate-version').Trim()
    if ($reportedBaseline -ne "client $BaselineVersion" -or $reportedCandidate -ne "client $CandidateVersion") { throw 'Built fixture versions do not match the requested versions' }
    $paths = [ordered]@{
        CoordinatorExe = Join-Path $artifacts 'coordinator.exe'
        BaselineClientExe = $baselinePath
        CandidateClientExe = $candidatePath
        CandidateSignature = $signaturePath
        UpdatePublicKey = Join-Path $artifacts 'test-update.pub'
        ReleaseToolExe = Join-Path $artifacts 'release-tool.exe'
    }
    $artifactRecords = @($paths.Keys | ForEach-Object {
        [ordered]@{ role = $_; path = $paths[$_]; relative_path = $paths[$_].Substring($fixture.Length + 1); sha256 = (Get-FileHash -LiteralPath $paths[$_] -Algorithm SHA256).Hash }
    })
    $manifest = [ordered]@{
        fixture_only = $true; do_not_distribute = 'Disposable signing key and synthetic versions. Never install these over a real client.'
        build_host = $env:COMPUTERNAME; source_repository = $root; copied_source = $source; target_cache = $targetDir
        started_at = $startedAt.ToString('o'); completed_at = [DateTime]::UtcNow.ToString('o')
        baseline_version = $BaselineVersion; candidate_version = $CandidateVersion; target = $updateTarget; copied_built_ui = [bool]$uiCopied
        signing_seed_persisted = $false; validation_arguments = $paths; artifacts = $artifactRecords
    }
    $manifestPath = Join-Path $fixture 'fixtures.json'
    Write-Utf8 $manifestPath (ConvertTo-Json -InputObject $manifest -Depth 8)
    Write-Host "TEST FIXTURES ready: $manifestPath"
    Write-Host 'Copy artifacts to an elevated disposable Windows target, then run client-lifecycle-validate.ps1 with a new workspace.'
} finally {
    [Environment]::SetEnvironmentVariable('SC_UPDATE_SIGNING_KEY', $previousSigningKey, 'Process')
    [Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', $previousTargetDir, 'Process')
    foreach ($name in $previousRustFlags.Keys) {
        if ($null -eq $previousRustFlags[$name]) { Remove-Item -LiteralPath "Env:\$name" -ErrorAction SilentlyContinue }
        else { [Environment]::SetEnvironmentVariable($name, $previousRustFlags[$name], 'Process') }
    }
    if ($null -ne $seed) { [Array]::Clear($seed, 0, $seed.Length) }
    $privateKey = $null
    if ($null -ne $rng) { $rng.Dispose() }
    if ($null -ne $cacheLock) { $cacheLock.Dispose() }
}
