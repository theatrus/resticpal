[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'ReleaseQualification.ps1')

$script:passed = 0
$script:failed = 0
$testPackageSignatureBytes = [byte[]]::new(64)
for ($signatureIndex = 0; $signatureIndex -lt 64; $signatureIndex++) {
    $testPackageSignatureBytes[$signatureIndex] = 1
}
$script:testPackageSignature = [Convert]::ToBase64String($testPackageSignatureBytes)
$script:testProbeSignature = [Convert]::ToBase64String([byte[]]::new(64))

function Assert-TestEqual {
    param(
        [AllowNull()] $Actual,
        [AllowNull()] $Expected,
        [Parameter(Mandatory)] [string] $Message
    )

    if ($Actual -cne $Expected) {
        throw "$Message Expected '$Expected', observed '$Actual'."
    }
}

function Invoke-PassingTest {
    param(
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [scriptblock] $Body
    )

    try {
        & $Body
        $script:passed++
        Write-Host "PASS: $Name"
    } catch {
        $script:failed++
        Write-Host "FAIL: $Name -- $($_.Exception.Message)" -ForegroundColor Red
    }
}

function Invoke-FailingTest {
    param(
        [Parameter(Mandatory)] [string] $Name,
        [Parameter(Mandatory)] [scriptblock] $Body,
        [Parameter(Mandatory)] [string] $MessagePattern
    )

    try {
        & $Body
        $script:failed++
        Write-Host "FAIL: $Name -- expected an exception" -ForegroundColor Red
    } catch {
        if ($_.Exception.Message -notlike $MessagePattern) {
            $script:failed++
            Write-Host (
                "FAIL: $Name -- expected '$MessagePattern', observed " +
                "'$($_.Exception.Message)'") -ForegroundColor Red
        } else {
            $script:passed++
            Write-Host "PASS: $Name"
        }
    }
}

function Copy-TestJsonObject {
    param([Parameter(Mandatory)] $Value)
    return ($Value | ConvertTo-Json -Depth 20 | ConvertFrom-Json)
}

function New-TestManifest {
    return [ordered]@{
        schema = 5
        version = '1.0.7'
        tag = 'v1.0.7'
        head_sha = ('e' * 40)
        run_id = 17
        package_host = 'UpdatesHost'
        files = [ordered]@{
            msi = [ordered]@{
                name = 'resticpal-1.0.7-x64.msi'
                length = 1000
                sha256 = ('a' * 64)
            }
            appcast_v2 = [ordered]@{
                name = 'appcast-v2.xml'
                length = 200
                sha256 = ('b' * 64)
            }
            appcast_v2_signature = [ordered]@{
                name = 'appcast-v2.xml.signature'
                length = 64
                sha256 = ('c' * 64)
            }
            legacy_appcast = [ordered]@{
                name = 'appcast.xml'
                length = 200
                sha256 = ('b' * 64)
            }
            legacy_appcast_signature = [ordered]@{
                name = 'appcast.xml.signature'
                length = 64
                sha256 = ('c' * 64)
            }
            checksums = [ordered]@{
                name = 'SHA256SUMS.txt'
                length = 300
                sha256 = ('f' * 64)
            }
        }
        update_package = [ordered]@{
            version = '1.0.7'
            url = 'https://updates.resticpal.com/releases/v1.0.7/resticpal-1.0.7-x64.msi'
            signature = $script:testPackageSignature
            length = 1000
        }
        dual_named_feed = [ordered]@{
            version = '1.0.7'
            appcast_sha256 = ('b' * 64)
            appcast_signature_sha256 = ('c' * 64)
        }
        qualification_files = [ordered]@{
            probe_appcast_v2 = [ordered]@{
                name = 'appcast-v2-probe.xml'
                length = 201
                sha256 = ('1' * 64)
            }
            probe_appcast_v2_signature = [ordered]@{
                name = 'appcast-v2-probe.xml.signature'
                length = 88
                sha256 = ('2' * 64)
            }
            probe_payload = [ordered]@{
                name = 'resticpal-1.0.8-x64.msi'
                length = 37
                sha256 = ('3' * 64)
            }
        }
        automatic_qualification = [ordered]@{
            strategy = 'published-service-ipc-bridge-with-candidate-tray-probe'
            probe = [ordered]@{
                version = '1.0.8'
                appcast_sha256 = ('1' * 64)
                appcast_signature_sha256 = ('2' * 64)
                payload_name = 'resticpal-1.0.8-x64.msi'
                payload_url = 'https://updates.resticpal.com/releases/v1.0.8/resticpal-1.0.8-x64.msi'
                payload_length = 37
                payload_sha256 = ('3' * 64)
                expected_signature = $script:testProbeSignature
            }
        }
        qualifications = [ordered]@{
            prompted = $null
            automatic = $null
        }
    }
}

function New-TestPublishedRelease {
    return [ordered]@{
        tagName = 'v1.0.6'
        isDraft = $false
        isPrerelease = $false
        assets = @(
            [ordered]@{
                name = 'resticpal-1.0.6-x64.msi'
                size = 900
                digest = ('sha256:' + ('d' * 64))
                url = 'https://github.com/theatrus/resticpal/releases/download/v1.0.6/resticpal-1.0.6-x64.msi'
            }
        )
        url = 'https://github.com/theatrus/resticpal/releases/tag/v1.0.6'
    }
}

