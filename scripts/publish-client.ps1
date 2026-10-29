<#
.SYNOPSIS
  Builds the client, signs it and uploads it to a coordinator as an update.

.DESCRIPTION
  The version is the workspace version in Cargo.toml: bump it before
  publishing. The signing key is read from a file outside the repository
  (default: %USERPROFILE%\.surface-coordinator\update-signing.key) and never
  written anywhere else. With automatic updates switched on in the
  coordinator, uploading starts the staged rollout.

.EXAMPLE
  .\scripts\publish-client.ps1 -Coordinator https://surface-coord.realdoc06.it -Username doc
#>
param(
    [Parameter(Mandatory)] [string]$Coordinator,
    [Parameter(Mandatory)] [string]$Username,
    # Asked for when not given. May also come from SC_COORDINATOR_PASSWORD.
    [securestring]$Password,
    [string]$Notes = '',
    [string]$KeyFile = (Join-Path $env:USERPROFILE '.surface-coordinator\update-signing.key')
)

$ErrorActionPreference = 'Stop'
$root = Split-Path -Parent $PSScriptRoot
$target = 'windows-x86_64'
$rustTarget = 'x86_64-pc-windows-msvc'
$Coordinator = $Coordinator.TrimEnd('/')

if ($env:OS -ne 'Windows_NT') { throw 'Build and publish the Windows client from Windows.' }
if (-not (Test-Path -LiteralPath $KeyFile -PathType Leaf)) { throw "signing key not found at $KeyFile (see release-keys\README.md)" }

$manifest = Join-Path $root 'Cargo.toml'
$buildEnvironment = @{}
# Cargo discovers repository configuration from the current directory. Do not
# let inherited flags replace its static Windows runtime setting.
Push-Location $root
try {
    foreach ($name in @('RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_TARGET_X86_64_PC_WINDOWS_MSVC_RUSTFLAGS')) {
        $buildEnvironment[$name] = [Environment]::GetEnvironmentVariable($name, 'Process')
        Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue
    }
    $metadataText = cargo metadata --locked --no-deps --format-version 1 --manifest-path $manifest
    if ($LASTEXITCODE -ne 0) { throw 'could not read the workspace metadata' }
    $metadata = $metadataText | ConvertFrom-Json
    $version = ($metadata.packages | Where-Object { $_.name -eq 'client' }).version
    if (-not $version) { throw 'the client package was not found in the workspace' }
    Write-Host "Publishing client $version for $target to $Coordinator" -ForegroundColor Cyan

    cargo build --locked --release --manifest-path $manifest --target $rustTarget -p client -p release-tool
    if ($LASTEXITCODE -ne 0) { throw 'build failed' }
}
finally {
    foreach ($name in $buildEnvironment.Keys) {
        if ($null -eq $buildEnvironment[$name]) { Remove-Item -LiteralPath "Env:$name" -ErrorAction SilentlyContinue }
        else { [Environment]::SetEnvironmentVariable($name, $buildEnvironment[$name], 'Process') }
    }
    Pop-Location
}

$buildDirectory = Join-Path $metadata.target_directory "$rustTarget\release"
$out = Join-Path $metadata.target_directory 'publish'
New-Item -ItemType Directory -Force $out | Out-Null
$artifact = Join-Path $out "surface-coordinator-client-$version-$target.exe"
Copy-Item -LiteralPath (Join-Path $buildDirectory 'client.exe') -Destination $artifact -Force

$releaseTool = Join-Path $buildDirectory 'release-tool.exe'
$previousSigningKey = $env:SC_UPDATE_SIGNING_KEY
try {
    $env:SC_UPDATE_SIGNING_KEY = (Get-Content -LiteralPath $KeyFile -Raw).Trim()
    & $releaseTool sign --file $artifact --version $version --target $target | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'signing failed' }
}
finally {
    if ($null -eq $previousSigningKey) { Remove-Item -LiteralPath 'Env:SC_UPDATE_SIGNING_KEY' -ErrorAction SilentlyContinue }
    else { $env:SC_UPDATE_SIGNING_KEY = $previousSigningKey }
    $previousSigningKey = $null
}
& $releaseTool verify --file $artifact --version $version --target $target --signature "$artifact.sig" `
    --public-key (Join-Path $root 'release-keys\client-update.pub')
if ($LASTEXITCODE -ne 0) { throw 'the signature does not verify against release-keys\client-update.pub' }
$signature = (Get-Content "$artifact.sig" -Raw).Trim()

if (-not $Password) {
    $Password = if ($env:SC_COORDINATOR_PASSWORD) {
        ConvertTo-SecureString $env:SC_COORDINATOR_PASSWORD -AsPlainText -Force
    } else {
        Read-Host "Password for $Username" -AsSecureString
    }
}
$passwordBuffer = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($Password)
try {
    $plain = [Runtime.InteropServices.Marshal]::PtrToStringBSTR($passwordBuffer)
    $login = @{ username = $Username; password = $plain } | ConvertTo-Json
    Invoke-RestMethod "$Coordinator/api/v1/auth/login" -Method Post -ContentType 'application/json' -Body $login `
        -SessionVariable session | Out-Null
}
finally {
    [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($passwordBuffer)
    $plain = $null
    $login = $null
}

$query = "version=$version&target=$target"
if ($Notes) { $query += '&notes=' + [uri]::EscapeDataString($Notes) }
try {
    $updates = Invoke-RestMethod "$Coordinator/api/v1/updates/upload?$query" -Method Post -WebSession $session `
        -ContentType 'application/octet-stream' -Headers @{ 'X-Signature' = $signature } -InFile $artifact
}
finally {
    Invoke-RestMethod "$Coordinator/api/v1/auth/logout" -Method Post -WebSession $session -ErrorAction SilentlyContinue | Out-Null
}

Write-Host "Uploaded. Newest version on the coordinator: $($updates.latest.version)" -ForegroundColor Green
if ($updates.auto_update) {
    Write-Host 'Automatic updates are on: one device is updated first, the others follow once it is back.'
} else {
    Write-Host 'Automatic updates are off: use "Update client" in the UI, or switch them on under Updates.'
}
