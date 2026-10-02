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
$Coordinator = $Coordinator.TrimEnd('/')

if (-not (Test-Path $KeyFile)) { throw "signing key not found at $KeyFile (see release-keys\README.md)" }

$version = (Select-String -Path (Join-Path $root 'Cargo.toml') -Pattern '^version = "(.+)"$').Matches[0].Groups[1].Value
Write-Host "Publishing client $version for $target to $Coordinator" -ForegroundColor Cyan

cargo build --release --manifest-path (Join-Path $root 'Cargo.toml') -p client -p release-tool
if ($LASTEXITCODE -ne 0) { throw 'build failed' }

$out = Join-Path $root 'target\publish'
New-Item -ItemType Directory -Force $out | Out-Null
$artifact = Join-Path $out "surface-coordinator-client-$version-$target.exe"
Copy-Item (Join-Path $root 'target\release\client.exe') $artifact -Force

$releaseTool = Join-Path $root 'target\release\release-tool.exe'
try {
    $env:SC_UPDATE_SIGNING_KEY = (Get-Content $KeyFile -Raw).Trim()
    & $releaseTool sign --file $artifact --version $version --target $target | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'signing failed' }
}
finally {
    Remove-Item Env:SC_UPDATE_SIGNING_KEY -ErrorAction SilentlyContinue
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
$plain = [Runtime.InteropServices.Marshal]::PtrToStringBSTR([Runtime.InteropServices.Marshal]::SecureStringToBSTR($Password))
$login = @{ username = $Username; password = $plain } | ConvertTo-Json
Invoke-RestMethod "$Coordinator/api/v1/auth/login" -Method Post -ContentType 'application/json' -Body $login `
    -SessionVariable session | Out-Null

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