function New-TestEvidence {
    param(
        [Parameter(Mandatory)]
        [ValidateSet('prompted', 'automatic')]
        [string] $Mode
    )

    $automatic = $Mode -ceq 'automatic'
    $stagedPath = if ($automatic) {
        'C:\ProgramData\ResticPal\Updates\resticpal-1.0.7-x64.msi'
    } else {
        'C:\Users\Test\AppData\Local\Temp\NetSparkle\resticpal-1.0.7-x64.msi'
    }
    $verification = [ordered]@{
        published_sha256 = ('d' * 64)
        candidate_sha256 = ('a' * 64)
        baseline_service_process_id = 100
        upgraded_service_process_id = 200
        published_tray_process_id = 300
        upgraded_tray_process_id = 400
        published_tray_exited = $true
        tray_process_count = 1
        installed_version = '1.0.7'
        installed_ui_file_version = '1.0.7.0'
        installed_service_file_version = '1.0.7.0'
        installed_tray_file_version = '1.0.7.0'
        service_identity = 'LocalSystem'
        service_state = 'Running'
        published_ui_exited = $true
        candidate_installer_process_id = 500
        candidate_installer_parent_process_id = if ($automatic) { 100 } else { 499 }
        candidate_installer_session_id = if ($automatic) { 0 } else { 1 }
        candidate_installer_owner = if ($automatic) {
            'NT AUTHORITY\SYSTEM'
        } else {
            'TEST\User'
        }
        candidate_installer_command_line = if ($automatic) {
            'msiexec.exe /i "C:\ProgramData\ResticPal\Updates\resticpal-1.0.7-x64.msi" /qn /norestart'
        } else {
            'msiexec.exe /i "C:\Users\Test\AppData\Local\Temp\NetSparkle\resticpal-1.0.7-x64.msi"'
        }
        candidate_installer_silent = $automatic
        download_confirmation_actions = if ($automatic) { 0 } else { 1 }
        install_confirmation_actions = if ($automatic) { 0 } else { 1 }
    }
    if ($automatic) {
        $verification.automatic_install_enabled = $true
        $verification.automatic_install_enabled_via =
            'published-client-ui-and-service-protocol'
        $verification.automatic_install_persisted_after_upgrade = $true
        $verification.update_dispatcher =
            'qualification-harness-via-published-service-ipc'
        $verification.automatic_setting_ui_actions = 1
        $verification.installer_dialog_interventions = 0
        $verification.automatic_installer_dialog_observed = $false
        $verification.no_user_confirmation_or_dialog_intervention = $true
        $verification.interactive_ui_process_count = 0
        $verification.automatic_ui_process_starts = 0
        $verification.no_uac_prompt = $true
        $verification.uac_consent_events = 0
        $verification.consent_process_starts = 0
        $verification.signed_feed_gated_during_setup = $true
        $verification.signed_appcast_fetched_by_published_tray = $false
        $verification.prepared_signed_appcast_metadata_dispatched_by_qualification_harness =
            $true
        $verification.silent_install_log =
            'C:\ProgramData\ResticPal\Updates\install.log'
        $verification.upgraded_service_protocol_version = 4
        $verification.upgraded_tray_protocol_version = 4
        $verification.dispatch_bridge = [ordered]@{
            reason = 'published-v1.0.6-tray-error-pipe-busy'
            protocol_version = 3
            request_type = 'install_update'
            response_type = 'accepted'
            appcast_sha256 = ('b' * 64)
            appcast_signature_sha256 = ('c' * 64)
            package = [ordered]@{
                version = '1.0.7'
                url = 'https://updates.resticpal.com/releases/v1.0.7/resticpal-1.0.7-x64.msi'
                signature = $script:testPackageSignature
                length = 1000
            }
        }
        $verification.candidate_tray_probe = [ordered]@{
            protocol_version = 4
            probe_version = '1.0.8'
            appcast_sha256 = ('1' * 64)
            appcast_signature_sha256 = ('2' * 64)
            payload = [ordered]@{
                name = 'resticpal-1.0.8-x64.msi'
                url = 'https://updates.resticpal.com/releases/v1.0.8/resticpal-1.0.8-x64.msi'
                length = 37
                sha256 = ('3' * 64)
                expected_signature = $script:testProbeSignature
            }
            requests = [ordered]@{
                appcast = [ordered]@{
                    url = 'https://updates.resticpal.com/appcast-v2.xml'
                    user_agent = 'resticpal/1.0.7'
                }
                appcast_signature = [ordered]@{
                    url = 'https://updates.resticpal.com/appcast-v2.xml.signature'
                    user_agent = 'resticpal/1.0.7'
                }
                payload = [ordered]@{
                    url = 'https://updates.resticpal.com/releases/v1.0.8/resticpal-1.0.8-x64.msi'
                    user_agent = 'resticpal/1.0.7'
                }
            }
            diagnostics = @(
                [ordered]@{
                    code = 'update.started'
                    observed_at = '2026-08-23T20:00:00.0000000Z'
                },
                [ordered]@{
                    code = 'update.failed'
                    failure_code = 'update_signature_invalid'
                    observed_at = '2026-08-23T20:00:01.0000000Z'
                })
            final_path = 'C:\ProgramData\ResticPal\Updates\resticpal-1.0.8-x64.msi'
            final_exists = $false
            partial_path = 'C:\ProgramData\ResticPal\Updates\resticpal-1.0.8-x64.msi.partial'
            partial_exists = $false
            staging_entries = @()
            msiexec_process_count = 0
            tray_process_id = 400
            service_process_id = 200
        }
    }

    return [ordered]@{
        schema = if ($automatic) { 2 } else { 1 }
        qualification = if ($automatic) {
            'previous-published-service-automatic-update-bridge'
        } else {
            'previous-published-client-prompted-update'
        }
        installation_mode = $Mode
        status = 'passed'
        exit_code = 0
        error = $null
        published_version = '1.0.6'
        published_release = [ordered]@{
            tag = 'v1.0.6'
            asset_name = 'resticpal-1.0.6-x64.msi'
            asset_length = 900
            asset_sha256 = ('d' * 64)
            asset_url = 'https://github.com/theatrus/resticpal/releases/download/v1.0.6/resticpal-1.0.6-x64.msi'
        }
        candidate_version = '1.0.7'
        appcast_sha256 = ('b' * 64)
        appcast_signature_sha256 = ('c' * 64)
        enclosure_url = 'https://updates.resticpal.com/releases/v1.0.7/resticpal-1.0.7-x64.msi'
        staged_update = [ordered]@{
            path = $stagedPath
            extension = '.msi'
            file_name = 'resticpal-1.0.7-x64.msi'
            length = 1000
            sha256 = ('a' * 64)
            same_length_files_examined = 1
            hash_matches = 1
            expected_path_match = $true
        }
        verification = $verification
    }
}

