# Shared, side-effect-free validation for the two release-blocking Windows
# update qualifications. Publish-Release.ps1 dot-sources this file, and the
# table-driven tests exercise these exact functions without GitHub or network
# access.

$script:FrozenLegacyAppCastLength = [uint64]969
$script:FrozenLegacyAppCastSha256 =
    'eeffa6fc466c0d3f5c95043538742665732a044118286ff94368c163fef7a4e2'
$script:FrozenLegacySignatureLength = [uint64]88
$script:FrozenLegacySignatureSha256 =
    '85d591ce0a7d936be3da429583737838f3ef075565a483c32dc7faaa6085d377'

function Assert-QualificationJsonObject {
    param(
        [AllowNull()] $Value,
        [Parameter(Mandatory)] [string] $Path
    )

    if ($null -eq $Value -or
        ($Value -isnot [pscustomobject] -and
         $Value -isnot [Collections.IDictionary])) {
        throw "$Path must be a JSON object."
    }
}

function Get-RequiredQualificationProperty {
    param(
        [AllowNull()] $InputObject,
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $Path
    )

    Assert-QualificationJsonObject -Value $InputObject -Path $Path
    if ($InputObject -is [Collections.IDictionary]) {
        $keys = @($InputObject.Keys | Where-Object { [string]$_ -ceq $Name })
        if ($keys.Count -ne 1) {
            throw "$Path.$Name is required with exact casing."
        }
        return $InputObject[$keys[0]]
    }

    $properties = @(
        $InputObject.PSObject.Properties |
            Where-Object Name -CEQ $Name)
    if ($properties.Count -ne 1) {
        throw "$Path.$Name is required with exact casing."
    }
    return $properties[0].Value
}

function Get-RequiredQualificationArray {
    param(
        [AllowNull()] $InputObject,
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $Path
    )

    Assert-QualificationJsonObject -Value $InputObject -Path $Path
    if ($InputObject -is [Collections.IDictionary]) {
        $keys = @($InputObject.Keys | Where-Object { [string]$_ -ceq $Name })
        if ($keys.Count -ne 1) {
            throw "$Path.$Name is required with exact casing."
        }
        $value = $InputObject[$keys[0]]
    } else {
        $properties = @(
            $InputObject.PSObject.Properties |
                Where-Object Name -CEQ $Name)
        if ($properties.Count -ne 1) {
            throw "$Path.$Name is required with exact casing."
        }
        $value = $properties[0].Value
    }
    if ($value -isnot [Array]) {
        throw "$Path.$Name must be a JSON array."
    }
    return $value
}

function Get-RequiredQualificationString {
    param(
        [AllowNull()] $InputObject,
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $Path,
        [switch] $AllowEmpty
    )

    $value = Get-RequiredQualificationProperty `
        -InputObject $InputObject -Name $Name -Path $Path
    if ($value -isnot [string] -or
        (-not $AllowEmpty -and [string]::IsNullOrWhiteSpace($value))) {
        throw "$Path.$Name must be a non-empty JSON string."
    }
    return [string]$value
}

function Get-RequiredQualificationBoolean {
    param(
        [AllowNull()] $InputObject,
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $Path
    )

    $value = Get-RequiredQualificationProperty `
        -InputObject $InputObject -Name $Name -Path $Path
    if ($value -isnot [bool]) {
        throw "$Path.$Name must be a JSON boolean."
    }
    return [bool]$value
}

function Test-QualificationIntegerType {
    param([AllowNull()] $Value)

    return ($Value -is [byte] -or
            $Value -is [sbyte] -or
            $Value -is [int16] -or
            $Value -is [uint16] -or
            $Value -is [int32] -or
            $Value -is [uint32] -or
            $Value -is [int64] -or
            $Value -is [uint64])
}

function Get-RequiredQualificationUInt64 {
    param(
        [AllowNull()] $InputObject,
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $Path,
        [uint64] $Maximum = [uint64]::MaxValue
    )

    $value = Get-RequiredQualificationProperty `
        -InputObject $InputObject -Name $Name -Path $Path
    if (-not (Test-QualificationIntegerType $value)) {
        throw "$Path.$Name must be a non-negative JSON integer."
    }
    try {
        $converted = [uint64]$value
    } catch {
        throw "$Path.$Name must be a non-negative JSON integer."
    }
    if ($converted -gt $Maximum) {
        throw "$Path.$Name exceeds its allowed range."
    }
    return $converted
}

function Get-RequiredQualificationHash {
    param(
        [AllowNull()] $InputObject,
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $Path
    )

    $value = Get-RequiredQualificationString `
        -InputObject $InputObject -Name $Name -Path $Path
    if ($value -cnotmatch '^[0-9a-f]{64}$') {
        throw "$Path.$Name must be a lowercase SHA-256 digest."
    }
    return $value
}

function Assert-QualificationExactString {
    param(
        [Parameter(Mandatory)] [string] $Actual,
        [Parameter(Mandatory)] [string] $Expected,
        [Parameter(Mandatory)] [string] $Path
    )

    if ($Actual -cne $Expected) {
        throw "$Path must be '$Expected'."
    }
}

function Get-RequiredQualificationTimestamp {
    param(
        [AllowNull()] $InputObject,
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $Path
    )

    $value = Get-RequiredQualificationProperty `
        -InputObject $InputObject -Name $Name -Path $Path
    if ($value -is [DateTimeOffset]) {
        return ([DateTimeOffset]$value).ToUniversalTime()
    }
    if ($value -is [DateTime]) {
        return ([DateTimeOffset]([DateTime]$value)).ToUniversalTime()
    }
    if ($value -isnot [string] -or [string]::IsNullOrWhiteSpace($value)) {
        throw "$Path.$Name must be an RFC3339 JSON string."
    }
    $parsed = [DateTimeOffset]::MinValue
    if (-not [DateTimeOffset]::TryParseExact(
            $value,
            'o',
            [Globalization.CultureInfo]::InvariantCulture,
            [Globalization.DateTimeStyles]::RoundtripKind,
            [ref]$parsed)) {
        throw "$Path.$Name must be an RFC3339 round-trip timestamp."
    }
    return $parsed.ToUniversalTime()
}

function Get-QualificationPackageRecord {
    param(
        [Parameter(Mandatory)] $Value,
        [Parameter(Mandatory)] [string] $Path
    )

    Assert-QualificationJsonObject -Value $Value -Path $Path
    $version = Get-RequiredQualificationString `
        -InputObject $Value -Name version -Path $Path
    $url = Get-RequiredQualificationString `
        -InputObject $Value -Name url -Path $Path
    $signature = Get-RequiredQualificationString `
        -InputObject $Value -Name signature -Path $Path
    $length = Get-RequiredQualificationUInt64 `
        -InputObject $Value -Name length -Path $Path
    if ($length -eq 0) {
        throw "$Path.length must be greater than zero."
    }
    return [pscustomobject]@{
        Version = $version
        Url = $url
        Signature = $signature
        Length = $length
    }
}

function Assert-ProbeRequest {
    param(
        [Parameter(Mandatory)] $Requests,
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [string] $ExpectedUrl,
        [Parameter(Mandatory)] [string] $ExpectedUserAgent
    )

    $request = Get-RequiredQualificationProperty `
        -InputObject $Requests -Name $Name -Path 'evidence.verification.candidate_tray_probe.requests'
    Assert-QualificationJsonObject `
        -Value $request `
        -Path "evidence.verification.candidate_tray_probe.requests.$Name"
    $url = Get-RequiredQualificationString `
        -InputObject $request -Name url `
        -Path "evidence.verification.candidate_tray_probe.requests.$Name"
    $userAgent = Get-RequiredQualificationString `
        -InputObject $request -Name user_agent `
        -Path "evidence.verification.candidate_tray_probe.requests.$Name"
    Assert-QualificationExactString `
        -Actual $url -Expected $ExpectedUrl `
        -Path "evidence.verification.candidate_tray_probe.requests.$Name.url"
    Assert-QualificationExactString `
        -Actual $userAgent -Expected $ExpectedUserAgent `
        -Path "evidence.verification.candidate_tray_probe.requests.$Name.user_agent"
}

function Assert-QualificationEd25519Signature {
    param(
        [Parameter(Mandatory)] [string] $Value,
        [Parameter(Mandatory)] [string] $Path
    )

    try {
        $bytes = [Convert]::FromBase64String($Value)
    } catch {
        throw "$Path must be a base64-encoded Ed25519 signature."
    }
    if ($bytes.Length -ne 64) {
        throw "$Path must decode to exactly 64 bytes."
    }
}

