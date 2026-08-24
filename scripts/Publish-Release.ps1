[CmdletBinding()]
param(
    [string] $Version,
    [uint64] $RunId,
    [string] $ReleaseNotesPath,
    [string] $UpdateQualificationPath,
    [switch] $Stage,
    [switch] $Finalize,
    [switch] $Publish,
    [ValidateSet('GitHub', 'UpdatesHost')]
    [string] $PackageHost = 'UpdatesHost',
    [string] $KeyPath = (Join-Path ([Environment]::GetFolderPath('UserProfile')) 'Dropbox\resticpal\keys\updates')
)

$ErrorActionPreference = 'Stop'
$repository = 'theatrus/resticpal'
$workflowName = 'Windows CI'
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'artifacts')).TrimEnd('\')

if ($Publish) {
    throw '-Publish has been replaced by the safe two-phase flow: use -Stage, then -Finalize.'
}
if ($Stage -and $Finalize) {
    throw '-Stage and -Finalize are mutually exclusive.'
}
if (-not $Finalize -and -not [string]::IsNullOrWhiteSpace($UpdateQualificationPath)) {
    throw '-UpdateQualificationPath is valid only with -Finalize.'
}
if (($Stage -or $Finalize) -and $PackageHost -ne 'UpdatesHost') {
    throw 'Staged releases must use -PackageHost UpdatesHost so the release hook can mirror a direct MSI before the appcast advances.'
}

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
$tag = "v$Version"
$releaseRoot = Join-Path $artifactRoot "release\$tag"
if (-not $releaseRoot.StartsWith($artifactRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
    throw "Refusing to prepare a release outside the artifact directory: $releaseRoot"
}
$preparedManifestPath = Join-Path $releaseRoot 'release-manifest.json'

function Invoke-GhJson {
    param(
        [Parameter(Mandatory)]
        [string[]] $Arguments,
        [Parameter(Mandatory)]
        [string] $FailureMessage
    )

    $output = @(& gh @Arguments 2>&1)
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        throw "$FailureMessage`n$($output | Out-String)"
    }
    try {
        return ($output | Out-String | ConvertFrom-Json)
    } catch {
        throw "$FailureMessage GitHub CLI returned invalid JSON: $($_.Exception.Message)"
    }
}

function Invoke-GhCommand {
    param(
        [Parameter(Mandatory)]
        [string[]] $Arguments,
        [Parameter(Mandatory)]
        [string] $FailureMessage
    )

    $output = @(& gh @Arguments 2>&1)
    $exitCode = $LASTEXITCODE
    $output | Write-Output
    if ($exitCode -ne 0) {
        throw "$FailureMessage (exit code $exitCode)."
    }
}

function Get-Release {
    $output = @(& gh release view $tag `
        --repo $repository `
        --json 'tagName,targetCommitish,isDraft,isPrerelease,isImmutable,assets,body,url' 2>&1)
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        $message = $output | Out-String
        if ($message -match '(?i)(release not found|HTTP 404|not found)') {
            return $null
        }
        throw "Checking GitHub release $tag failed: $message"
    }
    return ($output | Out-String | ConvertFrom-Json)
}

function Get-RemoteTagTarget {
    $remoteRef = "refs/tags/$tag"
    $peeledRef = "$remoteRef^{}"
    $output = @(& git -C $repositoryRoot ls-remote --tags origin $remoteRef $peeledRef 2>&1)
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        throw "Resolving remote tag $tag failed:`n$($output | Out-String)"
    }
    $references = @($output | ForEach-Object {
        $match = [Regex]::Match([string]$_, '^(?<sha>[0-9a-f]{40})\s+(?<ref>\S+)$')
        if (-not $match.Success) {
            throw "Git returned an invalid remote-tag record while resolving ${tag}: $_"
        }
        [pscustomobject]@{
            Sha = $match.Groups['sha'].Value
            Ref = $match.Groups['ref'].Value
        }
    })
    if ($references.Count -eq 0) {
        return $null
    }
    $peeled = @($references | Where-Object Ref -CEQ $peeledRef)
    if ($peeled.Count -gt 1) {
        throw "Remote tag $tag resolved to more than one peeled commit."
    }
    if ($peeled.Count -eq 1) {
        return $peeled[0].Sha
    }
    $direct = @($references | Where-Object Ref -CEQ $remoteRef)
    if ($direct.Count -ne 1) {
        throw "Remote tag $tag did not resolve to one exact ref."
    }
    return $direct[0].Sha
}

function Assert-RemoteTagTarget {
    param([switch] $AllowMissing)

    $target = Get-RemoteTagTarget
    if ([string]::IsNullOrWhiteSpace($target)) {
        if ($AllowMissing) {
            return
        }
        throw "Remote tag $tag does not exist."
    }
    if ($target -cne $head) {
        throw "Remote tag $tag resolves to $target, not release commit $head."
    }
}

function Assert-ReleaseIdentity {
    param(
        [Parameter(Mandatory)] $Release,
        [switch] $AllowDraft
    )

    if ($Release.tagName -cne $tag) {
        throw "GitHub returned release $($Release.tagName) while checking $tag."
    }
    if ($Release.targetCommitish -cne $head) {
        throw "GitHub release $tag targets $($Release.targetCommitish), not release commit $head."
    }
    if ($Release.isPrerelease) {
        throw "GitHub release $tag must not be a prerelease."
    }
    if ($Release.isDraft -and -not $AllowDraft) {
        throw "GitHub release $tag must be a published stable release."
    }
    if ($Release.isImmutable) {
        throw "GitHub release $tag is immutable and cannot complete the staged appcast flow."
    }
    Assert-RemoteTagTarget -AllowMissing:([bool]$Release.isDraft)
}

function Assert-ReleaseSource {
    if (-not [string]::IsNullOrWhiteSpace((& git -C $repositoryRoot status --porcelain))) {
        throw 'The repository must be clean before staging or finalizing a release.'
    }
    & git -C $repositoryRoot fetch origin main
    if ($LASTEXITCODE -ne 0) {
        throw 'Fetching origin/main failed.'
    }
    $originMain = (& git -C $repositoryRoot rev-parse origin/main).Trim()
    if ($head -cne $originMain) {
        throw "Release commit $head is not current origin/main $originMain."
    }
}

function Assert-SignedRun {
    param([Parameter(Mandatory)] $Run)

    if ([uint64]$Run.databaseId -ne $RunId) {
        throw "GitHub returned run $($Run.databaseId) while checking run $RunId."
    }
    if ($Run.workflowName -cne $workflowName) {
        throw "CI run $RunId belongs to '$($Run.workflowName)', not '$workflowName'."
    }
    if ($Run.headSha -cne $head) {
        throw "CI run $RunId built $($Run.headSha), not release commit $head."
    }
    if ($Run.status -cne 'completed' -or $Run.conclusion -cne 'success') {
        throw "CI run $RunId is $($Run.status)/$($Run.conclusion), not completed/success."
    }
    $allowedContext = (
        $Run.event -ceq 'workflow_dispatch' -or
        ($Run.event -ceq 'push' -and $Run.headBranch -ceq $tag)
    )
    if (-not $allowedContext) {
        throw ("CI run $RunId was not a signed manual or $tag tag build " +
               "(event=$($Run.event), ref=$($Run.headBranch)).")
    }
}

