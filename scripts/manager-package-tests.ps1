<#
.SYNOPSIS
  Exercises packaging failure boundaries without building, signing or installing.
.DESCRIPTION
  Cargo, Rust and Tauri are replaced with local test functions. Fixtures live
  under a fresh temporary directory; real keys and release artifacts are unused.
  Cryptographic verification is checked separately with signed artifacts and
  the verify-update example.
#>
$ErrorActionPreference = 'Stop'
$testRoot = Join-Path ([IO.Path]::GetTempPath()) ('sc-manager-package-tests-' + [guid]::NewGuid().ToString('N'))
$testScriptDirectory = Join-Path $testRoot 'scripts'
$testManager = Join-Path $testRoot 'crates\manager'
$testTarget = Join-Path $testRoot 'custom target'
$testOutput = Join-Path $testRoot 'release'
$testVerifier = Join-Path $testRoot 'verify-update.ps1'
$testKey = Join-Path $testRoot 'fake private.key'
$testPublicKey = Join-Path $testManager 'updater.pub'
$utf8 = [Text.UTF8Encoding]::new($false)
$saved = @{}
foreach ($name in @('TAURI_SIGNING_PRIVATE_KEY', 'TAURI_SIGNING_PRIVATE_KEY_PASSWORD', 'SC_UPDATER_PUB_FILE', 'CARGO_TARGET_DIR', 'CARGO_BUILD_TARGET', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS', 'SC_PACKAGE_TEST_REJECT_SIGNATURE')) {
    $saved[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$testState = @{}
$testState.Mode = 'success'
$testState.BuildConfigPath = $null
$testState.BuildConfig = $null
$testState.VerifierBuilds = 0
$testState.TauriBuilds = 0
$testState.Checks = 0

function Assert([bool]$Condition, [string]$Description) {
    if (-not $Condition) { throw "FAIL: $Description" }
    $testState.Checks++
    Write-Host "PASS: $Description"
}

function Assert-Rejected([hashtable]$Options, [string]$Expected) {
    $failure = $null
    try { & $testScript @common @Options | Out-Null } catch { $failure = $_.Exception.Message }
    Assert ($failure -like "*$Expected*") "Rejects $Expected"
}

function rustc {
    $global:LASTEXITCODE = 0
    'rustc test fixture'
    'host: x86_64-pc-windows-msvc'
}

function npm {
    if ($env:TAURI_SIGNING_PRIVATE_KEY) { throw 'UI checks inherited the signing secret.' }
    if (($args -join ' ') -notin @('ci', 'run typecheck', 'run build')) { throw "Unexpected npm invocation: $args" }
    $global:LASTEXITCODE = 0
}

function npm.cmd {
    if ($env:TAURI_SIGNING_PRIVATE_KEY) { throw 'UI tests inherited the signing secret.' }
    # PowerShell consumes -- for this function mock; the real native .cmd shim
    # receives it and forwards the worker limit to Vitest.
    if (($args -join ' ') -ne 'test --maxWorkers=2') { throw "UI tests must bound Vitest worker memory: $args" }
    $global:LASTEXITCODE = 0
}

function node {
    if ($env:TAURI_SIGNING_PRIVATE_KEY) { throw 'Launcher tests inherited the signing secret.' }
    if ($args[0] -ne '--test' -or $args[1] -notlike '*launcher.test.mjs') { throw "Unexpected Node invocation: $args" }
    $global:LASTEXITCODE = 0
}

function cargo {
    if ((Test-Path 'Env:RUSTFLAGS') -or (Test-Path 'Env:CARGO_ENCODED_RUSTFLAGS') -or
        (Test-Path 'Env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS')) { throw 'Cargo inherited a compiler flag override.' }
    $global:LASTEXITCODE = 0
    if ($args[0] -eq 'metadata') {
        @{ packages = @(@{ name = 'manager'; version = '0.2.2' }); target_directory = $testTarget } | ConvertTo-Json -Compress
    } elseif ($args[0] -eq 'build' -and $args -contains 'verify-update' -and $args -contains '--locked') {
        if ($env:TAURI_SIGNING_PRIVATE_KEY) { throw 'Verifier build inherited the signing secret.' }
        $testState.VerifierBuilds++
        @{ reason = 'compiler-artifact'; target = @{ name = 'verify-update' }; executable = $testVerifier } | ConvertTo-Json -Compress
    } else { throw "Unexpected Cargo invocation: $args" }
}

function npx.cmd {
    if ((Test-Path 'Env:RUSTFLAGS') -or (Test-Path 'Env:CARGO_ENCODED_RUSTFLAGS') -or
        (Test-Path 'Env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS')) { throw 'Tauri inherited a compiler flag override.' }
    if ($args[0] -ne '--yes' -or $args[1] -ne '@tauri-apps/cli@2.12.1' -or $args -notcontains '--locked') {
        throw 'Tauri invocation must pin CLI 2.12.1 and lock Cargo dependencies.'
    }
    if ($env:TAURI_SIGNING_PRIVATE_KEY -ne $testKey -or $env:SC_UPDATER_PUB_FILE -ne $testPublicKey) {
        throw 'Signing and compiled public-key paths were not supplied together.'
    }
    $testState.BuildConfigPath = $args[[Array]::IndexOf($args, '--config') + 1]
    $testState.BuildConfig = Get-Content -LiteralPath $testState.BuildConfigPath -Raw | ConvertFrom-Json
    $testState.TauriBuilds++
    if ($testState.Mode -eq 'build-failure') { $global:LASTEXITCODE = 1; return }
    $product = if ($testState.BuildConfig.productName) { $testState.BuildConfig.productName } else { 'Surface Coordinator Manager' }
    $bundleDirectory = Join-Path $testTarget 'release\bundle\nsis'
    New-Item -ItemType Directory -Path $bundleDirectory -Force | Out-Null
    $bundleFile = Join-Path $bundleDirectory ("{0}_{1}_x64-setup.exe" -f $product, $testState.BuildConfig.version)
    [IO.File]::WriteAllText($bundleFile, 'fake NSIS bytes', $utf8)
    [IO.File]::WriteAllText("$bundleFile.sig", 'ZmFrZS1zaWc=', $utf8)
    if ($testState.Mode -eq 'stale-signature') { (Get-Item -LiteralPath "$bundleFile.sig").LastWriteTimeUtc = [DateTime]::UtcNow.AddDays(-1) }
    $global:LASTEXITCODE = 0
}

try {
    foreach ($directory in @($testScriptDirectory, $testManager, (Join-Path $testRoot 'ui\dist'))) {
        New-Item -ItemType Directory -Path $directory -Force | Out-Null
    }
    $testScript = Join-Path $testScriptDirectory 'manager-package.ps1'
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'manager-package.ps1') -Destination $testScript
    [IO.File]::WriteAllText($testKey, 'fake key material, never a real secret', $utf8)
    [IO.File]::WriteAllText($testPublicKey, 'ZmFrZS1wdWJsaWMta2V5', $utf8)
    [IO.File]::WriteAllText((Join-Path $testRoot 'ui\dist\index.html'), 'fixture', $utf8)
    [IO.File]::WriteAllText((Join-Path $testManager 'tauri.conf.json'), (@{
        identifier = 'com.realdoc06.surface-coordinator.manager'
        productName = 'Surface Coordinator Manager'
        plugins = @{ updater = @{ endpoints = @('https://example.com/latest.json') } }
    } | ConvertTo-Json -Depth 8), $utf8)
    [IO.File]::WriteAllText($testVerifier, 'if ($env:SC_PACKAGE_TEST_REJECT_SIGNATURE) { exit 1 }; exit 0', $utf8)
    $common = @{ KeyFile = $testKey; PublicKeyFile = $testPublicKey }
    $fixture = @{ TestIdentifier = 'com.realdoc06.surface-coordinator.manager.test'; TestVersion = '0.2.3'; TestUpdateEndpoint = 'http://127.0.0.1:18089/latest.json'; OutputDir = (Join-Path $testRoot 'test-release') }
    $env:TAURI_SIGNING_PRIVATE_KEY = 'previous secret sentinel'
    $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD = 'previous password sentinel'
    $env:SC_UPDATER_PUB_FILE = 'previous public file'
    $env:CARGO_TARGET_DIR = 'previous target directory'
    $env:RUSTFLAGS = '-C target-feature=-crt-static'
    $env:CARGO_ENCODED_RUSTFLAGS = "-C$([char]31)target-feature=-crt-static"
    $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS = '-C opt-level=1'
    Remove-Item -LiteralPath 'Env:CARGO_BUILD_TARGET' -ErrorAction SilentlyContinue

    Assert-Rejected @{ DownloadBaseUrl = 'http://example.com' } 'DownloadBaseUrl must use HTTPS'
    Assert-Rejected @{ TestVersion = '0.2.3' } 'Isolated builds require'
    $invalidFixture = $fixture.Clone()
    $invalidFixture.TestUpdateEndpoint = 'http://192.168.1.2/latest.json'
    Assert-Rejected $invalidFixture 'TestUpdateEndpoint must use HTTPS'
    $invalidFixture.TestUpdateEndpoint = 'http://localhost/latest.json'
    Assert-Rejected $invalidFixture 'TestUpdateEndpoint must use HTTPS'
    Assert ($testState.VerifierBuilds -eq 0 -and $testState.TauriBuilds -eq 0) 'Invalid endpoints are rejected before any build'

    New-Item -ItemType Directory -Path $testOutput | Out-Null
    [IO.File]::WriteAllText((Join-Path $testOutput 'latest.json'), (@{
        version = '0.2.2'; platforms = @{ 'darwin-aarch64' = @{ signature = 'existing'; url = 'https://example.com/manager.app.tar.gz' } }
    } | ConvertTo-Json -Depth 8), $utf8)
    & $testScript @common -OutputDir $testOutput -Notes 'Local packaging check' | Out-Null
    $manifest = Get-Content -LiteralPath (Join-Path $testOutput 'latest.json') -Raw | ConvertFrom-Json
    $artifactName = 'surface-coordinator-manager-0.2.2-windows-x86_64-setup.exe'
    Assert ($manifest.version -eq '0.2.2' -and $manifest.platforms.'windows-x86_64'.signature -eq 'ZmFrZS1zaWc=' -and
        $manifest.platforms.'windows-x86_64'.url -eq "https://github.com/RealDoc06/surface-coordinator/releases/download/v0.2.2/$artifactName") 'Manifest names the actual version, signature and intended release URL'
    Assert ($manifest.platforms.'darwin-aarch64'.signature -eq 'existing') 'Updating a manifest preserves other platforms of the same version'
    $hash = (Get-FileHash -LiteralPath (Join-Path $testOutput $artifactName) -Algorithm SHA256).Hash.ToLowerInvariant()
    Assert ((Get-Content -LiteralPath (Join-Path $testOutput 'manager-SHA256SUMS.txt')) -contains "$hash  $artifactName") 'Checksums cover the exported installer bytes'
    Assert ($testState.BuildConfig.plugins.updater.requireSignedVersion -and -not $testState.BuildConfig.plugins.updater.dangerousInsecureTransportProtocol) 'Production configuration requires a signed version and HTTPS'
    Assert (-not (Test-Path -LiteralPath $testState.BuildConfigPath)) 'Successful packaging removes its temporary configuration'

    & $testScript @common @fixture -DownloadBaseUrl http://127.0.0.1:18089 | Out-Null
    Assert ($testState.BuildConfig.identifier -eq $fixture.TestIdentifier -and
        $testState.BuildConfig.productName -eq 'Surface Coordinator Manager Update Test' -and
        $testState.BuildConfig.version -eq '0.2.3' -and $testState.BuildConfig.plugins.updater.dangerousInsecureTransportProtocol) 'An explicit fixture gets a distinct app identity, version and loopback endpoint'

    foreach ($failureMode in @('stale-signature', 'build-failure', 'verification-failure')) {
        $testState.Mode = $failureMode
        $failureOutput = Join-Path $testRoot $failureMode
        if ($failureMode -eq 'verification-failure') { $env:SC_PACKAGE_TEST_REJECT_SIGNATURE = '1' }
        $expected = switch ($failureMode) {
            'stale-signature' { 'freshly signed' }
            'build-failure' { 'Manager build or updater signing failed' }
            'verification-failure' { 'Updater signature or signed version did not verify' }
        }
        Assert-Rejected @{ OutputDir = $failureOutput } $expected
        Assert (-not (Test-Path -LiteralPath $failureOutput) -and -not (Test-Path -LiteralPath $testState.BuildConfigPath)) "$failureMode exports nothing and removes temporary configuration"
    }
    Assert ($env:TAURI_SIGNING_PRIVATE_KEY -eq 'previous secret sentinel' -and
        $env:TAURI_SIGNING_PRIVATE_KEY_PASSWORD -eq 'previous password sentinel' -and
        $env:SC_UPDATER_PUB_FILE -eq 'previous public file' -and
        $env:CARGO_TARGET_DIR -eq 'previous target directory') 'All signing and Cargo environment variables survive success and failure unchanged'
    Assert ($env:RUSTFLAGS -eq '-C target-feature=-crt-static' -and
        $env:CARGO_ENCODED_RUSTFLAGS -eq "-C$([char]31)target-feature=-crt-static" -and
        $env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS -eq '-C opt-level=1') 'Compiler flag overrides are isolated from builds and restored exactly afterward'
    foreach ($name in @('TAURI_SIGNING_PRIVATE_KEY', 'SC_UPDATER_PUB_FILE', 'CARGO_TARGET_DIR', 'RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS')) {
        Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
    }
    $testState.Mode = 'build-failure'
    Assert-Rejected @{ OutputDir = (Join-Path $testRoot 'unset-environment') } 'Manager build or updater signing failed'
    Assert (-not (Test-Path 'Env:TAURI_SIGNING_PRIVATE_KEY') -and -not (Test-Path 'Env:SC_UPDATER_PUB_FILE') -and
        -not (Test-Path 'Env:CARGO_TARGET_DIR')) 'Initially unset environment variables are removed instead of restored as empty strings'
    Assert (-not (Test-Path 'Env:RUSTFLAGS') -and -not (Test-Path 'Env:CARGO_ENCODED_RUSTFLAGS') -and
        -not (Test-Path 'Env:CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS')) 'Initially unset compiler flag overrides remain absent after packaging failure'
    Write-Host "$($testState.Checks) packaging checks passed; no real builds, keys or release artifacts used." -ForegroundColor Green
}
finally {
    foreach ($name in $saved.Keys) {
        if ($null -eq $saved[$name]) { Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue }
        else { [Environment]::SetEnvironmentVariable($name, $saved[$name], 'Process') }
    }
    $saved.Clear()
    # Verify the entire recursive cleanup target before deleting test fixtures.
    $cleanupPath = [IO.Path]::GetFullPath($testRoot)
    $temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
    if ([IO.Path]::GetDirectoryName($cleanupPath) -ne $temporaryRoot -or
        [IO.Path]::GetFileName($cleanupPath) -notlike 'sc-manager-package-tests-*') {
        throw 'Refusing to clean up an unexpected test directory.'
    }
    if (Test-Path -LiteralPath $cleanupPath) { Remove-Item -LiteralPath $cleanupPath -Recurse -Force }
}