function New-TestOrdinaryScenario {
    $manifest = Copy-TestJsonObject (New-TestManifest)
    $manifest.schema = 6
    $manifest.version = '1.0.8'
    $manifest.tag = 'v1.0.8'
    $manifest.files.msi.name = 'resticpal-1.0.8-x64.msi'
    $manifest.files.legacy_appcast.length = $script:FrozenLegacyAppCastLength
    $manifest.files.legacy_appcast.sha256 = $script:FrozenLegacyAppCastSha256
    $manifest.files.legacy_appcast_signature.length =
        $script:FrozenLegacySignatureLength
    $manifest.files.legacy_appcast_signature.sha256 =
        $script:FrozenLegacySignatureSha256
    $manifest.PSObject.Properties.Remove('dual_named_feed')
    $manifest | Add-Member -NotePropertyName candidate_v2_feed -NotePropertyValue (
        [pscustomobject]@{
            version = '1.0.8'
            appcast_sha256 = ('b' * 64)
            appcast_signature_sha256 = ('c' * 64)
        })
    $manifest | Add-Member -NotePropertyName frozen_legacy_feed -NotePropertyValue (
        [pscustomobject]@{
            version = '1.0.7'
            baseline_tag = 'v1.0.7'
            baseline_release_url =
                'https://github.com/theatrus/resticpal/releases/tag/v1.0.7'
            source_tag = 'v1.0.7'
            source_release_url =
                'https://github.com/theatrus/resticpal/releases/tag/v1.0.7'
            appcast_sha256 = $script:FrozenLegacyAppCastSha256
            appcast_signature_sha256 = $script:FrozenLegacySignatureSha256
        })
    $manifest.update_package.version = '1.0.8'
    $manifest.update_package.url =
        'https://updates.resticpal.com/releases/v1.0.8/resticpal-1.0.8-x64.msi'
    $manifest.qualification_files = $null
    $manifest.automatic_qualification.strategy = 'published-client-tray'
    $manifest.automatic_qualification.probe = $null

    function New-OrdinaryEvidence([string] $Mode) {
        $automatic = $Mode -ceq 'automatic'
        $evidence = Copy-TestJsonObject (New-TestEvidence $Mode)
        $evidence.schema = 1
        if ($automatic) {
            $evidence.qualification = 'previous-published-client-automatic-update'
        }
        $evidence.published_version = '1.0.7'
        $evidence.published_release.tag = 'v1.0.7'
        $evidence.published_release.asset_name = 'resticpal-1.0.7-x64.msi'
        $evidence.published_release.asset_url =
            'https://github.com/theatrus/resticpal/releases/download/v1.0.7/resticpal-1.0.7-x64.msi'
        $evidence.candidate_version = '1.0.8'
        $evidence.enclosure_url =
            'https://updates.resticpal.com/releases/v1.0.8/resticpal-1.0.8-x64.msi'
        $evidence.staged_update.path = if ($automatic) {
            'C:\ProgramData\ResticPal\Updates\resticpal-1.0.8-x64.msi'
        } else {
            'C:\Users\Test\AppData\Local\Temp\NetSparkle\resticpal-1.0.8-x64.msi'
        }
        $evidence.staged_update.file_name = 'resticpal-1.0.8-x64.msi'
        $evidence.verification.installed_version = '1.0.8'
        $evidence.verification.installed_ui_file_version = '1.0.8.0'
        $evidence.verification.installed_service_file_version = '1.0.8.0'
        $evidence.verification.installed_tray_file_version = '1.0.8.0'
        $evidence.verification.candidate_installer_command_line = if ($automatic) {
            'msiexec.exe /i "C:\ProgramData\ResticPal\Updates\resticpal-1.0.8-x64.msi" /qn /norestart'
        } else {
            'msiexec.exe /i "C:\Users\Test\AppData\Local\Temp\NetSparkle\resticpal-1.0.8-x64.msi"'
        }
        if ($automatic) {
            $evidence.verification.update_dispatcher = 'published-client-tray'
            $evidence.verification.signed_appcast_fetched_by_published_tray = $true
        }
        foreach ($name in @(
                'prepared_signed_appcast_metadata_dispatched_by_qualification_harness',
                'upgraded_service_protocol_version',
                'upgraded_tray_protocol_version',
                'dispatch_bridge',
                'candidate_tray_probe')) {
            $evidence.verification.PSObject.Properties.Remove($name)
        }
        return $evidence
    }

    $publishedRelease = Copy-TestJsonObject (New-TestPublishedRelease)
    $publishedRelease.tagName = 'v1.0.7'
    $publishedRelease.assets[0].name = 'resticpal-1.0.7-x64.msi'
    $publishedRelease.assets[0].url =
        'https://github.com/theatrus/resticpal/releases/download/v1.0.7/resticpal-1.0.7-x64.msi'
    $publishedRelease.assets = @(
        $publishedRelease.assets[0],
        [pscustomobject]@{
            name = 'appcast.xml'
            size = $script:FrozenLegacyAppCastLength
            digest = ('sha256:' + $script:FrozenLegacyAppCastSha256)
            url = 'https://github.com/theatrus/resticpal/releases/download/v1.0.7/appcast.xml'
        },
        [pscustomobject]@{
            name = 'appcast.xml.signature'
            size = $script:FrozenLegacySignatureLength
            digest = ('sha256:' + $script:FrozenLegacySignatureSha256)
            url = 'https://github.com/theatrus/resticpal/releases/download/v1.0.7/appcast.xml.signature'
        })
    $publishedRelease.url = 'https://github.com/theatrus/resticpal/releases/tag/v1.0.7'
    return [pscustomobject]@{
        Manifest = $manifest
        PromptedEvidence = New-OrdinaryEvidence prompted
        AutomaticEvidence = New-OrdinaryEvidence automatic
        PublishedRelease = $publishedRelease
    }
}