function Get-QualificationManifestFileRecord {
    param(
        [Parameter(Mandatory)] $Files,
        [Parameter(Mandatory)] [string] $Name
    )

    $record = Get-RequiredQualificationProperty `
        -InputObject $Files -Name $Name -Path 'release_manifest.files'
    Assert-QualificationJsonObject `
        -Value $record -Path "release_manifest.files.$Name"
    $recordName = Get-RequiredQualificationString `
        -InputObject $record -Name name -Path "release_manifest.files.$Name"
    $recordLength = Get-RequiredQualificationUInt64 `
        -InputObject $record -Name length -Path "release_manifest.files.$Name"
    if ($recordLength -eq 0) {
        throw "release_manifest.files.$Name.length must be greater than zero."
    }
    $recordHash = Get-RequiredQualificationHash `
        -InputObject $record -Name sha256 -Path "release_manifest.files.$Name"
    return [pscustomobject]@{
        Name = $recordName
        Length = $recordLength
        Sha256 = $recordHash
    }
}

function Read-UpdateQualificationEvidence {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] [string] $LiteralPath,
        [uint64] $MaximumLength = 4MB
    )

    $file = Get-Item -LiteralPath (
        Resolve-Path -LiteralPath $LiteralPath).Path -ErrorAction Stop
    if ($file.PSIsContainer) {
        throw "Update qualification evidence is not a file: $($file.FullName)"
    }

    $stream = [IO.File]::Open(
        $file.FullName,
        [IO.FileMode]::Open,
        [IO.FileAccess]::Read,
        [IO.FileShare]::Read)
    try {
        if ($stream.Length -eq 0 -or [uint64]$stream.Length -gt $MaximumLength) {
            throw "Update qualification evidence must contain 1 to $MaximumLength bytes."
        }
        $bytes = [byte[]]::new([int]$stream.Length)
        $offset = 0
        while ($offset -lt $bytes.Length) {
            $read = $stream.Read($bytes, $offset, $bytes.Length - $offset)
            if ($read -eq 0) {
                throw 'Update qualification evidence ended before its recorded length.'
            }
            $offset += $read
        }
        if ($stream.ReadByte() -ne -1) {
            throw 'Update qualification evidence changed while it was being read.'
        }
    } finally {
        $stream.Dispose()
    }

    $sha256 = [Security.Cryptography.SHA256]::Create()
    try {
        $digest = [BitConverter]::ToString($sha256.ComputeHash($bytes)).Replace('-', '').ToLowerInvariant()
    } finally {
        $sha256.Dispose()
    }
    try {
        $json = [Text.UTF8Encoding]::new($false, $true).GetString($bytes)
    } catch {
        throw "Update qualification evidence is not valid UTF-8: $($_.Exception.Message)"
    }
    try {
        $evidence = $json | ConvertFrom-Json -ErrorAction Stop
    } catch {
        throw "Update qualification evidence is not valid JSON: $($_.Exception.Message)"
    }
    Assert-QualificationJsonObject -Value $evidence -Path 'evidence'

    return [pscustomobject]@{
        Evidence = $evidence
        FileName = $file.Name
        FullName = $file.FullName
        Length = [uint64]$bytes.Length
        Sha256 = $digest
    }
}

function Assert-BridgeCandidateTrayEvidence {
    param(
        [Parameter(Mandatory)] $Verification,
        [Parameter(Mandatory)] $UpdatePackage,
        [Parameter(Mandatory)] $ProbeManifest,
        [Parameter(Mandatory)] $AppcastRecord,
        [Parameter(Mandatory)] $AppcastSignatureRecord,
        [Parameter(Mandatory)] [uint32] $UpgradedServiceProcessId,
        [Parameter(Mandatory)] [uint32] $UpgradedTrayProcessId,
        [Parameter(Mandatory)] [string] $CandidateVersion
    )

    $serviceProtocolVersion = Get-RequiredQualificationUInt64 `
        -InputObject $Verification -Name upgraded_service_protocol_version `
        -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
    $trayProtocolVersion = Get-RequiredQualificationUInt64 `
        -InputObject $Verification -Name upgraded_tray_protocol_version `
        -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
    if ($serviceProtocolVersion -ne 4 -or $trayProtocolVersion -ne 4) {
        throw 'Bridge qualification must prove upgraded service and tray protocol version 4.'
    }

    $dispatch = Get-RequiredQualificationProperty `
        -InputObject $Verification -Name dispatch_bridge `
        -Path 'evidence.verification'
    Assert-QualificationJsonObject `
        -Value $dispatch -Path 'evidence.verification.dispatch_bridge'
    $dispatchReason = Get-RequiredQualificationString `
        -InputObject $dispatch -Name reason `
        -Path 'evidence.verification.dispatch_bridge'
    $dispatchProtocol = Get-RequiredQualificationUInt64 `
        -InputObject $dispatch -Name protocol_version `
        -Path 'evidence.verification.dispatch_bridge' -Maximum ([uint32]::MaxValue)
    $requestType = Get-RequiredQualificationString `
        -InputObject $dispatch -Name request_type `
        -Path 'evidence.verification.dispatch_bridge'
    $responseType = Get-RequiredQualificationString `
        -InputObject $dispatch -Name response_type `
        -Path 'evidence.verification.dispatch_bridge'
    $dispatchAppcastHash = Get-RequiredQualificationHash `
        -InputObject $dispatch -Name appcast_sha256 `
        -Path 'evidence.verification.dispatch_bridge'
    $dispatchSignatureHash = Get-RequiredQualificationHash `
        -InputObject $dispatch -Name appcast_signature_sha256 `
        -Path 'evidence.verification.dispatch_bridge'
    if ($dispatchReason -cne 'published-v1.0.6-tray-error-pipe-busy' -or
        $dispatchProtocol -ne 3 -or
        $requestType -cne 'install_update' -or
        $responseType -cne 'accepted' -or
        $dispatchAppcastHash -cne $AppcastRecord.Sha256 -or
        $dispatchSignatureHash -cne $AppcastSignatureRecord.Sha256) {
        throw 'Bridge qualification did not prove an Accepted v3 install_update for the prepared v2 feed.'
    }
    $dispatchPackageValue = Get-RequiredQualificationProperty `
        -InputObject $dispatch -Name package `
        -Path 'evidence.verification.dispatch_bridge'
    $dispatchPackage = Get-QualificationPackageRecord `
        -Value $dispatchPackageValue `
        -Path 'evidence.verification.dispatch_bridge.package'
    if ($dispatchPackage.Version -cne $UpdatePackage.Version -or
        $dispatchPackage.Url -cne $UpdatePackage.Url -or
        $dispatchPackage.Signature -cne $UpdatePackage.Signature -or
        $dispatchPackage.Length -ne $UpdatePackage.Length) {
        throw 'Bridge install_update package metadata does not match the prepared v2 appcast enclosure.'
    }

    $probe = Get-RequiredQualificationProperty `
        -InputObject $Verification -Name candidate_tray_probe `
        -Path 'evidence.verification'
    Assert-QualificationJsonObject `
        -Value $probe -Path 'evidence.verification.candidate_tray_probe'
    $probeProtocol = Get-RequiredQualificationUInt64 `
        -InputObject $probe -Name protocol_version `
        -Path 'evidence.verification.candidate_tray_probe' `
        -Maximum ([uint32]::MaxValue)
    $probeVersion = Get-RequiredQualificationString `
        -InputObject $probe -Name probe_version `
        -Path 'evidence.verification.candidate_tray_probe'
    $probeAppcastHash = Get-RequiredQualificationHash `
        -InputObject $probe -Name appcast_sha256 `
        -Path 'evidence.verification.candidate_tray_probe'
    $probeSignatureHash = Get-RequiredQualificationHash `
        -InputObject $probe -Name appcast_signature_sha256 `
        -Path 'evidence.verification.candidate_tray_probe'
    if ($probeProtocol -ne 4 -or
        $probeVersion -cne $ProbeManifest.Version -or
        $probeAppcastHash -cne $ProbeManifest.AppcastSha256 -or
        $probeSignatureHash -cne $ProbeManifest.AppcastSignatureSha256) {
        throw 'Candidate-tray probe protocol, version, or signed appcast bytes do not match the prepared probe.'
    }

    $probePayloadValue = Get-RequiredQualificationProperty `
        -InputObject $probe -Name payload `
        -Path 'evidence.verification.candidate_tray_probe'
    Assert-QualificationJsonObject `
        -Value $probePayloadValue `
        -Path 'evidence.verification.candidate_tray_probe.payload'
    $payloadName = Get-RequiredQualificationString `
        -InputObject $probePayloadValue -Name name `
        -Path 'evidence.verification.candidate_tray_probe.payload'
    $payloadUrl = Get-RequiredQualificationString `
        -InputObject $probePayloadValue -Name url `
        -Path 'evidence.verification.candidate_tray_probe.payload'
    $payloadLength = Get-RequiredQualificationUInt64 `
        -InputObject $probePayloadValue -Name length `
        -Path 'evidence.verification.candidate_tray_probe.payload'
    $payloadHash = Get-RequiredQualificationHash `
        -InputObject $probePayloadValue -Name sha256 `
        -Path 'evidence.verification.candidate_tray_probe.payload'
    $payloadExpectedSignature = Get-RequiredQualificationString `
        -InputObject $probePayloadValue -Name expected_signature `
        -Path 'evidence.verification.candidate_tray_probe.payload'
    if ($payloadName -cne $ProbeManifest.PayloadName -or
        $payloadUrl -cne $ProbeManifest.PayloadUrl -or
        $payloadLength -ne $ProbeManifest.PayloadLength -or
        $payloadHash -cne $ProbeManifest.PayloadSha256 -or
        $payloadExpectedSignature -cne $ProbeManifest.ExpectedSignature) {
        throw 'Candidate-tray probe payload metadata does not match the exact prepared sentinel.'
    }

    $requests = Get-RequiredQualificationProperty `
        -InputObject $probe -Name requests `
        -Path 'evidence.verification.candidate_tray_probe'
    Assert-QualificationJsonObject `
        -Value $requests -Path 'evidence.verification.candidate_tray_probe.requests'
    $userAgent = "resticpal/$CandidateVersion"
    Assert-ProbeRequest `
        -Requests $requests `
        -Name appcast `
        -ExpectedUrl 'https://updates.resticpal.com/appcast-v2.xml' `
        -ExpectedUserAgent $userAgent
    Assert-ProbeRequest `
        -Requests $requests `
        -Name appcast_signature `
        -ExpectedUrl 'https://updates.resticpal.com/appcast-v2.xml.signature' `
        -ExpectedUserAgent $userAgent
    Assert-ProbeRequest `
        -Requests $requests `
        -Name payload `
        -ExpectedUrl $ProbeManifest.PayloadUrl `
        -ExpectedUserAgent $userAgent

    $diagnostics = @(Get-RequiredQualificationArray `
        -InputObject $probe -Name diagnostics `
        -Path 'evidence.verification.candidate_tray_probe')
    if ($diagnostics.Count -ne 2) {
        throw 'Candidate-tray probe must contain exactly update.started then update.failed diagnostics.'
    }
    $startedCode = Get-RequiredQualificationString `
        -InputObject $diagnostics[0] -Name code `
        -Path 'evidence.verification.candidate_tray_probe.diagnostics[0]'
    $startedAt = Get-RequiredQualificationTimestamp `
        -InputObject $diagnostics[0] -Name observed_at `
        -Path 'evidence.verification.candidate_tray_probe.diagnostics[0]'
    $failedCode = Get-RequiredQualificationString `
        -InputObject $diagnostics[1] -Name code `
        -Path 'evidence.verification.candidate_tray_probe.diagnostics[1]'
    $failureCode = Get-RequiredQualificationString `
        -InputObject $diagnostics[1] -Name failure_code `
        -Path 'evidence.verification.candidate_tray_probe.diagnostics[1]'
    $failedAt = Get-RequiredQualificationTimestamp `
        -InputObject $diagnostics[1] -Name observed_at `
        -Path 'evidence.verification.candidate_tray_probe.diagnostics[1]'
    if ($startedCode -cne 'update.started' -or
        $failedCode -cne 'update.failed' -or
        $failureCode -cne 'update_signature_invalid' -or
        $failedAt -le $startedAt) {
        throw 'Candidate-tray probe did not prove update.started followed by update.failed/update_signature_invalid.'
    }

    $finalPath = Get-RequiredQualificationString `
        -InputObject $probe -Name final_path `
        -Path 'evidence.verification.candidate_tray_probe'
    $finalExists = Get-RequiredQualificationBoolean `
        -InputObject $probe -Name final_exists `
        -Path 'evidence.verification.candidate_tray_probe'
    $partialPath = Get-RequiredQualificationString `
        -InputObject $probe -Name partial_path `
        -Path 'evidence.verification.candidate_tray_probe'
    $partialExists = Get-RequiredQualificationBoolean `
        -InputObject $probe -Name partial_exists `
        -Path 'evidence.verification.candidate_tray_probe'
    $stagingEntries = @(Get-RequiredQualificationArray `
        -InputObject $probe -Name staging_entries `
        -Path 'evidence.verification.candidate_tray_probe')
    $installerCount = Get-RequiredQualificationUInt64 `
        -InputObject $probe -Name msiexec_process_count `
        -Path 'evidence.verification.candidate_tray_probe' `
        -Maximum ([uint32]::MaxValue)
    $trayProcessId = Get-RequiredQualificationUInt64 `
        -InputObject $probe -Name tray_process_id `
        -Path 'evidence.verification.candidate_tray_probe' `
        -Maximum ([uint32]::MaxValue)
    $serviceProcessId = Get-RequiredQualificationUInt64 `
        -InputObject $probe -Name service_process_id `
        -Path 'evidence.verification.candidate_tray_probe' `
        -Maximum ([uint32]::MaxValue)
    $expectedFinalPath = "C:\ProgramData\ResticPal\Updates\$($ProbeManifest.PayloadName)"
    $expectedPartialPath = "$expectedFinalPath.partial"
    if (-not [string]::Equals(
            $finalPath, $expectedFinalPath, [StringComparison]::OrdinalIgnoreCase) -or
        -not [string]::Equals(
            $partialPath, $expectedPartialPath, [StringComparison]::OrdinalIgnoreCase) -or
        $finalExists -or $partialExists -or $stagingEntries.Count -ne 0 -or
        $installerCount -ne 0 -or
        $trayProcessId -ne $UpgradedTrayProcessId -or
        $serviceProcessId -ne $UpgradedServiceProcessId) {
        throw 'Candidate-tray probe left staged bytes, launched msiexec, or did not run through the upgraded tray/service.'
    }

    return [ordered]@{
        upgraded_service_protocol_version = [uint32]$serviceProtocolVersion
        upgraded_tray_protocol_version = [uint32]$trayProtocolVersion
        dispatch_bridge = [ordered]@{
            reason = $dispatchReason
            protocol_version = [uint32]$dispatchProtocol
            request_type = $requestType
            response_type = $responseType
            appcast_sha256 = $dispatchAppcastHash
            appcast_signature_sha256 = $dispatchSignatureHash
            package = [ordered]@{
                version = $dispatchPackage.Version
                url = $dispatchPackage.Url
                signature = $dispatchPackage.Signature
                length = $dispatchPackage.Length
            }
        }
        candidate_tray_probe = [ordered]@{
            protocol_version = [uint32]$probeProtocol
            probe_version = $probeVersion
            appcast_sha256 = $probeAppcastHash
            appcast_signature_sha256 = $probeSignatureHash
            payload = [ordered]@{
                name = $payloadName
                url = $payloadUrl
                length = $payloadLength
                sha256 = $payloadHash
                expected_signature = $payloadExpectedSignature
            }
            failure_code = $failureCode
            final_path = $finalPath
            final_exists = $finalExists
            partial_path = $partialPath
            partial_exists = $partialExists
            staging_entries = @()
            msiexec_process_count = [uint32]$installerCount
            tray_process_id = [uint32]$trayProcessId
            service_process_id = [uint32]$serviceProcessId
        }
    }
}

