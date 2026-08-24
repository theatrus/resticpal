[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $CandidateVersion,

    [Parameter(Mandatory)]
    [string] $OutputDirectory,

    [string] $KeyPath = (Join-Path ([Environment]::GetFolderPath('UserProfile')) 'Dropbox\resticpal\keys\updates')
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'artifacts')).TrimEnd('\')

if ($CandidateVersion -cnotmatch '^\d+\.\d+\.\d+$') {
    throw "Candidate version must have three numeric parts: $CandidateVersion"
}
$candidate = [Version]$CandidateVersion
if ($candidate.Build -eq [int]::MaxValue) {
    throw 'The candidate patch version cannot be incremented for a qualification probe.'
}
$probeVersion = [Version]::new($candidate.Major, $candidate.Minor, $candidate.Build + 1).ToString(3)

if (-not [IO.Path]::IsPathRooted($OutputDirectory)) {
    $OutputDirectory = Join-Path $repositoryRoot $OutputDirectory
}
$resolvedOutput = [IO.Path]::GetFullPath($OutputDirectory).TrimEnd('\')
if (-not $resolvedOutput.StartsWith(
        $artifactRoot + '\',
        [StringComparison]::OrdinalIgnoreCase)) {
    throw "Qualification probe output must remain below the repository artifact directory: $resolvedOutput"
}
if (Test-Path -LiteralPath $resolvedOutput) {
    Remove-Item -LiteralPath $resolvedOutput -Recurse -Force
}
New-Item -ItemType Directory -Path $resolvedOutput | Out-Null

$privateKeyPath = Join-Path $KeyPath 'NetSparkle_Ed25519.priv'
$publicKeyPath = Join-Path $KeyPath 'NetSparkle_Ed25519.pub'
foreach ($keyFile in @($privateKeyPath, $publicKeyPath)) {
    if (-not (Test-Path -LiteralPath $keyFile -PathType Leaf)) {
        throw "The updater key file is missing: $keyFile"
    }
}
$trustedPublicKey = (
    Get-Content -LiteralPath (Join-Path $repositoryRoot 'config\update-public-key.txt') -Raw).Trim()
$backupPublicKey = (Get-Content -LiteralPath $publicKeyPath -Raw).Trim()
if ($trustedPublicKey -cne $backupPublicKey) {
    throw 'The updater public-key backup does not match the key embedded in resticpal.'
}

$payloadName = "resticpal-$probeVersion-x64.msi"
$payloadPath = Join-Path $resolvedOutput $payloadName
$payloadText = (
    "resticpal signed-update qualification sentinel`n" +
    "candidate=$CandidateVersion`n" +
    "advertised=$probeVersion`n" +
    'This is deliberately not a Windows Installer and must never be launched.' + "`n")
[IO.File]::WriteAllBytes(
    $payloadPath,
    [Text.UTF8Encoding]::new($false).GetBytes($payloadText))
$payload = Get-Item -LiteralPath $payloadPath
$payloadUrl = "https://updates.resticpal.com/releases/v$probeVersion/$payloadName"

# The outer appcast is signed by the real production update key. The enclosure
# signature is deliberately a well-formed but invalid Ed25519 signature, so the
# installed candidate must fetch and dispatch the package, then reject it before
# Windows Installer can start.
$invalidPackageSignature = [Convert]::ToBase64String([byte[]]::new(64))
$appCastPath = Join-Path $resolvedOutput 'appcast-v2-probe.xml'
$appCastSignaturePath = "$appCastPath.signature"
$xml = @"
<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0" xmlns:sparkle="http://www.andymatuschak.org/xml-namespaces/sparkle">
  <channel>
    <title>resticpal qualification probe</title>
    <link>https://updates.resticpal.com/appcast-v2.xml</link>
    <description>Signed release-pipeline probe; never publish this feed.</description>
    <language>en</language>
    <item>
      <title>resticpal $probeVersion qualification probe</title>
      <sparkle:version>$probeVersion</sparkle:version>
      <sparkle:shortVersionString>$probeVersion</sparkle:shortVersionString>
      <enclosure url="$payloadUrl" length="$($payload.Length)" type="application/octet-stream" sparkle:os="windows-x64" sparkle:version="$probeVersion" sparkle:shortVersionString="$probeVersion" sparkle:signature="$invalidPackageSignature" />
    </item>
  </channel>
</rss>
"@
[IO.File]::WriteAllText(
    $appCastPath,
    $xml.Replace("`r`n", "`n"),
    [Text.UTF8Encoding]::new($false))

Push-Location $repositoryRoot
try {
    & dotnet tool restore
    if ($LASTEXITCODE -ne 0) {
        throw "Restoring the pinned NetSparkle tool failed with exit code $LASTEXITCODE."
    }
    $signatureOutput = @(& dotnet tool run netsparkle-generate-appcast -- `
        --generate-signature $appCastPath `
        --key-path $KeyPath 2>&1)
    $signatureExitCode = $LASTEXITCODE
    $signatureLine = @($signatureOutput | Where-Object { [string]$_ -cmatch '^Signature: .+$' })
    if ($signatureExitCode -ne 0 -or $signatureLine.Count -ne 1) {
        throw "Signing the qualification-probe appcast failed: $($signatureOutput | Out-String)"
    }
    $appCastSignature = ([string]$signatureLine[0]).Substring('Signature: '.Length).Trim()
    [IO.File]::WriteAllText(
        $appCastSignaturePath,
        $appCastSignature,
        [Text.UTF8Encoding]::new($false))

    $verificationOutput = @(& dotnet tool run netsparkle-generate-appcast -- `
        --verify $appCastPath `
        --signature $appCastSignature `
        --key-path $KeyPath 2>&1)
    if ($LASTEXITCODE -ne 0 -or $verificationOutput -cnotcontains 'Signature valid') {
        throw 'The qualification-probe appcast signature did not verify.'
    }

    $payloadVerification = @(& dotnet tool run netsparkle-generate-appcast -- `
        --verify $payloadPath `
        --signature $invalidPackageSignature `
        --key-path $KeyPath 2>&1)
    if ($payloadVerification -contains 'Signature valid' -or
        $payloadVerification -cnotcontains 'Signature invalid') {
        throw 'The qualification-probe payload signature unexpectedly verified.'
    }
} finally {
    Pop-Location
}

[xml] $parsed = Get-Content -LiteralPath $appCastPath -Raw
$namespace = 'http://www.andymatuschak.org/xml-namespaces/sparkle'
$link = $parsed.SelectSingleNode('/rss/channel/link')
$enclosure = $parsed.SelectSingleNode('/rss/channel/item/enclosure')
if ($null -eq $link -or $link.InnerText -cne 'https://updates.resticpal.com/appcast-v2.xml' -or
    $null -eq $enclosure -or
    $enclosure.GetAttribute('url') -cne $payloadUrl -or
    $enclosure.GetAttribute('version', $namespace) -cne $probeVersion -or
    $enclosure.GetAttribute('shortVersionString', $namespace) -cne $probeVersion -or
    $enclosure.GetAttribute('os', $namespace) -cne 'windows-x64' -or
    $enclosure.GetAttribute('signature', $namespace) -cne $invalidPackageSignature -or
    [uint64]$enclosure.GetAttribute('length') -ne [uint64]$payload.Length) {
    throw 'The qualification-probe appcast did not retain its exact fail-closed package metadata.'
}

Write-Host (
    "Created signed resticpal $CandidateVersion candidate-tray probe for advertised " +
    "$probeVersion at $resolvedOutput")
Get-Item -LiteralPath $appCastPath, $appCastSignaturePath, $payloadPath