function New-TestRestoreProtocolScenario {
    $scenario = New-TestOrdinaryScenario
    $scenario.Manifest.version = '1.0.9'
    $scenario.Manifest.tag = 'v1.0.9'
    $scenario.Manifest.files.msi.name = 'resticpal-1.0.9-x64.msi'
    $scenario.Manifest.candidate_v2_feed.version = '1.0.9'
    $scenario.Manifest.frozen_legacy_feed.source_tag = 'v1.0.8'
    $scenario.Manifest.frozen_legacy_feed.source_release_url =
        'https://github.com/theatrus/resticpal/releases/tag/v1.0.8'
    $scenario.Manifest.update_package.version = '1.0.9'
    $scenario.Manifest.update_package.url =
        'https://updates.resticpal.com/releases/v1.0.9/resticpal-1.0.9-x64.msi'

    foreach ($evidence in @($scenario.PromptedEvidence, $scenario.AutomaticEvidence)) {
        $automatic = $evidence.installation_mode -ceq 'automatic'
        $evidence.published_version = '1.0.8'
        $evidence.published_release.tag = 'v1.0.8'
        $evidence.published_release.asset_name = 'resticpal-1.0.8-x64.msi'
        $evidence.published_release.asset_url =
            'https://github.com/theatrus/resticpal/releases/download/v1.0.8/resticpal-1.0.8-x64.msi'
        $evidence.candidate_version = '1.0.9'
        $evidence.enclosure_url =
            'https://updates.resticpal.com/releases/v1.0.9/resticpal-1.0.9-x64.msi'
        $evidence.staged_update.path = if ($automatic) {
            'C:\ProgramData\ResticPal\Updates\resticpal-1.0.9-x64.msi'
        } else {
            'C:\Users\Test\AppData\Local\Temp\NetSparkle\resticpal-1.0.9-x64.msi'
        }
        $evidence.staged_update.file_name = 'resticpal-1.0.9-x64.msi'
        $evidence.verification.installed_version = '1.0.9'
        $evidence.verification.installed_ui_file_version = '1.0.9.0'
        $evidence.verification.installed_service_file_version = '1.0.9.0'
        $evidence.verification.installed_tray_file_version = '1.0.9.0'
        $evidence.verification.candidate_installer_command_line = if ($automatic) {
            'msiexec.exe /i "C:\ProgramData\ResticPal\Updates\resticpal-1.0.9-x64.msi" /qn /norestart'
        } else {
            'msiexec.exe /i "C:\Users\Test\AppData\Local\Temp\NetSparkle\resticpal-1.0.9-x64.msi"'
        }
        $evidence.verification | Add-Member `
            -NotePropertyName upgraded_service_protocol_version `
            -NotePropertyValue ([uint32]5)
    }

    $scenario.PublishedRelease.tagName = 'v1.0.8'
    $scenario.PublishedRelease.assets[0].name = 'resticpal-1.0.8-x64.msi'
    $scenario.PublishedRelease.assets[0].url =
        'https://github.com/theatrus/resticpal/releases/download/v1.0.8/resticpal-1.0.8-x64.msi'
    $scenario.PublishedRelease.assets[1].url =
        'https://github.com/theatrus/resticpal/releases/download/v1.0.8/appcast.xml'
    $scenario.PublishedRelease.assets[2].url =
        'https://github.com/theatrus/resticpal/releases/download/v1.0.8/appcast.xml.signature'
    $scenario.PublishedRelease.url =
        'https://github.com/theatrus/resticpal/releases/tag/v1.0.8'
    return $scenario
}

function Write-AndReadTestEvidence {
    param(
        [Parameter(Mandatory)] $Evidence,
        [Parameter(Mandatory)] [string] $Name
    )

    $path = Join-Path $script:testRoot $Name
    $json = $Evidence | ConvertTo-Json -Depth 20
    [IO.File]::WriteAllText($path, $json, [Text.UTF8Encoding]::new($false))
    return Read-UpdateQualificationEvidence -LiteralPath $path
}

