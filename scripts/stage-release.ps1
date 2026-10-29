<#
.SYNOPSIS
  Verifies and assembles a local release bundle without building or publishing.
.DESCRIPTION
  Final binaries and START-HERE.md must already exist in target/production/VERSION.
  EvidenceManifest is a repository-relative JSON file containing a nonempty array:
  [{"source":"demo-data/review/report.json","destination":"evidence/report.json"}]
  Use forward slashes. Additional destinations must be under evidence/ or
  docs/screenshots/. Review and sanitize report contents and screenshots first;
  filename checks cannot establish that arbitrary evidence contains no secrets.

  Without -Finalize, verify all inputs, signatures and the reviewed Manager hash,
  then print the allowlist without writing. Finalization refuses existing archives,
  copies only the allowlist, and checks every archived file against its input hash.
.EXAMPLE
  .\scripts\stage-release.ps1 -Version 0.2.4 -ExpectedManagerSha256 <sha256> `
    -EvidenceManifest demo-data/readiness-20261005/release-evidence.json
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidatePattern('^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$')][string]$Version,
    [Parameter(Mandatory)][ValidatePattern('^[a-fA-F0-9]{64}$')][string]$ExpectedManagerSha256,
    [Parameter(Mandatory)][ValidateNotNullOrEmpty()][string]$EvidenceManifest,
    [switch]$Finalize
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version Latest
$repo = [IO.Path]::GetFullPath((Split-Path -Parent $PSScriptRoot))
$releaseRelative = "target/production/$Version"
$release = Join-Path $repo $releaseRelative
$archiveName = "surface-coordinator-$Version.zip"
$archivePrefix = "surface-coordinator-$Version/"
$utf8 = [Text.UTF8Encoding]::new($false)
$managerName = "surface-coordinator-manager-$Version-windows-x86_64-setup.exe"
$clientName = "surface-coordinator-client-$Version-windows-x86_64.exe"

# Reject aliases as well as lexical traversal so a manifest cannot escape through
# a junction, alternate data stream, Windows device name, or normalized filename.
function Resolve-DeliveryPath([string]$Root, [string]$Relative) {
    if ([string]::IsNullOrWhiteSpace($Relative) -or $Relative -match '[\\:\x00-\x1f<>"|?*]' -or
        [IO.Path]::IsPathRooted($Relative)) { throw 'Delivery paths must be relative paths using forward slashes.' }
    $segments = $Relative.Split('/')
    foreach ($segment in $segments) {
        if (-not $segment -or $segment -in @('.', '..') -or $segment -match '[ .]$' -or
            $segment -match '^(?i:CON|PRN|AUX|NUL|COM[1-9]|LPT[1-9])(?:\.|$)') {
            throw 'Delivery paths cannot contain traversal, empty segments or Windows path aliases.'
        }
    }
    $full = [IO.Path]::GetFullPath((Join-Path $Root $Relative))
    if (-not $full.StartsWith($Root.TrimEnd('\', '/') + [IO.Path]::DirectorySeparatorChar, [StringComparison]::OrdinalIgnoreCase)) {
        throw 'A delivery path escapes its expected directory.'
    }
    $current = $Root
    foreach ($segment in $segments) {
        $current = Join-Path $current $segment
        if (Test-Path -LiteralPath $current) {
            if ((Get-Item -LiteralPath $current -Force).Attributes -band [IO.FileAttributes]::ReparsePoint) {
                throw 'Delivery paths cannot traverse symbolic links or directory junctions.'
            }
        }
    }
    return $full
}

function Assert-AllowedFilename([string]$Relative) {
    if ($Relative -match '(?i)(^|/)(access|state|config|setup|credentials?|secrets?|keys?|jobs|original)\.json$|\.(key|pem|pfx|p12)$|\.sqlite(?:3)?(?:-|$)|\.db(?:-|$)|(^|/)\.env(?:\.|$)|(^|/)(\.git|\.ssh)(/|$)') {
        throw 'Credential, private-key, database and raw state files are forbidden in a delivery.'
    }
}

$manifestPath = Resolve-DeliveryPath $repo $EvidenceManifest
Assert-AllowedFilename $EvidenceManifest
if (-not (Test-Path -LiteralPath $manifestPath -PathType Leaf)) { throw 'EvidenceManifest does not exist.' }
$manifestText = [IO.File]::ReadAllText($manifestPath)
if ($manifestText -notmatch '^\s*\[') { throw 'EvidenceManifest must be a nonempty JSON array.' }
try { $evidence = @($manifestText | ConvertFrom-Json) }
catch { throw 'EvidenceManifest is not valid JSON.' }
if ($evidence.Count -eq 0) { throw 'EvidenceManifest must be a nonempty JSON array.' }

# Each pair is a repository-relative source and a bundle-relative destination.
# No evidence or screenshots are inherited from an earlier release.
$files = @(
    ,@("$releaseRelative/START-HERE.md", 'START-HERE.md')
    ,@("$releaseRelative/coordinator.exe", 'coordinator.exe')
    ,@("$releaseRelative/release-tool.exe", 'release-tool.exe')
    ,@("$releaseRelative/$clientName", $clientName)
    ,@("$releaseRelative/$clientName.sig", "$clientName.sig")
    ,@("$releaseRelative/$managerName", $managerName)
    ,@("$releaseRelative/$managerName.sig", "$managerName.sig")
    ,@("$releaseRelative/latest.json", 'latest.json')
    ,@("$releaseRelative/manager-SHA256SUMS.txt", 'manager-SHA256SUMS.txt')
    ,@("$releaseRelative/linux/coordinator", 'linux/coordinator')
    ,@("$releaseRelative/linux/surface-coordinator-$Version-image.tar", "linux/surface-coordinator-$Version-image.tar")
    ,@("$releaseRelative/linux/compose.prebuilt.yaml", 'linux/compose.prebuilt.yaml')
    ,@('release-keys/client-update.pub', 'release-keys/client-update.pub')
    ,@('crates/manager/updater.pub', 'release-keys/manager-updater.pub')
    ,@('docs/usage.md', 'docs/usage.md')
    ,@("docs/validation-$Version.md", "docs/validation-$Version.md")
    ,@('docs/api.md', 'docs/api.md')
    ,@('docs/manual-tests.md', 'docs/manual-tests.md')
    ,@('deploy/README.md', 'deploy/README.md')
    ,@('deploy/compose.yaml', 'deploy/compose.yaml')
    ,@('deploy/Dockerfile', 'deploy/Dockerfile')
    ,@('deploy/coordinator.service', 'deploy/coordinator.service')
    ,@('deploy/coordinator.env.example', 'deploy/coordinator.env.example')
    ,@('deploy/Caddyfile.example', 'deploy/Caddyfile.example')
    ,@('deploy/coordinator.service', 'linux/coordinator.service')
    ,@('deploy/coordinator.env.example', 'linux/coordinator.env.example')
    ,@('deploy/Caddyfile.example', 'linux/Caddyfile.example')
)
foreach ($entry in $evidence) {
    if ($null -eq $entry -or $entry -isnot [Management.Automation.PSCustomObject] -or
        (@($entry.PSObject.Properties.Name | Sort-Object) -join ',') -cne 'destination,source' -or
        $entry.source -isnot [string] -or $entry.destination -isnot [string]) {
        throw 'Each evidence entry must contain exactly the string fields source and destination.'
    }
    [void](Resolve-DeliveryPath $repo $entry.source)
    [void](Resolve-DeliveryPath $repo "$releaseRelative/$($entry.destination)")
    Assert-AllowedFilename $entry.source
    Assert-AllowedFilename $entry.destination
    if ($entry.destination -notmatch '^(evidence/|docs/screenshots/)' -or
        [IO.Path]::GetExtension($entry.destination) -notin @('.json', '.log', '.txt', '.md', '.png', '.jpg', '.jpeg', '.webp') -or
        [IO.Path]::GetExtension($entry.source) -ne [IO.Path]::GetExtension($entry.destination)) {
        throw 'Evidence must retain its report/image extension under evidence/ or docs/screenshots/.'
    }
    $files += ,@($entry.source, $entry.destination)
}

$destinations = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
$inputs = foreach ($entry in $files) {
    $source = Resolve-DeliveryPath $repo $entry[0]
    $destination = Resolve-DeliveryPath $repo "$releaseRelative/$($entry[1])"
    Assert-AllowedFilename $entry[0]
    Assert-AllowedFilename $entry[1]
    if (-not $destinations.Add($entry[1])) { throw "Duplicate delivery destination: $($entry[1])" }
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) { throw "Missing approved artifact: $($entry[0])" }
    if ((Get-Item -LiteralPath $source).Length -eq 0) { throw "Empty approved artifact: $($entry[0])" }
    [pscustomobject]@{ Name = $entry[1]; Source = $source; Destination = $destination; Hash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash }
}
$archive = Resolve-DeliveryPath $repo "target/production/$archiveName"
$temporaryArchive = Resolve-DeliveryPath $repo "target/production/$archiveName.partial"
$archiveChecksum = Resolve-DeliveryPath $repo "target/production/$archiveName.sha256"
$checksumsPath = Resolve-DeliveryPath $repo "$releaseRelative/SHA256SUMS.txt"
if ($Finalize -and ((Test-Path -LiteralPath $archive) -or (Test-Path -LiteralPath $temporaryArchive) -or (Test-Path -LiteralPath $archiveChecksum))) {
    throw 'A final archive, partial archive or archive checksum already exists; review it before retrying.'
}
$managerInput = $inputs | Where-Object Name -EQ $managerName
if ($managerInput.Hash -ne $ExpectedManagerSha256) { throw 'The Manager installer differs from the reviewed ExpectedManagerSha256.' }
$manifest = Get-Content -LiteralPath (Join-Path $release 'latest.json') -Raw | ConvertFrom-Json
if ($manifest.version -ne $Version -or
    $manifest.platforms.'windows-x86_64'.url -ne "https://github.com/RealDoc06/surface-coordinator/releases/download/v$Version/$managerName" -or
    $manifest.platforms.'windows-x86_64'.signature -ne [IO.File]::ReadAllText((Join-Path $release "$managerName.sig")).Trim()) {
    throw 'Manager update manifest does not match the final signed package.'
}
$managerSums = @(Get-Content -LiteralPath (Join-Path $release 'manager-SHA256SUMS.txt') | Where-Object { $_.Trim() })
if ($managerSums.Count -ne 3) { throw 'Manager checksum file must cover exactly its installer, signature and manifest.' }
foreach ($name in @($managerName, "$managerName.sig", 'latest.json')) {
    $inputFile = $inputs | Where-Object Name -EQ $name
    if (@($managerSums | Where-Object { $_ -ieq "$($inputFile.Hash)  $name" }).Count -ne 1) {
        throw "Manager checksum file does not match $name."
    }
}
& (Join-Path $release 'release-tool.exe') verify --file (Join-Path $release $clientName) --version $Version `
    --target windows-x86_64 --signature (Join-Path $release "$clientName.sig") --public-key (Join-Path $repo 'release-keys/client-update.pub')
