[CmdletBinding()]
param(
    [string] $Version,
    [uint64] $RunId,
    [string] $ReleaseNotesPath,
    [switch] $Publish,
    [string] $KeyPath = (Join-Path ([Environment]::GetFolderPath('UserProfile')) 'Dropbox\resticpal\keys\updates')
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'artifacts')).TrimEnd('\')
$manifestText = Get-Content -LiteralPath (Join-Path $repositoryRoot 'Cargo.toml') -Raw
$versionMatch = [Regex]::Match(
    $manifestText,
    '(?m)^version\s*=\s*"(?<version>\d+\.\d+\.\d+)"\s*$')
if (-not $versionMatch.Success) {
    throw 'Unable to determine the product version from Cargo.toml.'
}
$sourceVersion = $versionMatch.Groups['version'].Value
if ([string]::IsNullOrWhiteSpace($Version)) {
    $Version = $sourceVersion
}
if ($Version -cne $sourceVersion) {
    throw "Requested release $Version does not match the source version $sourceVersion."
}

$head = (& git -C $repositoryRoot rev-parse HEAD).Trim()
if ($LASTEXITCODE -ne 0 -or $head -notmatch '^[0-9a-f]{40}$') {
    throw 'Unable to resolve the release commit.'
}

if ($RunId -eq 0) {
    $runs = @(& gh run list `
        --repo theatrus/resticpal `
        --workflow ci.yml `
        --branch main `
        --commit $head `
        --status success `
        --limit 20 `
        --json databaseId,headSha,conclusion | ConvertFrom-Json)
    $run = $runs | Where-Object { $_.headSha -ceq $head -and $_.conclusion -eq 'success' } |
        Select-Object -First 1
    if ($null -eq $run) {
        throw "No successful Windows CI run exists for commit $head."
    }
    $RunId = [uint64]$run.databaseId
}

$releaseRoot = Join-Path $artifactRoot "release\v$Version"
if (-not $releaseRoot.StartsWith($artifactRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to prepare a release outside the artifact directory: $releaseRoot"
}
if (Test-Path -LiteralPath $releaseRoot) {
    Remove-Item -LiteralPath $releaseRoot -Recurse -Force
}
$downloadRoot = Join-Path $releaseRoot 'ci-artifact'
New-Item -ItemType Directory -Path $downloadRoot | Out-Null

& gh run download $RunId `
    --repo theatrus/resticpal `
    --name resticpal-windows-x64 `
    --dir $downloadRoot
if ($LASTEXITCODE -ne 0) {
    throw "Downloading CI artifact from run $RunId failed with exit code $LASTEXITCODE."
}

$msiFiles = @(Get-ChildItem -LiteralPath $downloadRoot -Recurse -Filter '*.msi' -File)
if ($msiFiles.Count -ne 1) {
    throw "Expected one MSI in CI run $RunId, found $($msiFiles.Count)."
}
$msi = $msiFiles[0]
$feedRoot = Join-Path $releaseRoot 'feed'
& (Join-Path $PSScriptRoot 'New-UpdateAppcast.ps1') `
    -MsiPath $msi.FullName `
    -Version $Version `
    -OutputDirectory $feedRoot `
    -KeyPath $KeyPath
if ($LASTEXITCODE -ne 0) {
    throw 'Signed appcast preparation failed.'
}

$appCast = Get-Item -LiteralPath (Join-Path $feedRoot 'appcast.xml')
$appCastSignature = Get-Item -LiteralPath (Join-Path $feedRoot 'appcast.xml.signature')
$checksumPath = Join-Path $releaseRoot 'SHA256SUMS.txt'
$checksumLines = @($msi, $appCast, $appCastSignature) |
    Sort-Object Name |
    ForEach-Object {
        $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        "$hash *$($_.Name)"
    }
$checksumLines | Set-Content -LiteralPath $checksumPath -Encoding ascii

$assets = @(
    $msi.FullName,
    $appCast.FullName,
    $appCastSignature.FullName,
    $checksumPath,
    (Join-Path $repositoryRoot 'LICENSE'),
    (Join-Path $repositoryRoot 'THIRD-PARTY-NOTICES.md')
)

if ($Publish) {
    if ([string]::IsNullOrWhiteSpace($ReleaseNotesPath)) {
        throw '-ReleaseNotesPath is required with -Publish.'
    }
    $resolvedNotes = (Resolve-Path -LiteralPath $ReleaseNotesPath).Path
    if (-not [string]::IsNullOrWhiteSpace((& git -C $repositoryRoot status --porcelain))) {
        throw 'The repository must be clean before publishing a release.'
    }
    & git -C $repositoryRoot fetch origin main
    if ($LASTEXITCODE -ne 0) {
        throw 'Fetching origin/main failed.'
    }
    $originMain = (& git -C $repositoryRoot rev-parse origin/main).Trim()
    if ($head -cne $originMain) {
        throw "Release commit $head is not current origin/main $originMain."
    }

    $tag = "v$Version"
    & gh release view $tag --repo theatrus/resticpal *> $null
    if ($LASTEXITCODE -eq 0) {
        throw "GitHub release $tag already exists."
    }

    $arguments = @(
        'release', 'create', $tag,
        '--repo', 'theatrus/resticpal',
        '--target', $head,
        '--title', "resticpal $Version",
        '--notes-file', $resolvedNotes
    ) + $assets
    & gh @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Publishing GitHub release $tag failed with exit code $LASTEXITCODE."
    }
    Write-Host "Published resticpal $Version from signed CI run $RunId."
} else {
    Write-Host "Prepared resticpal $Version release assets from signed CI run $RunId at $releaseRoot"
    Write-Host 'Re-run with -Publish and -ReleaseNotesPath after reviewing these files.'
}

Get-Item -LiteralPath $assets
