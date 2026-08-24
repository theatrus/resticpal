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

$trayBuildScript = Get-Content -LiteralPath (
    Join-Path $repositoryRoot 'apps\resticpal-tray\build.rs') -Raw
foreach ($trayManifestInvariant in @(
    '1 24 "{manifest}"',
    '.manifest_required()',
    '"x86_64" => "amd64"',
    'processorArchitecture="{processor_architecture}"',
    'requestedExecutionLevel level="asInvoker" uiAccess="false"',
    '>true/pm</dpiAware>',
    '>PerMonitorV2, PerMonitor</dpiAwareness>',
    'name="Microsoft.Windows.Common-Controls"',
    'version="6.0.0.0"'
)) {
    if (-not $trayBuildScript.Contains($trayManifestInvariant)) {
        throw "The tray build is missing DPI manifest invariant: $trayManifestInvariant"
    }
}
if ($trayBuildScript.Contains('requestedExecutionLevel level="requireAdministrator"')) {
    throw 'The tray application manifest must remain asInvoker.'
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
if (-not $appCastScript.Contains(
        "`$appCastUrl = 'https://updates.resticpal.com/appcast-v2.xml'")) {
    throw 'New-UpdateAppcast.ps1 must generate the version-2 update feed.'
}
if (-not $appCastScript.Contains("'--output-file-name', 'appcast-v2'")) {
    throw 'New-UpdateAppcast.ps1 must emit appcast-v2.xml and its detached signature.'
}

$updateTrust = Get-Content -LiteralPath (
    Join-Path $repositoryRoot 'apps\ResticPal.UI\Services\UpdateTrust.cs') -Raw
foreach ($requiredFeed in @(
    'https://updates.resticpal.com/appcast-v2.xml',
    'https://github.com/theatrus/resticpal/releases/latest/download/appcast-v2.xml'
)) {
    if (-not $updateTrust.Contains($requiredFeed)) {
        throw "The WinUI updater is missing the version-2 feed: $requiredFeed"
    }
}
if ($updateTrust.Contains('https://updates.resticpal.com/appcast.xml') -or
    $updateTrust.Contains(
        'https://github.com/theatrus/resticpal/releases/latest/download/appcast.xml')) {
    throw 'The WinUI updater must not consume the legacy update feed.'
}

$publishScript = Get-Content -LiteralPath (
    Join-Path $repositoryRoot 'scripts\Publish-Release.ps1') -Raw
foreach ($releaseSafetyInvariant in @(
    "[string] `$PackageHost = 'UpdatesHost'",
    '[switch] $Stage',
    '[switch] $Finalize',
    '[string] $UpdateQualificationPath',
    '[string] $AutomaticUpdateQualificationPath',
    '$Run.headSha -cne $head',
    "`$Run.status -cne 'completed' -or `$Run.conclusion -cne 'success'",
    "`$Run.event -ceq 'push' -and `$Run.headBranch -ceq `$tag",
    'release-manifest.json',
    '$bridgeManifestSchema = [uint64]5',
    'schema = if ($bridgeQualification)',
    '$steadyStateManifestSchema = [uint64]6',
    '$frozenLegacyAppCastLength = [uint64]969',
    "`$frozenLegacyAppCastSha256 = 'eeffa6fc466c0d3f5c95043538742665732a044118286ff94368c163fef7a4e2'",
    '$frozenLegacySignatureLength = [uint64]88',
    "`$frozenLegacySignatureSha256 = '85d591ce0a7d936be3da429583737838f3ef075565a483c32dc7faaa6085d377'",
    '$manifest[''dual_named_feed''] = [ordered]@{',
    'candidate_v2_feed',
    'frozen_legacy_feed',
    'Assert-UpdateQualificationPair',
    'Test-UpdateQualificationBindingState',
    'Assert-DirectPackageMirror -Msi $msi',
    'Assert-RemoteTagTarget',
    'Assert-LatestStableReleaseIsCandidate',
    'if ($parsedReleaseVersion -lt $firstV2Version) {',
    '$isOneTimeBridgeRelease = ($parsedReleaseVersion -eq $firstV2Version)',
    'Signed Windows CI run $RunId',
    "`$packageAssetNames = @(`$expectedMsiName, 'SHA256SUMS.txt', `$license.Name, `$notices.Name)",
    '$stageAssetNames = @($packageAssetNames)',
    '$stageRequiredAssetNames = @($packageAssetNames)',
    '$finalizationBaseAssetNames = @($expectedMsiName, $license.Name, $notices.Name)',
    '$finalAssetNames = @($packageAssetNames + $legacyFeedAssetNames + $v2FeedAssetNames)',
    '$checksumAssets = @($release.assets | Where-Object name -CEQ $checksumFile.Name)',
    '$checksumAssets.Count -eq 1',
    '$checksumAssets.Count -eq 0',
    'uploaded first and required again before either go-live boundary.',
    'Stage must never add, replace, or carry an appcast',
    'Assert-OneTimeBridgeFeed',
    'Assert-FrozenLegacyFeed',
    'Get-FrozenLegacyFeed',
    'Copy-Item -LiteralPath $appCast.FullName -Destination $legacyAppCastPath -Force',
    '-LiteralPath $appCastSignature.FullName',
    '-Destination $legacyAppCastSignaturePath',
    'Signed v2 update feed $Version',
    'Signed dual update feed $Version',
    'Frozen legacy update feed $frozenLegacyVersion',
    "`$metadata.File.Name -ceq 'appcast.xml'",
    "`$metadata.File.Name -ceq 'appcast-v2.xml'",
    'candidate v2 XML',
    'Write-StagedReleaseNotes',
    '$stageDeploymentId = [Guid]::NewGuid().ToString(''N'')',
    '<!-- resticpal-stage-deploy: $stageDeploymentId -->',
    'Write-FinalReleaseNotes',
    '<!-- resticpal-release-deploy:',
    'releases/download/$tag/SHA256SUMS.txt',
    'releases/download/$tag/appcast.xml',
    "-Url 'https://updates.resticpal.com/appcast-v2.xml'",
    "-Url 'https://updates.resticpal.com/appcast-v2.xml.signature'",
    "-Url 'https://updates.resticpal.com/appcast.xml'",
    "-Url 'https://updates.resticpal.com/appcast.xml.signature'",
    "-Url 'https://github.com/theatrus/resticpal/releases/latest/download/appcast-v2.xml'",
    "-Url 'https://github.com/theatrus/resticpal/releases/latest/download/appcast-v2.xml.signature'",
    "-Url 'https://github.com/theatrus/resticpal/releases/latest/download/appcast.xml'",
    "-Url 'https://github.com/theatrus/resticpal/releases/latest/download/appcast.xml.signature'",
    'Wait-HostedFileMatches',
    'appcast-v2.xml',
    'New-UpdateQualificationProbe.ps1',
    "'--notes-file', `$stagedNotes.FullName, '--draft'",
    "'--draft=false', '--title', `"resticpal `$Version`"",
    'Assert-FeedAssetLabels',
    'Assert-FinalizedReleaseAssets'
)) {
    if (-not $publishScript.Contains($releaseSafetyInvariant)) {
        throw "Publish-Release.ps1 is missing release safety invariant: $releaseSafetyInvariant"
    }
}

$stagedNotesWriter = [Regex]::Match(
    $publishScript,
    '(?s)function Write-StagedReleaseNotes \{(?<body>.*?)\r?\n\}\r?\n\r?\nfunction Write-FinalReleaseNotes \{')
if (-not $stagedNotesWriter.Success) {
    throw 'Publish-Release.ps1 must retain a distinct staged-release notes writer.'
}
foreach ($stageRecoveryInvariant in @(
    "`$stageDeploymentId = [Guid]::NewGuid().ToString('N')",
    '<!-- resticpal-signed-ci-run: $RunId -->',
    '<!-- resticpal-stage-deploy: $stageDeploymentId -->'
)) {
    if (-not $stagedNotesWriter.Groups['body'].Value.Contains($stageRecoveryInvariant)) {
        throw "Staged release notes are missing recovery invariant: $stageRecoveryInvariant"
    }
}

foreach ($obsoleteReleaseFlow in @(
    'Assert-DualNamedFeed',
    'This one-time dual-named legacy bridge is restricted to v1.0.7.'
)) {
    if ($publishScript.IndexOf(
            $obsoleteReleaseFlow,
            [StringComparison]::OrdinalIgnoreCase) -ge 0) {
        throw "Publish-Release.ps1 retains obsolete release flow: $obsoleteReleaseFlow"
    }
}

$latestCandidateCheckCount = [Regex]::Matches(
    $publishScript,
    '(?m)^\s*Assert-LatestStableReleaseIsCandidate\s*$').Count
if ($latestCandidateCheckCount -ne 6) {
    throw ("Publish-Release.ps1 must enforce the actual latest release at six " +
           "critical call sites; found $latestCandidateCheckCount.")
}
$forceLatestCount = [Regex]::Matches(
    $publishScript,
    "'--latest'").Count
if ($forceLatestCount -ne 0) {
    throw ("Publish-Release.ps1 must never force --latest across a concurrent release; " +
           "found $forceLatestCount occurrences.")
}

$checksumRepairBlock = [Regex]::Match(
    $publishScript,
    ('(?s)\$checksumAssets = @\(\$release\.assets \| Where-Object name -CEQ ' +
     '\$checksumFile\.Name\)(?<body>.*?)\r?\n\s*Set-QualificationBindings'))
if (-not $checksumRepairBlock.Success) {
    throw 'Publish-Release.ps1 must retain the anchored interrupted-checksum repair preflight.'
}
$checksumRepairBody = $checksumRepairBlock.Groups['body'].Value
foreach ($checksumInvariant in @(
    '$checksumAssets.Count -eq 1 -and',
    '-not (Test-RemoteAssetMatches -Release $release -File $stageChecksumFile) -and',
    '-not (Test-RemoteAssetMatches -Release $release -File $checksumFile)',
    '$checksumAssets.Count -eq 0',
    'it will be restored before update metadata advances.'
)) {
    if (-not $checksumRepairBody.Contains($checksumInvariant)) {
        throw "Publish-Release.ps1 checksum repair preflight is missing: $checksumInvariant"
    }
}
$missingChecksumBranch = [Regex]::Match(
    $checksumRepairBody,
    '(?s)if \(\$checksumAssets\.Count -eq 0\) \{(?<body>.*?)\r?\n\s*\}')
if (-not $missingChecksumBranch.Success -or
    $missingChecksumBranch.Groups['body'].Value.Contains('throw')) {
    throw 'A missing checksum must remain a non-fatal interrupted --clobber repair state.'
}

$orderedUploadBlock = [Regex]::Match(
    $publishScript,
    ('(?s)\$orderedMetadata = if \(\$isOneTimeBridgeRelease\) \{' +
     '(?<bridge>.*?)\r?\n\s*\} else \{(?<steady>.*?)\r?\n\s*\}' +
     '\r?\n\s*foreach \(\$metadata in \$orderedMetadata\)'))
if (-not $orderedUploadBlock.Success) {
    throw 'Publish-Release.ps1 must retain one anchored bridge/steady metadata upload block.'
}
foreach ($uploadOrder in @(
    [pscustomobject]@{
        Name = 'v1.0.7 bridge'
        Body = $orderedUploadBlock.Groups['bridge'].Value
        Tokens = @(
            '[pscustomobject]@{ File = $checksumFile; Label = $null }',
            '[pscustomobject]@{ File = $appCastSignature; Label = $v2FeedLabel }',
            '[pscustomobject]@{ File = $appCast; Label = $v2FeedLabel }',
            '[pscustomobject]@{ File = $legacyAppCastSignature; Label = $legacyFeedLabel }',
            '[pscustomobject]@{ File = $legacyAppCast; Label = $legacyFeedLabel }')
    },
    [pscustomobject]@{
        Name = 'v1.0.8+ steady-state'
        Body = $orderedUploadBlock.Groups['steady'].Value
        Tokens = @(
            '[pscustomobject]@{ File = $checksumFile; Label = $null }',
            '[pscustomobject]@{ File = $legacyAppCastSignature; Label = $legacyFeedLabel }',
            '[pscustomobject]@{ File = $legacyAppCast; Label = $legacyFeedLabel }',
            '[pscustomobject]@{ File = $appCastSignature; Label = $v2FeedLabel }',
            '[pscustomobject]@{ File = $appCast; Label = $v2FeedLabel }')
    }
)) {
    $uploadCursor = 0
    foreach ($token in $uploadOrder.Tokens) {
        $tokenIndex = $uploadOrder.Body.IndexOf(
            $token,
            $uploadCursor,
            [StringComparison]::Ordinal)
        if ($tokenIndex -lt 0) {
            throw ("Publish-Release.ps1 does not preserve the anchored " +
                   "$($uploadOrder.Name) upload order at: $token")
        }
        $uploadCursor = $tokenIndex + $token.Length
    }
}

if (-not $publishScript.Contains(
        ". (Join-Path `$PSScriptRoot 'ReleaseQualification.ps1')")) {
    throw 'Publish-Release.ps1 must use the shared, executable qualification validator.'
}
$qualificationScript = Get-Content -LiteralPath (
    Join-Path $repositoryRoot 'scripts\ReleaseQualification.ps1') -Raw
foreach ($qualificationInvariant in @(
    'Read-UpdateQualificationEvidence',
    'Assert-UpdateQualificationPair',
    'Test-UpdateQualificationBindingState',
    'previous-published-client-prompted-update',
    'previous-published-client-automatic-update',
    'previous-published-service-automatic-update-bridge',
    'release_manifest.candidate_v2_feed',
    'release_manifest.frozen_legacy_feed',
    'published_release_api must contain exactly one frozen',
    '$script:FrozenLegacyAppCastLength = [uint64]969',
    'eeffa6fc466c0d3f5c95043538742665732a044118286ff94368c163fef7a4e2',
    '$script:FrozenLegacySignatureLength = [uint64]88',
    '85d591ce0a7d936be3da429583737838f3ef075565a483c32dc7faaa6085d377',
    'qualification-harness-via-published-service-ipc',
    'update_signature_invalid',
    'candidate_tray_probe',
    'candidate_installer_parent_process_id',
    'no_user_confirmation_or_dialog_intervention'
)) {
    if (-not $qualificationScript.Contains($qualificationInvariant)) {
        throw "ReleaseQualification.ps1 is missing safety invariant: $qualificationInvariant"
    }
}

& (Join-Path $PSScriptRoot 'Test-ReleaseQualification.ps1')

Write-Host "OK: Rust, WinUI, tray/UI manifests, installer input, and appcast source version are $version."
