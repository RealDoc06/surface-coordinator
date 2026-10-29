<#
.SYNOPSIS
  Builds and signs a local Windows x64 manager installer and updater manifest.
.DESCRIPTION
  Requires Rust, Node, and an existing updater key pair. Installs locked UI
  dependencies, checks and rebuilds the UI, and runs the manager launcher tests.
  Uses pinned Tauri CLI 2.12.1 and locked Cargo dependencies. Nothing is uploaded
  or installed. Updater signing is separate from Windows Authenticode signing.

  The private key stays in its original file. For encrypted keys, provide the
  password through TAURI_SIGNING_PRIVATE_KEY_PASSWORD. The script restores the
  environment and removes its temporary configuration even when a build fails.

  TestIdentifier, TestVersion and TestUpdateEndpoint must be supplied together
  with OutputDir to build an isolated updater fixture. HTTP is permitted only
  for literal loopback addresses in that test configuration.
.EXAMPLE
  .\scripts\manager-package.ps1
.EXAMPLE
  .\scripts\manager-package.ps1 -OutputDir target\updater-test\0.2.3 `
    -TestIdentifier com.realdoc06.surface-coordinator.manager.update-test `
    -TestVersion 0.2.3 -TestUpdateEndpoint http://127.0.0.1:18089/latest.json `
    -DownloadBaseUrl http://127.0.0.1:18089
