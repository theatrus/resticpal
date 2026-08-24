[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $PublishedClientMsiPath,

    [Parameter(Mandatory)]
    [string] $CandidateMsiPath,

    [Parameter(Mandatory)]
    [string] $AppCastPath,

    [Parameter(Mandatory)]
    [string] $AppCastSignaturePath,

    [string] $ProbeAppCastPath,

    [string] $ProbeAppCastSignaturePath,

    [string] $ProbePayloadPath,

    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string] $ExpectedPublishedVersion,

    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string] $ExpectedCandidateVersion,

    [ValidateRange(2048, 32768)]
    [int] $MemoryInMB = 8192,

    [ValidateRange(1, 60)]
    [int] $TimeoutMinutes = 30,

    [ValidateRange(1, 15)]
    [int] $InstallerLaunchTimeoutMinutes = 7,

    [ValidateRange(1, 20)]
    [int] $InstallerCompletionTimeoutMinutes = 8,

    [ValidateSet('Prompted', 'Automatic')]
    [string] $InstallationMode = 'Prompted',

    [string] $KeyPath = (Join-Path ([Environment]::GetFolderPath('UserProfile')) 'Dropbox\resticpal\keys\updates'),

    [switch] $KeepOpen,
    [switch] $GenerateOnly,
    [switch] $UseLegacyLauncher
)

$ErrorActionPreference = 'Stop'
$repository = 'theatrus/resticpal'
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$sandboxExecutable = Join-Path $env:SystemRoot 'System32\WindowsSandbox.exe'
$sandboxCliCommand = Get-Command wsb -CommandType Application -ErrorAction SilentlyContinue
$useSandboxCli = -not $GenerateOnly -and -not $UseLegacyLauncher -and $null -ne $sandboxCliCommand
if (-not $GenerateOnly `
    -and -not $useSandboxCli `
    -and -not (Test-Path -LiteralPath $sandboxExecutable -PathType Leaf)) {
    throw ('Windows Sandbox is unavailable. Run scripts\Enable-WindowsSandbox.ps1 ' +
           'as administrator, restart Windows, and try again.')
}

function Get-MsiProperty([string] $Path, [string] $Property) {
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $null
    $view = $null
    $record = $null
    try {
        $database = $installer.GetType().InvokeMember(
            'OpenDatabase',
            'InvokeMethod',
            $null,
            $installer,
            @($Path, 0))
        $query = "SELECT ``Value`` FROM ``Property`` WHERE ``Property``='$Property'"
        $view = $database.GetType().InvokeMember(
            'OpenView', 'InvokeMethod', $null, $database, @($query))
        $view.GetType().InvokeMember(
            'Execute', 'InvokeMethod', $null, $view, $null) | Out-Null
        $record = $view.GetType().InvokeMember(
            'Fetch', 'InvokeMethod', $null, $view, $null)
        if ($null -eq $record) {
            throw "MSI property $Property was not found in $Path."
        }
        return $record.GetType().InvokeMember(
            'StringData', 'GetProperty', $null, $record, 1)
    } finally {
        if ($null -ne $record) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($record) | Out-Null
        }
        if ($null -ne $view) {
            $view.GetType().InvokeMember(
                'Close', 'InvokeMethod', $null, $view, $null) | Out-Null
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($view) | Out-Null
        }
        if ($null -ne $database) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($database) | Out-Null
        }
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
    }
}

function Assert-SignedMsi([string] $Path, [string] $Label) {
    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne 'Valid') {
        throw "$Label is not validly Authenticode-signed: $($signature.Status)."
    }
    if ($signature.SignerCertificate.Subject -notmatch '(^|, )CN=StackFoundry LLC(,|$)') {
        throw "$Label has unexpected signer $($signature.SignerCertificate.Subject)."
    }
}