function Assert-ReleaseMsi {
    param([Parameter(Mandatory)] [IO.FileInfo] $Msi)

    $expectedMsiName = "resticpal-$Version-x64.msi"
    if ($Msi.Name -cne $expectedMsiName) {
        throw "Expected MSI name $expectedMsiName, got $($Msi.Name)."
    }
    $authenticode = Get-AuthenticodeSignature -LiteralPath $Msi.FullName
    if ($authenticode.Status -ne 'Valid') {
        throw "The release MSI is not validly Authenticode-signed: $($authenticode.Status)."
    }
    if ($authenticode.SignerCertificate.Subject -notmatch '(^|, )CN=StackFoundry LLC(,|$)') {
        throw "The release MSI is signed by an unexpected publisher: $($authenticode.SignerCertificate.Subject)"
    }

    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $null
    $view = $null
    $record = $null
    try {
        $database = $installer.GetType().InvokeMember(
            'OpenDatabase', 'InvokeMethod', $null, $installer, @($Msi.FullName, 0))
        $view = $database.GetType().InvokeMember(
            'OpenView',
            'InvokeMethod',
            $null,
            $database,
            @("SELECT `Value` FROM `Property` WHERE `Property`='ProductVersion'"))
        $view.GetType().InvokeMember(
            'Execute', 'InvokeMethod', $null, $view, $null) | Out-Null
        $record = $view.GetType().InvokeMember(
            'Fetch', 'InvokeMethod', $null, $view, $null)
        if ($null -eq $record) {
            throw 'The release MSI has no ProductVersion property.'
        }
        $productVersion = $record.GetType().InvokeMember(
            'StringData', 'GetProperty', $null, $record, @(1))
        if ($productVersion -cne $Version) {
            throw "The release MSI ProductVersion is $productVersion, not $Version."
        }
    } finally {
        if ($null -ne $view) {
            $view.GetType().InvokeMember(
                'Close', 'InvokeMethod', $null, $view, $null) | Out-Null
        }
        foreach ($comObject in @($record, $view, $database, $installer)) {
            if ($null -ne $comObject) {
                [Runtime.InteropServices.Marshal]::FinalReleaseComObject($comObject) | Out-Null
            }
        }
    }
}

