[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$manifestText = Get-Content -LiteralPath (Join-Path $repositoryRoot 'Cargo.toml') -Raw
$versionMatch = [Regex]::Match(
    $manifestText,
    '(?m)^version\s*=\s*"(?<version>\d+\.\d+\.\d+)"\s*$')
if (-not $versionMatch.Success) {
    throw 'Cargo.toml does not contain one numeric three-part workspace version.'
}
$version = $versionMatch.Groups['version'].Value
$fourPartVersion = "$version.0"

[xml] $project = Get-Content -LiteralPath (
    Join-Path $repositoryRoot 'apps\ResticPal.UI\ResticPal.UI.csproj') -Raw
foreach ($propertyName in @('Version', 'InformationalVersion')) {
    $node = $project.SelectSingleNode("/Project/PropertyGroup/$propertyName")
    $value = if ($null -eq $node) { $null } else { $node.InnerText }
    if ($value -cne $version) {
        throw "WinUI $propertyName '$value' does not match workspace version '$version'."
    }
}
foreach ($propertyName in @('AssemblyVersion', 'FileVersion')) {
    $node = $project.SelectSingleNode("/Project/PropertyGroup/$propertyName")
    $value = if ($null -eq $node) { $null } else { $node.InnerText }
    if ($value -cne $fourPartVersion) {
        throw "WinUI $propertyName '$value' does not match '$fourPartVersion'."
    }
}

[xml] $applicationManifest = Get-Content -LiteralPath (
    Join-Path $repositoryRoot 'apps\ResticPal.UI\app.manifest') -Raw
$identity = $applicationManifest.SelectSingleNode("/*[local-name()='assembly']/*[local-name()='assemblyIdentity']")
if ($null -eq $identity -or $identity.GetAttribute('version') -cne $fourPartVersion) {
    throw "The WinUI application manifest identity must use version $fourPartVersion."
}

$lockText = Get-Content -LiteralPath (Join-Path $repositoryRoot 'Cargo.lock') -Raw
$workspacePackages = [Regex]::Matches(
    $lockText,
    '(?ms)\[\[package\]\]\s*name = "(?<name>resticpal-[^"]+)"\s*version = "(?<version>[^"]+)"')
if ($workspacePackages.Count -ne 5) {
    throw "Expected five resticpal workspace packages in Cargo.lock, found $($workspacePackages.Count)."
}
foreach ($package in $workspacePackages) {
    if ($package.Groups['version'].Value -cne $version) {
        throw "Cargo.lock package $($package.Groups['name'].Value) is version $($package.Groups['version'].Value), expected $version."
    }
}

$publicKeyPath = Join-Path $repositoryRoot 'config\update-public-key.txt'
$publicKey = (Get-Content -LiteralPath $publicKeyPath -Raw).Trim()
try {
    $publicKeyBytes = [Convert]::FromBase64String($publicKey)
} catch [FormatException] {
    throw 'config/update-public-key.txt is not valid base64.'
}
if ($publicKeyBytes.Length -ne 32) {
    throw "The embedded update public key is $($publicKeyBytes.Length) bytes instead of 32."
}

$trackedPrivateKeys = @(& git -C $repositoryRoot ls-files '*.priv' '*private*key*')
if ($trackedPrivateKeys.Count -gt 0) {
    throw "Private key material must not be tracked: $($trackedPrivateKeys -join ', ')"
}

$installerScript = Get-Content -LiteralPath (
    Join-Path $repositoryRoot 'scripts\Build-Installer.ps1') -Raw
foreach ($requiredInput in @(
    '-p:Version=$Version',
    '-p:AssemblyVersion="$Version.0"',
    '-p:FileVersion="$Version.0"',
    '-p:InformationalVersion=$Version',
    '-d "ProductVersion=$Version"'
)) {
    if (-not $installerScript.Contains($requiredInput)) {
        throw "Build-Installer.ps1 does not propagate the synchronized version via $requiredInput."
    }
}

$appCastScript = Get-Content -LiteralPath (
    Join-Path $repositoryRoot 'scripts\New-UpdateAppcast.ps1') -Raw
if (-not $appCastScript.Contains("--file-version', `$Version")) {
    throw 'New-UpdateAppcast.ps1 does not propagate the synchronized appcast version.'
}
if (-not $appCastScript.Contains("[string] `$PackageHost = 'UpdatesHost'")) {
    throw 'New-UpdateAppcast.ps1 must default to the direct updates host.'
}

$publishScript = Get-Content -LiteralPath (
    Join-Path $repositoryRoot 'scripts\Publish-Release.ps1') -Raw
foreach ($releaseSafetyInvariant in @(
    "[string] `$PackageHost = 'UpdatesHost'",
    '[switch] $Stage',
    '[switch] $Finalize',
    '[string] $UpdateQualificationPath',
    '$Run.headSha -cne $head',
    "`$Run.status -cne 'completed' -or `$Run.conclusion -cne 'success'",
    "`$Run.event -ceq 'push' -and `$Run.headBranch -ceq `$tag",
    'release-manifest.json',
    'schema = 2',
    'previous-published-client-prompted-update',
    'Assert-DirectPackageMirror -Msi $msi',
    'Assert-RemoteTagTarget',
    'Signed Windows CI run $RunId',
    'Previous signed fallback feed $previousTag',
    "'--notes-file', `$stagedNotes.FullName, '--draft'",
    "'--draft=false', '--latest'",
    'Assert-FeedAssetLabels',
    'Assert-FinalizedReleaseAssets'
)) {
    if (-not $publishScript.Contains($releaseSafetyInvariant)) {
        throw "Publish-Release.ps1 is missing release safety invariant: $releaseSafetyInvariant"
    }
}

Write-Host "OK: Rust, WinUI, application manifest, installer input, and appcast source version are $version."