function Get-PublishedReleaseAsset([string] $Version) {
    $tag = "v$Version"
    $output = @(& gh api "repos/$repository/releases/tags/$tag" 2>&1)
    $exitCode = $LASTEXITCODE
    if ($exitCode -ne 0) {
        throw "Reading the official GitHub release $tag failed: $($output | Out-String)"
    }
    try {
        $release = $output | Out-String | ConvertFrom-Json
    } catch {
        throw "GitHub returned invalid release metadata for ${tag}: $($_.Exception.Message)"
    }
    if ($release.tag_name -cne $tag -or $release.draft -or $release.prerelease) {
        throw "GitHub $tag is not the expected published stable release."
    }

    $expectedName = "resticpal-$Version-x64.msi"
    $assets = @($release.assets | Where-Object name -CEQ $expectedName)
    if ($assets.Count -ne 1) {
        throw "GitHub $tag must contain exactly one $expectedName asset; found $($assets.Count)."
    }
    $asset = $assets[0]
    $expectedUrl = "https://github.com/$repository/releases/download/$tag/$expectedName"
    $digestMatch = [Regex]::Match(
        [string]$asset.digest,
        '^sha256:(?<hash>[0-9a-fA-F]{64})$')
    if ($asset.state -cne 'uploaded' `
        -or $asset.browser_download_url -cne $expectedUrl `
        -or [uint64]$asset.size -eq 0 `
        -or -not $digestMatch.Success) {
        throw "GitHub $tag asset metadata is incomplete or does not identify $expectedUrl."
    }

    return [pscustomobject]@{
        tag = $tag
        asset_name = $expectedName
        asset_length = [uint64]$asset.size
        asset_sha256 = $digestMatch.Groups['hash'].Value.ToLowerInvariant()
        asset_url = $expectedUrl
    }
}

$publishedClientMsi = (Resolve-Path -LiteralPath $PublishedClientMsiPath).Path
$candidateMsi = (Resolve-Path -LiteralPath $CandidateMsiPath).Path
$appCast = (Resolve-Path -LiteralPath $AppCastPath).Path
$appCastSignature = (Resolve-Path -LiteralPath $AppCastSignaturePath).Path

$publishedRelease = Get-PublishedReleaseAsset $ExpectedPublishedVersion
$publishedHash = (Get-FileHash -LiteralPath $publishedClientMsi -Algorithm SHA256).Hash.ToLowerInvariant()
if ($publishedHash -cne $publishedRelease.asset_sha256 `
    -or [uint64](Get-Item -LiteralPath $publishedClientMsi).Length -ne
        [uint64]$publishedRelease.asset_length `
    -or [IO.Path]::GetFileName($publishedClientMsi) -cne $publishedRelease.asset_name) {
    throw ('The published-client MSI does not match the official GitHub release asset ' +
           "$($publishedRelease.asset_url).")
}
$candidateHash = (Get-FileHash -LiteralPath $candidateMsi -Algorithm SHA256).Hash.ToLowerInvariant()
$appCastHash = (Get-FileHash -LiteralPath $appCast -Algorithm SHA256).Hash.ToLowerInvariant()
$appCastSignatureHash = (
    Get-FileHash -LiteralPath $appCastSignature -Algorithm SHA256).Hash.ToLowerInvariant()

$publishedProductVersion = Get-MsiProperty $publishedClientMsi 'ProductVersion'
if ($publishedProductVersion -cne $ExpectedPublishedVersion) {
    throw ("Published-client MSI version is $publishedProductVersion; " +
           "expected $ExpectedPublishedVersion.")
}
$candidateProductVersion = Get-MsiProperty $candidateMsi 'ProductVersion'
if ($candidateProductVersion -cne $ExpectedCandidateVersion) {
    throw ("Candidate MSI version is $candidateProductVersion; " +
           "expected $ExpectedCandidateVersion.")
}
$publishedTuple = [Version]$ExpectedPublishedVersion
$candidateTuple = [Version]$ExpectedCandidateVersion
if ($candidateTuple -le $publishedTuple) {
    throw 'The candidate version must be newer than the published-client version.'
}
$bridgeTransition = (
    $InstallationMode -ceq 'Automatic' -and
    $ExpectedPublishedVersion -ceq '1.0.6' -and
    $ExpectedCandidateVersion -ceq '1.0.7')
$probeArguments = @(
    $ProbeAppCastPath,
    $ProbeAppCastSignaturePath,
    $ProbePayloadPath) | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
if ($bridgeTransition -and $probeArguments.Count -ne 3) {
    throw ('The 1.0.6-to-1.0.7 automatic transition requires -ProbeAppCastPath, ' +
           '-ProbeAppCastSignaturePath, and -ProbePayloadPath.')
}
if (-not $bridgeTransition -and $probeArguments.Count -ne 0) {
    throw 'Candidate-tray probe inputs are valid only for the 1.0.6-to-1.0.7 automatic transition.'
}

Assert-SignedMsi $publishedClientMsi 'Published-client MSI'
Assert-SignedMsi $candidateMsi 'Candidate MSI'

if ((Get-Item -LiteralPath $appCastSignature).Length -eq 0 `
    -or (Get-Item -LiteralPath $appCastSignature).Length -gt 1024) {
    throw 'The detached appcast signature is empty or unexpectedly large.'
}
[xml] $appCastDocument = Get-Content -LiteralPath $appCast -Raw
$sparkleNamespace = 'http://www.andymatuschak.org/xml-namespaces/sparkle'
$appCastLink = $appCastDocument.SelectSingleNode('/rss/channel/link')
if ($null -eq $appCastLink `
    -or $appCastLink.InnerText -cne 'https://updates.resticpal.com/appcast-v2.xml') {
    throw 'The signed appcast does not identify the primary resticpal update feed.'
}
$enclosure = $appCastDocument.SelectSingleNode('/rss/channel/item/enclosure')
if ($null -eq $enclosure) {
    throw 'The signed appcast does not contain an update enclosure.'
}
$enclosureUrl = $enclosure.GetAttribute('url')
$expectedGitHubUrl = (
    "https://github.com/theatrus/resticpal/releases/download/v{0}/resticpal-{0}-x64.msi" -f
    $ExpectedCandidateVersion)
$expectedUpdatesHostUrl = (
    "https://updates.resticpal.com/releases/v{0}/resticpal-{0}-x64.msi" -f
    $ExpectedCandidateVersion)
if ($enclosureUrl -cne $expectedGitHubUrl -and $enclosureUrl -cne $expectedUpdatesHostUrl) {
    throw "The appcast enclosure URL is outside the pinned release paths: $enclosureUrl"
}
if ($enclosure.GetAttribute('version', $sparkleNamespace) -cne $ExpectedCandidateVersion `
    -or $enclosure.GetAttribute('shortVersionString', $sparkleNamespace) -cne $ExpectedCandidateVersion `
    -or $enclosure.GetAttribute('os', $sparkleNamespace) -cne 'windows-x64' `
    -or [string]::IsNullOrWhiteSpace(
        $enclosure.GetAttribute('signature', $sparkleNamespace))) {
    throw 'The appcast enclosure identity does not match the candidate release.'
}
$candidateLength = (Get-Item -LiteralPath $candidateMsi).Length
if ([uint64]$enclosure.GetAttribute('length') -ne $candidateLength) {
    throw 'The appcast enclosure length does not match the candidate MSI.'
}
$enclosureSignature = $enclosure.GetAttribute('signature', $sparkleNamespace)

Push-Location $repositoryRoot
try {
    & dotnet tool restore
    if ($LASTEXITCODE -ne 0) {
        throw 'Restoring the pinned NetSparkle tool failed.'
    }
    $appCastDetachedSignature = (
        Get-Content -LiteralPath $appCastSignature -Raw).Trim()
    $appCastVerification = @(& dotnet tool run netsparkle-generate-appcast -- `
        --verify $appCast `
        --signature $appCastDetachedSignature `
        --key-path $KeyPath 2>&1)
    if ($LASTEXITCODE -ne 0 -or $appCastVerification -cnotcontains 'Signature valid') {
        throw 'The candidate appcast is not signed by the production update key.'
    }
} finally {
    Pop-Location
}

$probe = $null
if ($bridgeTransition) {
    $resolvedProbeAppCast = (Resolve-Path -LiteralPath $ProbeAppCastPath).Path
    $resolvedProbeAppCastSignature = (
        Resolve-Path -LiteralPath $ProbeAppCastSignaturePath).Path
    $resolvedProbePayload = (Resolve-Path -LiteralPath $ProbePayloadPath).Path
    [xml] $probeDocument = Get-Content -LiteralPath $resolvedProbeAppCast -Raw
    $probeLink = $probeDocument.SelectSingleNode('/rss/channel/link')
    $probeEnclosure = $probeDocument.SelectSingleNode('/rss/channel/item/enclosure')
    if ($null -eq $probeLink -or
        $probeLink.InnerText -cne 'https://updates.resticpal.com/appcast-v2.xml' -or
        $null -eq $probeEnclosure) {
        throw 'The candidate-tray probe does not identify the v2 update feed.'
    }
    $probeVersion = $probeEnclosure.GetAttribute('version', $sparkleNamespace)
    $expectedProbeTuple = [Version]::new(
        $candidateTuple.Major,
        $candidateTuple.Minor,
        $candidateTuple.Build + 1)
    if ($probeVersion -cne $expectedProbeTuple.ToString(3) -or
        $probeEnclosure.GetAttribute('shortVersionString', $sparkleNamespace) -cne $probeVersion -or
        $probeEnclosure.GetAttribute('os', $sparkleNamespace) -cne 'windows-x64') {
        throw 'The candidate-tray probe version or platform is not the exact next patch.'
    }
    $probePayload = Get-Item -LiteralPath $resolvedProbePayload
    $expectedProbePayloadName = "resticpal-$probeVersion-x64.msi"
    $expectedProbeUrl = (
        "https://updates.resticpal.com/releases/v$probeVersion/$expectedProbePayloadName")
    $probePackageSignature = $probeEnclosure.GetAttribute('signature', $sparkleNamespace)
    if ($probePayload.Name -cne $expectedProbePayloadName -or
        $probeEnclosure.GetAttribute('url') -cne $expectedProbeUrl -or
        [uint64]$probeEnclosure.GetAttribute('length') -ne [uint64]$probePayload.Length -or
        [string]::IsNullOrWhiteSpace($probePackageSignature)) {
        throw 'The candidate-tray probe enclosure does not match its exact sentinel payload.'
    }
    $probeXml = Get-Content -LiteralPath $resolvedProbeAppCast -Raw
    if ($probeXml -cnotmatch (
            '<sparkle:version>' + [Regex]::Escape($probeVersion) + '</sparkle:version>')) {
        throw 'The candidate-tray probe has no matching signed item version.'
    }

    Push-Location $repositoryRoot
    try {
        & dotnet tool restore
        if ($LASTEXITCODE -ne 0) {
            throw 'Restoring the pinned NetSparkle tool failed.'
        }
        $probeDetachedSignature = (
            Get-Content -LiteralPath $resolvedProbeAppCastSignature -Raw).Trim()
        $probeVerification = @(& dotnet tool run netsparkle-generate-appcast -- `
            --verify $resolvedProbeAppCast `
            --signature $probeDetachedSignature `
            --key-path $KeyPath 2>&1)
        if ($LASTEXITCODE -ne 0 -or $probeVerification -cnotcontains 'Signature valid') {
            throw 'The candidate-tray probe appcast is not signed by the production update key.'
        }
        $payloadVerification = @(& dotnet tool run netsparkle-generate-appcast -- `
            --verify $resolvedProbePayload `
            --signature $probePackageSignature `
            --key-path $KeyPath 2>&1)
        if ($payloadVerification -contains 'Signature valid' -or
            $payloadVerification -cnotcontains 'Signature invalid') {
            throw 'The candidate-tray probe payload signature is not deliberately invalid.'
        }
    } finally {
        Pop-Location
    }

    $probe = [ordered]@{
        version = $probeVersion
        appcast_path = $resolvedProbeAppCast
        appcast_sha256 = (
            Get-FileHash -LiteralPath $resolvedProbeAppCast -Algorithm SHA256).Hash.ToLowerInvariant()
        appcast_signature_path = $resolvedProbeAppCastSignature
        appcast_signature_sha256 = (
            Get-FileHash -LiteralPath $resolvedProbeAppCastSignature -Algorithm SHA256).Hash.ToLowerInvariant()
        payload_path = $resolvedProbePayload
        payload_name = $probePayload.Name
        payload_url = $expectedProbeUrl
        payload_length = [uint64]$probePayload.Length
        payload_sha256 = (
            Get-FileHash -LiteralPath $resolvedProbePayload -Algorithm SHA256).Hash.ToLowerInvariant()
        expected_signature = $probePackageSignature
    }
}

$runId = '{0}-{1}' -f (
    Get-Date -Format 'yyyyMMdd-HHmmss'), ([Guid]::NewGuid().ToString('N').Substring(0, 8))
$runRoot = Join-Path $repositoryRoot "artifacts\windows-sandbox-update\$runId"
New-Item -ItemType Directory -Path $runRoot -Force | Out-Null
Copy-Item -LiteralPath $publishedClientMsi -Destination (Join-Path $runRoot 'published-client.msi')
Copy-Item -LiteralPath $candidateMsi -Destination (Join-Path $runRoot 'candidate.msi')
Copy-Item -LiteralPath $appCast -Destination (Join-Path $runRoot 'appcast.xml')
Copy-Item -LiteralPath $appCastSignature -Destination (Join-Path $runRoot 'appcast.xml.signature')
if ($bridgeTransition) {
    Copy-Item -LiteralPath $probe.appcast_path -Destination (Join-Path $runRoot 'probe-appcast.xml')
    Copy-Item -LiteralPath $probe.appcast_signature_path `
        -Destination (Join-Path $runRoot 'probe-appcast.xml.signature')
    Copy-Item -LiteralPath $probe.payload_path `
        -Destination (Join-Path $runRoot $probe.payload_name)
}

$networking = 'Disable'
$runtimeConfiguration = @"
<Configuration>
  <VGpu>Disable</VGpu>
  <Networking>$networking</Networking>
  <AudioInput>Disable</AudioInput>
  <VideoInput>Disable</VideoInput>
  <PrinterRedirection>Disable</PrinterRedirection>
  <ClipboardRedirection>Disable</ClipboardRedirection>
  <MemoryInMB>$MemoryInMB</MemoryInMB>
</Configuration>
"@
$escapedRepositoryRoot = [Security.SecurityElement]::Escape($repositoryRoot)
$escapedRunRoot = [Security.SecurityElement]::Escape($runRoot)
$guestArguments = (
    ' -PublishedClientMsiPath C:\ResticPalRun\published-client.msi' +
    ' -CandidateMsiPath C:\ResticPalRun\candidate.msi' +
    ' -AppCastPath C:\ResticPalRun\appcast.xml' +
    ' -AppCastSignaturePath C:\ResticPalRun\appcast.xml.signature' +
    " -ExpectedPublishedVersion $ExpectedPublishedVersion" +
    " -ExpectedCandidateVersion $ExpectedCandidateVersion" +
    " -ExpectedPublishedSha256 $publishedHash" +
    " -ExpectedCandidateSha256 $candidateHash" +
    " -ExpectedAppCastSha256 $appCastHash" +
    " -ExpectedAppCastSignatureSha256 $appCastSignatureHash" +
    " -PublishedReleaseAssetName $($publishedRelease.asset_name)" +
    " -PublishedReleaseAssetLength $($publishedRelease.asset_length)" +
    " -PublishedReleaseAssetUrl $($publishedRelease.asset_url)" +
    " -EnclosureUrl $enclosureUrl" +
    " -EnclosureSignature $enclosureSignature" +
    " -InstallerLaunchTimeoutMinutes $InstallerLaunchTimeoutMinutes" +
    " -InstallerCompletionTimeoutMinutes $InstallerCompletionTimeoutMinutes" +
    " -InstallationMode $InstallationMode" +
    ' -ResultRoot C:\ResticPalRun')
if ($bridgeTransition) {
    $guestArguments += (
        ' -BridgeTransition' +
        ' -ProbeAppCastPath C:\ResticPalRun\probe-appcast.xml' +
        ' -ProbeAppCastSignaturePath C:\ResticPalRun\probe-appcast.xml.signature' +
        " -ProbePayloadPath C:\ResticPalRun\$($probe.payload_name)" +
        " -ExpectedProbeVersion $($probe.version)" +
        " -ExpectedProbeAppCastSha256 $($probe.appcast_sha256)" +
        " -ExpectedProbeAppCastSignatureSha256 $($probe.appcast_signature_sha256)" +
        " -ExpectedProbePayloadSha256 $($probe.payload_sha256)" +
        " -ExpectedProbePayloadLength $($probe.payload_length)" +
        " -ExpectedProbePayloadUrl $($probe.payload_url)" +
        " -ExpectedProbePackageSignature $($probe.expected_signature)")
}
$guestLaunchCommand = (
    'powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass' +
    ' -File C:\ResticPalSource\scripts\Invoke-WindowsSandboxUpdateTest.ps1' +
    $guestArguments +
    ' -KeepOpen')
$guestLauncherPath = Join-Path $runRoot 'guest-launch.cmd'
$guestLauncher = @"
@echo off
$guestLaunchCommand > C:\ResticPalRun\guest-bootstrap-output.log 2>&1
set "RESTICPAL_GUEST_EXIT=%ERRORLEVEL%"
> C:\ResticPalRun\guest-exit-code.txt echo %RESTICPAL_GUEST_EXIT%
exit /b %RESTICPAL_GUEST_EXIT%
"@
[IO.File]::WriteAllText(
    $guestLauncherPath,
    $guestLauncher,
    [Text.UTF8Encoding]::new($false))
$configuration = @"
<Configuration>
  <VGpu>Disable</VGpu>
  <Networking>$networking</Networking>
  <AudioInput>Disable</AudioInput>
  <VideoInput>Disable</VideoInput>
  <PrinterRedirection>Disable</PrinterRedirection>
  <ClipboardRedirection>Disable</ClipboardRedirection>
  <MemoryInMB>$MemoryInMB</MemoryInMB>
  <MappedFolders>
    <MappedFolder>
      <HostFolder>$escapedRepositoryRoot</HostFolder>
      <SandboxFolder>C:\ResticPalSource</SandboxFolder>
      <ReadOnly>true</ReadOnly>
    </MappedFolder>
    <MappedFolder>
      <HostFolder>$escapedRunRoot</HostFolder>
      <SandboxFolder>C:\ResticPalRun</SandboxFolder>
      <ReadOnly>false</ReadOnly>
    </MappedFolder>
  </MappedFolders>
  <LogonCommand>
    <Command>cmd.exe /d /c C:\ResticPalRun\guest-launch.cmd</Command>
  </LogonCommand>
</Configuration>
"@
$configurationPath = Join-Path $runRoot 'resticpal-update-test.wsb'
[IO.File]::WriteAllText($configurationPath, $configuration, [Text.UTF8Encoding]::new($false))

$qualification = [ordered]@{
    published_client_path = $publishedClientMsi
    published_version = $ExpectedPublishedVersion
    published_release = $publishedRelease
    candidate_path = $candidateMsi
    candidate_version = $ExpectedCandidateVersion
    candidate_sha256 = $candidateHash
    appcast_path = $appCast
    appcast_sha256 = $appCastHash
    appcast_signature_path = $appCastSignature
    appcast_signature_sha256 = $appCastSignatureHash
    enclosure_url = $enclosureUrl
    enclosure_signature = $enclosureSignature
    bridge_transition = $bridgeTransition
    candidate_tray_probe = $probe
    installation_mode = $InstallationMode.ToLowerInvariant()
    installer_launch_timeout_minutes = $InstallerLaunchTimeoutMinutes
    installer_completion_timeout_minutes = $InstallerCompletionTimeoutMinutes
}
$qualification | ConvertTo-Json -Depth 4 |
    Set-Content -LiteralPath (Join-Path $runRoot 'host-inputs.json') -Encoding UTF8

if ($GenerateOnly) {
    Write-Host "Generated $configurationPath"
    Get-Item -LiteralPath $configurationPath
    return
}

if ($null -ne $sandboxCliCommand) {
    $listOutput = & $sandboxCliCommand.Source list --raw 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "Could not query Windows Sandbox environments: $($listOutput | Out-String)"
    }
    $runningSandboxes = $listOutput | Out-String | ConvertFrom-Json
    if (@($runningSandboxes.WindowsSandboxEnvironments).Count -gt 0) {
        throw 'Another Windows Sandbox session is active. Close it before starting this test.'
    }
} else {
    $activeSandboxProcesses = @(Get-Process `
        -Name WindowsSandboxRemoteSession, WindowsSandboxServer `
        -ErrorAction SilentlyContinue)
    if ($activeSandboxProcesses.Count -gt 0) {
        throw 'Another Windows Sandbox session is active. Close it before starting this test.'
    }
}

Write-Host (
    "Starting $($InstallationMode.ToLowerInvariant()) published-client update qualification " +
    "$ExpectedPublishedVersion -> " +
    "$ExpectedCandidateVersion. Installer launch/completion budgets: " +
    "$InstallerLaunchTimeoutMinutes/$InstallerCompletionTimeoutMinutes minutes. Results: $runRoot")
$sandboxId = $null
$guestExecutionJob = $null
$sandboxCliTranscript = Join-Path $runRoot 'sandbox-cli-exec.log'

function Stop-AutomatedSandbox {
    if ($useSandboxCli -and -not [string]::IsNullOrWhiteSpace($sandboxId)) {
        & $sandboxCliCommand.Source stop --id $sandboxId --raw 2>&1 | Out-Null
        return
    }
    Get-Process `
        -Name WindowsSandboxRemoteSession, WindowsSandboxServer `
        -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
}

if ($useSandboxCli) {
    try {
        $startOutput = & $sandboxCliCommand.Source start --config $runtimeConfiguration --raw 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "Windows Sandbox CLI could not start a guest: $($startOutput | Out-String)"
        }
        $startResult = $startOutput | Out-String | ConvertFrom-Json
        $sandboxId = $startResult.Id
        if ([string]::IsNullOrWhiteSpace($sandboxId)) {
            throw 'Windows Sandbox CLI did not return a guest ID.'
        }

        $shareOutput = & $sandboxCliCommand.Source share `
            --id $sandboxId `
            --host-path $repositoryRoot `
            --sandbox-path 'C:\ResticPalSource' `
            --raw 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "Could not share the source tree: $($shareOutput | Out-String)"
        }
        $shareOutput = & $sandboxCliCommand.Source share `
            --id $sandboxId `
            --host-path $runRoot `
            --sandbox-path 'C:\ResticPalRun' `
            --allow-write `
            --raw 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "Could not share the update test directory: $($shareOutput | Out-String)"
        }

        $null = Start-Process `
            -FilePath $sandboxCliCommand.Source `
            -ArgumentList @('connect', '--id', $sandboxId) `
            -WindowStyle Hidden
        $loginDeadline = [DateTime]::UtcNow.AddSeconds(60)
        do {
            Start-Sleep -Seconds 2
            $previousErrorPreference = $ErrorActionPreference
            try {
                $ErrorActionPreference = 'Continue'
                $loginOutput = & $sandboxCliCommand.Source exec `
                    --id $sandboxId `
                    --run-as ExistingLogin `
                    --command 'cmd.exe /d /c exit 0' `
                    --raw 2>&1
                $loginExitCode = $LASTEXITCODE
            } finally {
                $ErrorActionPreference = $previousErrorPreference
            }
        } while ($loginExitCode -ne 0 -and [DateTime]::UtcNow -lt $loginDeadline)
        if ($loginExitCode -ne 0) {
            throw "Windows Sandbox did not establish its login: $($loginOutput | Out-String)"
        }

        $guestCommand = 'cmd.exe /d /c C:\ResticPalRun\guest-launch.cmd'
        $guestExecutionJob = Start-Job -ScriptBlock {
            param($Executable, $SandboxId, $Command)
            $output = & $Executable exec `
                --id $SandboxId `
                --run-as ExistingLogin `
                --working-directory 'C:\ResticPalSource' `
                --command $Command `
                --raw 2>&1
            [pscustomobject]@{
                ExitCode = $LASTEXITCODE
                Output = $output | Out-String
            }
        } -ArgumentList $sandboxCliCommand.Source, $sandboxId, $guestCommand
    } catch {
        Stop-AutomatedSandbox
        throw
    }
} else {
    $null = Start-Process -FilePath $sandboxExecutable -ArgumentList "`"$configurationPath`""
}

$resultPath = Join-Path $runRoot 'result.json'
$deadline = [DateTime]::UtcNow.AddMinutes($TimeoutMinutes)
$startupDeadline = [DateTime]::UtcNow.AddSeconds(120)
$guestLogPath = Join-Path $runRoot 'guest.log'
while (-not (Test-Path -LiteralPath $resultPath -PathType Leaf)) {
    if (-not (Test-Path -LiteralPath $guestLogPath -PathType Leaf) `
        -and [DateTime]::UtcNow -ge $startupDeadline) {
        Stop-AutomatedSandbox
        throw "Windows Sandbox did not start the update test within 120 seconds. See $runRoot"
    }
    if ($null -ne $guestExecutionJob `
        -and $guestExecutionJob.State -in @('Completed', 'Failed', 'Stopped')) {
        $executionResult = Receive-Job -Job $guestExecutionJob -ErrorAction SilentlyContinue
        $executionResult | Format-List | Out-String |
            Set-Content -LiteralPath $sandboxCliTranscript -Encoding UTF8
        Remove-Job -Job $guestExecutionJob -Force -ErrorAction SilentlyContinue
        $guestExecutionJob = $null
        if (-not $KeepOpen) {
            Stop-AutomatedSandbox
        }
        $bootstrapOutput = Join-Path $runRoot 'guest-bootstrap-output.log'
        throw ("The guest stopped before writing a result. See $bootstrapOutput and " +
               "$sandboxCliTranscript")
    }
    if ([DateTime]::UtcNow -ge $deadline) {
        Stop-AutomatedSandbox
        throw "Windows Sandbox update test timed out after $TimeoutMinutes minutes. See $runRoot"
    }
    Start-Sleep -Seconds 1
}

$result = Get-Content -LiteralPath $resultPath -Raw | ConvertFrom-Json
if ($null -ne $guestExecutionJob) {
    $null = Wait-Job -Job $guestExecutionJob -Timeout 10
    $executionResult = Receive-Job -Job $guestExecutionJob -ErrorAction SilentlyContinue
    $executionResult | Format-List | Out-String |
        Set-Content -LiteralPath $sandboxCliTranscript -Encoding UTF8
    Stop-Job -Job $guestExecutionJob -ErrorAction SilentlyContinue
    Remove-Job -Job $guestExecutionJob -Force -ErrorAction SilentlyContinue
    $guestExecutionJob = $null
}
if (-not $KeepOpen) {
    Stop-AutomatedSandbox
}

$summary = [pscustomobject]@{
    Status = $result.status
    ExitCode = $result.exit_code
    WindowsBuild = $result.windows_build
    PublishedVersion = $result.published_version
    CandidateVersion = $result.candidate_version
    InstallationMode = $result.installation_mode
    StagedPath = $result.staged_update.path
    StagedSha256 = $result.staged_update.sha256
    InstallerSession = $result.verification.candidate_installer_session_id
    InstallerOwner = $result.verification.candidate_installer_owner
    InstallerSilent = $result.verification.candidate_installer_silent
    UserInterventionFree = $result.verification.no_user_confirmation_or_dialog_intervention
    ServiceIdentity = $result.verification.service_identity
    ServiceState = $result.verification.service_state
    PublishedServicePid = $result.verification.baseline_service_process_id
    UpgradedServicePid = $result.verification.upgraded_service_process_id
    TrayProcessCount = $result.verification.tray_process_count
    QualificationResult = $resultPath
    RunDirectory = $runRoot
    Transcript = Join-Path $runRoot 'guest.log'
    TestArtifacts = Join-Path $runRoot 'test-artifacts'
}
$summary | Format-List

if ($result.status -ne 'passed' -or $result.exit_code -ne 0) {
    throw "Windows Sandbox update test failed: $($result.error). See $guestLogPath"
}