function Assert-DirectPackageMirror {
    param([Parameter(Mandatory)] [IO.FileInfo] $Msi)

    $expectedUrl = "https://updates.resticpal.com/releases/$tag/$($Msi.Name)"
    $expectedHash = (Get-FileHash -LiteralPath $Msi.FullName -Algorithm SHA256).Hash
    Add-Type -AssemblyName System.Net.Http
    $handler = [Net.Http.HttpClientHandler]::new()
    $handler.AllowAutoRedirect = $false
    $client = [Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromMinutes(20)
    $headRequest = $null
    $headResponse = $null
    $getResponse = $null
    $stream = $null
    $hasher = $null
    try {
        $headRequest = [Net.Http.HttpRequestMessage]::new(
            [Net.Http.HttpMethod]::Head,
            $expectedUrl)
        $headResponse = $client.SendAsync(
            $headRequest,
            [Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
        if ($headResponse.StatusCode -ne [Net.HttpStatusCode]::OK) {
            throw (("The direct MSI mirror returned HTTP {0} to HEAD; " +
                    'expected 200 without a redirect.') -f [int]$headResponse.StatusCode)
        }
        if ($null -eq $headResponse.Content.Headers.ContentLength -or
            [uint64]$headResponse.Content.Headers.ContentLength -ne [uint64]$Msi.Length) {
            throw 'The direct MSI mirror HEAD length does not match the signed CI artifact.'
        }

        $getResponse = $client.GetAsync(
            $expectedUrl,
            [Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
        if ($getResponse.StatusCode -ne [Net.HttpStatusCode]::OK) {
            throw (("The direct MSI mirror returned HTTP {0} to GET; " +
                    'expected 200 without a redirect.') -f [int]$getResponse.StatusCode)
        }
        if ($null -eq $getResponse.Content.Headers.ContentLength -or
            [uint64]$getResponse.Content.Headers.ContentLength -ne [uint64]$Msi.Length) {
            throw 'The direct MSI mirror GET length does not match the signed CI artifact.'
        }
        $stream = $getResponse.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $hasher = [Security.Cryptography.SHA256]::Create()
        $remoteHash = [BitConverter]::ToString($hasher.ComputeHash($stream)).Replace('-', '')
        if ($remoteHash -cne $expectedHash) {
            throw 'The direct MSI mirror hash does not match the signed CI artifact.'
        }
    } finally {
        if ($null -ne $hasher) { $hasher.Dispose() }
        if ($null -ne $stream) { $stream.Dispose() }
        if ($null -ne $getResponse) { $getResponse.Dispose() }
        if ($null -ne $headResponse) { $headResponse.Dispose() }
        if ($null -ne $headRequest) { $headRequest.Dispose() }
        $client.Dispose()
        $handler.Dispose()
    }
    Write-Host "Verified direct, non-redirecting MSI mirror: $expectedUrl"
}

function Write-ChecksumFile {
    param(
        [Parameter(Mandatory)]
        [IO.FileInfo[]] $Files,
        [Parameter(Mandatory)]
        [string] $Path
    )

    $lines = $Files |
        Sort-Object Name |
        ForEach-Object {
            $hash = (Get-FileHash -LiteralPath $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            "$hash *$($_.Name)"
        }
    $lines | Set-Content -LiteralPath $Path -Encoding ascii
}

function Test-RemoteAssetMatches {
    param(
        [Parameter(Mandatory)] $Release,
        [Parameter(Mandatory)] [IO.FileInfo] $File
    )

    $matches = @($Release.assets | Where-Object name -CEQ $File.Name)
    if ($matches.Count -ne 1) {
        return $false
    }
    $expectedDigest = 'sha256:' + (
        Get-FileHash -LiteralPath $File.FullName -Algorithm SHA256
    ).Hash.ToLowerInvariant()
    return (
        [uint64]$matches[0].size -eq [uint64]$File.Length -and
        $matches[0].digest -ceq $expectedDigest
    )
}

function Assert-RemoteAssetMatches {
    param(
        [Parameter(Mandatory)] $Release,
        [Parameter(Mandatory)] [IO.FileInfo] $File
    )

    if (-not (Test-RemoteAssetMatches -Release $Release -File $File)) {
        throw "GitHub release $tag does not contain the expected $($File.Name) bytes."
    }
}

function Assert-AssetNames {
    param(
        [Parameter(Mandatory)] $Release,
        [Parameter(Mandatory)] [string[]] $AllowedNames,
        [Parameter(Mandatory)] [string[]] $RequiredNames
    )

    $names = @($Release.assets | ForEach-Object name)
    $unexpected = @($names | Where-Object { $AllowedNames -cnotcontains $_ })
    $missing = @($RequiredNames | Where-Object { $names -cnotcontains $_ })
    $duplicates = @(
        $names |
            Group-Object -CaseSensitive |
            Where-Object Count -gt 1 |
            ForEach-Object Name)
    if ($unexpected.Count -gt 0 -or $missing.Count -gt 0 -or $duplicates.Count -gt 0) {
        throw ("GitHub release $tag has an unexpected staged asset set. " +
               "Missing=[$($missing -join ', ')]; unexpected=[$($unexpected -join ', ')]; " +
               "duplicates=[$($duplicates -join ', ')].")
    }
}

function Test-AssetLabel {
    param(
        [Parameter(Mandatory)] $Release,
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $Label
    )

    $assets = @($Release.assets | Where-Object name -CEQ $Name)
    return ($assets.Count -eq 1 -and [string]$assets[0].label -ceq $Label)
}

function Assert-FeedAssetLabels {
    param(
        [Parameter(Mandatory)] $Release,
        [Parameter(Mandatory)] [string] $Label
    )

    foreach ($name in @('appcast.xml', 'appcast.xml.signature')) {
        if (-not (Test-AssetLabel -Release $Release -Name $name -Label $Label)) {
            throw "GitHub release $tag does not label $name as '$Label'."
        }
    }
}

function Get-StagedRunId {
    param([Parameter(Mandatory)] $Release)

    $expectedName = "resticpal-$Version-x64.msi"
    $assets = @($Release.assets | Where-Object name -CEQ $expectedName)
    if ($assets.Count -gt 1) {
        throw "GitHub release $tag contains more than one $expectedName asset."
    }
    $assetRunId = $null
    if ($assets.Count -eq 1) {
        $labelMatch = [Regex]::Match(
            [string]$assets[0].label,
            '^Signed Windows CI run (?<runId>\d+)$')
        if ($labelMatch.Success) {
            $assetRunId = [uint64]$labelMatch.Groups['runId'].Value
        }
    }

    $bodyMatches = [Regex]::Matches(
        [string]$Release.body,
        '<!-- resticpal-signed-ci-run: (?<runId>\d+) -->')
    if ($bodyMatches.Count -gt 1) {
        throw "GitHub release $tag contains conflicting staged-run provenance markers."
    }
    $bodyRunId = if ($bodyMatches.Count -eq 1) {
        [uint64]$bodyMatches[0].Groups['runId'].Value
    } else {
        $null
    }
    if ($null -ne $assetRunId -and $null -ne $bodyRunId -and
        $assetRunId -ne $bodyRunId) {
        throw "GitHub release $tag records different signed CI runs in its MSI label and release body."
    }
    if ($null -ne $assetRunId) {
        return $assetRunId
    }
    if ($null -ne $bodyRunId) {
        return $bodyRunId
    }
    if ($assets.Count -eq 1) {
        throw ("GitHub release $tag does not record the signed CI run on its MSI asset. " +
               'Refusing to select a newer timestamp-signed build with different bytes.')
    }
    throw ("GitHub release $tag is missing both its staged MSI and signed-run provenance. " +
           'Pass the original -RunId only after independently verifying the partial release.')
}

function Assert-StagedRunIdentity {
    param([Parameter(Mandatory)] $Release)

    $stagedRunId = Get-StagedRunId -Release $Release
    if ($stagedRunId -ne $RunId) {
        throw ("GitHub release $tag was staged from signed CI run $stagedRunId, " +
               "not selected run $RunId.")
    }
}

function Write-StagedReleaseNotes {
    param([Parameter(Mandatory)] [string] $SourcePath)

    $source = Get-Content -LiteralPath $SourcePath -Raw
    if ($source.Contains('<!-- resticpal-signed-ci-run:')) {
        throw 'Release notes must not contain the internal staged-run provenance marker.'
    }
    $stagedNotesPath = Join-Path $releaseRoot 'staged-release-notes.md'
    $content = $source.TrimEnd() + "`r`n`r`n<!-- resticpal-signed-ci-run: $RunId -->`r`n"
    [IO.File]::WriteAllText($stagedNotesPath, $content, [Text.UTF8Encoding]::new($false))
    return (Get-Item -LiteralPath $stagedNotesPath)
}

function Assert-AppCastSignature {
    param(
        [Parameter(Mandatory)] [IO.FileInfo] $AppCast,
        [Parameter(Mandatory)] [IO.FileInfo] $AppCastSignature
    )

    $trustedPublicKey = (Get-Content -LiteralPath (Join-Path $repositoryRoot 'config\update-public-key.txt') -Raw).Trim()
    $backupPublicKeyPath = Join-Path $KeyPath 'NetSparkle_Ed25519.pub'
    if (-not (Test-Path -LiteralPath $backupPublicKeyPath -PathType Leaf)) {
        throw "The updater public-key backup is missing: $backupPublicKeyPath"
    }
    $backupPublicKey = (Get-Content -LiteralPath $backupPublicKeyPath -Raw).Trim()
    if ($trustedPublicKey -cne $backupPublicKey) {
        throw 'The Dropbox updater public key does not match the public key embedded in resticpal.'
    }

    Push-Location $repositoryRoot
    try {
        $restoreOutput = @(& dotnet tool restore 2>&1)
        $restoreExitCode = $LASTEXITCODE
        $restoreOutput | ForEach-Object { Write-Host $_ }
        if ($restoreExitCode -ne 0) {
            throw "Restoring the pinned NetSparkle tool failed with exit code $restoreExitCode."
        }
        $signature = (Get-Content -LiteralPath $AppCastSignature.FullName -Raw).Trim()
        $verificationOutput = @(& dotnet tool run netsparkle-generate-appcast -- `
            --verify $AppCast.FullName `
            --signature $signature `
            --key-path $KeyPath 2>&1)
        $verificationExitCode = $LASTEXITCODE
        $verificationOutput | ForEach-Object { Write-Host $_ }
        if ($verificationExitCode -ne 0 -or $verificationOutput -cnotcontains 'Signature valid') {
            throw 'The appcast does not verify with the public key embedded in resticpal.'
        }
    } finally {
        Pop-Location
    }
}

function Assert-PreparedAppCast {
    param(
        [Parameter(Mandatory)] [IO.FileInfo] $Msi,
        [Parameter(Mandatory)] [IO.FileInfo] $AppCast,
        [Parameter(Mandatory)] [IO.FileInfo] $AppCastSignature
    )

    Assert-AppCastSignature -AppCast $AppCast -AppCastSignature $AppCastSignature

    [xml] $document = Get-Content -LiteralPath $AppCast.FullName -Raw
    $namespace = 'http://www.andymatuschak.org/xml-namespaces/sparkle'
    $appCastLink = $document.SelectSingleNode('/rss/channel/link')
    $enclosure = $document.SelectSingleNode('/rss/channel/item/enclosure')
    $expectedUrl = "https://updates.resticpal.com/releases/$tag/$($Msi.Name)"
    $invalid = (
        $null -eq $appCastLink -or
        $appCastLink.InnerText -cne 'https://updates.resticpal.com/appcast.xml' -or
        $null -eq $enclosure -or
        $enclosure.GetAttribute('url') -cne $expectedUrl -or
        $enclosure.GetAttribute('version', $namespace) -cne $Version -or
        $enclosure.GetAttribute('shortVersionString', $namespace) -cne $Version -or
        $enclosure.GetAttribute('os', $namespace) -cne 'windows-x64' -or
        [string]::IsNullOrWhiteSpace($enclosure.GetAttribute('signature', $namespace)) -or
        [uint64]$enclosure.GetAttribute('length') -ne [uint64]$Msi.Length
    )
    if ($invalid) {
        throw 'The prepared appcast does not describe the exact direct-host release MSI.'
    }
}

function Assert-FinalizedReleaseAssets {
    param(
        [Parameter(Mandatory)] $Release,
        [Parameter(Mandatory)] [IO.FileInfo] $Msi,
        [Parameter(Mandatory)] [IO.FileInfo] $License,
        [Parameter(Mandatory)] [IO.FileInfo] $Notices,
        [Parameter(Mandatory)] [string[]] $ExpectedAssetNames
    )

    Assert-AssetNames `
        -Release $Release `
        -AllowedNames $ExpectedAssetNames `
        -RequiredNames $ExpectedAssetNames
    foreach ($file in @($Msi, $License, $Notices)) {
        Assert-RemoteAssetMatches -Release $Release -File $file
    }

    $validationRoot = Join-Path $releaseRoot (
        'finalized-validation-' + [Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $validationRoot | Out-Null
    try {
        Invoke-GhCommand `
            -Arguments @(
                'release', 'download', $tag, '--repo', $repository,
                '--pattern', 'appcast.xml',
                '--pattern', 'appcast.xml.signature',
                '--pattern', 'SHA256SUMS.txt',
                '--dir', $validationRoot
            ) `
            -FailureMessage "Downloading finalized metadata for $tag failed"

        $remoteAppCast = Get-Item -LiteralPath (Join-Path $validationRoot 'appcast.xml')
        $remoteAppCastSignature = Get-Item -LiteralPath (
            Join-Path $validationRoot 'appcast.xml.signature')
        $remoteChecksums = Get-Item -LiteralPath (Join-Path $validationRoot 'SHA256SUMS.txt')
        foreach ($file in @($remoteAppCast, $remoteAppCastSignature, $remoteChecksums)) {
            Assert-RemoteAssetMatches -Release $Release -File $file
        }
        Assert-PreparedAppCast `
            -Msi $Msi `
            -AppCast $remoteAppCast `
            -AppCastSignature $remoteAppCastSignature

        $expectedChecksumPath = Join-Path $validationRoot 'SHA256SUMS.expected.txt'
        Write-ChecksumFile `
            -Files @($Msi, $remoteAppCast, $remoteAppCastSignature) `
            -Path $expectedChecksumPath
        $expectedChecksums = @(
            Get-Content -LiteralPath $expectedChecksumPath | Sort-Object)
        $actualChecksums = @(
            Get-Content -LiteralPath $remoteChecksums.FullName | Sort-Object)
        if ($actualChecksums.Count -ne $expectedChecksums.Count -or
            ($actualChecksums -join "`n") -cne ($expectedChecksums -join "`n")) {
            throw 'The finalized release checksum asset does not match its exact MSI and appcast bytes.'
        }
        Assert-DirectPackageMirror -Msi $Msi
    } finally {
        Remove-Item -LiteralPath $validationRoot -Recurse -Force -ErrorAction SilentlyContinue
    }
}

function Write-PreparedManifest {
    param(
        [Parameter(Mandatory)] [IO.FileInfo] $Msi,
        [Parameter(Mandatory)] [IO.FileInfo] $AppCast,
        [Parameter(Mandatory)] [IO.FileInfo] $AppCastSignature,
        [Parameter(Mandatory)] [IO.FileInfo] $Checksums
    )

    function FileRecord([IO.FileInfo] $File) {
        [ordered]@{
            name = $File.Name
            length = [uint64]$File.Length
            sha256 = (Get-FileHash -LiteralPath $File.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }

    $manifest = [ordered]@{
        schema = 2
        version = $Version
        tag = $tag
        head_sha = $head
        run_id = [uint64]$RunId
        package_host = $PackageHost
        files = [ordered]@{
            msi = FileRecord $Msi
            appcast = FileRecord $AppCast
            appcast_signature = FileRecord $AppCastSignature
            checksums = FileRecord $Checksums
        }
        qualification = $null
    }
    $json = $manifest | ConvertTo-Json -Depth 5
    [IO.File]::WriteAllText($preparedManifestPath, $json, [Text.UTF8Encoding]::new($false))
}

function Assert-PreparedFileRecord {
    param(
        [Parameter(Mandatory)] $Record,
        [Parameter(Mandatory)] [IO.FileInfo] $File,
        [Parameter(Mandatory)] [string] $Label
    )

    $hash = (Get-FileHash -LiteralPath $File.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($Record.name -cne $File.Name -or
        [uint64]$Record.length -ne [uint64]$File.Length -or
        $Record.sha256 -cne $hash) {
        throw "Prepared $Label does not match release-manifest.json. Run preparation again and review the new files."
    }
}

function Get-PreviousStableReleaseVersion {
    $releaseList = Invoke-GhJson `
        -Arguments @(
            'release', 'list', '--repo', $repository, '--limit', '100',
            '--json', 'tagName,isDraft,isPrerelease'
        ) `
        -FailureMessage 'Listing stable GitHub releases for update qualification failed.'
    $currentVersion = [Version]$Version
    $candidates = @($releaseList | ForEach-Object {
        if (-not $_.isDraft -and -not $_.isPrerelease -and
            $_.tagName -cmatch '^v(?<version>\d+\.\d+\.\d+)$') {
            $parsed = [Version]$Matches['version']
            if ($parsed -lt $currentVersion) {
                [pscustomobject]@{
                    Text = $Matches['version']
                    Parsed = $parsed
                }
            }
        }
    } | Sort-Object Parsed -Descending)
    if ($candidates.Count -eq 0) {
        throw "No prior stable GitHub release exists below $Version."
    }
    return $candidates[0].Text
}

function Get-PreviousSignedFallbackFeed {
    $previousVersion = Get-PreviousStableReleaseVersion
    $previousTag = "v$previousVersion"
    $previousRelease = Invoke-GhJson `
        -Arguments @(
            'release', 'view', $previousTag, '--repo', $repository,
            '--json', 'tagName,isDraft,isPrerelease,assets,url'
        ) `
        -FailureMessage "Reading fallback release $previousTag failed."
    if ($previousRelease.tagName -cne $previousTag -or
        $previousRelease.isDraft -or $previousRelease.isPrerelease) {
        throw "Fallback release $previousTag is not a published stable release."
    }

    $msiName = "resticpal-$previousVersion-x64.msi"
    $msiAssets = @($previousRelease.assets | Where-Object name -CEQ $msiName)
    $appCastAssets = @($previousRelease.assets | Where-Object name -CEQ 'appcast.xml')
    $signatureAssets = @(
        $previousRelease.assets | Where-Object name -CEQ 'appcast.xml.signature')
    if ($msiAssets.Count -ne 1 -or $appCastAssets.Count -ne 1 -or
        $signatureAssets.Count -ne 1) {
        throw "Fallback release $previousTag does not contain one MSI and one signed appcast pair."
    }

    $fallbackRoot = Join-Path $releaseRoot 'fallback-feed'
    if (Test-Path -LiteralPath $fallbackRoot) {
        Remove-Item -LiteralPath $fallbackRoot -Recurse -Force
    }
    New-Item -ItemType Directory -Path $fallbackRoot | Out-Null
    Invoke-GhCommand `
        -Arguments @(
            'release', 'download', $previousTag, '--repo', $repository,
            '--pattern', 'appcast.xml',
            '--pattern', 'appcast.xml.signature',
            '--dir', $fallbackRoot
        ) `
        -FailureMessage "Downloading signed fallback feed from $previousTag failed"
    $appCast = Get-Item -LiteralPath (Join-Path $fallbackRoot 'appcast.xml')
    $appCastSignature = Get-Item -LiteralPath (
        Join-Path $fallbackRoot 'appcast.xml.signature')
    Assert-RemoteAssetMatches -Release $previousRelease -File $appCast
    Assert-RemoteAssetMatches -Release $previousRelease -File $appCastSignature
    Assert-AppCastSignature -AppCast $appCast -AppCastSignature $appCastSignature

    [xml] $document = Get-Content -LiteralPath $appCast.FullName -Raw
    $namespace = 'http://www.andymatuschak.org/xml-namespaces/sparkle'
    $appCastLink = $document.SelectSingleNode('/rss/channel/link')
    $enclosures = @($document.SelectNodes('/rss/channel/item/enclosure'))
    $expectedDirectUrl = (
        "https://updates.resticpal.com/releases/$previousTag/$msiName")
    $expectedGitHubUrl = (
        "https://github.com/theatrus/resticpal/releases/download/$previousTag/$msiName")
    $enclosure = if ($enclosures.Count -eq 1) { $enclosures[0] } else { $null }
    $invalid = (
        $null -eq $appCastLink -or
        $appCastLink.InnerText -cne 'https://updates.resticpal.com/appcast.xml' -or
        $null -eq $enclosure -or
        ($enclosure.GetAttribute('url') -cne $expectedDirectUrl -and
         $enclosure.GetAttribute('url') -cne $expectedGitHubUrl) -or
        $enclosure.GetAttribute('version', $namespace) -cne $previousVersion -or
        $enclosure.GetAttribute('shortVersionString', $namespace) -cne $previousVersion -or
        $enclosure.GetAttribute('os', $namespace) -cne 'windows-x64' -or
        [string]::IsNullOrWhiteSpace($enclosure.GetAttribute('signature', $namespace)) -or
        [uint64]$enclosure.GetAttribute('length') -ne [uint64]$msiAssets[0].size
    )
    if ($invalid) {
        throw "Fallback appcast from $previousTag does not describe its exact published MSI."
    }
    return [pscustomobject]@{
        Version = $previousVersion
        AppCast = $appCast
        AppCastSignature = $appCastSignature
        AssetLabel = "Previous signed fallback feed $previousTag"
    }
}

function Assert-UpdateQualification {
    param(
        [Parameter(Mandatory)] $Evidence,
        [Parameter(Mandatory)] [IO.FileInfo] $EvidenceFile,
        [Parameter(Mandatory)] $Manifest
    )

    if ($Evidence.schema -ne 1 -or
        $Evidence.qualification -cne 'previous-published-client-prompted-update' -or
        $Evidence.installation_mode -cne 'prompted' -or
        $Evidence.status -cne 'passed' -or
        [int]$Evidence.exit_code -ne 0) {
        throw 'Update qualification must be a successful schema-1 prompted previous-client result.'
    }
    if ($Evidence.candidate_version -cne $Version) {
        throw "Update qualification candidate is $($Evidence.candidate_version), not $Version."
    }
    $expectedEnclosureUrl = "https://updates.resticpal.com/releases/$tag/$($Manifest.files.msi.name)"
    if ($Evidence.enclosure_url -cne $expectedEnclosureUrl) {
        throw 'Update qualification used a different enclosure URL than the prepared release.'
    }

    foreach ($hashField in @(
        [pscustomobject]@{ Label = 'appcast'; Actual = $Evidence.appcast_sha256; Expected = $Manifest.files.appcast.sha256 },
        [pscustomobject]@{ Label = 'appcast signature'; Actual = $Evidence.appcast_signature_sha256; Expected = $Manifest.files.appcast_signature.sha256 },
        [pscustomobject]@{ Label = 'candidate MSI'; Actual = $Evidence.verification.candidate_sha256; Expected = $Manifest.files.msi.sha256 }
    )) {
        if ([string]$hashField.Actual -cnotmatch '^[0-9a-f]{64}$' -or
            $hashField.Actual -cne $hashField.Expected) {
            throw "Update qualification $($hashField.Label) hash does not match release-manifest.json."
        }
    }

    $staged = $Evidence.staged_update
    if ([string]::IsNullOrWhiteSpace([string]$staged.path) -or
        [IO.Path]::GetExtension([string]$staged.path) -ine '.msi' -or
        $staged.extension -ine '.msi' -or
        $staged.file_name -cne [IO.Path]::GetFileName([string]$staged.path)) {
        throw 'The actual NetSparkle staging path in the qualification result does not end in .msi.'
    }
    if ([uint64]$staged.length -ne [uint64]$Manifest.files.msi.length -or
        $staged.sha256 -cne $Manifest.files.msi.sha256) {
        throw 'The staged update bytes do not match the prepared signed MSI.'
    }
    if ([uint64]$staged.same_length_files_examined -lt 1 -or
        [uint64]$staged.hash_matches -lt 1) {
        throw 'The qualification did not record an exact staged-file hash match.'
    }

    $previousVersion = Get-PreviousStableReleaseVersion
    if ($Evidence.published_version -cne $previousVersion -or
        $Evidence.published_release.tag -cne "v$previousVersion") {
        throw ("Update qualification used $($Evidence.published_version), not the immediately " +
               "preceding stable release $previousVersion.")
    }
    $expectedPublishedAssetName = "resticpal-$previousVersion-x64.msi"
    $expectedPublishedAssetUrl = (
        "https://github.com/theatrus/resticpal/releases/download/v$previousVersion/" +
        $expectedPublishedAssetName)
    if ($Evidence.published_release.asset_name -cne $expectedPublishedAssetName -or
        $Evidence.published_release.asset_url -cne $expectedPublishedAssetUrl -or
        $Evidence.published_release.asset_sha256 -cnotmatch '^[0-9a-f]{64}$' -or
        $Evidence.verification.published_sha256 -cne $Evidence.published_release.asset_sha256) {
        throw 'The qualification does not identify one exact official previous-client MSI.'
    }
    $publishedRelease = Invoke-GhJson `
        -Arguments @(
            'release', 'view', "v$previousVersion", '--repo', $repository,
            '--json', 'tagName,isDraft,isPrerelease,assets,url'
        ) `
        -FailureMessage "Reading official release v$previousVersion failed."
    if ($publishedRelease.tagName -cne "v$previousVersion" -or
        $publishedRelease.isDraft -or $publishedRelease.isPrerelease) {
        throw "GitHub release v$previousVersion is not the expected published stable client."
    }
    $publishedAssets = @(
        $publishedRelease.assets | Where-Object name -CEQ $expectedPublishedAssetName)
    if ($publishedAssets.Count -ne 1 -or
        [uint64]$publishedAssets[0].size -ne [uint64]$Evidence.published_release.asset_length -or
        $publishedAssets[0].digest -cne ('sha256:' + $Evidence.published_release.asset_sha256) -or
        $publishedAssets[0].url -cne $expectedPublishedAssetUrl) {
        throw 'The qualified previous-client MSI does not match the official GitHub release asset.'
    }

    $verification = $Evidence.verification
    $expectedFileVersion = "$Version.0"
    if ($verification.installed_version -cne $Version -or
        $verification.installed_ui_file_version -cne $expectedFileVersion -or
        $verification.installed_service_file_version -cne $expectedFileVersion -or
        $verification.installed_tray_file_version -cne $expectedFileVersion) {
        throw 'The qualification did not prove that every installed resticpal binary upgraded.'
    }
    $baselineServiceProcessId = [uint32]$verification.baseline_service_process_id
    $upgradedServiceProcessId = [uint32]$verification.upgraded_service_process_id
    $publishedTrayProcessId = [uint32]$verification.published_tray_process_id
    $upgradedTrayProcessId = [uint32]$verification.upgraded_tray_process_id
    if ($baselineServiceProcessId -eq 0 -or $upgradedServiceProcessId -eq 0 -or
        $baselineServiceProcessId -eq $upgradedServiceProcessId) {
        throw 'The qualification did not prove that the LocalSystem service restarted.'
    }
    if ($publishedTrayProcessId -eq 0 -or $upgradedTrayProcessId -eq 0 -or
        $publishedTrayProcessId -eq $upgradedTrayProcessId -or
        $verification.published_tray_exited -isnot [bool] -or
        -not $verification.published_tray_exited -or
        [uint32]$verification.tray_process_count -ne 1) {
        throw 'The qualification did not prove a clean one-process tray restart.'
    }
    if ($verification.service_identity -cne 'LocalSystem' -or
        $verification.service_state -cne 'Running') {
        throw 'The qualified service is not running as LocalSystem after the update.'
    }

    $evidenceHash = (Get-FileHash -LiteralPath $EvidenceFile.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
    return [ordered]@{
        schema = 1
        result_file = $EvidenceFile.Name
        result_length = [uint64]$EvidenceFile.Length
        result_sha256 = $evidenceHash
        qualification = $Evidence.qualification
        installation_mode = $Evidence.installation_mode
        published_release = [ordered]@{
            tag = $Evidence.published_release.tag
            asset_name = $Evidence.published_release.asset_name
            asset_length = [uint64]$Evidence.published_release.asset_length
            asset_sha256 = $Evidence.published_release.asset_sha256
            asset_url = $Evidence.published_release.asset_url
        }
        appcast_sha256 = $Evidence.appcast_sha256
        appcast_signature_sha256 = $Evidence.appcast_signature_sha256
        staged_update = [ordered]@{
            path = $staged.path
            extension = $staged.extension
            length = [uint64]$staged.length
            sha256 = $staged.sha256
            same_length_files_examined = [uint64]$staged.same_length_files_examined
            hash_matches = [uint64]$staged.hash_matches
        }
        verification = [ordered]@{
            candidate_sha256 = $verification.candidate_sha256
            baseline_service_process_id = $baselineServiceProcessId
            upgraded_service_process_id = $upgradedServiceProcessId
            published_tray_process_id = $publishedTrayProcessId
            upgraded_tray_process_id = $upgradedTrayProcessId
            published_tray_exited = [bool]$verification.published_tray_exited
            tray_process_count = [uint32]$verification.tray_process_count
            installed_version = $verification.installed_version
            installed_ui_file_version = $verification.installed_ui_file_version
            installed_service_file_version = $verification.installed_service_file_version
            installed_tray_file_version = $verification.installed_tray_file_version
            service_identity = $verification.service_identity
            service_state = $verification.service_state
        }
    }
}

function Set-QualificationBinding {
    param(
        [Parameter(Mandatory)] $Manifest,
        [Parameter(Mandatory)] $Binding
    )

    if ($null -ne $Manifest.qualification) {
        $existing = $Manifest.qualification | ConvertTo-Json -Depth 8 -Compress
        $requested = $Binding | ConvertTo-Json -Depth 8 -Compress
        if ($existing -cne $requested) {
            throw 'release-manifest.json is already bound to different update qualification evidence.'
        }
        return
    }
    $Manifest.qualification = $Binding
    $temporaryPath = "$preparedManifestPath.tmp"
    $json = $Manifest | ConvertTo-Json -Depth 10
    [IO.File]::WriteAllText($temporaryPath, $json, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temporaryPath -Destination $preparedManifestPath -Force
}

$resolvedNotes = $null
if ($Stage -or $Finalize) {
    if ([string]::IsNullOrWhiteSpace($ReleaseNotesPath)) {
        throw '-ReleaseNotesPath is required with -Stage or -Finalize.'
    }
    $resolvedNotes = (Resolve-Path -LiteralPath $ReleaseNotesPath).Path
    if ($resolvedNotes.StartsWith($releaseRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw "Release notes must be outside $releaseRoot so release preparation cannot remove them."
    }
}

$qualificationFile = $null
$qualificationEvidence = $null
if ($Finalize) {
    if ([string]::IsNullOrWhiteSpace($UpdateQualificationPath)) {
        throw '-UpdateQualificationPath is required with -Finalize.'
    }
    $qualificationFile = Get-Item -LiteralPath (
        Resolve-Path -LiteralPath $UpdateQualificationPath).Path
    if ($qualificationFile.FullName.StartsWith(
            $releaseRoot + '\',
            [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Update qualification evidence must remain outside the replaceable release directory.'
    }
    try {
        $qualificationEvidence = Get-Content -LiteralPath $qualificationFile.FullName -Raw |
            ConvertFrom-Json
    } catch {
        throw "Update qualification evidence is not valid JSON: $($_.Exception.Message)"
    }
}

$preparedManifest = $null
if ($Finalize) {
    if (-not (Test-Path -LiteralPath $preparedManifestPath -PathType Leaf)) {
        throw "Prepared release manifest is missing: $preparedManifestPath. Run Publish-Release.ps1 without -Stage or -Finalize after the direct MSI mirror is live."
    }
    $preparedManifest = Get-Content -LiteralPath $preparedManifestPath -Raw | ConvertFrom-Json
    if ($preparedManifest.schema -ne 2 -or
        $preparedManifest.version -cne $Version -or
        $preparedManifest.tag -cne $tag -or
        $preparedManifest.head_sha -cne $head -or
        $preparedManifest.package_host -cne 'UpdatesHost') {
        throw 'release-manifest.json does not match this release source and the required UpdatesHost package origin.'
    }
    if ($RunId -ne 0 -and $RunId -ne [uint64]$preparedManifest.run_id) {
        throw "Requested CI run $RunId does not match prepared run $($preparedManifest.run_id)."
    }
    $RunId = [uint64]$preparedManifest.run_id
}

if ($RunId -eq 0) {
    $stagedRelease = Get-Release
    if ($null -ne $stagedRelease) {
        Assert-ReleaseIdentity -Release $stagedRelease -AllowDraft:$Stage
        $RunId = Get-StagedRunId -Release $stagedRelease
    } else {
        $runList = Invoke-GhJson `
            -Arguments @(
                'run', 'list', '--repo', $repository, '--workflow', 'ci.yml',
                '--commit', $head, '--status', 'success', '--limit', '50',
                '--json', 'databaseId,headSha,headBranch,event,status,conclusion,workflowName,url'
            ) `
            -FailureMessage 'Listing successful Windows CI runs failed.'
        $runs = if ($null -eq $runList) { @() } else { @($runList) }
        $run = $runs | Where-Object {
            $_.headSha -ceq $head -and
            $_.status -ceq 'completed' -and
            $_.conclusion -ceq 'success' -and
            $_.workflowName -ceq $workflowName -and
            ($_.event -ceq 'workflow_dispatch' -or
             ($_.event -ceq 'push' -and $_.headBranch -ceq $tag))
        } | Select-Object -First 1
        if ($null -eq $run) {
            throw ("No successful signed tag/manual Windows CI run exists for commit $head. " +
                   "Dispatch ci.yml manually or push tag $tag, then try again.")
        }
        $RunId = [uint64]$run.databaseId
    }
}

$run = Invoke-GhJson `
    -Arguments @(
        'run', 'view', [string]$RunId, '--repo', $repository,
        '--json', 'databaseId,headSha,headBranch,event,status,conclusion,workflowName,url'
    ) `
    -FailureMessage "Reading CI run $RunId failed."
Assert-SignedRun -Run $run

if ($Stage -or $Finalize) {
    Assert-ReleaseSource
}

$license = Get-Item -LiteralPath (Join-Path $repositoryRoot 'LICENSE')
$notices = Get-Item -LiteralPath (Join-Path $repositoryRoot 'THIRD-PARTY-NOTICES.md')
$expectedMsiName = "resticpal-$Version-x64.msi"
$stageCoreAssetNames = @($expectedMsiName, $license.Name, $notices.Name)
$packageAssetNames = @($expectedMsiName, 'SHA256SUMS.txt', $license.Name, $notices.Name)
$stageAssetNames = @($packageAssetNames + @('appcast.xml', 'appcast.xml.signature'))
$finalAssetNames = @($stageAssetNames)
$finalFeedLabel = "Signed update feed $Version"

if ($Finalize) {
    $downloadRoot = Join-Path $releaseRoot 'ci-artifact'
    $msiFiles = @(Get-ChildItem -LiteralPath $downloadRoot -Recurse -Filter '*.msi' -File -ErrorAction SilentlyContinue)
    if ($msiFiles.Count -ne 1) {
        throw "Prepared release must contain exactly one CI MSI; found $($msiFiles.Count)."
    }
    $msi = $msiFiles[0]
    $appCast = Get-Item -LiteralPath (Join-Path $releaseRoot 'feed\appcast.xml')
    $appCastSignature = Get-Item -LiteralPath (Join-Path $releaseRoot 'feed\appcast.xml.signature')
    $checksumFile = Get-Item -LiteralPath (Join-Path $releaseRoot 'SHA256SUMS.txt')

    Assert-ReleaseMsi -Msi $msi
    Assert-PreparedFileRecord -Record $preparedManifest.files.msi -File $msi -Label 'MSI'
    Assert-PreparedFileRecord -Record $preparedManifest.files.appcast -File $appCast -Label 'appcast'
    Assert-PreparedFileRecord -Record $preparedManifest.files.appcast_signature -File $appCastSignature -Label 'appcast signature'
    Assert-PreparedFileRecord -Record $preparedManifest.files.checksums -File $checksumFile -Label 'checksum file'
    Assert-PreparedAppCast -Msi $msi -AppCast $appCast -AppCastSignature $appCastSignature
    $qualificationBinding = Assert-UpdateQualification `
        -Evidence $qualificationEvidence `
        -EvidenceFile $qualificationFile `
        -Manifest $preparedManifest

    $expectedChecksumPath = Join-Path $releaseRoot 'SHA256SUMS.expected.txt'
    Write-ChecksumFile -Files @($msi, $appCast, $appCastSignature) -Path $expectedChecksumPath
    try {
        $expectedChecksums = (Get-Content -LiteralPath $expectedChecksumPath -Raw).Trim()
        $actualChecksums = (Get-Content -LiteralPath $checksumFile.FullName -Raw).Trim()
        if ($actualChecksums -cne $expectedChecksums) {
            throw 'Prepared SHA256SUMS.txt does not exactly match the MSI and appcast pair.'
        }
    } finally {
        Remove-Item -LiteralPath $expectedChecksumPath -Force -ErrorAction SilentlyContinue
    }

    $release = Get-Release
    if ($null -eq $release) {
        throw "GitHub release $tag is not staged. Run Publish-Release.ps1 -Stage first."
    }
    Assert-ReleaseIdentity -Release $release
    Assert-StagedRunIdentity -Release $release
    Assert-AssetNames -Release $release -AllowedNames $finalAssetNames -RequiredNames $stageCoreAssetNames
    Assert-RemoteAssetMatches -Release $release -File $msi
    Assert-RemoteAssetMatches -Release $release -File $license
    Assert-RemoteAssetMatches -Release $release -File $notices
    Set-QualificationBinding `
        -Manifest $preparedManifest `
        -Binding $qualificationBinding

    $fallbackFeedLabel = "Previous signed fallback feed v$(Get-PreviousStableReleaseVersion)"
    $feedAssets = @(
        $release.assets |
            Where-Object { $_.name -ceq 'appcast.xml' -or $_.name -ceq 'appcast.xml.signature' })
    foreach ($asset in $feedAssets) {
        if ([string]$asset.label -cne $fallbackFeedLabel -and
            [string]$asset.label -cne $finalFeedLabel) {
            throw ("GitHub release $tag has an unrecognized $($asset.name) label. " +
                   'Refusing to overwrite update metadata with unknown provenance.')
        }
    }
    $finalLabeledCount = @(
        $feedAssets | Where-Object { [string]$_.label -ceq $finalFeedLabel }).Count
    if ($finalLabeledCount -eq 0 -and $feedAssets.Count -eq 2) {
        $fallbackFeed = Get-PreviousSignedFallbackFeed
        Assert-FeedAssetLabels -Release $release -Label $fallbackFeed.AssetLabel
        Assert-RemoteAssetMatches -Release $release -File $fallbackFeed.AppCast
        Assert-RemoteAssetMatches -Release $release -File $fallbackFeed.AppCastSignature
    } elseif ($finalLabeledCount -eq 0) {
        Write-Host ("Repairing incomplete known staged metadata for $tag from the exact " +
                    'prepared and qualification-bound candidate files.')
    }

    $finalFiles = @($msi, $appCast, $appCastSignature, $checksumFile, $license, $notices)
    $alreadyFinal = $finalLabeledCount -eq 2
    if ($alreadyFinal) {
        foreach ($file in $finalFiles) {
            if (-not (Test-RemoteAssetMatches -Release $release -File $file)) {
                $alreadyFinal = $false
                break
            }
        }
    }
    if ($alreadyFinal) {
        Assert-FinalizedReleaseAssets `
            -Release $release `
            -Msi $msi `
            -License $license `
            -Notices $notices `
            -ExpectedAssetNames $finalAssetNames
        Write-Host ("GitHub release $tag already has the exact prepared and qualified bytes; " +
                    're-emitting the release event so the primary mirror can recover.')
    } else {
        # Recheck the direct host at the last possible point before publishing
        # appcast bytes that name it.
        Assert-DirectPackageMirror -Msi $msi
        $labeledAppCast = "$($appCast.FullName)#$finalFeedLabel"
        $labeledAppCastSignature = "$($appCastSignature.FullName)#$finalFeedLabel"
        $finalUploadItems = @()
        if (-not (Test-RemoteAssetMatches -Release $release -File $appCast) -or
            -not (Test-AssetLabel `
                -Release $release `
                -Name 'appcast.xml' `
                -Label $finalFeedLabel)) {
            $finalUploadItems += $labeledAppCast
        }
        if (-not (Test-RemoteAssetMatches -Release $release -File $appCastSignature) -or
            -not (Test-AssetLabel `
                -Release $release `
                -Name 'appcast.xml.signature' `
                -Label $finalFeedLabel)) {
            $finalUploadItems += $labeledAppCastSignature
        }
        if (-not (Test-RemoteAssetMatches -Release $release -File $checksumFile)) {
            $finalUploadItems += $checksumFile.FullName
        }
        if ($finalUploadItems.Count -eq 0) {
            throw 'Finalized metadata was reported incomplete, but no exact repair operation was identified.'
        }
        Invoke-GhCommand `
            -Arguments (@(
                'release', 'upload', $tag, '--repo', $repository, '--clobber') +
                $finalUploadItems) `
            -FailureMessage "Uploading the signed appcast pair for $tag failed"
    }

    # A release edit emits the webhook that reruns the mirror only after the
    # package, appcast, and detached signature all exist on GitHub.
    Invoke-GhCommand `
        -Arguments @(
            'release', 'edit', $tag, '--repo', $repository,
            '--draft=false', '--latest', '--title', "resticpal $Version",
            '--notes-file', $resolvedNotes
        ) `
        -FailureMessage "Triggering final mirror deployment for $tag failed"

    $release = Get-Release
    Assert-ReleaseIdentity -Release $release
    Assert-StagedRunIdentity -Release $release
    Assert-AssetNames -Release $release -AllowedNames $finalAssetNames -RequiredNames $finalAssetNames
    Assert-FeedAssetLabels -Release $release -Label $finalFeedLabel
    foreach ($file in $finalFiles) {
        Assert-RemoteAssetMatches -Release $release -File $file
    }
    Assert-FinalizedReleaseAssets `
        -Release $release `
        -Msi $msi `
        -License $license `
        -Notices $notices `
        -ExpectedAssetNames $finalAssetNames
    Write-Host "Finalized resticpal $Version from signed CI run $RunId. The release webhook can now atomically advance the mirrored appcast pair."
    Get-Item -LiteralPath @(
        $msi.FullName,
        $appCast.FullName,
        $appCastSignature.FullName,
        $checksumFile.FullName,
        $preparedManifestPath
    )
    return
}

if ($Stage -and (Test-Path -LiteralPath $preparedManifestPath -PathType Leaf)) {
    throw ("Refusing to discard the prepared release manifest during -Stage. " +
           'Finish or deliberately restart preparation instead.')
}
if (-not $Stage -and (Test-Path -LiteralPath $preparedManifestPath -PathType Leaf)) {
    throw ("A prepared release manifest already exists at $preparedManifestPath. " +
           'Use those exact reviewed files, or deliberately remove the versioned release directory before restarting preparation.')
}
if (Test-Path -LiteralPath $releaseRoot) {
    Remove-Item -LiteralPath $releaseRoot -Recurse -Force
}
$downloadRoot = Join-Path $releaseRoot 'ci-artifact'
New-Item -ItemType Directory -Path $downloadRoot | Out-Null

Invoke-GhCommand `
    -Arguments @(
        'run', 'download', [string]$RunId, '--repo', $repository,
        '--name', 'resticpal-windows-x64', '--dir', $downloadRoot
    ) `
    -FailureMessage "Downloading CI artifact from run $RunId failed"

$msiFiles = @(Get-ChildItem -LiteralPath $downloadRoot -Recurse -Filter '*.msi' -File)
if ($msiFiles.Count -ne 1) {
    throw "Expected one MSI in CI run $RunId, found $($msiFiles.Count)."
}
$msi = $msiFiles[0]
Assert-ReleaseMsi -Msi $msi

$checksumPath = Join-Path $releaseRoot 'SHA256SUMS.txt'
if ($Stage) {
    Write-ChecksumFile -Files @($msi) -Path $checksumPath
    $checksumFile = Get-Item -LiteralPath $checksumPath
    $stagedNotes = Write-StagedReleaseNotes -SourcePath $resolvedNotes
    $labeledMsi = "$($msi.FullName)#Signed Windows CI run $RunId"
    $release = Get-Release
    if ($null -ne $release) {
        Assert-ReleaseIdentity -Release $release -AllowDraft
        Assert-StagedRunIdentity -Release $release
        $finalLabeledAssets = @(
            $release.assets |
                Where-Object {
                    ($_.name -ceq 'appcast.xml' -or $_.name -ceq 'appcast.xml.signature') -and
                    [string]$_.label -ceq $finalFeedLabel
                })
        if ($finalLabeledAssets.Count -gt 0) {
            if ($release.isDraft -or $finalLabeledAssets.Count -ne 2) {
                throw ("GitHub release $tag contains an interrupted final appcast upload. " +
                       'Re-run -Finalize with the prepared manifest and qualification evidence.')
            }
            Assert-FinalizedReleaseAssets `
                -Release $release `
                -Msi $msi `
                -License $license `
                -Notices $notices `
                -ExpectedAssetNames $finalAssetNames
            Write-Host "GitHub release $tag is already finalized; staging made no changes."
            Get-Item -LiteralPath @($msi.FullName, $license.FullName, $notices.FullName)
            return
        }
    }

    $fallbackFeed = Get-PreviousSignedFallbackFeed
    $stageFiles = @(
        $msi,
        $checksumFile,
        $license,
        $notices,
        $fallbackFeed.AppCast,
        $fallbackFeed.AppCastSignature)
    $labeledFallbackAppCast = "$($fallbackFeed.AppCast.FullName)#$($fallbackFeed.AssetLabel)"
    $labeledFallbackSignature = "$($fallbackFeed.AppCastSignature.FullName)#$($fallbackFeed.AssetLabel)"

    if ($null -eq $release) {
        Assert-RemoteTagTarget -AllowMissing
        # Release creation and asset upload are separate GitHub operations. Create
        # a provenance-bearing draft first so any interrupted upload is safely
        # discoverable and repairable by the next -Stage run.
        Invoke-GhCommand `
            -Arguments @(
                'release', 'create', $tag, '--repo', $repository,
                '--target', $head, '--title', "resticpal $Version",
                '--notes-file', $stagedNotes.FullName, '--draft'
            ) `
            -FailureMessage "Creating draft GitHub release $tag failed"
        $release = Get-Release
        if ($null -eq $release) {
            throw "GitHub did not return draft release $tag after creating it."
        }
        Assert-ReleaseIdentity -Release $release -AllowDraft
        Assert-StagedRunIdentity -Release $release
    }

    $currentAssetNames = @($release.assets | ForEach-Object name)
    $unexpectedAssets = @(
        $currentAssetNames | Where-Object { $stageAssetNames -cnotcontains $_ })
    if ($unexpectedAssets.Count -gt 0) {
        throw ("GitHub release $tag contains unexpected assets and cannot be safely repaired: " +
               ($unexpectedAssets -join ', '))
    }
    foreach ($asset in @(
            $release.assets |
                Where-Object { $_.name -ceq 'appcast.xml' -or $_.name -ceq 'appcast.xml.signature' })) {
        if ([string]$asset.label -cne $fallbackFeed.AssetLabel) {
            throw ("GitHub release $tag has an unrecognized $($asset.name) label. " +
                   'Refusing to replace update metadata with unknown provenance.')
        }
    }

    # Upload only assets that actually need repair. With --clobber, GitHub
    # deletes each named asset before replacing it, so preserving already exact
    # assets minimizes the recoverable partial state if a network call fails.
    $stageUploadItems = @()
    if (-not (Test-RemoteAssetMatches -Release $release -File $msi) -or
        -not (Test-AssetLabel `
            -Release $release `
            -Name $expectedMsiName `
            -Label "Signed Windows CI run $RunId")) {
        $stageUploadItems += $labeledMsi
    }
    foreach ($file in @($checksumFile, $license, $notices)) {
        if (-not (Test-RemoteAssetMatches -Release $release -File $file)) {
            $stageUploadItems += $file.FullName
        }
    }
    if (-not (Test-RemoteAssetMatches -Release $release -File $fallbackFeed.AppCast) -or
        -not (Test-AssetLabel `
            -Release $release `
            -Name 'appcast.xml' `
            -Label $fallbackFeed.AssetLabel)) {
        $stageUploadItems += $labeledFallbackAppCast
    }
    if (-not (Test-RemoteAssetMatches -Release $release -File $fallbackFeed.AppCastSignature) -or
        -not (Test-AssetLabel `
            -Release $release `
            -Name 'appcast.xml.signature' `
            -Label $fallbackFeed.AssetLabel)) {
        $stageUploadItems += $labeledFallbackSignature
    }
    if ($stageUploadItems.Count -gt 0) {
        Invoke-GhCommand `
            -Arguments (@(
                'release', 'upload', $tag, '--repo', $repository, '--clobber') +
                $stageUploadItems) `
            -FailureMessage "Repairing the staged assets for $tag failed"
    }

    $release = Get-Release
    Assert-ReleaseIdentity -Release $release -AllowDraft
    Assert-StagedRunIdentity -Release $release
    Assert-AssetNames -Release $release -AllowedNames $stageAssetNames -RequiredNames $stageAssetNames
    Assert-FeedAssetLabels -Release $release -Label $fallbackFeed.AssetLabel
    foreach ($file in $stageFiles) {
        Assert-RemoteAssetMatches -Release $release -File $file
    }

    # Publish only after the complete staged asset set has been re-read and
    # verified. The carried-forward signed feed keeps GitHub's latest fallback
    # valid while the candidate MSI is mirrored and qualified.
    Invoke-GhCommand `
        -Arguments @(
            'release', 'edit', $tag, '--repo', $repository,
            '--draft=false', '--latest', '--title', "resticpal $Version",
            '--notes-file', $stagedNotes.FullName
        ) `
        -FailureMessage "Publishing staged release $tag and triggering its MSI mirror failed"
    $release = Get-Release
    Assert-ReleaseIdentity -Release $release
    Assert-StagedRunIdentity -Release $release
    Assert-AssetNames -Release $release -AllowedNames $stageAssetNames -RequiredNames $stageAssetNames
    Assert-FeedAssetLabels -Release $release -Label $fallbackFeed.AssetLabel
    foreach ($file in $stageFiles) {
        Assert-RemoteAssetMatches -Release $release -File $file
    }
    Write-Host ("Staged resticpal $Version from signed CI run $RunId with the previous signed " +
                "v$($fallbackFeed.Version) fallback feed. Wait for and verify the direct MSI mirror " +
                'before preparing the candidate feed.')
    Get-Item -LiteralPath $stageFiles.FullName
    return
}

$stagedRelease = Get-Release
if ($null -eq $stagedRelease) {
    throw "GitHub release $tag is not staged. Run Publish-Release.ps1 -Stage first."
}
Assert-ReleaseIdentity -Release $stagedRelease
Assert-StagedRunIdentity -Release $stagedRelease
Assert-AssetNames `
    -Release $stagedRelease `
    -AllowedNames $stageAssetNames `
    -RequiredNames $stageAssetNames
$fallbackFeed = Get-PreviousSignedFallbackFeed
Assert-FeedAssetLabels -Release $stagedRelease -Label $fallbackFeed.AssetLabel
Write-ChecksumFile -Files @($msi) -Path $checksumPath
$stagedChecksumFile = Get-Item -LiteralPath $checksumPath
foreach ($file in @(
        $msi,
        $stagedChecksumFile,
        $license,
        $notices,
        $fallbackFeed.AppCast,
        $fallbackFeed.AppCastSignature)) {
    Assert-RemoteAssetMatches -Release $stagedRelease -File $file
}

$feedRoot = Join-Path $releaseRoot 'feed'
& (Join-Path $PSScriptRoot 'New-UpdateAppcast.ps1') `
    -MsiPath $msi.FullName `
    -Version $Version `
    -OutputDirectory $feedRoot `
    -PackageHost $PackageHost `
    -KeyPath $KeyPath
if ($LASTEXITCODE -ne 0) {
    throw 'Signed appcast preparation failed.'
}

$appCast = Get-Item -LiteralPath (Join-Path $feedRoot 'appcast.xml')
$appCastSignature = Get-Item -LiteralPath (Join-Path $feedRoot 'appcast.xml.signature')
Write-ChecksumFile -Files @($msi, $appCast, $appCastSignature) -Path $checksumPath
$checksumFile = Get-Item -LiteralPath $checksumPath
Assert-PreparedAppCast -Msi $msi -AppCast $appCast -AppCastSignature $appCastSignature
Write-PreparedManifest `
    -Msi $msi `
    -AppCast $appCast `
    -AppCastSignature $appCastSignature `
    -Checksums $checksumFile

$preparedFiles = @($msi, $appCast, $appCastSignature, $checksumFile)
Write-Host "Prepared resticpal $Version release assets from signed CI run $RunId at $releaseRoot"
Write-Host 'Review and exercise these exact files with the previously published client, then re-run with -Finalize and -ReleaseNotesPath.'
Get-Item -LiteralPath @($preparedFiles.FullName + $preparedManifestPath)