function Invoke-TestPair {
    param(
        [Parameter(Mandatory)] $Prompted,
        [Parameter(Mandatory)] $Automatic,
        [string] $PromptedName = 'prompted.json',
        [string] $AutomaticName = 'automatic.json'
    )

    $promptedLoaded = Write-AndReadTestEvidence $Prompted $PromptedName
    $automaticLoaded = Write-AndReadTestEvidence $Automatic $AutomaticName
    return Assert-UpdateQualificationPair `
        -PromptedEvidence $promptedLoaded `
        -AutomaticEvidence $automaticLoaded `
        -Manifest (New-TestManifest) `
        -Version '1.0.7' `
        -Tag 'v1.0.7' `
        -PreviousVersion '1.0.6' `
        -PublishedRelease (New-TestPublishedRelease)
}

$script:testRoot = Join-Path ([IO.Path]::GetTempPath()) (
    'resticpal-release-qualification-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $script:testRoot | Out-Null
try {
    Invoke-PassingTest 'accepts and binds one exact prompted and automatic result' {
        $bindings = Invoke-TestPair `
            -Prompted (New-TestEvidence prompted) `
            -Automatic (New-TestEvidence automatic)
        Assert-TestEqual `
            $bindings.prompted.installation_mode 'prompted' `
            'The prompted binding mode changed.'
        Assert-TestEqual `
            $bindings.automatic.installation_mode 'automatic' `
            'The automatic binding mode changed.'
        Assert-TestEqual `
            $bindings.automatic.verification.mode.candidate_installer_owner `
            'NT AUTHORITY\SYSTEM' `
            'The automatic LocalSystem proof was not bound.'
        Assert-TestEqual `
            $bindings.prompted.verification.mode.download_confirmation_actions `
            ([uint32]1) `
            'The prompted confirmation proof was not bound.'
    }

    Invoke-PassingTest 'accepts ordinary v1.0.7 to v1.0.8 prompted and automatic schema-1 updates' {
        $scenario = New-TestOrdinaryScenario
        $promptedLoaded = Write-AndReadTestEvidence `
            $scenario.PromptedEvidence 'ordinary-prompted.json'
        $automaticLoaded = Write-AndReadTestEvidence `
            $scenario.AutomaticEvidence 'ordinary-automatic.json'
        $bindings = Assert-UpdateQualificationPair `
            -PromptedEvidence $promptedLoaded `
            -AutomaticEvidence $automaticLoaded `
            -Manifest $scenario.Manifest `
            -Version '1.0.8' `
            -Tag 'v1.0.8' `
            -PreviousVersion '1.0.7' `
            -PublishedRelease $scenario.PublishedRelease
        Assert-TestEqual `
            $bindings.prompted.schema ([uint32]1) `
            'Ordinary prompted qualification did not remain schema 1.'
        Assert-TestEqual `
            $bindings.automatic.schema ([uint32]1) `
            'Ordinary automatic qualification did not remain schema 1.'
        Assert-TestEqual `
            $bindings.automatic.verification.mode.update_dispatcher `
            'published-client-tray' `
            'Future automatic qualification did not bind the published tray.'
    }

    Invoke-PassingTest 'accepts and binds protocol v5 for v1.0.9 prompted and automatic updates' {
        $scenario = New-TestRestoreProtocolScenario
        $promptedLoaded = Write-AndReadTestEvidence `
            $scenario.PromptedEvidence 'restore-protocol-prompted.json'
        $automaticLoaded = Write-AndReadTestEvidence `
            $scenario.AutomaticEvidence 'restore-protocol-automatic.json'
        $bindings = Assert-UpdateQualificationPair `
            -PromptedEvidence $promptedLoaded `
            -AutomaticEvidence $automaticLoaded `
            -Manifest $scenario.Manifest `
            -Version '1.0.9' `
            -Tag 'v1.0.9' `
            -PreviousVersion '1.0.8' `
            -PublishedRelease $scenario.PublishedRelease
        foreach ($mode in @('prompted', 'automatic')) {
            Assert-TestEqual `
                $bindings[$mode].verification.upgraded_service_protocol_version `
                ([uint32]5) `
                "The $mode protocol-v5 service proof was not bound."
        }
    }

    foreach ($mode in @('prompted', 'automatic')) {
        Invoke-FailingTest "rejects v1.0.9 $mode qualification without service protocol proof" {
            $scenario = New-TestRestoreProtocolScenario
            $evidence = if ($mode -ceq 'automatic') {
                $scenario.AutomaticEvidence
            } else {
                $scenario.PromptedEvidence
            }
            $evidence.verification.PSObject.Properties.Remove(
                'upgraded_service_protocol_version')
            $loaded = Write-AndReadTestEvidence `
                $evidence "missing-service-protocol-$mode.json"
            Assert-UpdateQualificationEvidence `
                -LoadedEvidence $loaded `
                -Manifest $scenario.Manifest `
                -ExpectedInstallationMode $mode `
                -Version '1.0.9' `
                -Tag 'v1.0.9' `
                -PreviousVersion '1.0.8' `
                -PublishedRelease $scenario.PublishedRelease | Out-Null
        } '*upgraded_service_protocol_version is required*'

        Invoke-FailingTest "rejects v1.0.9 $mode qualification with legacy protocol v4" {
            $scenario = New-TestRestoreProtocolScenario
            $evidence = if ($mode -ceq 'automatic') {
                $scenario.AutomaticEvidence
            } else {
                $scenario.PromptedEvidence
            }
            $evidence.verification.upgraded_service_protocol_version = 4
            $loaded = Write-AndReadTestEvidence `
                $evidence "legacy-service-protocol-$mode.json"
            Assert-UpdateQualificationEvidence `
                -LoadedEvidence $loaded `
                -Manifest $scenario.Manifest `
                -ExpectedInstallationMode $mode `
                -Version '1.0.9' `
                -Tag 'v1.0.9' `
                -PreviousVersion '1.0.8' `
                -PublishedRelease $scenario.PublishedRelease | Out-Null
        } '*upgraded_service_protocol_version must be 5*'
    }

    Invoke-FailingTest 'rejects the one-time service bridge on a future transition' {
        $scenario = New-TestOrdinaryScenario
        $scenario.AutomaticEvidence.schema = 2
        $scenario.AutomaticEvidence.qualification =
            'previous-published-service-automatic-update-bridge'
        $scenario.AutomaticEvidence.verification.update_dispatcher =
            'qualification-harness-via-published-service-ipc'
        $loaded = Write-AndReadTestEvidence `
            $scenario.AutomaticEvidence 'future-bridge.json'
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $scenario.Manifest `
            -ExpectedInstallationMode automatic `
            -Version '1.0.8' `
            -Tag 'v1.0.8' `
            -PreviousVersion '1.0.7' `
            -PublishedRelease $scenario.PublishedRelease | Out-Null
    } '*evidence.schema must be the integer 1*'

    Invoke-FailingTest 'rejects bridge manifest schema on a steady-state release' {
        $scenario = New-TestOrdinaryScenario
        $scenario.Manifest.schema = 5
        $loaded = Write-AndReadTestEvidence `
            $scenario.PromptedEvidence 'steady-schema-five.json'
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $scenario.Manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.8' `
            -Tag 'v1.0.8' `
            -PreviousVersion '1.0.7' `
            -PublishedRelease $scenario.PublishedRelease | Out-Null
    } '*release_manifest.schema must be the integer 6*'

    Invoke-FailingTest 'rejects candidate v2 metadata bound to the frozen legacy hash' {
        $scenario = New-TestOrdinaryScenario
        $scenario.Manifest.candidate_v2_feed.appcast_sha256 =
            $script:FrozenLegacyAppCastSha256
        $loaded = Write-AndReadTestEvidence `
            $scenario.PromptedEvidence 'candidate-v2-hash-changed.json'
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $scenario.Manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.8' `
            -Tag 'v1.0.8' `
            -PreviousVersion '1.0.7' `
            -PublishedRelease $scenario.PublishedRelease | Out-Null
    } '*candidate_v2_feed must bind the candidate version*'

    Invoke-FailingTest 'rejects a self-consistent mutation of the frozen v1.0.7 bytes' {
        $scenario = New-TestOrdinaryScenario
        $mutatedHash = ('0' * 64)
        $scenario.Manifest.files.legacy_appcast.sha256 = $mutatedHash
        $scenario.Manifest.frozen_legacy_feed.appcast_sha256 = $mutatedHash
        $scenario.PublishedRelease.assets[1].digest = "sha256:$mutatedHash"
        $loaded = Write-AndReadTestEvidence `
            $scenario.PromptedEvidence 'frozen-legacy-pin-mutated.json'
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $scenario.Manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.8' `
            -Tag 'v1.0.8' `
            -PreviousVersion '1.0.7' `
            -PublishedRelease $scenario.PublishedRelease | Out-Null
    } '*legacy records do not match the immutable v1.0.7 byte pins*'

    Invoke-FailingTest 'rejects frozen legacy bytes not carried by the official previous release' {
        $scenario = New-TestOrdinaryScenario
        $scenario.PublishedRelease.assets[1].digest = ('sha256:' + ('0' * 64))
        $loaded = Write-AndReadTestEvidence `
            $scenario.PromptedEvidence 'frozen-legacy-official-hash-changed.json'
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $scenario.Manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.8' `
            -Tag 'v1.0.8' `
            -PreviousVersion '1.0.7' `
            -PublishedRelease $scenario.PublishedRelease | Out-Null
    } '*frozen appcast.xml does not match the official previous GitHub release asset*'

    Invoke-FailingTest 'rejects a frozen legacy binding sourced from another release' {
        $scenario = New-TestOrdinaryScenario
        $scenario.Manifest.frozen_legacy_feed.source_tag = 'v1.0.6'
        $loaded = Write-AndReadTestEvidence `
            $scenario.PromptedEvidence 'frozen-legacy-source-changed.json'
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $scenario.Manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.8' `
            -Tag 'v1.0.8' `
            -PreviousVersion '1.0.7' `
            -PublishedRelease $scenario.PublishedRelease | Out-Null
    } '*frozen_legacy_feed must bind exact v1.0.7 bytes*'

    Invoke-PassingTest 'hashes and parses the same immutable byte snapshot' {
        $path = Join-Path $script:testRoot 'read-once.json'
        $json = New-TestEvidence prompted | ConvertTo-Json -Depth 20
        $originalBytes = [Text.UTF8Encoding]::new($false).GetBytes($json)
        [IO.File]::WriteAllBytes($path, $originalBytes)
        $loaded = Read-UpdateQualificationEvidence -LiteralPath $path

        $hasher = [Security.Cryptography.SHA256]::Create()
        try {
            $expectedHash = [BitConverter]::ToString(
                $hasher.ComputeHash($originalBytes)).Replace('-', '').ToLowerInvariant()
        } finally {
            $hasher.Dispose()
        }
        [IO.File]::WriteAllText($path, '{}', [Text.UTF8Encoding]::new($false))
        $binding = Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest (New-TestManifest) `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease)
        Assert-TestEqual $binding.result_sha256 $expectedHash (
            'The binding hash did not describe the parsed bytes.')
        Assert-TestEqual $binding.result_length ([uint64]$originalBytes.Length) (
            'The binding length did not describe the parsed bytes.')
    }

    $negativeEvidenceCases = @(
        [pscustomobject]@{
            Name = 'rejects a string schema'
            Mode = 'prompted'
            Message = '*evidence.schema must be a non-negative JSON integer*'
            Mutate = { param($e) $e.schema = '1' }
        },
        [pscustomobject]@{
            Name = 'rejects an unsupported schema'
            Mode = 'prompted'
            Message = '*evidence.schema must be the integer 1*'
            Mutate = { param($e) $e.schema = 2 }
        },
        [pscustomobject]@{
            Name = 'rejects a passing result that reports an error'
            Mode = 'prompted'
            Message = '*evidence.error must be null*'
            Mutate = { param($e) $e.error = 'update failed after all' }
        },
        [pscustomobject]@{
            Name = 'rejects a missing top-level object'
            Mode = 'prompted'
            Message = '*evidence.verification is required*'
            Mutate = { param($e) $e.PSObject.Properties.Remove('verification') }
        },
        [pscustomobject]@{
            Name = 'rejects partial staged evidence'
            Mode = 'prompted'
            Message = '*evidence.staged_update.expected_path_match is required*'
            Mutate = {
                param($e)
                $e.staged_update.PSObject.Properties.Remove('expected_path_match')
            }
        },
        [pscustomobject]@{
            Name = 'rejects stringly typed booleans'
            Mode = 'prompted'
            Message = '*expected_path_match must be a JSON boolean*'
            Mutate = { param($e) $e.staged_update.expected_path_match = 'true' }
        },
        [pscustomobject]@{
            Name = 'rejects stringly typed counters'
            Mode = 'automatic'
            Message = '*download_confirmation_actions must be a non-negative JSON integer*'
            Mutate = { param($e) $e.verification.download_confirmation_actions = '0' }
        },
        [pscustomobject]@{
            Name = 'rejects legacy automatic evidence schema for the rescue bridge'
            Mode = 'automatic'
            Message = '*evidence.schema must be the integer 2*'
            Mutate = { param($e) $e.schema = 1 }
        },
        [pscustomobject]@{
            Name = 'rejects a bridge dispatched with the wrong protocol'
            Mode = 'automatic'
            Message = '*Accepted v3 install_update*'
            Mutate = { param($e) $e.verification.dispatch_bridge.protocol_version = 4 }
        },
        [pscustomobject]@{
            Name = 'rejects a bridge response that was not Accepted'
            Mode = 'automatic'
            Message = '*Accepted v3 install_update*'
            Mutate = { param($e) $e.verification.dispatch_bridge.response_type = 'error' }
        },
        [pscustomobject]@{
            Name = 'rejects bridge package metadata not taken from the prepared feed'
            Mode = 'automatic'
            Message = '*does not match the prepared v2 appcast enclosure*'
            Mutate = {
                param($e)
                $e.verification.dispatch_bridge.package.signature =
                    [Convert]::ToBase64String([byte[]]::new(64))
            }
        },
        [pscustomobject]@{
            Name = 'rejects a candidate tray that did not use protocol v4'
            Mode = 'automatic'
            Message = '*service and tray protocol version 4*'
            Mutate = { param($e) $e.verification.upgraded_tray_protocol_version = 3 }
        },
        [pscustomobject]@{
            Name = 'rejects probe appcast bytes not bound by the manifest'
            Mode = 'automatic'
            Message = '*signed appcast bytes do not match*'
            Mutate = { param($e) $e.verification.candidate_tray_probe.appcast_sha256 = ('0' * 64) }
        },
        [pscustomobject]@{
            Name = 'rejects probe payload length not bound by the manifest'
            Mode = 'automatic'
            Message = '*payload metadata does not match*'
            Mutate = { param($e) $e.verification.candidate_tray_probe.payload.length = 38 }
        },
        [pscustomobject]@{
            Name = 'rejects a probe request from the wrong client identity'
            Mode = 'automatic'
            Message = '*requests.payload.user_agent must be*resticpal/1.0.7*'
            Mutate = {
                param($e)
                $e.verification.candidate_tray_probe.requests.payload.user_agent =
                    'resticpal/1.0.6'
            }
        },
        [pscustomobject]@{
            Name = 'rejects a probe without the signature failure diagnostic'
            Mode = 'automatic'
            Message = '*update.failed/update_signature_invalid*'
            Mutate = {
                param($e)
                $e.verification.candidate_tray_probe.diagnostics[1].failure_code =
                    'update_download_failed'
            }
        },
        [pscustomobject]@{
            Name = 'rejects probe diagnostics in reverse temporal order'
            Mode = 'automatic'
            Message = '*update.failed/update_signature_invalid*'
            Mutate = {
                param($e)
                $e.verification.candidate_tray_probe.diagnostics[1].observed_at =
                    '2026-08-23T19:59:59.0000000Z'
            }
        },
        [pscustomobject]@{
            Name = 'rejects a probe that left its final MSI staged'
            Mode = 'automatic'
            Message = '*left staged bytes, launched msiexec*'
            Mutate = { param($e) $e.verification.candidate_tray_probe.final_exists = $true }
        },
        [pscustomobject]@{
            Name = 'rejects a probe that launched Windows Installer'
            Mode = 'automatic'
            Message = '*left staged bytes, launched msiexec*'
            Mutate = { param($e) $e.verification.candidate_tray_probe.msiexec_process_count = 1 }
        },
        [pscustomobject]@{
            Name = 'rejects any residual probe staging entry'
            Mode = 'automatic'
            Message = '*left staged bytes, launched msiexec*'
            Mutate = {
                param($e)
                $e.verification.candidate_tray_probe.staging_entries = @(
                    'C:\ProgramData\ResticPal\Updates\resticpal-1.0.8-x64.msi.tmp')
            }
        },
        [pscustomobject]@{
            Name = 'rejects a probe attributed to another service process'
            Mode = 'automatic'
            Message = '*did not run through the upgraded tray/service*'
            Mutate = { param($e) $e.verification.candidate_tray_probe.service_process_id = 201 }
        },
        [pscustomobject]@{
            Name = 'rejects an automatic result with a prompted qualification label'
            Mode = 'automatic'
            Message = '*evidence.qualification must be*automatic*'
            Mutate = {
                param($e)
                $e.qualification = 'previous-published-client-prompted-update'
            }
        },
        [pscustomobject]@{
            Name = 'rejects a prompted result claiming an automatic mode'
            Mode = 'prompted'
            Message = '*evidence.installation_mode must be*prompted*'
            Mutate = { param($e) $e.installation_mode = 'automatic' }
        },
        [pscustomobject]@{
            Name = 'rejects automatic staging outside the exact ProgramData path'
            Mode = 'automatic'
            Message = '*exact ProgramData MSI path*'
            Mutate = {
                param($e)
                $e.staged_update.path = 'C:\Temp\resticpal-1.0.7-x64.msi'
            }
        },
        [pscustomobject]@{
            Name = 'rejects an automatic installer owned by the interactive user'
            Mode = 'automatic'
            Message = '*service-child LocalSystem session-0*'
            Mutate = { param($e) $e.verification.candidate_installer_owner = 'TEST\User' }
        },
        [pscustomobject]@{
            Name = 'rejects a silent run with a confirmation'
            Mode = 'automatic'
            Message = '*required confirmation*'
            Mutate = { param($e) $e.verification.install_confirmation_actions = 1 }
        },
        [pscustomobject]@{
            Name = 'rejects an automatic run missing one no-UAC proof'
            Mode = 'automatic'
            Message = '*no_uac_prompt is required*'
            Mutate = { param($e) $e.verification.PSObject.Properties.Remove('no_uac_prompt') }
        },
        [pscustomobject]@{
            Name = 'rejects any transient automatic UI process start'
            Mode = 'automatic'
            Message = '*required confirmation, interactive UI*'
            Mutate = { param($e) $e.verification.automatic_ui_process_starts = 1 }
        },
        [pscustomobject]@{
            Name = 'rejects a prompted run without interactive confirmation'
            Mode = 'prompted'
            Message = '*one explicit interactive download and install confirmation*'
            Mutate = { param($e) $e.verification.download_confirmation_actions = 0 }
        },
        [pscustomobject]@{
            Name = 'rejects a prompted run with a string session ID'
            Mode = 'prompted'
            Message = '*candidate_installer_session_id must be a non-negative JSON integer*'
            Mutate = { param($e) $e.verification.candidate_installer_session_id = '1' }
        },
        [pscustomobject]@{
            Name = 'rejects a prompted installer that did not run the staged MSI'
            Mode = 'prompted'
            Message = '*candidate_installer_command_line must name the exact staged MSI path*'
            Mutate = {
                param($e)
                $e.verification.candidate_installer_command_line =
                    'msiexec.exe /i "C:\Temp\resticpal-1.0.7-x64.msi"'
            }
        },
        [pscustomobject]@{
            Name = 'rejects a candidate hash with uppercase hex'
            Mode = 'prompted'
            Message = '*candidate_sha256 must be a lowercase SHA-256 digest*'
            Mutate = { param($e) $e.verification.candidate_sha256 = ('A' * 64) }
        }
    )
    $negativeIndex = 0
    foreach ($case in $negativeEvidenceCases) {
        $negativeIndex++
        $evidence = Copy-TestJsonObject (New-TestEvidence $case.Mode)
        & $case.Mutate $evidence
        $loaded = Write-AndReadTestEvidence `
            $evidence "negative-$negativeIndex.json"
        Invoke-FailingTest $case.Name {
            Assert-UpdateQualificationEvidence `
                -LoadedEvidence $loaded `
                -Manifest (New-TestManifest) `
                -ExpectedInstallationMode $case.Mode `
                -Version '1.0.7' `
                -Tag 'v1.0.7' `
                -PreviousVersion '1.0.6' `
                -PublishedRelease (New-TestPublishedRelease) | Out-Null
        } $case.Message
    }

    Invoke-FailingTest 'rejects swapped prompted and automatic files' {
        Invoke-TestPair `
            -Prompted (New-TestEvidence automatic) `
            -Automatic (New-TestEvidence prompted) | Out-Null
    } '*evidence.schema must be the integer 1*'

    Invoke-FailingTest 'rejects one file supplied for both modes' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'same-result.json'
        Assert-UpdateQualificationPair `
            -PromptedEvidence $loaded `
            -AutomaticEvidence $loaded `
            -Manifest (New-TestManifest) `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*must be separate result files*'

    Invoke-FailingTest 'rejects a missing automatic result' {
        $prompted = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'missing-automatic-prompted.json'
        Assert-UpdateQualificationPair `
            -PromptedEvidence $prompted `
            -AutomaticEvidence $null `
            -Manifest (New-TestManifest) `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*cannot bind argument*null*'

    Invoke-FailingTest 'rejects invalid JSON before qualification validation' {
        $invalidPath = Join-Path $script:testRoot 'invalid.json'
        [IO.File]::WriteAllText(
            $invalidPath,
            '{"schema":',
            [Text.UTF8Encoding]::new($false))
        Read-UpdateQualificationEvidence -LiteralPath $invalidPath | Out-Null
    } '*not valid JSON*'

    $bindings = Invoke-TestPair `
        -Prompted (New-TestEvidence prompted) `
        -Automatic (New-TestEvidence automatic) `
        -PromptedName 'binding-prompted.json' `
        -AutomaticName 'binding-automatic.json'
    Invoke-PassingTest 'accepts an unbound dual qualification manifest' {
        $manifest = New-TestManifest
        Assert-TestEqual `
            (Test-UpdateQualificationBindingState $manifest $bindings) `
            $false `
            'An unbound manifest was reported as bound.'
    }
    Invoke-PassingTest 'accepts an identical existing dual binding' {
        $manifest = New-TestManifest
        $manifest.qualifications = Copy-TestJsonObject $bindings
        Assert-TestEqual `
            (Test-UpdateQualificationBindingState $manifest $bindings) `
            $true `
            'An identical binding was not idempotent.'
    }
    Invoke-FailingTest 'rejects a missing qualification slot' {
        $manifest = Copy-TestJsonObject (New-TestManifest)
        $manifest.qualifications.PSObject.Properties.Remove('automatic')
        Test-UpdateQualificationBindingState $manifest $bindings | Out-Null
    } '*qualifications.automatic is required*'
    Invoke-FailingTest 'rejects a partially bound manifest' {
        $manifest = New-TestManifest
        $manifest.qualifications.prompted = $bindings.prompted
        Test-UpdateQualificationBindingState $manifest $bindings | Out-Null
    } '*partially bound*'
    Invoke-FailingTest 'rejects a different existing evidence binding' {
        $manifest = New-TestManifest
        $manifest.qualifications = Copy-TestJsonObject $bindings
        $manifest.qualifications.automatic.result_sha256 = ('0' * 64)
        Test-UpdateQualificationBindingState $manifest $bindings | Out-Null
    } '*already bound to different qualification evidence*'
    Invoke-FailingTest 'rejects swapped requested binding slots' {
        $swapped = [ordered]@{
            prompted = $bindings.automatic
            automatic = $bindings.prompted
        }
        Test-UpdateQualificationBindingState (New-TestManifest) $swapped | Out-Null
    } '*requested_bindings.prompted.schema must be the integer 1*'
    Invoke-FailingTest 'rejects non-array official release assets' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'assets-object.json'
        $publishedRelease = New-TestPublishedRelease
        $publishedRelease.assets = $publishedRelease.assets[0]
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest (New-TestManifest) `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease $publishedRelease | Out-Null
    } '*published_release_api.assets must be a JSON array*'
    Invoke-FailingTest 'rejects stringly typed official release flags' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'release-flag-string.json'
        $publishedRelease = New-TestPublishedRelease
        $publishedRelease.isDraft = 'false'
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest (New-TestManifest) `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease $publishedRelease | Out-Null
    } '*published_release_api.isDraft must be a JSON boolean*'
    Invoke-FailingTest 'rejects a stringly typed manifest schema' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'manifest-schema-string.json'
        $manifest = New-TestManifest
        $manifest.schema = '5'
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*release_manifest.schema must be a non-negative JSON integer*'

    Invoke-FailingTest 'rejects the previous manifest schema' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'manifest-schema-four.json'
        $manifest = New-TestManifest
        $manifest.schema = 4
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*release_manifest.schema must be the integer 5*'

    Invoke-FailingTest 'rejects a legacy appcast hash that differs from v2' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'legacy-appcast-hash-changed.json'
        $manifest = Copy-TestJsonObject (New-TestManifest)
        $manifest.files.legacy_appcast.sha256 = ('0' * 64)
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*legacy appcast records must be byte-identical to the v2 appcast records*'

    Invoke-FailingTest 'rejects a legacy appcast length that differs from v2' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'legacy-appcast-length-changed.json'
        $manifest = Copy-TestJsonObject (New-TestManifest)
        $manifest.files.legacy_appcast.length++
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*legacy appcast records must be byte-identical to the v2 appcast records*'

    Invoke-FailingTest 'rejects a legacy signature hash that differs from v2' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'legacy-signature-hash-changed.json'
        $manifest = Copy-TestJsonObject (New-TestManifest)
        $manifest.files.legacy_appcast_signature.sha256 = ('0' * 64)
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*legacy appcast records must be byte-identical to the v2 appcast records*'

    Invoke-FailingTest 'rejects a legacy signature length that differs from v2' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'legacy-signature-length-changed.json'
        $manifest = Copy-TestJsonObject (New-TestManifest)
        $manifest.files.legacy_appcast_signature.length++
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*legacy appcast records must be byte-identical to the v2 appcast records*'

    Invoke-FailingTest 'rejects a dual-named feed bound to another version' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'dual-named-version-changed.json'
        $manifest = Copy-TestJsonObject (New-TestManifest)
        $manifest.dual_named_feed.version = '1.0.6'
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*dual_named_feed must bind the release version*'

    Invoke-FailingTest 'rejects a dual-named feed with another appcast hash' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'dual-named-appcast-changed.json'
        $manifest = Copy-TestJsonObject (New-TestManifest)
        $manifest.dual_named_feed.appcast_sha256 = ('0' * 64)
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*dual_named_feed must bind the release version*'

    Invoke-FailingTest 'rejects a dual-named feed with another signature hash' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'dual-named-signature-changed.json'
        $manifest = Copy-TestJsonObject (New-TestManifest)
        $manifest.dual_named_feed.appcast_signature_sha256 = ('0' * 64)
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*dual_named_feed must bind the release version*'

    Invoke-FailingTest 'rejects prepared package metadata outside the exact v2 enclosure URL' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence prompted) 'package-url-changed.json'
        $manifest = Copy-TestJsonObject (New-TestManifest)
        $manifest.update_package.url =
            'https://github.com/theatrus/resticpal/releases/download/v1.0.7/resticpal-1.0.7-x64.msi'
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $manifest `
            -ExpectedInstallationMode prompted `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*release_manifest.update_package.url must be*updates.resticpal.com*'

    Invoke-FailingTest 'rejects ordinary tray strategy for the one-time bridge transition' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence automatic) 'bridge-strategy-changed.json'
        $manifest = Copy-TestJsonObject (New-TestManifest)
        $manifest.automatic_qualification.strategy = 'published-client-tray'
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $manifest `
            -ExpectedInstallationMode automatic `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*automatic_qualification.strategy must be*published-service-ipc-bridge*'

    Invoke-FailingTest 'rejects probe metadata not bound to its exact qualification file' {
        $loaded = Write-AndReadTestEvidence `
            (New-TestEvidence automatic) 'probe-manifest-changed.json'
        $manifest = Copy-TestJsonObject (New-TestManifest)
        $manifest.automatic_qualification.probe.payload_sha256 = ('0' * 64)
        Assert-UpdateQualificationEvidence `
            -LoadedEvidence $loaded `
            -Manifest $manifest `
            -ExpectedInstallationMode automatic `
            -Version '1.0.7' `
            -Tag 'v1.0.7' `
            -PreviousVersion '1.0.6' `
            -PublishedRelease (New-TestPublishedRelease) | Out-Null
    } '*candidate-tray probe metadata is not the exact signed invalid-package probe*'
} finally {
    Remove-Item -LiteralPath $script:testRoot -Recurse -Force -ErrorAction SilentlyContinue
}

if ($script:failed -ne 0) {
    throw "$($script:failed) release qualification test(s) failed; $($script:passed) passed."
}
Write-Host "OK: $($script:passed) release qualification tests passed."