function Assert-UpdateQualificationEvidence {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] $LoadedEvidence,
        [Parameter(Mandatory)] $Manifest,
        [Parameter(Mandatory)]
        [ValidateSet('prompted', 'automatic')]
        [string] $ExpectedInstallationMode,
        [Parameter(Mandatory)] [string] $Version,
        [Parameter(Mandatory)] [string] $Tag,
        [Parameter(Mandatory)] [string] $PreviousVersion,
        [Parameter(Mandatory)] $PublishedRelease
    )

    Assert-QualificationJsonObject -Value $LoadedEvidence -Path 'loaded_evidence'
    $evidence = Get-RequiredQualificationProperty `
        -InputObject $LoadedEvidence -Name Evidence -Path 'loaded_evidence'
    Assert-QualificationJsonObject -Value $evidence -Path 'evidence'
    $evidenceFileName = Get-RequiredQualificationString `
        -InputObject $LoadedEvidence -Name FileName -Path 'loaded_evidence'
    $evidenceFullName = Get-RequiredQualificationString `
        -InputObject $LoadedEvidence -Name FullName -Path 'loaded_evidence'
    $evidenceLength = Get-RequiredQualificationUInt64 `
        -InputObject $LoadedEvidence -Name Length -Path 'loaded_evidence'
    if ($evidenceLength -eq 0) {
        throw 'loaded_evidence.Length must be greater than zero.'
    }
    $evidenceHash = Get-RequiredQualificationHash `
        -InputObject $LoadedEvidence -Name Sha256 -Path 'loaded_evidence'
    if ($evidenceFileName -cne [IO.Path]::GetFileName($evidenceFullName)) {
        throw 'loaded_evidence.FileName does not match loaded_evidence.FullName.'
    }

    Assert-QualificationJsonObject -Value $Manifest -Path 'release_manifest'
    $isBridgeTransition = (
        $PreviousVersion -ceq '1.0.6' -and $Version -ceq '1.0.7')
    if (-not $isBridgeTransition -and
        ([Version]$Version -le [Version]'1.0.7' -or
         [Version]$PreviousVersion -lt [Version]'1.0.7')) {
        throw 'Steady-state qualification requires a v1.0.8+ candidate and v1.0.7+ previous client.'
    }
    $expectedManifestSchema = if ($isBridgeTransition) {
        [uint64]5
    } else {
        [uint64]6
    }
    $manifestSchema = Get-RequiredQualificationUInt64 `
        -InputObject $Manifest -Name schema -Path 'release_manifest'
    if ($manifestSchema -ne $expectedManifestSchema) {
        throw "release_manifest.schema must be the integer $expectedManifestSchema."
    }
    $manifestVersion = Get-RequiredQualificationString `
        -InputObject $Manifest -Name version -Path 'release_manifest'
    $manifestTag = Get-RequiredQualificationString `
        -InputObject $Manifest -Name tag -Path 'release_manifest'
    Assert-QualificationExactString `
        -Actual $manifestVersion -Expected $Version -Path 'release_manifest.version'
    Assert-QualificationExactString `
        -Actual $manifestTag -Expected $Tag -Path 'release_manifest.tag'
    $files = Get-RequiredQualificationProperty `
        -InputObject $Manifest -Name files -Path 'release_manifest'
    Assert-QualificationJsonObject -Value $files -Path 'release_manifest.files'
    $msiRecord = Get-QualificationManifestFileRecord -Files $files -Name msi
    $appcastRecord = Get-QualificationManifestFileRecord -Files $files -Name appcast_v2
    $signatureRecord = Get-QualificationManifestFileRecord `
        -Files $files -Name appcast_v2_signature
    $legacyAppcastRecord = Get-QualificationManifestFileRecord `
        -Files $files -Name legacy_appcast
    $legacySignatureRecord = Get-QualificationManifestFileRecord `
        -Files $files -Name legacy_appcast_signature
    Assert-QualificationExactString `
        -Actual $msiRecord.Name `
        -Expected "resticpal-$Version-x64.msi" `
        -Path 'release_manifest.files.msi.name'
    Assert-QualificationExactString `
        -Actual $appcastRecord.Name -Expected 'appcast-v2.xml' `
        -Path 'release_manifest.files.appcast_v2.name'
    Assert-QualificationExactString `
        -Actual $signatureRecord.Name -Expected 'appcast-v2.xml.signature' `
        -Path 'release_manifest.files.appcast_v2_signature.name'
    Assert-QualificationExactString `
        -Actual $legacyAppcastRecord.Name -Expected 'appcast.xml' `
        -Path 'release_manifest.files.legacy_appcast.name'
    Assert-QualificationExactString `
        -Actual $legacySignatureRecord.Name -Expected 'appcast.xml.signature' `
        -Path 'release_manifest.files.legacy_appcast_signature.name'
    $frozenLegacyBinding = $null
    if ($isBridgeTransition) {
        if ($legacyAppcastRecord.Length -ne $appcastRecord.Length -or
            $legacyAppcastRecord.Sha256 -cne $appcastRecord.Sha256 -or
            $legacySignatureRecord.Length -ne $signatureRecord.Length -or
            $legacySignatureRecord.Sha256 -cne $signatureRecord.Sha256) {
            throw ('release_manifest legacy appcast records must be byte-identical ' +
                   'to the v2 appcast records for the v1.0.7 bridge.')
        }
        $dualNamedFeed = Get-RequiredQualificationProperty `
            -InputObject $Manifest -Name dual_named_feed -Path 'release_manifest'
        Assert-QualificationJsonObject `
            -Value $dualNamedFeed -Path 'release_manifest.dual_named_feed'
        $dualNamedFeedVersion = Get-RequiredQualificationString `
            -InputObject $dualNamedFeed -Name version `
            -Path 'release_manifest.dual_named_feed'
        $dualNamedFeedAppcastHash = Get-RequiredQualificationHash `
            -InputObject $dualNamedFeed -Name appcast_sha256 `
            -Path 'release_manifest.dual_named_feed'
        $dualNamedFeedSignatureHash = Get-RequiredQualificationHash `
            -InputObject $dualNamedFeed -Name appcast_signature_sha256 `
            -Path 'release_manifest.dual_named_feed'
        if ($dualNamedFeedVersion -cne $Version -or
            $dualNamedFeedAppcastHash -cne $legacyAppcastRecord.Sha256 -or
            $dualNamedFeedSignatureHash -cne $legacySignatureRecord.Sha256) {
            throw ('release_manifest.dual_named_feed must bind the release version ' +
                   'and byte-identical legacy appcast file hashes.')
        }
    } else {
        if ($legacyAppcastRecord.Length -ne $script:FrozenLegacyAppCastLength -or
            $legacyAppcastRecord.Sha256 -cne $script:FrozenLegacyAppCastSha256 -or
            $legacySignatureRecord.Length -ne $script:FrozenLegacySignatureLength -or
            $legacySignatureRecord.Sha256 -cne
                $script:FrozenLegacySignatureSha256) {
            throw 'release_manifest legacy records do not match the immutable v1.0.7 byte pins.'
        }
        $candidateV2Feed = Get-RequiredQualificationProperty `
            -InputObject $Manifest -Name candidate_v2_feed -Path 'release_manifest'
        Assert-QualificationJsonObject `
            -Value $candidateV2Feed -Path 'release_manifest.candidate_v2_feed'
        $candidateV2Version = Get-RequiredQualificationString `
            -InputObject $candidateV2Feed -Name version `
            -Path 'release_manifest.candidate_v2_feed'
        $candidateV2AppcastHash = Get-RequiredQualificationHash `
            -InputObject $candidateV2Feed -Name appcast_sha256 `
            -Path 'release_manifest.candidate_v2_feed'
        $candidateV2SignatureHash = Get-RequiredQualificationHash `
            -InputObject $candidateV2Feed -Name appcast_signature_sha256 `
            -Path 'release_manifest.candidate_v2_feed'
        if ($candidateV2Version -cne $Version -or
            $candidateV2AppcastHash -cne $appcastRecord.Sha256 -or
            $candidateV2SignatureHash -cne $signatureRecord.Sha256) {
            throw ('release_manifest.candidate_v2_feed must bind the candidate version ' +
                   'and exact v2 appcast file hashes.')
        }

        $frozenLegacyFeed = Get-RequiredQualificationProperty `
            -InputObject $Manifest -Name frozen_legacy_feed -Path 'release_manifest'
        Assert-QualificationJsonObject `
            -Value $frozenLegacyFeed -Path 'release_manifest.frozen_legacy_feed'
        $frozenLegacyVersion = Get-RequiredQualificationString `
            -InputObject $frozenLegacyFeed -Name version `
            -Path 'release_manifest.frozen_legacy_feed'
        $frozenLegacyBaselineTag = Get-RequiredQualificationString `
            -InputObject $frozenLegacyFeed -Name baseline_tag `
            -Path 'release_manifest.frozen_legacy_feed'
        $frozenLegacyBaselineUrl = Get-RequiredQualificationString `
            -InputObject $frozenLegacyFeed -Name baseline_release_url `
            -Path 'release_manifest.frozen_legacy_feed'
        $frozenLegacySourceTag = Get-RequiredQualificationString `
            -InputObject $frozenLegacyFeed -Name source_tag `
            -Path 'release_manifest.frozen_legacy_feed'
        $frozenLegacySourceUrl = Get-RequiredQualificationString `
            -InputObject $frozenLegacyFeed -Name source_release_url `
            -Path 'release_manifest.frozen_legacy_feed'
        $frozenLegacyAppcastHash = Get-RequiredQualificationHash `
            -InputObject $frozenLegacyFeed -Name appcast_sha256 `
            -Path 'release_manifest.frozen_legacy_feed'
        $frozenLegacySignatureHash = Get-RequiredQualificationHash `
            -InputObject $frozenLegacyFeed -Name appcast_signature_sha256 `
            -Path 'release_manifest.frozen_legacy_feed'
        $expectedPreviousTag = "v$PreviousVersion"
        $expectedPreviousReleaseUrl = (
            "https://github.com/theatrus/resticpal/releases/tag/$expectedPreviousTag")
        if ($frozenLegacyVersion -cne '1.0.7' -or
            $frozenLegacyBaselineTag -cne 'v1.0.7' -or
            $frozenLegacyBaselineUrl -cne
                'https://github.com/theatrus/resticpal/releases/tag/v1.0.7' -or
            $frozenLegacySourceTag -cne $expectedPreviousTag -or
            $frozenLegacySourceUrl -cne $expectedPreviousReleaseUrl -or
            $frozenLegacyAppcastHash -cne $legacyAppcastRecord.Sha256 -or
            $frozenLegacySignatureHash -cne $legacySignatureRecord.Sha256) {
            throw ('release_manifest.frozen_legacy_feed must bind exact v1.0.7 ' +
                   'bytes fetched from the official previous release.')
        }
        $frozenLegacyBinding = [pscustomobject]@{
            SourceTag = $frozenLegacySourceTag
            Appcast = $legacyAppcastRecord
            Signature = $legacySignatureRecord
        }
    }

    $updatePackageValue = Get-RequiredQualificationProperty `
        -InputObject $Manifest -Name update_package -Path 'release_manifest'
    $updatePackage = Get-QualificationPackageRecord `
        -Value $updatePackageValue -Path 'release_manifest.update_package'
    $expectedEnclosureUrl = (
        "https://updates.resticpal.com/releases/$Tag/$($msiRecord.Name)")
    Assert-QualificationExactString `
        -Actual $updatePackage.Version -Expected $Version `
        -Path 'release_manifest.update_package.version'
    Assert-QualificationExactString `
        -Actual $updatePackage.Url -Expected $expectedEnclosureUrl `
        -Path 'release_manifest.update_package.url'
    if ($updatePackage.Length -ne $msiRecord.Length) {
        throw 'release_manifest.update_package.length must match the prepared MSI length.'
    }
    Assert-QualificationEd25519Signature `
        -Value $updatePackage.Signature `
        -Path 'release_manifest.update_package.signature'

    $automaticQualification = Get-RequiredQualificationProperty `
        -InputObject $Manifest -Name automatic_qualification -Path 'release_manifest'
    Assert-QualificationJsonObject `
        -Value $automaticQualification -Path 'release_manifest.automatic_qualification'
    $qualificationStrategy = Get-RequiredQualificationString `
        -InputObject $automaticQualification -Name strategy `
        -Path 'release_manifest.automatic_qualification'
    $expectedStrategy = if ($isBridgeTransition) {
        'published-service-ipc-bridge-with-candidate-tray-probe'
    } else {
        'published-client-tray'
    }
    Assert-QualificationExactString `
        -Actual $qualificationStrategy -Expected $expectedStrategy `
        -Path 'release_manifest.automatic_qualification.strategy'
    $manifestProbe = Get-RequiredQualificationProperty `
        -InputObject $automaticQualification -Name probe `
        -Path 'release_manifest.automatic_qualification'
    $qualificationFiles = Get-RequiredQualificationProperty `
        -InputObject $Manifest -Name qualification_files -Path 'release_manifest'
    $probeManifest = $null
    if ($isBridgeTransition) {
        Assert-QualificationJsonObject `
            -Value $qualificationFiles -Path 'release_manifest.qualification_files'
        Assert-QualificationJsonObject `
            -Value $manifestProbe -Path 'release_manifest.automatic_qualification.probe'
        $probeAppcastRecord = Get-QualificationManifestFileRecord `
            -Files $qualificationFiles -Name probe_appcast_v2
        $probeSignatureRecord = Get-QualificationManifestFileRecord `
            -Files $qualificationFiles -Name probe_appcast_v2_signature
        $probePayloadRecord = Get-QualificationManifestFileRecord `
            -Files $qualificationFiles -Name probe_payload
        Assert-QualificationExactString `
            -Actual $probeAppcastRecord.Name -Expected 'appcast-v2-probe.xml' `
            -Path 'release_manifest.qualification_files.probe_appcast_v2.name'
        Assert-QualificationExactString `
            -Actual $probeSignatureRecord.Name -Expected 'appcast-v2-probe.xml.signature' `
            -Path 'release_manifest.qualification_files.probe_appcast_v2_signature.name'
        $probeVersion = Get-RequiredQualificationString `
            -InputObject $manifestProbe -Name version `
            -Path 'release_manifest.automatic_qualification.probe'
        Assert-QualificationExactString `
            -Actual $probeVersion -Expected '1.0.8' `
            -Path 'release_manifest.automatic_qualification.probe.version'
        $probeAppcastHash = Get-RequiredQualificationHash `
            -InputObject $manifestProbe -Name appcast_sha256 `
            -Path 'release_manifest.automatic_qualification.probe'
        $probeSignatureHash = Get-RequiredQualificationHash `
            -InputObject $manifestProbe -Name appcast_signature_sha256 `
            -Path 'release_manifest.automatic_qualification.probe'
        $probePayloadName = Get-RequiredQualificationString `
            -InputObject $manifestProbe -Name payload_name `
            -Path 'release_manifest.automatic_qualification.probe'
        $probePayloadUrl = Get-RequiredQualificationString `
            -InputObject $manifestProbe -Name payload_url `
            -Path 'release_manifest.automatic_qualification.probe'
        $probePayloadLength = Get-RequiredQualificationUInt64 `
            -InputObject $manifestProbe -Name payload_length `
            -Path 'release_manifest.automatic_qualification.probe'
        $probePayloadHash = Get-RequiredQualificationHash `
            -InputObject $manifestProbe -Name payload_sha256 `
            -Path 'release_manifest.automatic_qualification.probe'
        $probeExpectedSignature = Get-RequiredQualificationString `
            -InputObject $manifestProbe -Name expected_signature `
            -Path 'release_manifest.automatic_qualification.probe'
        $zeroSignature = [Convert]::ToBase64String([byte[]]::new(64))
        if ($probeAppcastHash -cne $probeAppcastRecord.Sha256 -or
            $probeSignatureHash -cne $probeSignatureRecord.Sha256 -or
            $probePayloadName -cne $probePayloadRecord.Name -or
            $probePayloadName -cne 'resticpal-1.0.8-x64.msi' -or
            $probePayloadLength -ne $probePayloadRecord.Length -or
            $probePayloadHash -cne $probePayloadRecord.Sha256 -or
            $probePayloadUrl -cne
                'https://updates.resticpal.com/releases/v1.0.8/resticpal-1.0.8-x64.msi' -or
            $probeExpectedSignature -cne $zeroSignature) {
            throw 'release_manifest candidate-tray probe metadata is not the exact signed invalid-package probe.'
        }
        $probeManifest = [pscustomobject]@{
            Version = $probeVersion
            AppcastSha256 = $probeAppcastHash
            AppcastSignatureSha256 = $probeSignatureHash
            PayloadName = $probePayloadName
            PayloadUrl = $probePayloadUrl
            PayloadLength = $probePayloadLength
            PayloadSha256 = $probePayloadHash
            ExpectedSignature = $probeExpectedSignature
        }
    } elseif ($null -ne $manifestProbe -or $null -ne $qualificationFiles) {
        throw 'Candidate-tray probe metadata is allowed only for the v1.0.6 to v1.0.7 bridge.'
    }

    $schema = Get-RequiredQualificationUInt64 `
        -InputObject $evidence -Name schema -Path 'evidence'
    $expectedEvidenceSchema = if (
        $ExpectedInstallationMode -ceq 'automatic' -and $isBridgeTransition) {
        [uint64]2
    } else {
        [uint64]1
    }
    if ($schema -ne $expectedEvidenceSchema) {
        throw "evidence.schema must be the integer $expectedEvidenceSchema."
    }
    $expectedQualification = if (
        $ExpectedInstallationMode -ceq 'automatic' -and $isBridgeTransition) {
        'previous-published-service-automatic-update-bridge'
    } elseif ($ExpectedInstallationMode -ceq 'automatic') {
        'previous-published-client-automatic-update'
    } else {
        'previous-published-client-prompted-update'
    }
    $qualification = Get-RequiredQualificationString `
        -InputObject $evidence -Name qualification -Path 'evidence'
    $installationMode = Get-RequiredQualificationString `
        -InputObject $evidence -Name installation_mode -Path 'evidence'
    $status = Get-RequiredQualificationString `
        -InputObject $evidence -Name status -Path 'evidence'
    $exitCode = Get-RequiredQualificationUInt64 `
        -InputObject $evidence -Name exit_code -Path 'evidence'
    $reportedError = Get-RequiredQualificationProperty `
        -InputObject $evidence -Name error -Path 'evidence'
    Assert-QualificationExactString `
        -Actual $qualification -Expected $expectedQualification `
        -Path 'evidence.qualification'
    Assert-QualificationExactString `
        -Actual $installationMode -Expected $ExpectedInstallationMode `
        -Path 'evidence.installation_mode'
    Assert-QualificationExactString `
        -Actual $status -Expected 'passed' -Path 'evidence.status'
    if ($exitCode -ne 0) {
        throw 'evidence.exit_code must be the integer 0.'
    }
    if ($null -ne $reportedError) {
        throw 'evidence.error must be null for a passing qualification.'
    }

    $candidateVersion = Get-RequiredQualificationString `
        -InputObject $evidence -Name candidate_version -Path 'evidence'
    Assert-QualificationExactString `
        -Actual $candidateVersion -Expected $Version `
        -Path 'evidence.candidate_version'
    $enclosureUrl = Get-RequiredQualificationString `
        -InputObject $evidence -Name enclosure_url -Path 'evidence'
    Assert-QualificationExactString `
        -Actual $enclosureUrl -Expected $expectedEnclosureUrl `
        -Path 'evidence.enclosure_url'
    $appcastHash = Get-RequiredQualificationHash `
        -InputObject $evidence -Name appcast_sha256 -Path 'evidence'
    $signatureHash = Get-RequiredQualificationHash `
        -InputObject $evidence -Name appcast_signature_sha256 -Path 'evidence'
    Assert-QualificationExactString `
        -Actual $appcastHash -Expected $appcastRecord.Sha256 `
        -Path 'evidence.appcast_sha256'
    Assert-QualificationExactString `
        -Actual $signatureHash -Expected $signatureRecord.Sha256 `
        -Path 'evidence.appcast_signature_sha256'

    $staged = Get-RequiredQualificationProperty `
        -InputObject $evidence -Name staged_update -Path 'evidence'
    Assert-QualificationJsonObject -Value $staged -Path 'evidence.staged_update'
    $stagedPath = Get-RequiredQualificationString `
        -InputObject $staged -Name path -Path 'evidence.staged_update'
    $stagedExtension = Get-RequiredQualificationString `
        -InputObject $staged -Name extension -Path 'evidence.staged_update'
    $stagedFileName = Get-RequiredQualificationString `
        -InputObject $staged -Name file_name -Path 'evidence.staged_update'
    if ([IO.Path]::GetExtension($stagedPath) -ine '.msi' -or
        $stagedExtension -ine '.msi' -or
        $stagedFileName -cne [IO.Path]::GetFileName($stagedPath)) {
        throw 'evidence.staged_update must record the actual staged .msi path and file name.'
    }
    $stagedLength = Get-RequiredQualificationUInt64 `
        -InputObject $staged -Name length -Path 'evidence.staged_update'
    $stagedHash = Get-RequiredQualificationHash `
        -InputObject $staged -Name sha256 -Path 'evidence.staged_update'
    if ($stagedLength -ne $msiRecord.Length -or $stagedHash -cne $msiRecord.Sha256) {
        throw 'evidence.staged_update bytes do not match the prepared signed MSI.'
    }
    $sameLengthFiles = Get-RequiredQualificationUInt64 `
        -InputObject $staged -Name same_length_files_examined `
        -Path 'evidence.staged_update'
    $hashMatches = Get-RequiredQualificationUInt64 `
        -InputObject $staged -Name hash_matches -Path 'evidence.staged_update'
    $expectedPathMatch = Get-RequiredQualificationBoolean `
        -InputObject $staged -Name expected_path_match `
        -Path 'evidence.staged_update'
    if ($sameLengthFiles -lt 1 -or $hashMatches -ne 1 -or -not $expectedPathMatch) {
        throw 'evidence.staged_update did not record one exact expected-path hash match.'
    }

    $publishedVersion = Get-RequiredQualificationString `
        -InputObject $evidence -Name published_version -Path 'evidence'
    Assert-QualificationExactString `
        -Actual $publishedVersion -Expected $PreviousVersion `
        -Path 'evidence.published_version'
    $publishedEvidence = Get-RequiredQualificationProperty `
        -InputObject $evidence -Name published_release -Path 'evidence'
    Assert-QualificationJsonObject `
        -Value $publishedEvidence -Path 'evidence.published_release'
    $publishedTag = Get-RequiredQualificationString `
        -InputObject $publishedEvidence -Name tag -Path 'evidence.published_release'
    $publishedAssetName = Get-RequiredQualificationString `
        -InputObject $publishedEvidence -Name asset_name `
        -Path 'evidence.published_release'
    $publishedAssetLength = Get-RequiredQualificationUInt64 `
        -InputObject $publishedEvidence -Name asset_length `
        -Path 'evidence.published_release'
    if ($publishedAssetLength -eq 0) {
        throw 'evidence.published_release.asset_length must be greater than zero.'
    }
    $publishedAssetHash = Get-RequiredQualificationHash `
        -InputObject $publishedEvidence -Name asset_sha256 `
        -Path 'evidence.published_release'
    $publishedAssetUrl = Get-RequiredQualificationString `
        -InputObject $publishedEvidence -Name asset_url `
        -Path 'evidence.published_release'
    $expectedPublishedTag = "v$PreviousVersion"
    $expectedPublishedAssetName = "resticpal-$PreviousVersion-x64.msi"
    $expectedPublishedAssetUrl = (
        "https://github.com/theatrus/resticpal/releases/download/" +
        "$expectedPublishedTag/$expectedPublishedAssetName")
    Assert-QualificationExactString `
        -Actual $publishedTag -Expected $expectedPublishedTag `
        -Path 'evidence.published_release.tag'
    Assert-QualificationExactString `
        -Actual $publishedAssetName -Expected $expectedPublishedAssetName `
        -Path 'evidence.published_release.asset_name'
    Assert-QualificationExactString `
        -Actual $publishedAssetUrl -Expected $expectedPublishedAssetUrl `
        -Path 'evidence.published_release.asset_url'

    Assert-QualificationJsonObject -Value $PublishedRelease -Path 'published_release_api'
    $apiTag = Get-RequiredQualificationString `
        -InputObject $PublishedRelease -Name tagName -Path 'published_release_api'
    $apiDraft = Get-RequiredQualificationBoolean `
        -InputObject $PublishedRelease -Name isDraft -Path 'published_release_api'
    $apiPrerelease = Get-RequiredQualificationBoolean `
        -InputObject $PublishedRelease -Name isPrerelease `
        -Path 'published_release_api'
    if ($apiTag -cne $expectedPublishedTag -or $apiDraft -or $apiPrerelease) {
        throw 'published_release_api is not the immediately previous stable release.'
    }
    $apiAssets = @(Get-RequiredQualificationArray `
        -InputObject $PublishedRelease -Name assets -Path 'published_release_api'
    )
    $matchingApiAssets = @($apiAssets | Where-Object {
        if ($null -eq $_ -or
            ($_ -isnot [pscustomobject] -and
             $_ -isnot [Collections.IDictionary])) {
            return $false
        }
        try {
            (Get-RequiredQualificationString `
                -InputObject $_ -Name name -Path 'published_release_api.assets[]') -ceq
                $expectedPublishedAssetName
        } catch {
            $false
        }
    })
    if ($matchingApiAssets.Count -ne 1) {
        throw 'published_release_api must contain exactly one previous-client MSI asset.'
    }
    $apiAsset = $matchingApiAssets[0]
    $apiAssetSize = Get-RequiredQualificationUInt64 `
        -InputObject $apiAsset -Name size -Path 'published_release_api.assets[]'
    $apiAssetDigest = Get-RequiredQualificationString `
        -InputObject $apiAsset -Name digest -Path 'published_release_api.assets[]'
    $apiAssetUrl = Get-RequiredQualificationString `
        -InputObject $apiAsset -Name url -Path 'published_release_api.assets[]'
    if ($apiAssetSize -ne $publishedAssetLength -or
        $apiAssetDigest -cne "sha256:$publishedAssetHash" -or
        $apiAssetUrl -cne $expectedPublishedAssetUrl) {
        throw 'evidence.published_release does not match the official GitHub release asset.'
    }
    if (-not $isBridgeTransition) {
        foreach ($legacySource in @(
            [pscustomobject]@{
                Name = 'appcast.xml'
                Record = $frozenLegacyBinding.Appcast
            },
            [pscustomobject]@{
                Name = 'appcast.xml.signature'
                Record = $frozenLegacyBinding.Signature
            }
        )) {
            $matchingLegacyAssets = @($apiAssets | Where-Object {
                if ($null -eq $_ -or
                    ($_ -isnot [pscustomobject] -and
                     $_ -isnot [Collections.IDictionary])) {
                    return $false
                }
                try {
                    (Get-RequiredQualificationString `
                        -InputObject $_ -Name name `
                        -Path 'published_release_api.assets[]') -ceq
                        $legacySource.Name
                } catch {
                    $false
                }
            })
            if ($matchingLegacyAssets.Count -ne 1) {
                throw ("published_release_api must contain exactly one frozen " +
                       "$($legacySource.Name) asset.")
            }
            $legacyApiAsset = $matchingLegacyAssets[0]
            $legacyApiSize = Get-RequiredQualificationUInt64 `
                -InputObject $legacyApiAsset -Name size `
                -Path 'published_release_api.assets[]'
            $legacyApiDigest = Get-RequiredQualificationString `
                -InputObject $legacyApiAsset -Name digest `
                -Path 'published_release_api.assets[]'
            $legacyApiUrl = Get-RequiredQualificationString `
                -InputObject $legacyApiAsset -Name url `
                -Path 'published_release_api.assets[]'
            $expectedLegacyApiUrl = (
                "https://github.com/theatrus/resticpal/releases/download/" +
                "$expectedPublishedTag/$($legacySource.Name)")
            if ($legacyApiSize -ne $legacySource.Record.Length -or
                $legacyApiDigest -cne "sha256:$($legacySource.Record.Sha256)" -or
                $legacyApiUrl -cne $expectedLegacyApiUrl) {
                throw ("release_manifest frozen $($legacySource.Name) does not " +
                       'match the official previous GitHub release asset.')
            }
        }
    }

    $verification = Get-RequiredQualificationProperty `
        -InputObject $evidence -Name verification -Path 'evidence'
    Assert-QualificationJsonObject -Value $verification -Path 'evidence.verification'
    $candidateHash = Get-RequiredQualificationHash `
        -InputObject $verification -Name candidate_sha256 `
        -Path 'evidence.verification'
    $installedVersion = Get-RequiredQualificationString `
        -InputObject $verification -Name installed_version `
        -Path 'evidence.verification'
    $installedUiVersion = Get-RequiredQualificationString `
        -InputObject $verification -Name installed_ui_file_version `
        -Path 'evidence.verification'
    $installedServiceVersion = Get-RequiredQualificationString `
        -InputObject $verification -Name installed_service_file_version `
        -Path 'evidence.verification'
    $installedTrayVersion = Get-RequiredQualificationString `
        -InputObject $verification -Name installed_tray_file_version `
        -Path 'evidence.verification'
    Assert-QualificationExactString `
        -Actual $candidateHash -Expected $msiRecord.Sha256 `
        -Path 'evidence.verification.candidate_sha256'
    Assert-QualificationExactString `
        -Actual $installedVersion -Expected $Version `
        -Path 'evidence.verification.installed_version'
    $expectedFileVersion = "$Version.0"
    foreach ($installedFile in @(
        [pscustomobject]@{ Path = 'installed_ui_file_version'; Value = $installedUiVersion },
        [pscustomobject]@{ Path = 'installed_service_file_version'; Value = $installedServiceVersion },
        [pscustomobject]@{ Path = 'installed_tray_file_version'; Value = $installedTrayVersion }
    )) {
        Assert-QualificationExactString `
            -Actual $installedFile.Value -Expected $expectedFileVersion `
            -Path "evidence.verification.$($installedFile.Path)"
    }
    $publishedInstalledHash = Get-RequiredQualificationHash `
        -InputObject $verification -Name published_sha256 `
        -Path 'evidence.verification'
    Assert-QualificationExactString `
        -Actual $publishedInstalledHash -Expected $publishedAssetHash `
        -Path 'evidence.verification.published_sha256'

    $baselineServiceProcessId = Get-RequiredQualificationUInt64 `
        -InputObject $verification -Name baseline_service_process_id `
        -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
    $upgradedServiceProcessId = Get-RequiredQualificationUInt64 `
        -InputObject $verification -Name upgraded_service_process_id `
        -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
    if ($baselineServiceProcessId -eq 0 -or $upgradedServiceProcessId -eq 0 -or
        $baselineServiceProcessId -eq $upgradedServiceProcessId) {
        throw 'evidence.verification does not prove that the service restarted.'
    }
    $upgradedServiceProtocolVersion = $null
    if ([Version]$Version -ge [Version]'1.0.9') {
        $upgradedServiceProtocolVersion = Get-RequiredQualificationUInt64 `
            -InputObject $verification -Name upgraded_service_protocol_version `
            -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
        if ($upgradedServiceProtocolVersion -ne 5) {
            throw ("evidence.verification.upgraded_service_protocol_version must be " +
                   "5 for resticpal $Version.")
        }
    }
    $publishedTrayProcessId = Get-RequiredQualificationUInt64 `
        -InputObject $verification -Name published_tray_process_id `
        -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
    $upgradedTrayProcessId = Get-RequiredQualificationUInt64 `
        -InputObject $verification -Name upgraded_tray_process_id `
        -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
    $publishedTrayExited = Get-RequiredQualificationBoolean `
        -InputObject $verification -Name published_tray_exited `
        -Path 'evidence.verification'
    $trayProcessCount = Get-RequiredQualificationUInt64 `
        -InputObject $verification -Name tray_process_count `
        -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
    if ($publishedTrayProcessId -eq 0 -or $upgradedTrayProcessId -eq 0 -or
        $publishedTrayProcessId -eq $upgradedTrayProcessId -or
        -not $publishedTrayExited -or $trayProcessCount -ne 1) {
        throw 'evidence.verification does not prove one cleanly restarted tray process.'
    }
    $serviceIdentity = Get-RequiredQualificationString `
        -InputObject $verification -Name service_identity `
        -Path 'evidence.verification'
    $serviceState = Get-RequiredQualificationString `
        -InputObject $verification -Name service_state `
        -Path 'evidence.verification'
    $publishedUiExited = Get-RequiredQualificationBoolean `
        -InputObject $verification -Name published_ui_exited `
        -Path 'evidence.verification'
    if ($serviceIdentity -cne 'LocalSystem' -or
        $serviceState -cne 'Running' -or
        -not $publishedUiExited) {
        throw 'evidence.verification does not prove a running LocalSystem service and exited previous UI.'
    }

    $candidateInstallerProcessId = Get-RequiredQualificationUInt64 `
        -InputObject $verification -Name candidate_installer_process_id `
        -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
    $candidateInstallerSessionId = Get-RequiredQualificationUInt64 `
        -InputObject $verification -Name candidate_installer_session_id `
        -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
    $candidateInstallerSilent = Get-RequiredQualificationBoolean `
        -InputObject $verification -Name candidate_installer_silent `
        -Path 'evidence.verification'
    $candidateInstallerCommandLine = Get-RequiredQualificationString `
        -InputObject $verification -Name candidate_installer_command_line `
        -Path 'evidence.verification'
    if ($candidateInstallerProcessId -eq 0) {
        throw 'evidence.verification.candidate_installer_process_id must be greater than zero.'
    }
    $quotedStagedPathPattern = (
        '(?i)(?:^|\s)"?' + [Regex]::Escape($stagedPath) + '"?(?:\s|$)')

    $downloadConfirmationActions = Get-RequiredQualificationUInt64 `
        -InputObject $verification -Name download_confirmation_actions `
        -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
    $installConfirmationActions = Get-RequiredQualificationUInt64 `
        -InputObject $verification -Name install_confirmation_actions `
        -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
    $modeVerification = [ordered]@{}
    if ($ExpectedInstallationMode -ceq 'automatic') {
        $expectedStagedPath = "C:\ProgramData\ResticPal\Updates\$($msiRecord.Name)"
        if (-not [string]::Equals(
                $stagedPath,
                $expectedStagedPath,
                [StringComparison]::OrdinalIgnoreCase)) {
            throw 'Automatic qualification did not use the exact ProgramData MSI path.'
        }
        $candidateInstallerParentProcessId = Get-RequiredQualificationUInt64 `
            -InputObject $verification -Name candidate_installer_parent_process_id `
            -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
        $candidateInstallerOwner = Get-RequiredQualificationString `
            -InputObject $verification -Name candidate_installer_owner `
            -Path 'evidence.verification'
        if ($candidateInstallerParentProcessId -ne $baselineServiceProcessId -or
            $candidateInstallerSessionId -ne 0 -or
            $candidateInstallerOwner -cne 'NT AUTHORITY\SYSTEM' -or
            -not $candidateInstallerSilent -or
            $candidateInstallerCommandLine -cnotmatch $quotedStagedPathPattern -or
            $candidateInstallerCommandLine -cnotmatch '(?i)(?:^|\s)/qn(?:\s|$)' -or
            $candidateInstallerCommandLine -cnotmatch '(?i)(?:^|\s)/norestart(?:\s|$)') {
            throw ('Automatic qualification did not prove a service-child LocalSystem ' +
                   'session-0 /qn /norestart MSI launch.')
        }
        $automaticInstallEnabled = Get-RequiredQualificationBoolean `
            -InputObject $verification -Name automatic_install_enabled `
            -Path 'evidence.verification'
        $automaticInstallVia = Get-RequiredQualificationString `
            -InputObject $verification -Name automatic_install_enabled_via `
            -Path 'evidence.verification'
        $automaticInstallPersisted = Get-RequiredQualificationBoolean `
            -InputObject $verification -Name automatic_install_persisted_after_upgrade `
            -Path 'evidence.verification'
        $updateDispatcher = Get-RequiredQualificationString `
            -InputObject $verification -Name update_dispatcher `
            -Path 'evidence.verification'
        $automaticSettingActions = Get-RequiredQualificationUInt64 `
            -InputObject $verification -Name automatic_setting_ui_actions `
            -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
        $expectedDispatcher = if ($isBridgeTransition) {
            'qualification-harness-via-published-service-ipc'
        } else {
            'published-client-tray'
        }
        if (-not $automaticInstallEnabled -or
            $automaticInstallVia -cne 'published-client-ui-and-service-protocol' -or
            -not $automaticInstallPersisted -or
            $updateDispatcher -cne $expectedDispatcher -or
            $automaticSettingActions -ne 1) {
            throw 'Automatic qualification did not prove enablement, tray dispatch, and persistence.'
        }
        $installerDialogInterventions = Get-RequiredQualificationUInt64 `
            -InputObject $verification -Name installer_dialog_interventions `
            -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
        $automaticDialogObserved = Get-RequiredQualificationBoolean `
            -InputObject $verification -Name automatic_installer_dialog_observed `
            -Path 'evidence.verification'
        $noConfirmation = Get-RequiredQualificationBoolean `
            -InputObject $verification -Name no_user_confirmation_or_dialog_intervention `
            -Path 'evidence.verification'
        $interactiveUiProcessCount = Get-RequiredQualificationUInt64 `
            -InputObject $verification -Name interactive_ui_process_count `
            -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
        $automaticUiProcessStarts = Get-RequiredQualificationUInt64 `
            -InputObject $verification -Name automatic_ui_process_starts `
            -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
        $noUacPrompt = Get-RequiredQualificationBoolean `
            -InputObject $verification -Name no_uac_prompt `
            -Path 'evidence.verification'
        $uacConsentEvents = Get-RequiredQualificationUInt64 `
            -InputObject $verification -Name uac_consent_events `
            -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
        $consentProcessStarts = Get-RequiredQualificationUInt64 `
            -InputObject $verification -Name consent_process_starts `
            -Path 'evidence.verification' -Maximum ([uint32]::MaxValue)
        if ($downloadConfirmationActions -ne 0 -or
            $installConfirmationActions -ne 0 -or
            $installerDialogInterventions -ne 0 -or
            $automaticDialogObserved -or -not $noConfirmation -or
            $interactiveUiProcessCount -ne 0 -or $automaticUiProcessStarts -ne 0 -or
            -not $noUacPrompt -or
            $uacConsentEvents -ne 0 -or $consentProcessStarts -ne 0) {
            throw 'Automatic qualification required confirmation, interactive UI, installer intervention, or UAC.'
        }
        $feedGated = Get-RequiredQualificationBoolean `
            -InputObject $verification -Name signed_feed_gated_during_setup `
            -Path 'evidence.verification'
        $feedFetchedByTray = Get-RequiredQualificationBoolean `
            -InputObject $verification -Name signed_appcast_fetched_by_published_tray `
            -Path 'evidence.verification'
        $metadataDispatchedByHarness = if ($isBridgeTransition) {
            Get-RequiredQualificationBoolean `
                -InputObject $verification `
                -Name prepared_signed_appcast_metadata_dispatched_by_qualification_harness `
                -Path 'evidence.verification'
        } else {
            $false
        }
        $silentInstallLog = Get-RequiredQualificationString `
            -InputObject $verification -Name silent_install_log `
            -Path 'evidence.verification'
        $feedFetcherInvalid = if ($isBridgeTransition) {
            $feedFetchedByTray -or -not $metadataDispatchedByHarness
        } else {
            -not $feedFetchedByTray
        }
        if (-not $feedGated -or $feedFetcherInvalid -or
            $silentInstallLog -cne 'C:\ProgramData\ResticPal\Updates\install.log') {
            throw 'Automatic qualification did not prove the gated feed and service-owned silent MSI transaction.'
        }
        $bridgeVerification = if ($isBridgeTransition) {
            Assert-BridgeCandidateTrayEvidence `
                -Verification $verification `
                -UpdatePackage $updatePackage `
                -ProbeManifest $probeManifest `
                -AppcastRecord $appcastRecord `
                -AppcastSignatureRecord $signatureRecord `
                -UpgradedServiceProcessId ([uint32]$upgradedServiceProcessId) `
                -UpgradedTrayProcessId ([uint32]$upgradedTrayProcessId) `
                -CandidateVersion $Version
        } else {
            $null
        }
        $modeVerification = [ordered]@{
            automatic_install_enabled = $automaticInstallEnabled
            automatic_install_enabled_via = $automaticInstallVia
            automatic_install_persisted_after_upgrade = $automaticInstallPersisted
            automatic_setting_ui_actions = [uint32]$automaticSettingActions
            update_dispatcher = $updateDispatcher
            candidate_installer_process_id = [uint32]$candidateInstallerProcessId
            candidate_installer_parent_process_id = [uint32]$candidateInstallerParentProcessId
            candidate_installer_session_id = [uint32]$candidateInstallerSessionId
            candidate_installer_owner = $candidateInstallerOwner
            candidate_installer_command_line = $candidateInstallerCommandLine
            candidate_installer_silent = $candidateInstallerSilent
            download_confirmation_actions = [uint32]$downloadConfirmationActions
            install_confirmation_actions = [uint32]$installConfirmationActions
            installer_dialog_interventions = [uint32]$installerDialogInterventions
            automatic_installer_dialog_observed = $automaticDialogObserved
            no_user_confirmation_or_dialog_intervention = $noConfirmation
            interactive_ui_process_count = [uint32]$interactiveUiProcessCount
            automatic_ui_process_starts = [uint32]$automaticUiProcessStarts
            no_uac_prompt = $noUacPrompt
            uac_consent_events = [uint32]$uacConsentEvents
            consent_process_starts = [uint32]$consentProcessStarts
            signed_feed_gated_during_setup = $feedGated
            signed_appcast_fetched_by_published_tray = $feedFetchedByTray
            prepared_signed_appcast_metadata_dispatched_by_qualification_harness = (
                $metadataDispatchedByHarness)
            silent_install_log = $silentInstallLog
            bridge = $bridgeVerification
        }
    } else {
        if ($candidateInstallerCommandLine -cnotmatch $quotedStagedPathPattern) {
            throw 'evidence.verification.candidate_installer_command_line must name the exact staged MSI path.'
        }
        if ($downloadConfirmationActions -ne 1 -or
            $installConfirmationActions -ne 1 -or
            $candidateInstallerSilent -or
            $candidateInstallerSessionId -eq 0) {
            throw 'Prompted qualification did not prove one explicit interactive download and install confirmation.'
        }
        $modeVerification = [ordered]@{
            download_confirmation_actions = [uint32]$downloadConfirmationActions
            install_confirmation_actions = [uint32]$installConfirmationActions
            candidate_installer_process_id = [uint32]$candidateInstallerProcessId
            candidate_installer_session_id = [uint32]$candidateInstallerSessionId
            candidate_installer_command_line = $candidateInstallerCommandLine
            candidate_installer_silent = $candidateInstallerSilent
        }
    }

    $binding = [ordered]@{
        schema = [uint32]$schema
        result_file = $evidenceFileName
        result_length = $evidenceLength
        result_sha256 = $evidenceHash
        qualification = $qualification
        installation_mode = $installationMode
        published_release = [ordered]@{
            tag = $publishedTag
            asset_name = $publishedAssetName
            asset_length = $publishedAssetLength
            asset_sha256 = $publishedAssetHash
            asset_url = $publishedAssetUrl
        }
        appcast_sha256 = $appcastHash
        appcast_signature_sha256 = $signatureHash
        staged_update = [ordered]@{
            path = $stagedPath
            extension = $stagedExtension
            file_name = $stagedFileName
            length = $stagedLength
            sha256 = $stagedHash
            same_length_files_examined = $sameLengthFiles
            hash_matches = $hashMatches
            expected_path_match = $expectedPathMatch
        }
        verification = [ordered]@{
            candidate_sha256 = $candidateHash
            baseline_service_process_id = [uint32]$baselineServiceProcessId
            upgraded_service_process_id = [uint32]$upgradedServiceProcessId
            published_tray_process_id = [uint32]$publishedTrayProcessId
            upgraded_tray_process_id = [uint32]$upgradedTrayProcessId
            published_tray_exited = $publishedTrayExited
            tray_process_count = [uint32]$trayProcessCount
            installed_version = $installedVersion
            installed_ui_file_version = $installedUiVersion
            installed_service_file_version = $installedServiceVersion
            installed_tray_file_version = $installedTrayVersion
            service_identity = $serviceIdentity
            service_state = $serviceState
            published_ui_exited = $publishedUiExited
            mode = $modeVerification
        }
    }
    if ($null -ne $upgradedServiceProtocolVersion) {
        $binding.verification.upgraded_service_protocol_version =
            [uint32]$upgradedServiceProtocolVersion
    }
    return $binding
}

function Assert-UpdateQualificationPair {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] $PromptedEvidence,
        [Parameter(Mandatory)] $AutomaticEvidence,
        [Parameter(Mandatory)] $Manifest,
        [Parameter(Mandatory)] [string] $Version,
        [Parameter(Mandatory)] [string] $Tag,
        [Parameter(Mandatory)] [string] $PreviousVersion,
        [Parameter(Mandatory)] $PublishedRelease
    )

    $promptedPath = Get-RequiredQualificationString `
        -InputObject $PromptedEvidence -Name FullName -Path 'prompted_evidence'
    $automaticPath = Get-RequiredQualificationString `
        -InputObject $AutomaticEvidence -Name FullName -Path 'automatic_evidence'
    if ([string]::Equals(
            $promptedPath,
            $automaticPath,
            [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Prompted and automatic qualifications must be separate result files.'
    }

    $promptedBinding = Assert-UpdateQualificationEvidence `
        -LoadedEvidence $PromptedEvidence `
        -Manifest $Manifest `
        -ExpectedInstallationMode prompted `
        -Version $Version `
        -Tag $Tag `
        -PreviousVersion $PreviousVersion `
        -PublishedRelease $PublishedRelease
    $automaticBinding = Assert-UpdateQualificationEvidence `
        -LoadedEvidence $AutomaticEvidence `
        -Manifest $Manifest `
        -ExpectedInstallationMode automatic `
        -Version $Version `
        -Tag $Tag `
        -PreviousVersion $PreviousVersion `
        -PublishedRelease $PublishedRelease
    return [ordered]@{
        prompted = $promptedBinding
        automatic = $automaticBinding
    }
}

function Test-UpdateQualificationBindingState {
    [CmdletBinding()]
    param(
        [Parameter(Mandatory)] $Manifest,
        [Parameter(Mandatory)] $RequestedBindings
    )

    $qualifications = Get-RequiredQualificationProperty `
        -InputObject $Manifest -Name qualifications -Path 'release_manifest'
    Assert-QualificationJsonObject `
        -Value $qualifications -Path 'release_manifest.qualifications'
    $prompted = Get-RequiredQualificationProperty `
        -InputObject $qualifications -Name prompted `
        -Path 'release_manifest.qualifications'
    $automatic = Get-RequiredQualificationProperty `
        -InputObject $qualifications -Name automatic `
        -Path 'release_manifest.qualifications'
    Assert-QualificationJsonObject `
        -Value $RequestedBindings -Path 'requested_bindings'
    $requestedPrompted = Get-RequiredQualificationProperty `
        -InputObject $RequestedBindings -Name prompted -Path 'requested_bindings'
    $requestedAutomatic = Get-RequiredQualificationProperty `
        -InputObject $RequestedBindings -Name automatic -Path 'requested_bindings'
    Assert-QualificationJsonObject `
        -Value $requestedPrompted -Path 'requested_bindings.prompted'
    Assert-QualificationJsonObject `
        -Value $requestedAutomatic -Path 'requested_bindings.automatic'
    $automaticQualification = Get-RequiredQualificationProperty `
        -InputObject $Manifest -Name automatic_qualification -Path 'release_manifest'
    Assert-QualificationJsonObject `
        -Value $automaticQualification -Path 'release_manifest.automatic_qualification'
    $automaticStrategy = Get-RequiredQualificationString `
        -InputObject $automaticQualification -Name strategy `
        -Path 'release_manifest.automatic_qualification'
    if ($automaticStrategy -cne 'published-client-tray' -and
        $automaticStrategy -cne
            'published-service-ipc-bridge-with-candidate-tray-probe') {
        throw 'release_manifest.automatic_qualification.strategy is not supported.'
    }
    $bridgeBinding = (
        $automaticStrategy -ceq
            'published-service-ipc-bridge-with-candidate-tray-probe')
    foreach ($requestedMode in @(
        [pscustomobject]@{
            Name = 'prompted'
            Binding = $requestedPrompted
            Qualification = 'previous-published-client-prompted-update'
            Schema = [uint64]1
        },
        [pscustomobject]@{
            Name = 'automatic'
            Binding = $requestedAutomatic
            Qualification = if ($bridgeBinding) {
                'previous-published-service-automatic-update-bridge'
            } else {
                'previous-published-client-automatic-update'
            }
            Schema = if ($bridgeBinding) { [uint64]2 } else { [uint64]1 }
        }
    )) {
        $bindingSchema = Get-RequiredQualificationUInt64 `
            -InputObject $requestedMode.Binding -Name schema `
            -Path "requested_bindings.$($requestedMode.Name)"
        if ($bindingSchema -ne $requestedMode.Schema) {
            throw ("requested_bindings.$($requestedMode.Name).schema must be the integer " +
                   "$($requestedMode.Schema).")
        }
        $bindingQualification = Get-RequiredQualificationString `
            -InputObject $requestedMode.Binding -Name qualification `
            -Path "requested_bindings.$($requestedMode.Name)"
        $bindingInstallationMode = Get-RequiredQualificationString `
            -InputObject $requestedMode.Binding -Name installation_mode `
            -Path "requested_bindings.$($requestedMode.Name)"
        $bindingResultHash = Get-RequiredQualificationHash `
            -InputObject $requestedMode.Binding -Name result_sha256 `
            -Path "requested_bindings.$($requestedMode.Name)"
        Assert-QualificationExactString `
            -Actual $bindingQualification `
            -Expected $requestedMode.Qualification `
            -Path "requested_bindings.$($requestedMode.Name).qualification"
        Assert-QualificationExactString `
            -Actual $bindingInstallationMode `
            -Expected $requestedMode.Name `
            -Path "requested_bindings.$($requestedMode.Name).installation_mode"
    }

    if (($null -eq $prompted) -xor ($null -eq $automatic)) {
        throw 'release_manifest.qualifications is partially bound; both modes must be null or present.'
    }
    if ($null -eq $prompted) {
        return $false
    }
    Assert-QualificationJsonObject `
        -Value $prompted -Path 'release_manifest.qualifications.prompted'
    Assert-QualificationJsonObject `
        -Value $automatic -Path 'release_manifest.qualifications.automatic'
    $existingJson = $qualifications | ConvertTo-Json -Depth 20 -Compress
    $requestedJson = $RequestedBindings | ConvertTo-Json -Depth 20 -Compress
    if ($existingJson -cne $requestedJson) {
        throw 'release_manifest.json is already bound to different qualification evidence.'
    }
    return $true
}
