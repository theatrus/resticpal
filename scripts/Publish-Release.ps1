[CmdletBinding()]
param(
    [string] $Version,
    [uint64] $RunId,
    [string] $ReleaseNotesPath,
    [string] $UpdateQualificationPath,
    [string] $AutomaticUpdateQualificationPath,
    [switch] $Stage,
    [switch] $Finalize,
    [switch] $Publish,
    [ValidateSet('GitHub', 'UpdatesHost')]
    [string] $PackageHost = 'UpdatesHost',
    [string] $KeyPath = (Join-Path ([Environment]::GetFolderPath('UserProfile')) 'Dropbox\resticpal\keys\updates')
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'ReleaseQualification.ps1')
$repository = 'theatrus/resticpal'
$workflowName = 'Windows CI'
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'artifacts')).TrimEnd('\')
$firstV2Version = [Version]'1.0.7'
$invalidProbePackageSignature = [Convert]::ToBase64String([byte[]]::new(64))

if ($Publish) {
    throw '-Publish has been replaced by the safe two-phase flow: use -Stage, then -Finalize.'
}
if ($Stage -and $Finalize) {
    throw '-Stage and -Finalize are mutually exclusive.'
}
if (-not $Finalize -and
    (-not [string]::IsNullOrWhiteSpace($UpdateQualificationPath) -or
     -not [string]::IsNullOrWhiteSpace($AutomaticUpdateQualificationPath))) {
    throw '-UpdateQualificationPath and -AutomaticUpdateQualificationPath are valid only with -Finalize.'
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
if ([Version]$Version -ne $firstV2Version) {
    throw 'This one-time dual-named legacy bridge is restricted to v1.0.7.'
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

function Assert-LatestStableReleaseIsCandidate {
    $latest = Invoke-GhJson `
        -Arguments @('api', "repos/$repository/releases/latest") `
        -FailureMessage 'Reading the latest stable GitHub release failed.'
    if ([string]$latest.tag_name -cne $tag -or
        [bool]$latest.draft -or [bool]$latest.prerelease) {
        throw ("GitHub's latest stable release is $($latest.tag_name), not $tag. " +
               'Stop this release; publication requires an exclusive release window.')
    }
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

function Test-HostedFileMatches {
    param(
        [Parameter(Mandatory)] [string] $Url,
        [Parameter(Mandatory)] [IO.FileInfo] $File,
        [switch] $AllowRedirect
    )

    $expectedHash = (Get-FileHash -LiteralPath $File.FullName -Algorithm SHA256).Hash
    Add-Type -AssemblyName System.Net.Http
    $handler = [Net.Http.HttpClientHandler]::new()
    $handler.AllowAutoRedirect = [bool]$AllowRedirect
    $client = [Net.Http.HttpClient]::new($handler)
    $client.Timeout = [TimeSpan]::FromSeconds(30)
    $response = $null
    $stream = $null
    $hasher = $null
    try {
        $response = $client.GetAsync(
            $Url,
            [Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
        if ($response.StatusCode -ne [Net.HttpStatusCode]::OK -or
            $null -eq $response.Content.Headers.ContentLength -or
            [uint64]$response.Content.Headers.ContentLength -ne [uint64]$File.Length) {
            return $false
        }
        $stream = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $hasher = [Security.Cryptography.SHA256]::Create()
        $remoteHash = [BitConverter]::ToString($hasher.ComputeHash($stream)).Replace('-', '')
        return $remoteHash -ceq $expectedHash
    } catch {
        return $false
    } finally {
        if ($null -ne $hasher) { $hasher.Dispose() }
        if ($null -ne $stream) { $stream.Dispose() }
        if ($null -ne $response) { $response.Dispose() }
        $client.Dispose()
        $handler.Dispose()
    }
}

function Wait-HostedFileMatches {
    param(
        [Parameter(Mandatory)] [string] $Url,
        [Parameter(Mandatory)] [IO.FileInfo] $File,
        [switch] $AllowRedirect,
        [TimeSpan] $Timeout = ([TimeSpan]::FromMinutes(20))
    )

    $deadline = [DateTimeOffset]::UtcNow.Add($Timeout)
    do {
        if (Test-HostedFileMatches -Url $Url -File $File -AllowRedirect:$AllowRedirect) {
            Write-Host "Verified hosted release bytes: $Url"
            return
        }
        Start-Sleep -Seconds 5
    } while ([DateTimeOffset]::UtcNow -lt $deadline)
    throw "Timed out waiting for $Url to match $($File.Name)."
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
        [Parameter(Mandatory)] [string[]] $Names,
        [Parameter(Mandatory)] [string] $Label
    )

    foreach ($name in $Names) {
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
    if ($source.Contains('<!-- resticpal-stage-deploy:')) {
        throw 'Release notes must not contain the internal stage deployment marker.'
    }
    $stagedNotesPath = Join-Path $releaseRoot 'staged-release-notes.md'
    # A recovery edit may otherwise submit byte-identical notes and fail to
    # emit another release-edited webhook. Give every Stage invocation a new
    # hidden marker so the direct MSI mirror can always be retriggered.
    $stageDeploymentId = [Guid]::NewGuid().ToString('N')
    $content = $source.TrimEnd() +
        "`r`n`r`n<!-- resticpal-signed-ci-run: $RunId -->" +
        "`r`n<!-- resticpal-stage-deploy: $stageDeploymentId -->`r`n"
    [IO.File]::WriteAllText($stagedNotesPath, $content, [Text.UTF8Encoding]::new($false))
    return (Get-Item -LiteralPath $stagedNotesPath)
}

function Write-FinalReleaseNotes {
    param([Parameter(Mandatory)] [string] $SourcePath)

    $source = Get-Content -LiteralPath $SourcePath -Raw
    if ($source.Contains('<!-- resticpal-release-deploy:')) {
        throw 'Release notes must not contain the internal deployment marker.'
    }
    $finalNotesPath = Join-Path $releaseRoot 'final-release-notes.md'
    # The unique hidden marker guarantees that a recovery run changes the
    # release and therefore emits another release-edited webhook.
    $deploymentId = [Guid]::NewGuid().ToString('N')
    $content = $source.TrimEnd() +
        "`r`n`r`n<!-- resticpal-release-deploy: $deploymentId -->`r`n"
    [IO.File]::WriteAllText($finalNotesPath, $content, [Text.UTF8Encoding]::new($false))
    return (Get-Item -LiteralPath $finalNotesPath)
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
        $appCastLink.InnerText -cne 'https://updates.resticpal.com/appcast-v2.xml' -or
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

function Assert-DualNamedFeed {
    param(
        [Parameter(Mandatory)] [IO.FileInfo] $AppCast,
        [Parameter(Mandatory)] [IO.FileInfo] $AppCastSignature,
        [Parameter(Mandatory)] [IO.FileInfo] $LegacyAppCast,
        [Parameter(Mandatory)] [IO.FileInfo] $LegacyAppCastSignature
    )

    if ($AppCast.Name -cne 'appcast-v2.xml' -or
        $AppCastSignature.Name -cne 'appcast-v2.xml.signature' -or
        $LegacyAppCast.Name -cne 'appcast.xml' -or
        $LegacyAppCastSignature.Name -cne 'appcast.xml.signature') {
        throw 'The dual-named feed files do not have their exact required names.'
    }
    $appCastHash = (Get-FileHash -LiteralPath $AppCast.FullName -Algorithm SHA256).Hash
    $legacyAppCastHash = (
        Get-FileHash -LiteralPath $LegacyAppCast.FullName -Algorithm SHA256).Hash
    $signatureHash = (
        Get-FileHash -LiteralPath $AppCastSignature.FullName -Algorithm SHA256).Hash
    $legacySignatureHash = (
        Get-FileHash -LiteralPath $LegacyAppCastSignature.FullName -Algorithm SHA256).Hash
    if ([uint64]$AppCast.Length -ne [uint64]$LegacyAppCast.Length -or
        $appCastHash -cne $legacyAppCastHash -or
        [uint64]$AppCastSignature.Length -ne [uint64]$LegacyAppCastSignature.Length -or
        $signatureHash -cne $legacySignatureHash) {
        throw 'The legacy and v2 update-feed aliases are not byte-identical.'
    }
}

function Get-AppCastPackageMetadata {
    param(
        [Parameter(Mandatory)] [IO.FileInfo] $AppCast,
        [Parameter(Mandatory)] [string] $ExpectedVersion
    )

    [xml] $document = Get-Content -LiteralPath $AppCast.FullName -Raw
    $namespace = 'http://www.andymatuschak.org/xml-namespaces/sparkle'
    $enclosures = @($document.SelectNodes('/rss/channel/item/enclosure'))
    if ($enclosures.Count -ne 1) {
        throw "$($AppCast.Name) must contain exactly one update enclosure."
    }
    $enclosure = $enclosures[0]
    $version = $enclosure.GetAttribute('version', $namespace)
    $url = $enclosure.GetAttribute('url')
    $signature = $enclosure.GetAttribute('signature', $namespace)
    $lengthText = $enclosure.GetAttribute('length')
    [uint64] $length = 0
    if ($version -cne $ExpectedVersion -or
        [string]::IsNullOrWhiteSpace($url) -or
        [string]::IsNullOrWhiteSpace($signature) -or
        -not [uint64]::TryParse(
            $lengthText,
            [Globalization.NumberStyles]::None,
            [Globalization.CultureInfo]::InvariantCulture,
            [ref]$length) -or
        $length -eq 0) {
        throw "$($AppCast.Name) does not contain complete signed package metadata."
    }
    return [ordered]@{
        version = $version
        url = $url
        signature = $signature
        length = [uint64]$length
    }
}

function Assert-FinalizedReleaseAssets {
    param(
        [Parameter(Mandatory)] $Release,
        [Parameter(Mandatory)] [IO.FileInfo] $Msi,
        [Parameter(Mandatory)] [IO.FileInfo] $License,
        [Parameter(Mandatory)] [IO.FileInfo] $Notices,
        [Parameter(Mandatory)] [IO.FileInfo] $Checksums,
        [Parameter(Mandatory)] [IO.FileInfo] $AppCast,
        [Parameter(Mandatory)] [IO.FileInfo] $AppCastSignature,
        [Parameter(Mandatory)] [IO.FileInfo] $LegacyAppCast,
        [Parameter(Mandatory)] [IO.FileInfo] $LegacyAppCastSignature,
        [Parameter(Mandatory)] [string[]] $ExpectedAssetNames
    )

    Assert-AssetNames `
        -Release $Release `
        -AllowedNames $ExpectedAssetNames `
        -RequiredNames $ExpectedAssetNames
    Assert-FeedAssetLabels `
        -Release $Release `
        -Names @(
            'appcast.xml',
            'appcast.xml.signature',
            'appcast-v2.xml',
            'appcast-v2.xml.signature') `
        -Label "Signed dual update feed $Version"
    foreach ($file in @(
            $Msi,
            $License,
            $Notices,
            $Checksums,
            $AppCast,
            $AppCastSignature,
            $LegacyAppCast,
            $LegacyAppCastSignature)) {
        Assert-RemoteAssetMatches -Release $Release -File $file
    }

    Assert-DualNamedFeed `
        -AppCast $AppCast `
        -AppCastSignature $AppCastSignature `
        -LegacyAppCast $LegacyAppCast `
        -LegacyAppCastSignature $LegacyAppCastSignature
    Assert-PreparedAppCast `
        -Msi $Msi `
        -AppCast $AppCast `
        -AppCastSignature $AppCastSignature

    $expectedChecksumPath = Join-Path $releaseRoot (
        'SHA256SUMS.finalized-' + [Guid]::NewGuid().ToString('N') + '.txt')
    try {
        Write-ChecksumFile `
            -Files @(
                $Msi,
                $LegacyAppCast,
                $LegacyAppCastSignature,
                $AppCast,
                $AppCastSignature) `
            -Path $expectedChecksumPath
        $expectedChecksums = (Get-Content -LiteralPath $expectedChecksumPath -Raw).Trim()
        $actualChecksums = (Get-Content -LiteralPath $Checksums.FullName -Raw).Trim()
        if ($actualChecksums -cne $expectedChecksums) {
            throw 'The finalized checksum asset does not match its exact MSI and dual-named feed bytes.'
        }
        Assert-DirectPackageMirror -Msi $Msi
    } finally {
        Remove-Item -LiteralPath $expectedChecksumPath -Force -ErrorAction SilentlyContinue
    }
}

function Write-PreparedManifest {
    param(
        [Parameter(Mandatory)] [IO.FileInfo] $Msi,
        [Parameter(Mandatory)] [IO.FileInfo] $AppCast,
        [Parameter(Mandatory)] [IO.FileInfo] $AppCastSignature,
        [Parameter(Mandatory)] [IO.FileInfo] $LegacyAppCast,
        [Parameter(Mandatory)] [IO.FileInfo] $LegacyAppCastSignature,
        [Parameter(Mandatory)] [IO.FileInfo] $Checksums,
        [AllowNull()] [IO.FileInfo] $ProbeAppCast,
        [AllowNull()] [IO.FileInfo] $ProbeAppCastSignature,
        [AllowNull()] [IO.FileInfo] $ProbePayload,
        [Parameter(Mandatory)] [string] $PreviousVersion
    )

    function FileRecord([IO.FileInfo] $File) {
        [ordered]@{
            name = $File.Name
            length = [uint64]$File.Length
            sha256 = (Get-FileHash -LiteralPath $File.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
        }
    }

    $package = Get-AppCastPackageMetadata `
        -AppCast $AppCast `
        -ExpectedVersion $Version
    if ($package.length -ne [uint64]$Msi.Length) {
        throw 'The prepared v2 appcast package length does not match the candidate MSI.'
    }
    Assert-DualNamedFeed `
        -AppCast $AppCast `
        -AppCastSignature $AppCastSignature `
        -LegacyAppCast $LegacyAppCast `
        -LegacyAppCastSignature $LegacyAppCastSignature
    $bridgeQualification = (
        $PreviousVersion -ceq '1.0.6' -and $Version -ceq '1.0.7')
    if ($bridgeQualification -and
        ($null -eq $ProbeAppCast -or $null -eq $ProbeAppCastSignature -or
         $null -eq $ProbePayload)) {
        throw 'The v1.0.6 to v1.0.7 bridge requires an invalid-package candidate-tray probe.'
    }
    if (-not $bridgeQualification -and
        ($null -ne $ProbeAppCast -or $null -ne $ProbeAppCastSignature -or
         $null -ne $ProbePayload)) {
        throw 'Candidate-tray probe files are allowed only for the v1.0.6 to v1.0.7 bridge.'
    }

    $qualificationFiles = $null
    $automaticQualification = [ordered]@{
        strategy = if ($bridgeQualification) {
            'published-service-ipc-bridge-with-candidate-tray-probe'
        } else {
            'published-client-tray'
        }
        probe = $null
    }
    if ($bridgeQualification) {
        $parsedVersion = [Version]$Version
        $probeVersion = '{0}.{1}.{2}' -f `
            $parsedVersion.Major, $parsedVersion.Minor, ($parsedVersion.Build + 1)
        $probePackage = Get-AppCastPackageMetadata `
            -AppCast $ProbeAppCast `
            -ExpectedVersion $probeVersion
        # The generator advertises the next patch, not the current candidate.
        if ($probePackage.version -cne $probeVersion -or
            $probePackage.length -ne [uint64]$ProbePayload.Length -or
            $probePackage.signature -cne $invalidProbePackageSignature -or
            [IO.Path]::GetFileName(([Uri]$probePackage.url).AbsolutePath) -cne
                $ProbePayload.Name) {
            throw 'The candidate-tray probe appcast does not describe its exact sentinel payload.'
        }
        $qualificationFiles = [ordered]@{
            probe_appcast_v2 = FileRecord $ProbeAppCast
            probe_appcast_v2_signature = FileRecord $ProbeAppCastSignature
            probe_payload = FileRecord $ProbePayload
        }
        $automaticQualification.probe = [ordered]@{
            version = $probeVersion
            appcast_sha256 = $qualificationFiles.probe_appcast_v2.sha256
            appcast_signature_sha256 = $qualificationFiles.probe_appcast_v2_signature.sha256
            payload_name = $ProbePayload.Name
            payload_url = $probePackage.url
            payload_length = [uint64]$ProbePayload.Length
            payload_sha256 = $qualificationFiles.probe_payload.sha256
            expected_signature = $probePackage.signature
        }
    }

    $manifest = [ordered]@{
        schema = 5
        version = $Version
        tag = $tag
        head_sha = $head
        run_id = [uint64]$RunId
        package_host = $PackageHost
        files = [ordered]@{
            msi = FileRecord $Msi
            appcast_v2 = FileRecord $AppCast
            appcast_v2_signature = FileRecord $AppCastSignature
            legacy_appcast = FileRecord $LegacyAppCast
            legacy_appcast_signature = FileRecord $LegacyAppCastSignature
            checksums = FileRecord $Checksums
        }
        update_package = $package
        dual_named_feed = [ordered]@{
            version = $Version
            appcast_sha256 = (
                Get-FileHash -LiteralPath $AppCast.FullName -Algorithm SHA256
            ).Hash.ToLowerInvariant()
            appcast_signature_sha256 = (
                Get-FileHash -LiteralPath $AppCastSignature.FullName -Algorithm SHA256
            ).Hash.ToLowerInvariant()
        }
        qualification_files = $qualificationFiles
        automatic_qualification = $automaticQualification
        qualifications = [ordered]@{
            prompted = $null
            automatic = $null
        }
    }
    $json = $manifest | ConvertTo-Json -Depth 10
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

function Assert-ReleaseVersionIsMonotonic {
    param([AllowEmptyString()] [string] $ExpectedPreviousVersion = '')

    $releaseList = Invoke-GhJson `
        -Arguments @(
            'release', 'list', '--repo', $repository, '--limit', '100',
            '--json', 'tagName,isDraft,isPrerelease'
        ) `
        -FailureMessage 'Listing stable GitHub releases for version-order validation failed.'
    $currentVersion = [Version]$Version
    $stableReleases = @($releaseList | ForEach-Object {
        if (-not $_.isDraft -and -not $_.isPrerelease -and
            $_.tagName -cmatch '^v(?<version>\d+\.\d+\.\d+)$') {
            $parsed = [Version]$Matches['version']
            [pscustomobject]@{
                Tag = [string]$_.tagName
                Text = [string]$Matches['version']
                Parsed = $parsed
            }
        }
    })
    $blockingReleases = @($stableReleases | Where-Object {
        $_.Parsed -gt $currentVersion -or
        ($_.Parsed -eq $currentVersion -and $_.Tag -cne $tag)
    } | Sort-Object Parsed -Descending)
    if ($blockingReleases.Count -gt 0) {
        throw ("Release $tag is not newer than every other published stable release. " +
               "Newest blocking release: $($blockingReleases[0].Tag).")
    }
    if (-not [string]::IsNullOrWhiteSpace($ExpectedPreviousVersion)) {
        $previousReleases = @($stableReleases | Where-Object {
            $_.Parsed -lt $currentVersion
        } | Sort-Object Parsed -Descending)
        $actualPreviousVersion = if ($previousReleases.Count -eq 0) {
            '<none>'
        } else {
            $previousReleases[0].Text
        }
        if ($actualPreviousVersion -cne $ExpectedPreviousVersion) {
            throw ("The immediately previous stable release changed from " +
                   "v$ExpectedPreviousVersion to v$actualPreviousVersion during release " +
                   'processing. Restart preparation and qualify from the new previous client.')
        }
    }
}

function Set-QualificationBindings {
    param(
        [Parameter(Mandatory)] $Manifest,
        [Parameter(Mandatory)] $Bindings
    )

    if (Test-UpdateQualificationBindingState `
            -Manifest $Manifest `
            -RequestedBindings $Bindings) {
        return
    }
    $Manifest.qualifications = $Bindings
    $temporaryPath = "$preparedManifestPath.tmp"
    $json = $Manifest | ConvertTo-Json -Depth 14
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
$automaticQualificationFile = $null
$automaticQualificationEvidence = $null
if ($Finalize) {
    if ([string]::IsNullOrWhiteSpace($UpdateQualificationPath)) {
        throw '-UpdateQualificationPath (prompted mode) is required with -Finalize.'
    }
    if ([string]::IsNullOrWhiteSpace($AutomaticUpdateQualificationPath)) {
        throw '-AutomaticUpdateQualificationPath is required with -Finalize.'
    }
    $qualificationFile = Get-Item -LiteralPath (
        Resolve-Path -LiteralPath $UpdateQualificationPath).Path
    $automaticQualificationFile = Get-Item -LiteralPath (
        Resolve-Path -LiteralPath $AutomaticUpdateQualificationPath).Path
    foreach ($evidenceFile in @($qualificationFile, $automaticQualificationFile)) {
        if ($evidenceFile.FullName.StartsWith(
                $releaseRoot + '\',
                [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Update qualification evidence must remain outside the replaceable release directory.'
        }
    }
    if ([string]::Equals(
            $qualificationFile.FullName,
            $automaticQualificationFile.FullName,
            [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Prompted and automatic update qualification must be separate result files.'
    }
    $qualificationEvidence = Read-UpdateQualificationEvidence `
        -LiteralPath $qualificationFile.FullName
    $automaticQualificationEvidence = Read-UpdateQualificationEvidence `
        -LiteralPath $automaticQualificationFile.FullName
}

$preparedManifest = $null
if ($Finalize) {
    if (-not (Test-Path -LiteralPath $preparedManifestPath -PathType Leaf)) {
        throw "Prepared release manifest is missing: $preparedManifestPath. Run Publish-Release.ps1 without -Stage or -Finalize after the direct MSI mirror is live."
    }
    $preparedManifest = Get-Content -LiteralPath $preparedManifestPath -Raw | ConvertFrom-Json
    if ($preparedManifest.schema -ne 5 -or
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
if ($Stage -or $Finalize) {
    # This check runs before release creation, upload, or edit. The exact current
    # tag remains eligible for interrupted-stage recovery, but no older checkout
    # may make itself latest while a higher stable version already exists.
    Assert-ReleaseVersionIsMonotonic
}

$license = Get-Item -LiteralPath (Join-Path $repositoryRoot 'LICENSE')
$notices = Get-Item -LiteralPath (Join-Path $repositoryRoot 'THIRD-PARTY-NOTICES.md')
$expectedMsiName = "resticpal-$Version-x64.msi"
$packageAssetNames = @($expectedMsiName, 'SHA256SUMS.txt', $license.Name, $notices.Name)
$legacyFeedAssetNames = @('appcast.xml', 'appcast.xml.signature')
$v2FeedAssetNames = @('appcast-v2.xml', 'appcast-v2.xml.signature')
$feedAssetNames = @($legacyFeedAssetNames + $v2FeedAssetNames)
$stageAssetNames = @($packageAssetNames)
$stageRequiredAssetNames = @($packageAssetNames)
$finalAssetNames = @($packageAssetNames + $legacyFeedAssetNames + $v2FeedAssetNames)
$finalizationBaseAssetNames = @($packageAssetNames)
$finalFeedLabel = "Signed dual update feed $Version"

if ($Finalize) {
    $downloadRoot = Join-Path $releaseRoot 'ci-artifact'
    $msiFiles = @(Get-ChildItem -LiteralPath $downloadRoot -Recurse -Filter '*.msi' -File -ErrorAction SilentlyContinue)
    if ($msiFiles.Count -ne 1) {
        throw "Prepared release must contain exactly one CI MSI; found $($msiFiles.Count)."
    }
    $msi = $msiFiles[0]
    $appCast = Get-Item -LiteralPath (Join-Path $releaseRoot 'feed\appcast-v2.xml')
    $appCastSignature = Get-Item -LiteralPath (
        Join-Path $releaseRoot 'feed\appcast-v2.xml.signature')
    $legacyAppCast = Get-Item -LiteralPath (Join-Path $releaseRoot 'feed\appcast.xml')
    $legacyAppCastSignature = Get-Item -LiteralPath (
        Join-Path $releaseRoot 'feed\appcast.xml.signature')
    $checksumFile = Get-Item -LiteralPath (Join-Path $releaseRoot 'SHA256SUMS.txt')

    Assert-ReleaseMsi -Msi $msi
    Assert-PreparedFileRecord -Record $preparedManifest.files.msi -File $msi -Label 'MSI'
    Assert-PreparedFileRecord -Record $preparedManifest.files.appcast_v2 -File $appCast -Label 'v2 appcast'
    Assert-PreparedFileRecord -Record $preparedManifest.files.appcast_v2_signature -File $appCastSignature -Label 'v2 appcast signature'
    Assert-PreparedFileRecord -Record $preparedManifest.files.legacy_appcast -File $legacyAppCast -Label 'legacy appcast alias'
    Assert-PreparedFileRecord -Record $preparedManifest.files.legacy_appcast_signature -File $legacyAppCastSignature -Label 'legacy appcast signature alias'
    Assert-PreparedFileRecord -Record $preparedManifest.files.checksums -File $checksumFile -Label 'checksum file'
    Assert-DualNamedFeed `
        -AppCast $appCast `
        -AppCastSignature $appCastSignature `
        -LegacyAppCast $legacyAppCast `
        -LegacyAppCastSignature $legacyAppCastSignature
    Assert-PreparedAppCast -Msi $msi -AppCast $appCast -AppCastSignature $appCastSignature
    $previousQualificationVersion = Get-PreviousStableReleaseVersion
    $previousQualificationRelease = Invoke-GhJson `
        -Arguments @(
            'release', 'view', "v$previousQualificationVersion", '--repo', $repository,
            '--json', 'tagName,isDraft,isPrerelease,assets,url'
        ) `
        -FailureMessage (
            "Reading official release v$previousQualificationVersion failed.")
    $qualificationBindings = Assert-UpdateQualificationPair `
        -PromptedEvidence $qualificationEvidence `
        -AutomaticEvidence $automaticQualificationEvidence `
        -Manifest $preparedManifest `
        -Version $Version `
        -Tag $tag `
        -PreviousVersion $previousQualificationVersion `
        -PublishedRelease $previousQualificationRelease

    $expectedChecksumPath = Join-Path $releaseRoot 'SHA256SUMS.expected.txt'
    Write-ChecksumFile `
        -Files @(
            $msi,
            $legacyAppCast,
            $legacyAppCastSignature,
            $appCast,
            $appCastSignature) `
        -Path $expectedChecksumPath
    try {
        $expectedChecksums = (Get-Content -LiteralPath $expectedChecksumPath -Raw).Trim()
        $actualChecksums = (Get-Content -LiteralPath $checksumFile.FullName -Raw).Trim()
        if ($actualChecksums -cne $expectedChecksums) {
            throw 'Prepared SHA256SUMS.txt does not exactly match the MSI and dual-named feed pairs.'
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
    Assert-LatestStableReleaseIsCandidate
    Assert-AssetNames `
        -Release $release `
        -AllowedNames $finalAssetNames `
        -RequiredNames $finalizationBaseAssetNames
    Assert-RemoteAssetMatches -Release $release -File $msi
    Assert-RemoteAssetMatches -Release $release -File $license
    Assert-RemoteAssetMatches -Release $release -File $notices

    $stageChecksumRoot = Join-Path $releaseRoot 'stage-checksum-validation'
    New-Item -ItemType Directory -Path $stageChecksumRoot -Force | Out-Null
    $stageChecksumPath = Join-Path $stageChecksumRoot 'SHA256SUMS.txt'
    Write-ChecksumFile -Files @($msi) -Path $stageChecksumPath
    $stageChecksumFile = Get-Item -LiteralPath $stageChecksumPath
    if (-not (Test-RemoteAssetMatches -Release $release -File $stageChecksumFile) -and
        -not (Test-RemoteAssetMatches -Release $release -File $checksumFile)) {
        throw 'The staged checksum is neither the exact MSI-only bridge checksum nor the prepared final checksum.'
    }
    Set-QualificationBindings `
        -Manifest $preparedManifest `
        -Bindings $qualificationBindings
    # Validate and materialize the final release body before any candidate XML
    # can become visible through the already-published GitHub release.
    $finalNotes = Write-FinalReleaseNotes -SourcePath $resolvedNotes

    $feedFilesByName = @{
        'appcast-v2.xml' = $appCast
        'appcast-v2.xml.signature' = $appCastSignature
        'appcast.xml' = $legacyAppCast
        'appcast.xml.signature' = $legacyAppCastSignature
    }
    foreach ($asset in @($release.assets | Where-Object {
            $feedAssetNames -ccontains $_.name })) {
        if ([string]$asset.label -cne $finalFeedLabel) {
            throw ("GitHub release $tag has an unrecognized $($asset.name) label. " +
                   'Refusing to overwrite update metadata with unknown provenance.')
        }
        $knownFile = $feedFilesByName[[string]$asset.name]
        if ($null -eq $knownFile) {
            throw "No prepared file is bound to existing feed asset $($asset.name)."
        }
    }

    # The legacy XML upload below is the go-live point because the already-latest
    # release immediately exposes it through GitHub fallback. Complete every
    # qualification and immutable-byte check before beginning this sequence.
    Assert-ReleaseVersionIsMonotonic `
        -ExpectedPreviousVersion $previousQualificationVersion
    $finalFiles = @(
        $msi,
        $legacyAppCast,
        $legacyAppCastSignature,
        $appCast,
        $appCastSignature,
        $checksumFile,
        $license,
        $notices)
    $alreadyFinal = $true
    if ($alreadyFinal) {
        foreach ($file in $finalFiles) {
            if (-not (Test-RemoteAssetMatches -Release $release -File $file) -or
                ($feedAssetNames -ccontains $file.Name -and
                 -not (Test-AssetLabel `
                     -Release $release `
                     -Name $file.Name `
                     -Label $finalFeedLabel))) {
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
            -Checksums $checksumFile `
            -AppCast $appCast `
            -AppCastSignature $appCastSignature `
            -LegacyAppCast $legacyAppCast `
            -LegacyAppCastSignature $legacyAppCastSignature `
            -ExpectedAssetNames $finalAssetNames
        Write-Host ("GitHub release $tag already has the exact prepared and qualified bytes; " +
                     're-emitting the release event so the primary mirror can recover.')
    } else {
        Assert-DirectPackageMirror -Msi $msi

        # Complete v2 fallback before exposing the legacy XML. For each asset,
        # re-read GitHub after upload so a partial run remains recognizable and
        # safely repairable with the exact prepared files.
        $orderedMetadata = @(
            [pscustomobject]@{ File = $checksumFile; Label = $null },
            [pscustomobject]@{ File = $appCastSignature; Label = $finalFeedLabel },
            [pscustomobject]@{ File = $appCast; Label = $finalFeedLabel },
            [pscustomobject]@{ File = $legacyAppCastSignature; Label = $finalFeedLabel },
            [pscustomobject]@{ File = $legacyAppCast; Label = $finalFeedLabel })
        foreach ($metadata in $orderedMetadata) {
            $release = Get-Release
            $matches = Test-RemoteAssetMatches -Release $release -File $metadata.File
            if ($matches -and $null -ne $metadata.Label) {
                $matches = Test-AssetLabel `
                    -Release $release `
                    -Name $metadata.File.Name `
                    -Label $metadata.Label
            }
            if ($matches) {
                continue
            }

            if ($metadata.File.Name -ceq 'appcast.xml') {
                # This is the irreversible rollout boundary for legacy clients.
                Assert-DirectPackageMirror -Msi $msi
                Assert-LatestStableReleaseIsCandidate
                $release = Get-Release
                foreach ($requiredFile in @(
                        $checksumFile,
                        $appCastSignature,
                        $appCast,
                        $legacyAppCastSignature)) {
                    Assert-RemoteAssetMatches -Release $release -File $requiredFile
                }
                Assert-FeedAssetLabels `
                    -Release $release `
                    -Names @(
                        'appcast-v2.xml.signature',
                        'appcast-v2.xml',
                        'appcast.xml.signature') `
                    -Label $finalFeedLabel
            }

            Assert-ReleaseVersionIsMonotonic `
                -ExpectedPreviousVersion $previousQualificationVersion
            $uploadItem = if ($null -eq $metadata.Label) {
                $metadata.File.FullName
            } else {
                "$($metadata.File.FullName)#$($metadata.Label)"
            }
            Invoke-GhCommand `
                -Arguments @(
                    'release', 'upload', $tag, '--repo', $repository,
                    '--clobber', $uploadItem) `
                -FailureMessage "Uploading final release asset $($metadata.File.Name) failed"
            $release = Get-Release
            Assert-RemoteAssetMatches -Release $release -File $metadata.File
            if ($null -ne $metadata.Label) {
                Assert-FeedAssetLabels `
                    -Release $release `
                    -Names @($metadata.File.Name) `
                    -Label $metadata.Label
            }
        }
    }

    # The deployed helper treats a transient legacy-appcast 404 as a successful
    # package-only stage. Prove the exact tag assets are publicly downloadable
    # before emitting its one final deployment webhook.
    Wait-HostedFileMatches `
        -Url "https://github.com/theatrus/resticpal/releases/download/$tag/SHA256SUMS.txt" `
        -File $checksumFile `
        -AllowRedirect
    Wait-HostedFileMatches `
        -Url "https://github.com/theatrus/resticpal/releases/download/$tag/appcast-v2.xml" `
        -File $appCast `
        -AllowRedirect
    Wait-HostedFileMatches `
        -Url "https://github.com/theatrus/resticpal/releases/download/$tag/appcast-v2.xml.signature" `
        -File $appCastSignature `
        -AllowRedirect
    Wait-HostedFileMatches `
        -Url "https://github.com/theatrus/resticpal/releases/download/$tag/appcast.xml" `
        -File $legacyAppCast `
        -AllowRedirect
    Wait-HostedFileMatches `
        -Url "https://github.com/theatrus/resticpal/releases/download/$tag/appcast.xml.signature" `
        -File $legacyAppCastSignature `
        -AllowRedirect

    # A release edit emits the webhook that reruns the mirror only after the
    # package, appcast, and detached signature all exist on GitHub.
    Assert-ReleaseVersionIsMonotonic `
        -ExpectedPreviousVersion $previousQualificationVersion
    Assert-LatestStableReleaseIsCandidate
    Invoke-GhCommand `
        -Arguments @(
            'release', 'edit', $tag, '--repo', $repository,
            '--draft=false', '--title', "resticpal $Version",
            '--notes-file', $finalNotes.FullName
        ) `
        -FailureMessage "Triggering final mirror deployment for $tag failed"

    $release = Get-Release
    Assert-ReleaseIdentity -Release $release
    Assert-StagedRunIdentity -Release $release
    Assert-LatestStableReleaseIsCandidate
    Assert-AssetNames -Release $release -AllowedNames $finalAssetNames -RequiredNames $finalAssetNames
    Assert-FeedAssetLabels `
        -Release $release `
        -Names $feedAssetNames `
        -Label $finalFeedLabel
    foreach ($file in $finalFiles) {
        Assert-RemoteAssetMatches -Release $release -File $file
    }
    Assert-FinalizedReleaseAssets `
        -Release $release `
        -Msi $msi `
        -License $license `
        -Notices $notices `
        -Checksums $checksumFile `
        -AppCast $appCast `
        -AppCastSignature $appCastSignature `
        -LegacyAppCast $legacyAppCast `
        -LegacyAppCastSignature $legacyAppCastSignature `
        -ExpectedAssetNames $finalAssetNames
    Wait-HostedFileMatches `
        -Url 'https://updates.resticpal.com/appcast.xml' `
        -File $legacyAppCast
    Wait-HostedFileMatches `
        -Url 'https://updates.resticpal.com/appcast.xml.signature' `
        -File $legacyAppCastSignature
    Wait-HostedFileMatches `
        -Url 'https://github.com/theatrus/resticpal/releases/latest/download/appcast-v2.xml' `
        -File $appCast `
        -AllowRedirect
    Wait-HostedFileMatches `
        -Url 'https://github.com/theatrus/resticpal/releases/latest/download/appcast-v2.xml.signature' `
        -File $appCastSignature `
        -AllowRedirect
    Wait-HostedFileMatches `
        -Url 'https://github.com/theatrus/resticpal/releases/latest/download/appcast.xml' `
        -File $legacyAppCast `
        -AllowRedirect
    Wait-HostedFileMatches `
        -Url 'https://github.com/theatrus/resticpal/releases/latest/download/appcast.xml.signature' `
        -File $legacyAppCastSignature `
        -AllowRedirect
    Assert-DirectPackageMirror -Msi $msi
    Write-Host "Finalized and verified live resticpal $Version from signed CI run $RunId."
    Get-Item -LiteralPath @(
        $msi.FullName,
        $appCast.FullName,
        $appCastSignature.FullName,
        $legacyAppCast.FullName,
        $legacyAppCastSignature.FullName,
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
        $existingFeedAssets = @($release.assets | Where-Object {
                $feedAssetNames -ccontains $_.name })
        if ($existingFeedAssets.Count -gt 0) {
            throw ("GitHub release $tag already contains update-feed assets. " +
                   'Stage must never add, replace, or carry an appcast; resume with the exact prepared manifest and -Finalize.')
        }
    }

    $stagedPreviousVersion = Get-PreviousStableReleaseVersion
    if ($Version -ceq '1.0.7' -and $stagedPreviousVersion -cne '1.0.6') {
        throw 'The v1.0.7 live bridge requires v1.0.6 to be the immediately previous stable release.'
    }
    $stageFiles = @(
        $msi,
        $checksumFile,
        $license,
        $notices)

    if ($null -eq $release) {
        Assert-ReleaseVersionIsMonotonic `
            -ExpectedPreviousVersion $stagedPreviousVersion
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

    # Stage is intentionally package-only. The deployed host helper mirrors the
    # direct MSI, observes that appcast.xml is absent, and leaves every live feed
    # unchanged until the exact candidate passes both update qualifications.
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
    if ($stageUploadItems.Count -gt 0) {
        Assert-ReleaseVersionIsMonotonic `
            -ExpectedPreviousVersion $stagedPreviousVersion
        Invoke-GhCommand `
            -Arguments (@(
                'release', 'upload', $tag, '--repo', $repository, '--clobber') +
                $stageUploadItems) `
            -FailureMessage "Repairing the staged assets for $tag failed"
    }

    $release = Get-Release
    Assert-ReleaseIdentity -Release $release -AllowDraft
    Assert-StagedRunIdentity -Release $release
    Assert-AssetNames `
        -Release $release `
        -AllowedNames $stageAssetNames `
        -RequiredNames $stageRequiredAssetNames
    foreach ($file in $stageFiles) {
        Assert-RemoteAssetMatches -Release $release -File $file
    }

    # Publish only after the exact package-only asset set has been re-read. No
    # candidate feed exists yet, so publishing cannot offer the unqualified MSI.
    Assert-ReleaseVersionIsMonotonic `
        -ExpectedPreviousVersion $stagedPreviousVersion
    Invoke-GhCommand `
        -Arguments @(
            'release', 'edit', $tag, '--repo', $repository,
            '--draft=false', '--title', "resticpal $Version",
            '--notes-file', $stagedNotes.FullName
        ) `
        -FailureMessage "Publishing staged release $tag and triggering its MSI mirror failed"
    $release = Get-Release
    Assert-ReleaseIdentity -Release $release
    Assert-StagedRunIdentity -Release $release
    Assert-LatestStableReleaseIsCandidate
    Assert-AssetNames `
        -Release $release `
        -AllowedNames $stageAssetNames `
        -RequiredNames $stageRequiredAssetNames
    foreach ($file in $stageFiles) {
        Assert-RemoteAssetMatches -Release $release -File $file
    }
    Write-Host ("Staged package-only resticpal $Version from signed CI run $RunId. " +
                'No appcast assets were published. Wait for and verify the direct MSI mirror ' +
                'before preparing the dual-named candidate feed.')
    Get-Item -LiteralPath $stageFiles.FullName
    return
}

$stagedRelease = Get-Release
if ($null -eq $stagedRelease) {
    throw "GitHub release $tag is not staged. Run Publish-Release.ps1 -Stage first."
}
Assert-ReleaseIdentity -Release $stagedRelease
Assert-StagedRunIdentity -Release $stagedRelease
Assert-LatestStableReleaseIsCandidate
Assert-AssetNames `
    -Release $stagedRelease `
    -AllowedNames $stageAssetNames `
    -RequiredNames $stageRequiredAssetNames
Write-ChecksumFile -Files @($msi) -Path $checksumPath
$stagedChecksumFile = Get-Item -LiteralPath $checksumPath
foreach ($file in @($msi, $stagedChecksumFile, $license, $notices)) {
    Assert-RemoteAssetMatches -Release $stagedRelease -File $file
}

# This is the release invariant: generate no update metadata until the exact
# signed MSI is available at a direct, non-redirecting path ending in .msi.
Assert-DirectPackageMirror -Msi $msi

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

$appCast = Get-Item -LiteralPath (Join-Path $feedRoot 'appcast-v2.xml')
$appCastSignature = Get-Item -LiteralPath (Join-Path $feedRoot 'appcast-v2.xml.signature')
$legacyAppCastPath = Join-Path $feedRoot 'appcast.xml'
$legacyAppCastSignaturePath = Join-Path $feedRoot 'appcast.xml.signature'
Copy-Item -LiteralPath $appCast.FullName -Destination $legacyAppCastPath -Force
Copy-Item `
    -LiteralPath $appCastSignature.FullName `
    -Destination $legacyAppCastSignaturePath `
    -Force
$legacyAppCast = Get-Item -LiteralPath $legacyAppCastPath
$legacyAppCastSignature = Get-Item -LiteralPath $legacyAppCastSignaturePath
Assert-DualNamedFeed `
    -AppCast $appCast `
    -AppCastSignature $appCastSignature `
    -LegacyAppCast $legacyAppCast `
    -LegacyAppCastSignature $legacyAppCastSignature
Write-ChecksumFile `
    -Files @(
        $msi,
        $legacyAppCast,
        $legacyAppCastSignature,
        $appCast,
        $appCastSignature) `
    -Path $checksumPath
$checksumFile = Get-Item -LiteralPath $checksumPath
Assert-PreparedAppCast -Msi $msi -AppCast $appCast -AppCastSignature $appCastSignature
$previousQualificationVersion = Get-PreviousStableReleaseVersion
if ($Version -ceq '1.0.7' -and $previousQualificationVersion -cne '1.0.6') {
    throw 'The one-time v1.0.7 rescue release requires v1.0.6 to be the immediately previous stable release.'
}
$probeAppCast = $null
$probeAppCastSignature = $null
$probePayload = $null
if ($previousQualificationVersion -ceq '1.0.6' -and $Version -ceq '1.0.7') {
    $probeRoot = Join-Path $releaseRoot 'probe'
    & (Join-Path $PSScriptRoot 'New-UpdateQualificationProbe.ps1') `
        -CandidateVersion $Version `
        -OutputDirectory $probeRoot `
        -KeyPath $KeyPath
    $probeAppCast = Get-Item -LiteralPath (
        Join-Path $probeRoot 'appcast-v2-probe.xml')
    $probeAppCastSignature = Get-Item -LiteralPath (
        Join-Path $probeRoot 'appcast-v2-probe.xml.signature')
    $parsedCandidateVersion = [Version]$Version
    $probeVersion = '{0}.{1}.{2}' -f `
        $parsedCandidateVersion.Major,
        $parsedCandidateVersion.Minor,
        ($parsedCandidateVersion.Build + 1)
    $probePayload = Get-Item -LiteralPath (
        Join-Path $probeRoot "resticpal-$probeVersion-x64.msi")
    Assert-AppCastSignature `
        -AppCast $probeAppCast `
        -AppCastSignature $probeAppCastSignature
}
Write-PreparedManifest `
    -Msi $msi `
    -AppCast $appCast `
    -AppCastSignature $appCastSignature `
    -LegacyAppCast $legacyAppCast `
    -LegacyAppCastSignature $legacyAppCastSignature `
    -Checksums $checksumFile `
    -ProbeAppCast $probeAppCast `
    -ProbeAppCastSignature $probeAppCastSignature `
    -ProbePayload $probePayload `
    -PreviousVersion $previousQualificationVersion

$preparedFiles = @(
    $msi,
    $appCast,
    $appCastSignature,
    $legacyAppCast,
    $legacyAppCastSignature,
    $checksumFile)
if ($null -ne $probeAppCast) {
    $preparedFiles += @($probeAppCast, $probeAppCastSignature, $probePayload)
}
Write-Host "Prepared resticpal $Version release assets from signed CI run $RunId at $releaseRoot"
Write-Host 'Review these exact files through both prompted and automatic previous-client Sandbox modes, then re-run with -Finalize, -ReleaseNotesPath, and both qualification paths.'
Get-Item -LiteralPath @($preparedFiles.FullName + $preparedManifestPath)