#>
[CmdletBinding()]
param(
    [string]$OutputDir,
    [string]$KeyFile = (Join-Path $env:USERPROFILE '.surface-coordinator\manager-updater.key'),
    [string]$PublicKeyFile = (Join-Path (Split-Path -Parent $PSScriptRoot) 'crates\manager\updater.pub'),
    [string]$DownloadBaseUrl,
    [string]$Notes = '',
    [string]$TestIdentifier,
    [ValidatePattern('^[0-9]+\.[0-9]+\.[0-9]+$')][string]$TestVersion,
    [string]$TestUpdateEndpoint
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$managerDirectory = Join-Path $root 'crates\manager'
$platform = 'windows-x86_64'
$rustTarget = 'x86_64-pc-windows-msvc'
$cli = '@tauri-apps/cli@2.12.1'
$utf8 = [Text.UTF8Encoding]::new($false)

# URI hostnames such as localhost may be remapped. Only IP literals are allowed
# when explicitly building an isolated HTTP test fixture.
function Assert-UpdateUrl([string]$Value, [bool]$AllowLoopbackHttp, [string]$Name) {
    $uri = $null
    if (-not [uri]::TryCreate($Value, [UriKind]::Absolute, [ref]$uri) -or
        $uri.UserInfo -or $uri.Fragment -or $uri.Query) {
        throw "$Name must be an absolute URL without credentials, query or fragment."
    }
    if ($uri.Scheme -eq 'https') { return }
    $address = $null
    if ($AllowLoopbackHttp -and $uri.Scheme -eq 'http' -and
        [Net.IPAddress]::TryParse($uri.DnsSafeHost, [ref]$address) -and
        [Net.IPAddress]::IsLoopback($address)) { return }
    throw "$Name must use HTTPS (isolated test builds also allow literal loopback HTTP)."
}

if ($env:OS -ne 'Windows_NT') { throw 'Build the Windows manager from Windows.' }
$testOptions = @($TestIdentifier, $TestVersion, $TestUpdateEndpoint) | Where-Object { $_ }
$testBuild = $testOptions.Count -gt 0
if ($testBuild -and ($testOptions.Count -ne 3 -or -not $OutputDir)) {
    throw 'Isolated builds require TestIdentifier, TestVersion, TestUpdateEndpoint and OutputDir together.'
}
if ($DownloadBaseUrl) { Assert-UpdateUrl $DownloadBaseUrl $testBuild 'DownloadBaseUrl' }
if ($testBuild) { Assert-UpdateUrl $TestUpdateEndpoint $true 'TestUpdateEndpoint' }
if (-not (Test-Path -LiteralPath $KeyFile -PathType Leaf)) { throw "Updater private key not found: $KeyFile" }
if (-not (Test-Path -LiteralPath $PublicKeyFile -PathType Leaf)) { throw "Updater public key not found: $PublicKeyFile" }
$KeyFile = (Resolve-Path -LiteralPath $KeyFile).Path
$PublicKeyFile = (Resolve-Path -LiteralPath $PublicKeyFile).Path
if ($OutputDir) { $OutputDir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($OutputDir) }
$publicKey = [IO.File]::ReadAllText($PublicKeyFile).Trim()
if (-not $publicKey -or $publicKey -match '[\r\n]') { throw 'The public key must be the single base64 line from Tauri signer.' }
try { [void][Convert]::FromBase64String($publicKey) } catch { throw 'The updater public key is not valid base64.' }
$config = Get-Content -LiteralPath (Join-Path $managerDirectory 'tauri.conf.json') -Raw | ConvertFrom-Json
if ($testBuild -and ($TestIdentifier -eq $config.identifier -or $TestIdentifier -notmatch '^[A-Za-z0-9-]+(\.[A-Za-z0-9-]+)+$')) {
    throw 'TestIdentifier must be a distinct reverse-domain app identifier.'
}

$savedEnvironment = @{}
$rustFlagEnvironment = @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS')
foreach ($name in (@('TAURI_SIGNING_PRIVATE_KEY', 'SC_UPDATER_PUB_FILE', 'CARGO_TARGET_DIR') + $rustFlagEnvironment)) {
    $savedEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
}
$temporaryDirectory = Join-Path ([IO.Path]::GetTempPath()) ('sc-manager-package-' + [guid]::NewGuid().ToString('N'))
$temporaryConfig = Join-Path $temporaryDirectory 'tauri.release.conf.json'
Push-Location $managerDirectory
try {
    Remove-Item -LiteralPath 'Env:TAURI_SIGNING_PRIVATE_KEY' -ErrorAction SilentlyContinue
    # Release artifacts must follow the repository's static CRT policy even
    # when the caller has unrelated compiler flags in their shell environment.
    foreach ($name in $rustFlagEnvironment) { Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue }
    Push-Location (Join-Path $root 'ui')
    try {
        & npm ci
        if ($LASTEXITCODE -ne 0) { throw 'Could not install the locked UI dependencies.' }
        & npm run typecheck
        if ($LASTEXITCODE -ne 0) { throw 'UI type checking failed.' }
        # Keep enough memory available for Rust and the disposable test VM.
        # The .cmd shim preserves the argument separator for Vitest.
        & npm.cmd test -- --maxWorkers=2
        if ($LASTEXITCODE -ne 0) { throw 'UI tests failed.' }
        & npm run build
        if ($LASTEXITCODE -ne 0) { throw 'UI build failed.' }
    } finally { Pop-Location }
    & node --test (Join-Path $managerDirectory 'tests\launcher.test.mjs')
    if ($LASTEXITCODE -ne 0) { throw 'Manager launcher tests failed.' }
    if (-not (Test-Path -LiteralPath (Join-Path $root 'ui\dist\index.html') -PathType Leaf)) {
        throw 'The UI build did not produce ui/dist/index.html.'
    }
    $rustInfo = & rustc -vV
    if ($LASTEXITCODE -ne 0 -or $rustInfo -notcontains "host: $rustTarget") {
        throw "This script requires the $rustTarget Rust host before labeling a Windows x64 release."
    }
    if ($env:CARGO_BUILD_TARGET -and $env:CARGO_BUILD_TARGET -ne $rustTarget) {
        throw "CARGO_BUILD_TARGET must be unset or $rustTarget."
    }
    # Resolve from the same working directory as Tauri, including Cargo config
    # and relative CARGO_TARGET_DIR overrides, then pin that resolved directory.
    $metadataText = & cargo metadata --locked --no-deps --format-version 1
    if ($LASTEXITCODE -ne 0) { throw 'Could not read Cargo workspace metadata.' }
    $metadata = $metadataText | ConvertFrom-Json
    $package = @($metadata.packages | Where-Object { $_.name -eq 'manager' })
    if ($package.Count -ne 1) { throw 'The manager package was not found in Cargo metadata.' }
    $version = if ($testBuild) { $TestVersion } else { $package[0].version }
    if ($version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+$') { throw 'Installer version must be major.minor.patch.' }
    $env:CARGO_TARGET_DIR = $metadata.target_directory
    # Use the same Minisign verifier as the updater. Tauri's CLI only warns
    # about mismatched key IDs, so signing success alone is not sufficient.
    $verifierMessages = & cargo build --locked -p manager --example verify-update --message-format=json-render-diagnostics
    if ($LASTEXITCODE -ne 0) { throw 'Could not build the updater signature verifier.' }
    $verifierArtifacts = @($verifierMessages | ForEach-Object { $_ | ConvertFrom-Json } | Where-Object {
        $_.reason -eq 'compiler-artifact' -and $_.target.name -eq 'verify-update' -and $_.executable
    })
    if ($verifierArtifacts.Count -ne 1) { throw 'Cargo did not report the signature verifier executable.' }
    $verifier = $verifierArtifacts[0].executable
    if (-not $OutputDir) { $OutputDir = Join-Path $metadata.target_directory "production\$version" }
    if (-not $DownloadBaseUrl) {
        $DownloadBaseUrl = "https://github.com/RealDoc06/surface-coordinator/releases/download/v$version"
    }
    $DownloadBaseUrl = $DownloadBaseUrl.TrimEnd('/')
    $updater = @{ pubkey = $publicKey; requireSignedVersion = $true; dangerousInsecureTransportProtocol = $false }
    $overlay = @{ version = $version; bundle = @{ active = $true; createUpdaterArtifacts = $true }; plugins = @{ updater = $updater } }
    $productName = $config.productName
    if ($testBuild) {
        $overlay.identifier = $TestIdentifier
        $productName = 'Surface Coordinator Manager Update Test'
        $overlay.productName = $productName
        $updater.endpoints = @($TestUpdateEndpoint)
        $updater.dangerousInsecureTransportProtocol = ([uri]$TestUpdateEndpoint).Scheme -eq 'http' -or ([uri]$DownloadBaseUrl).Scheme -eq 'http'
    } else {
        foreach ($endpoint in $config.plugins.updater.endpoints) { Assert-UpdateUrl $endpoint $false 'Updater endpoint' }
    }
    New-Item -ItemType Directory -Path $temporaryDirectory | Out-Null
    [IO.File]::WriteAllText($temporaryConfig, ($overlay | ConvertTo-Json -Depth 8), $utf8)
    # The pinned CLI accepts a private-key path here, so PowerShell never reads
    # or copies its contents. --ci uses an empty password if none is provided.
    $env:TAURI_SIGNING_PRIVATE_KEY = $KeyFile
    $env:SC_UPDATER_PUB_FILE = $PublicKeyFile
    Write-Host "Building manager $version for $platform on $env:COMPUTERNAME" -ForegroundColor Cyan
    $buildStarted = [DateTime]::UtcNow
    # Use the Windows command shim: npx.ps1 consumes Cargo's -- separator.
    & npx.cmd --yes $cli build --ci --bundles nsis --config $temporaryConfig -- --locked
    if ($LASTEXITCODE -ne 0) { throw 'Manager build or updater signing failed; no release files were exported.' }

    # Cargo build.target may explicitly select the native target. Accept its
    # output or the usual native output, but never a stale or foreign installer.
    $bundleName = "${productName}_${version}_x64-setup.exe"
    $candidates = @(
        (Join-Path $metadata.target_directory "release\bundle\nsis\$bundleName"),
        (Join-Path $metadata.target_directory "$rustTarget\release\bundle\nsis\$bundleName")
    ) | Where-Object {
        (Test-Path -LiteralPath $_ -PathType Leaf) -and
        (Test-Path -LiteralPath "$_`.sig" -PathType Leaf) -and
        (Get-Item -LiteralPath "$_`.sig").LastWriteTimeUtc -ge $buildStarted
    }
    if (@($candidates).Count -ne 1) { throw 'Expected exactly one freshly signed Windows x64 NSIS installer.' }
    $installer = @($candidates)[0]
    $signatureFile = "$installer.sig"

    & $verifier --file $installer --signature $signatureFile --public-key $PublicKeyFile --version $version
    if ($LASTEXITCODE -ne 0) { throw 'Updater signature or signed version did not verify; no release files were exported.' }

    $artifactName = "surface-coordinator-manager-$version-$platform-setup.exe"
    $signature = [IO.File]::ReadAllText($signatureFile).Trim()
    $manifestFile = Join-Path $OutputDir 'latest.json'
    $updateManifest = [ordered]@{ version = $version; notes = $Notes; pub_date = [DateTime]::UtcNow.ToString('yyyy-MM-ddTHH:mm:ssZ'); platforms = @{} }
    if (Test-Path -LiteralPath $manifestFile -PathType Leaf) {
        $existing = Get-Content -LiteralPath $manifestFile -Raw | ConvertFrom-Json
        if ($existing.version -ne $version) { throw 'OutputDir already contains latest.json for another version; use a separate directory.' }
        foreach ($property in $existing.platforms.PSObject.Properties) {
            $updateManifest.platforms[$property.Name] = $property.Value
        }
    }
    $updateManifest.platforms[$platform] = @{ signature = $signature; url = "$DownloadBaseUrl/$artifactName" }
    New-Item -ItemType Directory -Path $OutputDir -Force | Out-Null
    Copy-Item -LiteralPath $installer -Destination (Join-Path $OutputDir $artifactName) -Force
    Copy-Item -LiteralPath $signatureFile -Destination (Join-Path $OutputDir "$artifactName.sig") -Force
    [IO.File]::WriteAllText($manifestFile, ($updateManifest | ConvertTo-Json -Depth 8) + "`n", $utf8)
    $checksums = foreach ($name in @($artifactName, "$artifactName.sig", 'latest.json')) {
        $hash = (Get-FileHash -LiteralPath (Join-Path $OutputDir $name) -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash  $name"
    }
    [IO.File]::WriteAllText((Join-Path $OutputDir 'manager-SHA256SUMS.txt'), ($checksums -join "`n") + "`n", $utf8)
    Write-Host "Signed manager installer, signature, latest.json and manager-SHA256SUMS.txt: $OutputDir" -ForegroundColor Green
}
finally {
    foreach ($name in $savedEnvironment.Keys) {
        if ($null -eq $savedEnvironment[$name]) { Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue }
        else { [Environment]::SetEnvironmentVariable($name, $savedEnvironment[$name], 'Process') }
    }
    $savedEnvironment.Clear()
    if (Test-Path -LiteralPath $temporaryConfig -PathType Leaf) { Remove-Item -LiteralPath $temporaryConfig -Force }
    if (Test-Path -LiteralPath $temporaryDirectory -PathType Container) { Remove-Item -LiteralPath $temporaryDirectory }
    Pop-Location
}
