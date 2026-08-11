[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $MsiPath,
    [string] $Version,
    [string] $OutputDirectory,
    [ValidateSet('GitHub', 'UpdatesHost')]
    [string] $PackageHost = 'GitHub',
    [string] $KeyPath = (Join-Path ([Environment]::GetFolderPath('UserProfile')) 'Dropbox\resticpal\keys\updates')
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'artifacts')).TrimEnd('\')
$resolvedMsiPath = (Resolve-Path -LiteralPath $MsiPath).Path

if ([string]::IsNullOrWhiteSpace($Version)) {
    $manifestText = Get-Content -LiteralPath (Join-Path $repositoryRoot 'Cargo.toml') -Raw
    $versionMatch = [Regex]::Match(
        $manifestText,
        '(?m)^version\s*=\s*"(?<version>\d+\.\d+\.\d+)"\s*$')
    if (-not $versionMatch.Success) {
        throw 'Unable to determine the product version from Cargo.toml.'
    }
    $Version = $versionMatch.Groups['version'].Value
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "Update version must have three numeric parts: $Version"
}

$expectedMsiName = "resticpal-$Version-x64.msi"
if ([IO.Path]::GetFileName($resolvedMsiPath) -cne $expectedMsiName) {
    throw "Expected MSI name $expectedMsiName, got $([IO.Path]::GetFileName($resolvedMsiPath))."
}
$tag = "v$Version"
$releaseBaseUrl = if ($PackageHost -eq 'UpdatesHost') {
    "https://updates.resticpal.com/releases/$tag"
} else {
    "https://github.com/theatrus/resticpal/releases/download/$tag"
}
$expectedDownload = "$releaseBaseUrl/$expectedMsiName"

$authenticode = Get-AuthenticodeSignature -LiteralPath $resolvedMsiPath
if ($authenticode.Status -ne 'Valid') {
    throw "The release MSI is not validly Authenticode-signed: $($authenticode.Status)."
}
if ($authenticode.SignerCertificate.Subject -notmatch '(^|, )CN=StackFoundry LLC(,|$)') {
    throw "The release MSI is signed by an unexpected publisher: $($authenticode.SignerCertificate.Subject)"
}

$privateKeyPath = Join-Path $KeyPath 'NetSparkle_Ed25519.priv'
$publicKeyPath = Join-Path $KeyPath 'NetSparkle_Ed25519.pub'
foreach ($keyFile in @($privateKeyPath, $publicKeyPath)) {
    if (-not (Test-Path -LiteralPath $keyFile -PathType Leaf)) {
        throw "The updater key file is missing: $keyFile"
    }
}

$trustedPublicKey = (Get-Content -LiteralPath (Join-Path $repositoryRoot 'config\update-public-key.txt') -Raw).Trim()
$backupPublicKey = (Get-Content -LiteralPath $publicKeyPath -Raw).Trim()
if ($trustedPublicKey -cne $backupPublicKey) {
    throw 'The Dropbox updater public key does not match the public key embedded in resticpal.'
}

if ([string]::IsNullOrWhiteSpace($OutputDirectory)) {
    $OutputDirectory = Join-Path $artifactRoot "updates\v$Version"
} elseif (-not [IO.Path]::IsPathRooted($OutputDirectory)) {
    $OutputDirectory = Join-Path $repositoryRoot $OutputDirectory
}
$resolvedOutput = [IO.Path]::GetFullPath($OutputDirectory).TrimEnd('\')
if (-not $resolvedOutput.StartsWith(
        $artifactRoot + '\',
        [StringComparison]::OrdinalIgnoreCase)) {
    throw "Update output must remain below the repository artifact directory: $resolvedOutput"
}
if (Test-Path -LiteralPath $resolvedOutput) {
    Remove-Item -LiteralPath $resolvedOutput -Recurse -Force
}
New-Item -ItemType Directory -Path $resolvedOutput | Out-Null

if ($PackageHost -eq 'UpdatesHost') {
    Add-Type -AssemblyName System.Net.Http
    $handler = [Net.Http.HttpClientHandler]::new()
    $handler.AllowAutoRedirect = $false
    $client = [Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromMinutes(20)
    $response = $null
    $stream = $null
    $hasher = $null
    try {
        $response = $client.GetAsync(
            $expectedDownload,
            [Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
        if ($response.StatusCode -ne [Net.HttpStatusCode]::OK) {
            throw "The updates-host MSI returned HTTP $([int]$response.StatusCode); expected 200 without a redirect."
        }
        $localLength = (Get-Item -LiteralPath $resolvedMsiPath).Length
        if ($response.Content.Headers.ContentLength -ne $localLength) {
            throw "The updates-host MSI length does not match the signed CI artifact."
        }
        $stream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $hasher = [Security.Cryptography.SHA256]::Create()
        $remoteHash = [BitConverter]::ToString($hasher.ComputeHash($stream)).Replace('-', '')
        $localHash = (Get-FileHash -LiteralPath $resolvedMsiPath -Algorithm SHA256).Hash
        if ($remoteHash -cne $localHash) {
            throw "The updates-host MSI hash does not match the signed CI artifact."
        }
    } finally {
        if ($null -ne $hasher) { $hasher.Dispose() }
        if ($null -ne $stream) { $stream.Dispose() }
        if ($null -ne $response) { $response.Dispose() }
        $client.Dispose()
        $handler.Dispose()
    }
}

Push-Location $repositoryRoot
try {
    & dotnet tool restore
    if ($LASTEXITCODE -ne 0) {
        throw "Restoring the pinned NetSparkle tool failed with exit code $LASTEXITCODE."
    }

    $appCastUrl = 'https://updates.resticpal.com/appcast.xml'
    $arguments = @(
        '--single-file', $resolvedMsiPath,
        '--file-version', $Version,
        '--appcast-output-directory', $resolvedOutput,
        '--base-url', $releaseBaseUrl,
        '--link-tag', $appCastUrl,
        '--product-name', 'resticpal',
        '--os', 'windows-x64',
        '--key-path', $KeyPath,
        '--human-readable'
    )
    & dotnet tool run netsparkle-generate-appcast -- @arguments
    if ($LASTEXITCODE -ne 0) {
        throw "Generating the NetSparkle appcast failed with exit code $LASTEXITCODE."
    }
} finally {
    Pop-Location
}

$appCastPath = Join-Path $resolvedOutput 'appcast.xml'
$appCastSignaturePath = "$appCastPath.signature"
foreach ($outputFile in @($appCastPath, $appCastSignaturePath)) {
    if (-not (Test-Path -LiteralPath $outputFile -PathType Leaf)) {
        throw "The NetSparkle generator did not create $outputFile."
    }
}

$appCastSignature = (Get-Content -LiteralPath $appCastSignaturePath -Raw).Trim()
Push-Location $repositoryRoot
try {
    $verificationOutput = @(& dotnet tool run netsparkle-generate-appcast -- `
        --verify $appCastPath `
        --signature $appCastSignature `
        --key-path $KeyPath 2>&1)
    $verificationExitCode = $LASTEXITCODE
    $verificationOutput | Write-Output
    $signatureValid = @($verificationOutput | Where-Object { $_ -eq 'Signature valid' }).Count -gt 0
    if ($verificationExitCode -ne 0 -or -not $signatureValid) {
        throw 'The generated appcast does not verify with the public key embedded in resticpal.'
    }
} finally {
    Pop-Location
}

[xml] $appCast = Get-Content -LiteralPath $appCastPath -Raw
$namespace = 'http://www.andymatuschak.org/xml-namespaces/sparkle'
$expectedAppCastLink = $appCastUrl
$appCastLink = $appCast.SelectSingleNode('/rss/channel/link')
if ($null -eq $appCastLink -or $appCastLink.InnerText -cne $expectedAppCastLink) {
    throw 'The generated appcast does not identify the primary update feed.'
}
$enclosure = $appCast.SelectSingleNode('/rss/channel/item/enclosure')
if ($null -eq $enclosure) {
    throw 'The generated appcast does not contain an update enclosure.'
}
$invalidEnclosure = (
    $enclosure.GetAttribute('url') -cne $expectedDownload -or
    $enclosure.GetAttribute('version', $namespace) -cne $Version -or
    $enclosure.GetAttribute('shortVersionString', $namespace) -cne $Version -or
    $enclosure.GetAttribute('os', $namespace) -cne 'windows-x64' -or
    [string]::IsNullOrWhiteSpace($enclosure.GetAttribute('signature', $namespace))
)
if ($invalidEnclosure) {
    throw 'The generated appcast enclosure does not match the release identity.'
}
if ([uint64]$enclosure.GetAttribute('length') -ne (Get-Item -LiteralPath $resolvedMsiPath).Length) {
    throw 'The generated appcast reports the wrong MSI size.'
}

Write-Host "Created and verified the signed resticpal $Version appcast at $resolvedOutput"
Get-Item -LiteralPath $appCastPath, $appCastSignaturePath