if ($LASTEXITCODE -ne 0) { throw 'Client signature verification failed.' }
$managerVerifier = Resolve-DeliveryPath $repo 'target/debug/examples/verify-update.exe'
if (-not (Test-Path -LiteralPath $managerVerifier -PathType Leaf)) { throw 'Build the manager verify-update example before staging.' }
& $managerVerifier --file (Join-Path $release $managerName) --signature (Join-Path $release "$managerName.sig") `
    --public-key (Join-Path $repo 'crates/manager/updater.pub') --version $Version
if ($LASTEXITCODE -ne 0) { throw 'Manager signature or signed version verification failed.' }

# Bind both verification and archive contents to the same frozen input bytes.
foreach ($entry in $inputs) {
    if ((Get-FileHash -LiteralPath $entry.Source -Algorithm SHA256).Hash -ne $entry.Hash) {
        throw "Input changed during verification: $($entry.Name)"
    }
}
if (-not $Finalize) {
    $inputs.Name | Sort-Object
    Write-Host "Verified $($inputs.Count) files and both signatures. Dry run: no copies, checksums or archive written."
    return
}
foreach ($entry in $inputs) {
    if ($entry.Source -ne $entry.Destination) {
        New-Item -ItemType Directory -Path (Split-Path -Parent $entry.Destination) -Force | Out-Null
        Copy-Item -LiteralPath $entry.Source -Destination $entry.Destination -Force
    }
    if ((Get-FileHash -LiteralPath $entry.Destination -Algorithm SHA256).Hash -ne $entry.Hash) {
        throw "Staged file differs from the verified input: $($entry.Name)"
    }
}
$ordered = @($inputs | Sort-Object Name)
$checksumLines = @($ordered | ForEach-Object { "$($_.Hash.ToLowerInvariant())  $($_.Name)" })
[IO.File]::WriteAllText($checksumsPath, ($checksumLines -join "`n") + "`n", $utf8)
$archiveInputs = @($ordered) + [pscustomobject]@{ Name = 'SHA256SUMS.txt'; Destination = $checksumsPath; Hash = (Get-FileHash -LiteralPath $checksumsPath -Algorithm SHA256).Hash }

Add-Type -AssemblyName System.IO.Compression.FileSystem
$zip = [IO.Compression.ZipFile]::Open($temporaryArchive, [IO.Compression.ZipArchiveMode]::Create)
try {
    foreach ($entry in $archiveInputs) {
        [IO.Compression.ZipFileExtensions]::CreateEntryFromFile($zip, $entry.Destination, "$archivePrefix$($entry.Name)", [IO.Compression.CompressionLevel]::Optimal) | Out-Null
    }
} finally { $zip.Dispose() }
$zip = [IO.Compression.ZipFile]::OpenRead($temporaryArchive)
try {
    if ($zip.Entries.Count -ne $archiveInputs.Count) { throw 'Archive entry count does not match the allowlist.' }
    foreach ($entry in $archiveInputs) {
        $archived = $zip.GetEntry("$archivePrefix$($entry.Name)")
        if ($null -eq $archived) { throw "Archive is missing $($entry.Name)." }
        $stream = $archived.Open()
        $sha = [Security.Cryptography.SHA256]::Create()
        try { $hash = [BitConverter]::ToString($sha.ComputeHash($stream)).Replace('-', '') }
        finally { $stream.Dispose(); $sha.Dispose() }
        if ($hash -ne $entry.Hash) { throw "Archive contents failed SHA256 verification: $($entry.Name)" }
    }
} finally { $zip.Dispose() }
Move-Item -LiteralPath $temporaryArchive -Destination $archive
$archiveHash = (Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText($archiveChecksum, "$archiveHash  $archiveName`n", $utf8)
Write-Host "Verified $($archiveInputs.Count) archive entries: $archive"
Write-Host "SHA256 $archiveHash"
