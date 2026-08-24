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

    [Parameter(Mandatory)]
    [string] $ExpectedPublishedVersion,

    [Parameter(Mandatory)]
    [string] $ExpectedCandidateVersion,

    [Parameter(Mandatory)]
    [string] $ExpectedPublishedSha256,

    [Parameter(Mandatory)]
    [string] $ExpectedCandidateSha256,

    [Parameter(Mandatory)]
    [string] $ExpectedAppCastSha256,

    [Parameter(Mandatory)]
    [string] $ExpectedAppCastSignatureSha256,

    [Parameter(Mandatory)]
    [string] $PublishedReleaseAssetName,

    [Parameter(Mandatory)]
    [uint64] $PublishedReleaseAssetLength,

    [Parameter(Mandatory)]
    [string] $PublishedReleaseAssetUrl,

    [Parameter(Mandatory)]
    [string] $EnclosureUrl,

    [Parameter(Mandatory)]
    [string] $EnclosureSignature,

    [switch] $BridgeTransition,

    [string] $ProbeAppCastPath,

    [string] $ProbeAppCastSignaturePath,

    [string] $ProbePayloadPath,

    [string] $ExpectedProbeVersion,

    [string] $ExpectedProbeAppCastSha256,

    [string] $ExpectedProbeAppCastSignatureSha256,

    [string] $ExpectedProbePayloadSha256,

    [uint64] $ExpectedProbePayloadLength,

    [string] $ExpectedProbePayloadUrl,

    [string] $ExpectedProbePackageSignature,

    [Parameter(Mandatory)]
    [ValidateRange(1, 15)]
    [int] $InstallerLaunchTimeoutMinutes,

    [Parameter(Mandatory)]
    [ValidateRange(1, 20)]
    [int] $InstallerCompletionTimeoutMinutes,

    [ValidateSet('Prompted', 'Automatic')]
    [string] $InstallationMode = 'Prompted',

    [Parameter(Mandatory)]
    [string] $ResultRoot,

    [switch] $KeepOpen
)

$ErrorActionPreference = 'Stop'
$startedAt = [DateTimeOffset]::UtcNow
$resultPath = Join-Path $ResultRoot 'result.json'
$temporaryResultPath = Join-Path $ResultRoot 'result.json.tmp'
$transcriptPath = Join-Path $ResultRoot 'guest.log'
$bootstrapErrorPath = Join-Path $ResultRoot 'bootstrap-error.txt'
$transcriptStarted = $false
New-Item -ItemType Directory -Path $ResultRoot -Force | Out-Null
try {
    Start-Transcript -LiteralPath $transcriptPath -Force | Out-Null
    $transcriptStarted = $true
} catch {
    $bootstrapMessage = "Could not start the guest transcript: $($_.Exception.Message)"
    $bootstrapMessage | Set-Content -LiteralPath $bootstrapErrorPath -Encoding UTF8
    [ordered]@{
        schema = 1
        status = 'failed'
        exit_code = 1
        error = $bootstrapMessage
        started_at = $startedAt.ToString('o')
        finished_at = [DateTimeOffset]::UtcNow.ToString('o')
    } | ConvertTo-Json | Set-Content -LiteralPath $resultPath -Encoding UTF8
    exit 1
}
trap {
    $bootstrapMessage = "Guest initialization failed before the test body: $($_.Exception.Message)"
    try {
        $bootstrapMessage | Set-Content -LiteralPath $bootstrapErrorPath -Encoding UTF8
    } catch {}
    if ($transcriptStarted) {
        try { Stop-Transcript | Out-Null } catch {}
    }
    try {
        $bootstrapResult = [ordered]@{
            schema = 1
            status = 'failed'
            exit_code = 1
            error = $bootstrapMessage
            started_at = $startedAt.ToString('o')
            finished_at = [DateTimeOffset]::UtcNow.ToString('o')
        } | ConvertTo-Json
        [IO.File]::WriteAllText(
            $resultPath,
            $bootstrapResult,
            [Text.UTF8Encoding]::new($false))
    } catch {}
    exit 1
}

Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;

public static class ResticPalUpdateTestNativeMethods
{
    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    public static extern IntPtr SendMessage(
        IntPtr window,
        uint message,
        IntPtr wordParameter,
        IntPtr longParameter);
}
'@

$localRoot = 'C:\ResticPalUpdateTest'
$localArtifactRoot = Join-Path $localRoot 'artifacts'
$exportedArtifactRoot = Join-Path $ResultRoot 'test-artifacts'
$publishedMsi = Join-Path $localRoot 'published-client.msi'
$candidateMsi = Join-Path $localRoot 'candidate.msi'
$appCast = Join-Path $localRoot 'appcast.xml'
$appCastSignature = Join-Path $localRoot 'appcast.xml.signature'
$probeAppCast = Join-Path $localRoot 'probe-appcast.xml'
$probeAppCastSignature = Join-Path $localRoot 'probe-appcast.xml.signature'
$probePayload = if ([string]::IsNullOrWhiteSpace($ProbePayloadPath)) {
    $null
} else {
    Join-Path $localRoot ([IO.Path]::GetFileName($ProbePayloadPath))
}
$baselineInstallLog = Join-Path $localArtifactRoot 'published-client-install.log'
$originLog = Join-Path $localArtifactRoot 'update-origin.log'
$originReady = Join-Path $localArtifactRoot 'update-origin.ready'
$originFeedGate = Join-Path $localArtifactRoot 'update-origin-feed.enabled'
$installerMonitorReady = Join-Path $localArtifactRoot 'installer-process-monitor.ready'
$installerMonitorStop = Join-Path $localArtifactRoot 'installer-process-monitor.stop'
$installerProcessEvents = Join-Path $localArtifactRoot 'installer-process-events.jsonl'
$automaticTrayStdout = Join-Path $localArtifactRoot 'automatic-tray.stdout.log'
$automaticTrayStderr = Join-Path $localArtifactRoot 'automatic-tray.stderr.log'
$installRoot = Join-Path $env:ProgramFiles 'resticpal'
$hostsPath = Join-Path $env:SystemRoot 'System32\drivers\etc\hosts'
$interactiveSessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
$automaticMode = $InstallationMode -ieq 'Automatic'
$bridgeMode = [bool]$BridgeTransition
$publishedUsesV2Feed = ([Version]$ExpectedPublishedVersion -ge [Version]'1.0.7')
$installationModeName = $InstallationMode.ToLowerInvariant()
$status = 'failed'
$exitCode = 1
$errorMessage = $null
$serverJob = $null
$installerMonitorJob = $null
$certificateThumbprint = $null
$originalHostsBytes = $null
$stagedUpdate = $null
$verification = [ordered]@{}
$timing = [ordered]@{}
$installerRequestedAt = $null
$knownInstallerCommandPaths = @()
$msiFilesInUseHandled = $false
$downloadConfirmationActions = 0
$installConfirmationActions = 0
$automaticSettingUiActions = 0
$protocolVersion = $null
$requestId = 0L

if ($bridgeMode -and (
        -not $automaticMode -or
        $ExpectedPublishedVersion -cne '1.0.6' -or
        $ExpectedCandidateVersion -cne '1.0.7')) {
    throw 'The automatic service bridge is restricted to the exact 1.0.6-to-1.0.7 transition.'
}
$missingProbeInputs = @(
        $ProbeAppCastPath,
        $ProbeAppCastSignaturePath,
        $ProbePayloadPath,
        $ExpectedProbeVersion,
        $ExpectedProbeAppCastSha256,
        $ExpectedProbeAppCastSignatureSha256,
        $ExpectedProbePayloadSha256,
        $ExpectedProbePayloadUrl,
        $ExpectedProbePackageSignature) |
    Where-Object { [string]::IsNullOrWhiteSpace([string]$_) }
if ($bridgeMode -and (
        $missingProbeInputs.Count -ne 0 -or $ExpectedProbePayloadLength -eq 0)) {
    throw 'The automatic service bridge is missing candidate-tray probe inputs.'
}

function Write-TestProgress([string] $Message) {
    Write-Host ("[{0:o}] {1}" -f [DateTimeOffset]::UtcNow, $Message)
}

function Invoke-Installer([string] $Arguments, [string] $Action) {
    $process = Start-Process -FilePath "$env:SystemRoot\System32\msiexec.exe" `
        -ArgumentList $Arguments `
        -WindowStyle Hidden `
        -Wait `
        -PassThru
    if ($process.ExitCode -ne 0 -and $process.ExitCode -ne 3010) {
        throw "$Action failed with Windows Installer exit code $($process.ExitCode)."
    }
}

function Wait-Path([string] $Path, [TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        if (Test-Path -LiteralPath $Path -PathType Leaf) {
            return
        }
        if ($null -ne $script:serverJob `
            -and $script:serverJob.State -in @('Completed', 'Failed', 'Stopped')) {
            $jobOutput = Receive-Job -Job $script:serverJob -Keep -ErrorAction SilentlyContinue |
                Out-String
            throw "The local update origin stopped before becoming ready: $jobOutput"
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for $Path."
}

function Wait-InteractiveProcess(
    [string] $Name,
    [TimeSpan] $Timeout,
    [int[]] $ExcludedProcessIds = @()
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $process = Get-Process -Name $Name -ErrorAction SilentlyContinue |
            Where-Object {
                $_.SessionId -eq $interactiveSessionId `
                    -and $ExcludedProcessIds -notcontains $_.Id
            } |
            Select-Object -First 1
        if ($null -ne $process) {
            return $process
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for a new $Name process in interactive session $interactiveSessionId."
}

function Wait-AutomationElement(
    [Diagnostics.Process] $Process,
    [string] $AutomationId,
    [TimeSpan] $Timeout
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $Process.Refresh()
        if ($Process.HasExited) {
            throw "$($Process.ProcessName) exited while waiting for $AutomationId."
        }
        if ($Process.MainWindowHandle -ne [IntPtr]::Zero) {
            $root = [Windows.Automation.AutomationElement]::FromHandle($Process.MainWindowHandle)
            $condition = [Windows.Automation.PropertyCondition]::new(
                [Windows.Automation.AutomationElement]::AutomationIdProperty,
                $AutomationId
            )
            $element = $root.FindFirst([Windows.Automation.TreeScope]::Descendants, $condition)
            if ($null -ne $element) {
                return $element
            }
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for UI element $AutomationId."
}

function Wait-AutomationElementEnabled(
    [Diagnostics.Process] $Process,
    [string] $AutomationId,
    [TimeSpan] $Timeout
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        try {
            $element = Wait-AutomationElement $Process $AutomationId ([TimeSpan]::FromSeconds(2))
            if ($element.Current.IsEnabled -and -not $element.Current.IsOffscreen) {
                return $element
            }
        } catch {
            if ($Process.HasExited) {
                throw
            }
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for enabled UI element $AutomationId."
}

function Wait-AutomationElementByName([string] $Name, [TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    $nameCondition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::NameProperty,
        $Name
    )
    $buttonCondition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::ControlTypeProperty,
        [Windows.Automation.ControlType]::Button
    )
    $condition = [Windows.Automation.AndCondition]::new($nameCondition, $buttonCondition)
    do {
        $elements = [Windows.Automation.AutomationElement]::RootElement.FindAll(
            [Windows.Automation.TreeScope]::Descendants,
            $condition
        )
        foreach ($element in $elements) {
            if ($element.Current.IsEnabled -and -not $element.Current.IsOffscreen) {
                return $element
            }
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for enabled button '$Name'."
}

function Wait-AutomationTextContains(
    [Diagnostics.Process] $Process,
    [string] $AutomationId,
    [string] $ExpectedText,
    [TimeSpan] $Timeout
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    $lastText = ''
    do {
        try {
            $element = Wait-AutomationElement $Process $AutomationId ([TimeSpan]::FromSeconds(2))
            $text = @(
                $element.Current.Name
                $element.FindAll(
                    [Windows.Automation.TreeScope]::Descendants,
                    [Windows.Automation.Condition]::TrueCondition
                ) | ForEach-Object { $_.Current.Name }
            ) -join ' '
            $lastText = $text
            if ($text.IndexOf($ExpectedText, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                return $text
            }
        } catch {
            if ($Process.HasExited) {
                throw
            }
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for '$ExpectedText' in $AutomationId. Observed: $lastText"
}

function Invoke-AutomationElement([Windows.Automation.AutomationElement] $Element) {
    $pattern = $Element.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern)
    $pattern.Invoke()
}

function Enable-AutomaticUpdatesThroughUi(
    [Diagnostics.Process] $Process,
    [TimeSpan] $Timeout
) {
    $toggle = Wait-AutomationElementEnabled `
        $Process 'AutomaticUpdatesToggle' $Timeout
    $pattern = [Windows.Automation.TogglePattern]$toggle.GetCurrentPattern(
        [Windows.Automation.TogglePattern]::Pattern)
    if ($pattern.Current.ToggleState -eq [Windows.Automation.ToggleState]::Indeterminate) {
        throw 'The published client exposed an indeterminate automatic-update setting.'
    }
    if ($pattern.Current.ToggleState -eq [Windows.Automation.ToggleState]::Off) {
        $script:automaticSettingUiActions++
        $pattern.Toggle()
    }
}

function Read-Exact([IO.Stream] $Stream, [int] $Count) {
    $buffer = [byte[]]::new($Count)
    $offset = 0
    while ($offset -lt $Count) {
        $read = $Stream.Read($buffer, $offset, $Count - $offset)
        if ($read -eq 0) {
            throw 'The resticpal service closed the named pipe before completing a frame.'
        }
        $offset += $read
    }
    return ,$buffer
}

function Resolve-ResticPalProtocolVersion {
    $matches = @(Get-ChildItem -Path '\\.\pipe\' -ErrorAction Stop |
        ForEach-Object {
            $match = [Regex]::Match([string]$_.Name, '^ResticPal\.v(?<version>\d+)$')
            if ($match.Success) {
                [uint32]$match.Groups['version'].Value
            }
        } |
        Sort-Object -Unique)
    if ($matches.Count -ne 1) {
        throw ('Expected exactly one versioned ResticPal service pipe; found: ' +
               "$($matches -join ', ').")
    }
    return [uint32]$matches[0]
}

function Invoke-ResticPalRequest([hashtable] $Command) {
    if ($null -eq $script:protocolVersion) {
        $script:protocolVersion = Resolve-ResticPalProtocolVersion
    }
    $script:requestId++
    $request = [ordered]@{
        protocol_version = $script:protocolVersion
        request_id = $script:requestId
        command = $Command
    }
    $json = $request | ConvertTo-Json -Compress -Depth 12
    $utf8 = [Text.UTF8Encoding]::new($false)
    $payload = $utf8.GetBytes($json)
    if ($payload.Length -eq 0 -or $payload.Length -gt 1024 * 1024) {
        throw "Invalid outgoing IPC frame length: $($payload.Length)"
    }

    $client = [IO.Pipes.NamedPipeClientStream]::new(
        '.',
        "ResticPal.v$($script:protocolVersion)",
        [IO.Pipes.PipeDirection]::InOut,
        [IO.Pipes.PipeOptions]::None)
    try {
        $client.Connect(5000)
        $length = [BitConverter]::GetBytes([uint32]$payload.Length)
        if (-not [BitConverter]::IsLittleEndian) {
            [Array]::Reverse($length)
        }
        $client.Write($length, 0, $length.Length)
        $client.Write($payload, 0, $payload.Length)
        $client.Flush()

        $responseLengthBytes = Read-Exact $client 4
        if (-not [BitConverter]::IsLittleEndian) {
            [Array]::Reverse($responseLengthBytes)
        }
        $responseLength = [BitConverter]::ToUInt32($responseLengthBytes, 0)
        if ($responseLength -eq 0 -or $responseLength -gt 1024 * 1024) {
            throw "Invalid incoming IPC frame length: $responseLength"
        }
        $responseBytes = Read-Exact $client ([int]$responseLength)
        $response = $utf8.GetString($responseBytes) | ConvertFrom-Json
    } finally {
        $client.Dispose()
    }

    if ([uint32]$response.protocol_version -ne [uint32]$script:protocolVersion `
        -or [uint64]$response.request_id -ne [uint64]$script:requestId) {
        throw 'The service returned a mismatched IPC response.'
    }
    return $response.payload
}

function Wait-AutomaticUpdatesEnabled([TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    $lastPayload = $null
    $lastError = $null
    do {
        try {
            $lastPayload = Invoke-ResticPalRequest @{ type = 'get_update_settings' }
            if ($lastPayload.type -ceq 'update_settings' `
                -and $lastPayload.configuration.automatic_install -eq $true) {
                return $lastPayload.configuration
            }
        } catch {
            $lastError = $_.Exception.Message
            $script:protocolVersion = $null
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw ('The service did not report automatic_install=true within the allowed time. ' +
           "Last response: $($lastPayload | ConvertTo-Json -Compress -Depth 5). " +
           "Last error: $lastError")
}

function Wait-CandidateTrayProbeFailure(
    [DateTimeOffset] $NotBefore,
    [TimeSpan] $Timeout
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    $lastPayload = $null
    $lastError = $null
    do {
        try {
            Assert-NoAutomaticUpdateConfirmation
            $lastPayload = Invoke-ResticPalRequest @{
                type = 'get_diagnostics'
                limit = 200
            }
            if ($lastPayload.type -cne 'diagnostics') {
                throw "Unexpected diagnostics response type $($lastPayload.type)."
            }
            $entries = @($lastPayload.entries | Where-Object {
                [DateTimeOffset]$_.timestamp -ge $NotBefore
            } | Sort-Object { [DateTimeOffset]$_.timestamp })
            $started = @($entries | Where-Object event_id -CEQ 'update.started') |
                Select-Object -First 1
            if ($null -ne $started) {
                $startedAt = [DateTimeOffset]$started.timestamp
                $failed = @($entries | Where-Object {
                    $_.event_id -ceq 'update.failed' -and
                    $_.code -ceq 'update_signature_invalid' -and
                    [DateTimeOffset]$_.timestamp -gt $startedAt
                }) | Select-Object -First 1
                if ($null -ne $failed) {
                    return [ordered]@{
                        started_at = $startedAt.ToUniversalTime().ToString('o')
                        failed_at = (
                            [DateTimeOffset]$failed.timestamp).ToUniversalTime().ToString('o')
                    }
                }
            }
        } catch {
            $lastError = $_.Exception.Message
            $script:protocolVersion = $null
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw ('The upgraded tray/service did not complete the invalid-signature probe. ' +
           "Last response: $($lastPayload | ConvertTo-Json -Compress -Depth 6). " +
           "Last error: $lastError")
}

function Find-VisibleAutomationButton([string] $Name) {
    $nameCondition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::NameProperty,
        $Name)
    $buttonCondition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::ControlTypeProperty,
        [Windows.Automation.ControlType]::Button)
    $condition = [Windows.Automation.AndCondition]::new($nameCondition, $buttonCondition)
    $elements = [Windows.Automation.AutomationElement]::RootElement.FindAll(
        [Windows.Automation.TreeScope]::Descendants,
        $condition)
    foreach ($element in $elements) {
        if ($element.Current.IsEnabled -and -not $element.Current.IsOffscreen) {
            return $element
        }
    }
    return $null
}

function Assert-NoAutomaticUpdateConfirmation {
    if (-not $automaticMode) {
        return
    }
    if ($null -ne (Find-VisibleAutomationButton 'Download')) {
        throw 'Automatic mode exposed a Download confirmation button.'
    }
    if ($null -ne (Find-VisibleAutomationButton 'Install update')) {
        throw 'Automatic mode exposed an Install update confirmation button.'
    }
    $interactiveInstallerIds = @(Get-Process -Name msiexec -ErrorAction SilentlyContinue |
        Where-Object SessionId -eq $interactiveSessionId |
        ForEach-Object Id)
    if ($interactiveInstallerIds.Count -gt 0) {
        $windowCondition = [Windows.Automation.PropertyCondition]::new(
            [Windows.Automation.AutomationElement]::ControlTypeProperty,
            [Windows.Automation.ControlType]::Window)
        $windows = [Windows.Automation.AutomationElement]::RootElement.FindAll(
            [Windows.Automation.TreeScope]::Descendants,
            $windowCondition)
        foreach ($window in $windows) {
            if ($window.Current.IsEnabled `
                -and -not $window.Current.IsOffscreen `
                -and $interactiveInstallerIds -contains $window.Current.ProcessId) {
                throw ("Automatic mode exposed an interactive Windows Installer window: " +
                       "'$($window.Current.Name)'.")
            }
        }
    }
    if (@(Get-Process -Name consent -ErrorAction SilentlyContinue).Count -ne 0) {
        throw 'Automatic mode started a UAC consent process.'
    }
}

function Get-InstalledVersion {
    $uninstallRoots = @(
        'HKLM:\Software\Microsoft\Windows\CurrentVersion\Uninstall\*',
        'HKLM:\Software\WOW6432Node\Microsoft\Windows\CurrentVersion\Uninstall\*'
    )
    $product = Get-ItemProperty -Path $uninstallRoots -ErrorAction SilentlyContinue |
        Where-Object DisplayName -eq 'resticpal' |
        Select-Object -First 1
    if ($null -eq $product) {
        return $null
    }
    return $product.DisplayVersion
}

function Handle-CandidateFilesInUseDialog([uint32] $InstallerProcessId) {
    if ($InstallerProcessId -eq 0 -or $script:msiFilesInUseHandled) {
        return
    }

    $nameCondition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::NameProperty,
        'resticpal'
    )
    $windowCondition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::ControlTypeProperty,
        [Windows.Automation.ControlType]::Window
    )
    $windows = [Windows.Automation.AutomationElement]::RootElement.FindAll(
        # Windows Installer owns a disabled top-level frame whose enabled FilesInUse
        # dialog is a nested #32770 window. Search the full desktop subtree, then bind
        # the match back to the exact candidate msiexec process below.
        [Windows.Automation.TreeScope]::Descendants,
        [Windows.Automation.AndCondition]::new($nameCondition, $windowCondition)
    )
    foreach ($window in $windows) {
        if ([uint32]$window.Current.ProcessId -ne $InstallerProcessId `
            -or -not $window.Current.IsEnabled `
            -or $window.Current.ClassName -cne '#32770') {
            continue
        }

        $messageCondition = [Windows.Automation.PropertyCondition]::new(
            [Windows.Automation.AutomationElement]::AutomationIdProperty,
            '3000'
        )
        $message = $window.FindFirst(
            [Windows.Automation.TreeScope]::Descendants,
            $messageCondition
        )
        if ($null -eq $message `
            -or $message.Current.Name -cne
                'The following applications should be closed before continuing the install:' `
            -or $message.Current.ClassName -cne 'Static') {
            continue
        }

        $autoCloseCondition = [Windows.Automation.PropertyCondition]::new(
            [Windows.Automation.AutomationElement]::AutomationIdProperty,
            '1014'
        )
        $autoClose = $window.FindFirst(
            [Windows.Automation.TreeScope]::Descendants,
            $autoCloseCondition
        )
        $doNotCloseCondition = [Windows.Automation.PropertyCondition]::new(
            [Windows.Automation.AutomationElement]::AutomationIdProperty,
            '1015'
        )
        $doNotClose = $window.FindFirst(
            [Windows.Automation.TreeScope]::Descendants,
            $doNotCloseCondition
        )
        $okCondition = [Windows.Automation.PropertyCondition]::new(
            [Windows.Automation.AutomationElement]::AutomationIdProperty,
            '3001'
        )
        $okButton = $window.FindFirst(
            [Windows.Automation.TreeScope]::Descendants,
            $okCondition
        )
        if ($null -eq $autoClose -or $null -eq $doNotClose -or $null -eq $okButton) {
            # Windows can expose the dialog before all of its native child controls exist.
            # Let the caller's bounded poll inspect it again rather than treating construction
            # of the signed MSI dialog as a malformed prompt.
            continue
        }
        if ($autoClose.Current.ClassName -cne 'Button' `
            -or $doNotClose.Current.ClassName -cne 'Button' `
            -or $okButton.Current.ClassName -cne 'Button' `
            -or $autoClose.Current.Name -cne
                'Automatically close applications and attempt to restart them after setup is complete.' `
            -or $doNotClose.Current.Name -cne
                'Do not close applications. (A Reboot may be required.)' `
            -or $okButton.Current.Name -cne 'OK') {
            throw 'The candidate MSI FilesInUse dialog controls did not match the expected choices.'
        }
        if (-not $autoClose.Current.IsEnabled -or -not $okButton.Current.IsEnabled) {
            continue
        }
        if ($automaticMode) {
            $verification.automatic_installer_dialog_observed = $true
            throw ('The silent LocalSystem MSI exposed a FilesInUse dialog instead of ' +
                   'continuing without user intervention.')
        }
        $autoCloseHandle = [IntPtr]$autoClose.Current.NativeWindowHandle
        $okHandle = [IntPtr]$okButton.Current.NativeWindowHandle
        if ($autoCloseHandle -eq [IntPtr]::Zero -or $okHandle -eq [IntPtr]::Zero) {
            throw 'The candidate MSI FilesInUse dialog controls have no native window handles.'
        }

        $bmGetCheck = [uint32]0x00F0
        $bmClick = [uint32]0x00F5
        [ResticPalUpdateTestNativeMethods]::SendMessage(
            $autoCloseHandle,
            $bmClick,
            [IntPtr]::Zero,
            [IntPtr]::Zero) | Out-Null
        $checked = [ResticPalUpdateTestNativeMethods]::SendMessage(
            $autoCloseHandle,
            $bmGetCheck,
            [IntPtr]::Zero,
            [IntPtr]::Zero).ToInt64()
        if ($checked -ne 1) {
            throw 'Windows Installer did not select automatic close/restart in FilesInUse.'
        }
        [ResticPalUpdateTestNativeMethods]::SendMessage(
            $okHandle,
            $bmClick,
            [IntPtr]::Zero,
            [IntPtr]::Zero) | Out-Null
        $script:msiFilesInUseHandled = $true
        $verification.msi_files_in_use_prompt_handled = $true
        $verification.msi_files_in_use_process_id = $InstallerProcessId
        $timing.msi_files_in_use_prompt_handled_at = [DateTimeOffset]::UtcNow.ToString('o')
        Write-TestProgress (
            "Handled the signed candidate MSI FilesInUse prompt from process " +
            "${InstallerProcessId}: selected automatic close/restart and invoked OK.")
        return
    }
}

function Wait-InstalledVersion(
    [string] $ExpectedVersion,
    [TimeSpan] $Timeout,
    [uint32] $CandidateInstallerProcessId = 0
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    $nextProgress = [DateTime]::UtcNow
    $lastVersion = $null
    do {
        Assert-NoAutomaticUpdateConfirmation
        Handle-CandidateFilesInUseDialog $CandidateInstallerProcessId
        $lastVersion = Get-InstalledVersion
        if ($lastVersion -ceq $ExpectedVersion) {
            return
        }
        if ([DateTime]::UtcNow -ge $nextProgress) {
            $installerProcessIds = @(
                Get-Process -Name msiexec -ErrorAction SilentlyContinue |
                    ForEach-Object Id)
            Write-TestProgress (
                "Waiting for installed version $ExpectedVersion; observed '$lastVersion'; " +
                "active msiexec process IDs: $($installerProcessIds -join ', ').")
            $nextProgress = [DateTime]::UtcNow.AddSeconds(30)
        }
        Start-Sleep -Milliseconds 500
    } while ([DateTime]::UtcNow -lt $deadline)
    $installerProcessIds = @(
        Get-Process -Name msiexec -ErrorAction SilentlyContinue |
            ForEach-Object Id)
    throw ("Timed out waiting for installed version $ExpectedVersion; observed '$lastVersion'; " +
           "active msiexec process IDs: $($installerProcessIds -join ', ').")
}

function Wait-MsiTransactionStart(
    [string] $PackagePath,
    [DateTime] $NotBefore,
    [TimeSpan] $Timeout,
    [uint32] $CandidateInstallerProcessId = 0
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    $nextProgress = [DateTime]::UtcNow
    do {
        Assert-NoAutomaticUpdateConfirmation
        Handle-CandidateFilesInUseDialog $CandidateInstallerProcessId
        $event = Get-WinEvent -FilterHashtable @{
            LogName = 'Application'
            ProviderName = 'MsiInstaller'
            Id = 1040
            StartTime = $NotBefore
        } -ErrorAction SilentlyContinue |
            Where-Object {
                $_.Message.IndexOf($PackagePath, [StringComparison]::OrdinalIgnoreCase) -ge 0
            } |
            Sort-Object TimeCreated -Descending |
            Select-Object -First 1
        if ($null -ne $event) {
            return $event
        }
        if ([DateTime]::UtcNow -ge $nextProgress) {
            $installerProcessIds = @(
                Get-Process -Name msiexec -ErrorAction SilentlyContinue |
                    ForEach-Object Id)
            Write-TestProgress (
                "Waiting for Windows Installer to begin the candidate transaction; " +
                "active msiexec process IDs: $($installerProcessIds -join ', ').")
            $nextProgress = [DateTime]::UtcNow.AddSeconds(15)
        }
        Start-Sleep -Milliseconds 500
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Windows Installer did not begin a transaction for $PackagePath within $Timeout."
}

function Wait-CandidateInstallerProcessStart(
    [string] $PackagePath,
    [TimeSpan] $Timeout,
    [uint32] $ExpectedSessionId,
    [uint32] $ExpectedParentProcessId = 0,
    [switch] $RequireSilent
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    $nextProgress = [DateTime]::UtcNow
    do {
        Assert-NoAutomaticUpdateConfirmation
        $matches = @(Get-CimInstance Win32_Process -Filter "Name='msiexec.exe'" |
            Where-Object {
                [uint32]$_.SessionId -eq $ExpectedSessionId `
                    -and -not [string]::IsNullOrWhiteSpace([string]$_.CommandLine) `
                    -and $_.CommandLine.IndexOf(
                        $PackagePath,
                        [StringComparison]::OrdinalIgnoreCase) -ge 0
            })
        if ($matches.Count -gt 1) {
            throw ('Multiple msiexec processes reference the exact candidate package: ' +
                   "$($matches.ProcessId -join ', ').")
        }
        if ($matches.Count -eq 1) {
            $process = $matches[0]
            if ($ExpectedParentProcessId -ne 0 `
                -and [uint32]$process.ParentProcessId -ne $ExpectedParentProcessId) {
                throw ("The candidate msiexec parent is $($process.ParentProcessId), not " +
                       "the LocalSystem service $ExpectedParentProcessId.")
            }
            $commandLine = [string]$process.CommandLine
            if ($RequireSilent `
                -and ($commandLine -cnotmatch '(?i)(?:^|\s)/qn(?:\s|$)' `
                    -or $commandLine -cnotmatch '(?i)(?:^|\s)/norestart(?:\s|$)')) {
                throw "The LocalSystem candidate installer was not launched with /qn /norestart."
            }
            $ownerResult = Invoke-CimMethod -InputObject $process -MethodName GetOwner
            $owner = if ([string]::IsNullOrWhiteSpace([string]$ownerResult.Domain)) {
                [string]$ownerResult.User
            } else {
                "$($ownerResult.Domain)\$($ownerResult.User)"
            }
            if ($RequireSilent `
                -and ($ownerResult.ReturnValue -ne 0 `
                    -or $ownerResult.User -cne 'SYSTEM' `
                    -or $ownerResult.Domain -cne 'NT AUTHORITY')) {
                throw "The silent candidate installer runs as '$owner', not NT AUTHORITY\SYSTEM."
            }
            return [pscustomobject]@{
                ProcessId = [uint32]$process.ProcessId
                ParentProcessId = [uint32]$process.ParentProcessId
                SessionId = [uint32]$process.SessionId
                CreationDate = $process.CreationDate
                ExecutablePath = $process.ExecutablePath
                CommandLine = $commandLine
                Owner = $owner
            }
        }
        if ([DateTime]::UtcNow -ge $nextProgress) {
            Write-TestProgress (
                "Waiting for msiexec in session $ExpectedSessionId to launch the exact candidate path.")
            $nextProgress = [DateTime]::UtcNow.AddSeconds(15)
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "No msiexec in session $ExpectedSessionId launched the exact candidate path within $Timeout."
}

function Start-InstallerProcessMonitor {
    Remove-Item -LiteralPath $installerMonitorReady, $installerMonitorStop `
        -Force -ErrorAction SilentlyContinue
    $script:installerMonitorJob = Start-Job -ScriptBlock {
        param($ReadyPath, $StopPath, $EventsPath)

        $ErrorActionPreference = 'Stop'
        $startSource = 'ResticPalInstallerProcessStart'
        $stopSource = 'ResticPalInstallerProcessStop'

        function Write-ProcessEvent($Record) {
            $line = $Record | ConvertTo-Json -Depth 5 -Compress
            [IO.File]::AppendAllText(
                $EventsPath,
                "$line`r`n",
                [Text.UTF8Encoding]::new($false))
        }

        try {
            Register-WmiEvent `
                -Query ("SELECT * FROM Win32_ProcessStartTrace WHERE " +
                        "ProcessName='cmd.exe' OR ProcessName='consent.exe' OR " +
                        "ProcessName='msiexec.exe' OR ProcessName='resticpal-ui.exe'") `
                -SourceIdentifier $startSource | Out-Null
            Register-WmiEvent `
                -Query ("SELECT * FROM Win32_ProcessStopTrace WHERE " +
                        "ProcessName='cmd.exe' OR ProcessName='consent.exe' OR " +
                        "ProcessName='msiexec.exe' OR ProcessName='resticpal-ui.exe'") `
                -SourceIdentifier $stopSource | Out-Null

            # Subscribe before taking the snapshot so a short-lived consent or
            # installer process cannot fall between observation and event
            # registration. Duplicate snapshot/event records are harmless: the
            # release gate separately requires zero consent starts.
            $initialProcesses = @(Get-CimInstance Win32_Process |
                Where-Object Name -in @(
                    'cmd.exe', 'consent.exe', 'msiexec.exe', 'resticpal-ui.exe') |
                ForEach-Object {
                    [ordered]@{
                        name = $_.Name
                        process_id = [uint32]$_.ProcessId
                        parent_process_id = [uint32]$_.ParentProcessId
                        session_id = [uint32]$_.SessionId
                        creation_date = if ($null -eq $_.CreationDate) {
                            $null
                        } else {
                            ([DateTimeOffset]$_.CreationDate).ToUniversalTime().ToString('o')
                        }
                        executable_path = $_.ExecutablePath
                        command_line = $_.CommandLine
                    }
                })
            Write-ProcessEvent ([ordered]@{
                observed_at = [DateTimeOffset]::UtcNow.ToString('o')
                event = 'initial_snapshot'
                processes = $initialProcesses
            })
            [IO.File]::WriteAllText($ReadyPath, 'ready', [Text.UTF8Encoding]::new($false))

            $quietPollsAfterStop = 0
            while ($quietPollsAfterStop -lt 2) {
                $event = Wait-Event -Timeout 1
                if ($null -eq $event) {
                    if (Test-Path -LiteralPath $StopPath -PathType Leaf) {
                        $quietPollsAfterStop++
                    } else {
                        $quietPollsAfterStop = 0
                    }
                    continue
                }
                $quietPollsAfterStop = 0
                try {
                    $trace = $event.SourceEventArgs.NewEvent
                    $record = [ordered]@{
                        observed_at = [DateTimeOffset]::UtcNow.ToString('o')
                        event = if ($event.SourceIdentifier -ceq $startSource) {
                            'process_start'
                        } else {
                            'process_stop'
                        }
                        name = [string]$trace.ProcessName
                        process_id = [uint32]$trace.ProcessID
                        parent_process_id = [uint32]$trace.ParentProcessID
                        session_id = [uint32]$trace.SessionID
                        exit_status = if ($event.SourceIdentifier -ceq $stopSource) {
                            [uint32]$trace.ExitStatus
                        } else {
                            $null
                        }
                        executable_path = $null
                        command_line = $null
                    }
                    if ($event.SourceIdentifier -ceq $startSource) {
                        $process = Get-CimInstance Win32_Process `
                            -Filter "ProcessId=$($record.process_id)" `
                            -ErrorAction SilentlyContinue
                        if ($null -ne $process) {
                            $record.executable_path = $process.ExecutablePath
                            $record.command_line = $process.CommandLine
                        }
                    }
                    Write-ProcessEvent $record
                } finally {
                    Remove-Event -EventIdentifier $event.EventIdentifier -ErrorAction SilentlyContinue
                }
            }
            Write-ProcessEvent ([ordered]@{
                observed_at = [DateTimeOffset]::UtcNow.ToString('o')
                event = 'monitor_drained'
            })
        } catch {
            Write-ProcessEvent ([ordered]@{
                observed_at = [DateTimeOffset]::UtcNow.ToString('o')
                event = 'monitor_error'
                message = $_.Exception.Message
            })
            if (-not (Test-Path -LiteralPath $ReadyPath -PathType Leaf)) {
                [IO.File]::WriteAllText($ReadyPath, 'failed', [Text.UTF8Encoding]::new($false))
            }
        } finally {
            Unregister-Event -SourceIdentifier $startSource -ErrorAction SilentlyContinue
            Unregister-Event -SourceIdentifier $stopSource -ErrorAction SilentlyContinue
        }
    } -ArgumentList $installerMonitorReady, $installerMonitorStop, $installerProcessEvents

    Wait-Path $installerMonitorReady ([TimeSpan]::FromSeconds(15))
    if ((Get-Content -LiteralPath $installerMonitorReady -Raw) -cne 'ready') {
        throw 'The installer process monitor could not subscribe to Windows process events.'
    }
    if ($script:installerMonitorJob.State -ne 'Running') {
        throw "The installer process monitor exited after reporting ready: $($script:installerMonitorJob.State)."
    }
}

function Stop-InstallerProcessMonitor([switch] $RequireHealthy) {
    if ($null -eq $script:installerMonitorJob) {
        return
    }

    $initialState = [string]$script:installerMonitorJob.State
    if ($initialState -ceq 'Running') {
        [IO.File]::WriteAllText(
            $installerMonitorStop,
            'stop',
            [Text.UTF8Encoding]::new($false))
    }
    $null = Wait-Job -Job $script:installerMonitorJob -Timeout 5
    $stopTimedOut = $script:installerMonitorJob.State -notin @('Completed', 'Failed', 'Stopped')
    if ($stopTimedOut) {
        Stop-Job -Job $script:installerMonitorJob -ErrorAction SilentlyContinue
    }
    $finalState = [string]$script:installerMonitorJob.State
    $jobReason = $script:installerMonitorJob.JobStateInfo.Reason
    Receive-Job -Job $script:installerMonitorJob -ErrorAction Continue 2>&1 |
        Out-String |
        Set-Content -LiteralPath (
            Join-Path $localArtifactRoot 'installer-process-monitor-job.log') -Encoding UTF8
    $monitorErrors = @()
    $drainMarkers = @()
    if (Test-Path -LiteralPath $installerProcessEvents -PathType Leaf) {
        $monitorRecords = @(Get-Content -LiteralPath $installerProcessEvents |
            Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
            ForEach-Object { $_ | ConvertFrom-Json })
        $monitorErrors = @($monitorRecords | Where-Object event -CEQ 'monitor_error')
        $drainMarkers = @($monitorRecords | Where-Object event -CEQ 'monitor_drained')
    }
    Remove-Job -Job $script:installerMonitorJob -Force -ErrorAction SilentlyContinue
    $script:installerMonitorJob = $null

    if ($RequireHealthy -and (
            $initialState -cne 'Running' -or
            $stopTimedOut -or
            $finalState -cne 'Completed' -or
            $null -ne $jobReason -or
            $monitorErrors.Count -ne 0 -or
            $drainMarkers.Count -ne 1)) {
        $reasonText = if ($null -eq $jobReason) { '' } else { $jobReason.Message }
        throw ('The installer process monitor did not remain healthy through its drain barrier ' +
               "(initial=$initialState, final=$finalState, timed_out=$stopTimedOut, " +
               "errors=$($monitorErrors.Count), drains=$($drainMarkers.Count), " +
               "reason=$reasonText).")
    }
}

function Assert-NoAutomaticConsentEvent {
    if (-not $automaticMode) {
        return
    }
    if (-not (Test-Path -LiteralPath $installerProcessEvents -PathType Leaf)) {
        throw 'The automatic update process trace is missing.'
    }
    $records = @(Get-Content -LiteralPath $installerProcessEvents |
        Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
        ForEach-Object { $_ | ConvertFrom-Json })
    $consentProcessIds = @($records | ForEach-Object {
        if ($_.event -ceq 'process_start' -and $_.name -ieq 'consent.exe') {
            [uint32]$_.process_id
        } elseif ($_.event -ceq 'initial_snapshot') {
            $_.processes |
                Where-Object name -ieq 'consent.exe' |
                ForEach-Object { [uint32]$_.process_id }
        }
    } | Sort-Object -Unique)
    $verification.consent_process_starts = $consentProcessIds.Count
    if ($consentProcessIds.Count -ne 0) {
        throw 'Automatic mode started a UAC consent process during the update.'
    }
    $automaticUiProcessIds = @($records | ForEach-Object {
        if ($_.event -ceq 'process_start' -and $_.name -ieq 'resticpal-ui.exe') {
            [uint32]$_.process_id
        }
    } | Sort-Object -Unique)
    $verification.automatic_ui_process_starts = $automaticUiProcessIds.Count
    if ($automaticUiProcessIds.Count -ne 0) {
        throw 'Automatic mode started an interactive resticpal UI process during the update.'
    }
}

function Export-InstallerDiagnostics {
    $processes = @(Get-CimInstance Win32_Process |
        Where-Object Name -in @(
            'cmd.exe',
            'consent.exe',
            'msiexec.exe',
            'resticpal-ui.exe',
            'resticpal-service.exe',
            'resticpal-tray.exe'
        ) |
        ForEach-Object {
            [ordered]@{
                name = $_.Name
                process_id = [uint32]$_.ProcessId
                parent_process_id = [uint32]$_.ParentProcessId
                session_id = [uint32]$_.SessionId
                creation_date = if ($null -eq $_.CreationDate) {
                    $null
                } else {
                    ([DateTimeOffset]$_.CreationDate).ToUniversalTime().ToString('o')
                }
                executable_path = $_.ExecutablePath
                command_line = $_.CommandLine
            }
        })
    @($processes) | ConvertTo-Json -Depth 4 |
        Set-Content -LiteralPath (
            Join-Path $localArtifactRoot 'installer-process-final.json') -Encoding UTF8

    if ($null -eq $installerRequestedAt) {
        return
    }
    $knownPaths = @($knownInstallerCommandPaths | ForEach-Object { $_.ToLowerInvariant() })
    $commands = @(Get-ChildItem -LiteralPath $env:TEMP `
        -File `
        -Recurse `
        -Force `
        -ErrorAction SilentlyContinue |
        Where-Object {
            $_.Extension -ieq '.cmd' `
                -and $knownPaths -notcontains $_.FullName.ToLowerInvariant() `
                -and $_.LastWriteTime -ge $installerRequestedAt.AddSeconds(-5)
        } |
        Sort-Object LastWriteTime)
    $records = @()
    $index = 0
    foreach ($command in $commands) {
        $index++
        $exportName = "netsparkle-installer-$index.cmd.txt"
        Copy-Item -LiteralPath $command.FullName `
            -Destination (Join-Path $localArtifactRoot $exportName) `
            -Force
        $records += [ordered]@{
            path = $command.FullName
            exported_name = $exportName
            length = [uint64]$command.Length
            sha256 = (Get-FileHash -LiteralPath $command.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            creation_time = ([DateTimeOffset]$command.CreationTime).ToUniversalTime().ToString('o')
            last_write_time = ([DateTimeOffset]$command.LastWriteTime).ToUniversalTime().ToString('o')
            references_candidate = (Get-Content -LiteralPath $command.FullName -Raw).IndexOf(
                $stagedUpdate.path,
                [StringComparison]::OrdinalIgnoreCase) -ge 0
        }
    }
    @($records) | ConvertTo-Json -Depth 4 |
        Set-Content -LiteralPath (
            Join-Path $localArtifactRoot 'netsparkle-launchers.json') -Encoding UTF8

    if ($null -ne $stagedUpdate -and (Test-Path -LiteralPath $stagedUpdate.path -PathType Leaf)) {
        $stagedFile = Get-Item -LiteralPath $stagedUpdate.path
        [ordered]@{
            path = $stagedFile.FullName
            length = [uint64]$stagedFile.Length
            sha256 = (Get-FileHash -LiteralPath $stagedFile.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            still_present = $true
        } | ConvertTo-Json |
            Set-Content -LiteralPath (
                Join-Path $localArtifactRoot 'staged-update-final.json') -Encoding UTF8
    }
}

function Wait-ServiceRunning([TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $service = Get-Service -Name ResticPal -ErrorAction SilentlyContinue
        if ($null -ne $service `
            -and $service.Status -eq [ServiceProcess.ServiceControllerStatus]::Running) {
            $configuration = Get-CimInstance Win32_Service -Filter "Name='ResticPal'"
            if ($configuration.StartName -cne 'LocalSystem') {
                throw "The upgraded service uses unexpected identity $($configuration.StartName)."
            }
            return $configuration
        }
        Start-Sleep -Milliseconds 500
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'Timed out waiting for the upgraded ResticPal service.'
}

function Wait-ProcessExit([Diagnostics.Process] $Process, [TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    $nextProgress = [DateTime]::UtcNow.AddSeconds(15)
    do {
        try {
            $Process.Refresh()
            if ($Process.HasExited) {
                return
            }
        } catch [ArgumentException] {
            return
        }
        if ([DateTime]::UtcNow -ge $nextProgress) {
            Write-TestProgress (
                "Waiting for process $($Process.ProcessName) $($Process.Id) to exit.")
            $nextProgress = [DateTime]::UtcNow.AddSeconds(15)
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Process $($Process.ProcessName) $($Process.Id) did not exit during the upgrade."
}

function Wait-SingleReplacementTray([int] $PublishedProcessId, [TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    $stableSamples = 0
    do {
        $trays = @(Get-Process -Name 'resticpal-tray' -ErrorAction SilentlyContinue |
            Where-Object SessionId -eq $interactiveSessionId)
        if ($trays.Count -eq 1 -and $trays[0].Id -ne $PublishedProcessId) {
            $stableSamples++
            if ($stableSamples -ge 4) {
                return $trays[0]
            }
        } else {
            $stableSamples = 0
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    $observed = @(Get-Process -Name 'resticpal-tray' -ErrorAction SilentlyContinue |
        Where-Object SessionId -eq $interactiveSessionId |
        ForEach-Object Id)
    throw ("Timed out waiting for exactly one replacement tray process; " +
           "observed process IDs: $($observed -join ', ').")
}

function Find-StagedUpdate(
    [TimeSpan] $Timeout,
    [string] $SearchRoot = $env:TEMP,
    [string] $ExpectedPath = ''
) {
    $expectedLength = (Get-Item -LiteralPath $candidateMsi).Length
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        Assert-NoAutomaticUpdateConfirmation
        $files = if (-not [string]::IsNullOrWhiteSpace($ExpectedPath)) {
            @(Get-Item -LiteralPath $ExpectedPath -Force -ErrorAction SilentlyContinue |
                Where-Object Length -eq $expectedLength)
        } else {
            @(Get-ChildItem -LiteralPath $SearchRoot `
                -File `
                -Recurse `
                -Force `
                -ErrorAction SilentlyContinue |
                Where-Object Length -eq $expectedLength |
                Sort-Object LastWriteTimeUtc -Descending)
        }
        $matches = @()
        foreach ($file in $files) {
            try {
                $fileHash = Get-FileHash `
                    -LiteralPath $file.FullName -Algorithm SHA256 -ErrorAction Stop
                $hash = $fileHash.Hash.ToLowerInvariant()
            } catch [IO.IOException] {
                continue
            }
            if ($hash -ceq $ExpectedCandidateSha256.ToLowerInvariant()) {
                $matches += [pscustomobject]@{
                    path = $file.FullName
                    file_name = $file.Name
                    extension = [IO.Path]::GetExtension($file.FullName)
                    length = $file.Length
                    sha256 = $hash
                    same_length_files_examined = $files.Count
                    hash_matches = 1
                    expected_path_match = [string]::IsNullOrWhiteSpace($ExpectedPath) -or
                        [string]::Equals(
                            $file.FullName,
                            $ExpectedPath,
                            [StringComparison]::OrdinalIgnoreCase)
                }
            }
        }
        if ($matches.Count -gt 1) {
            throw ("Multiple temporary files matched the exact candidate MSI, so the " +
                   "NetSparkle staging path is ambiguous: $($matches.path -join ', ')")
        }
        if ($matches.Count -eq 1) {
            return $matches[0]
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "No staged file below $SearchRoot matched the signed candidate MSI length and SHA-256."
}

function Start-LocalUpdateOrigin {
    $redirectUrl = 'https://objects.githubusercontent.com/release-assets/resticpal/opaque-update-object'
    $script:serverJob = Start-Job -ScriptBlock {
        param(
            $CertificateThumbprint,
            $AppCastPath,
            $AppCastSignaturePath,
            $CandidateMsiPath,
            $EnclosureUrl,
            $RedirectUrl,
            $LogPath,
            $ReadyPath,
            $FeedGatePath,
            $ProbeAppCastPath,
            $ProbeAppCastSignaturePath,
            $ProbePayloadPath,
            $ProbePayloadUrl
        )

        $ErrorActionPreference = 'Stop'
        $certificate = Get-Item -LiteralPath "Cert:\LocalMachine\My\$CertificateThumbprint"
        $listener = [Net.Sockets.TcpListener]::new([Net.IPAddress]::Loopback, 443)

        function Write-OriginLog([string] $Message) {
            $line = "{0:o} {1}`r`n" -f [DateTimeOffset]::UtcNow, $Message
            [IO.File]::AppendAllText(
                $LogPath,
                $line,
                [Text.UTF8Encoding]::new($false))
        }

        function Write-Response(
            [Net.Security.SslStream] $Stream,
            [string] $Method,
            [int] $StatusCode,
            [string] $Reason,
            [hashtable] $Headers,
            [byte[]] $Body
        ) {
            $allHeaders = [ordered]@{
                'Connection' = 'close'
                'Cache-Control' = 'no-store'
            }
            foreach ($key in $Headers.Keys) {
                $allHeaders[$key] = $Headers[$key]
            }
            $allHeaders['Content-Length'] = $Body.Length
            $headerText = "HTTP/1.1 $StatusCode $Reason`r`n"
            foreach ($key in $allHeaders.Keys) {
                $headerText += "$key`: $($allHeaders[$key])`r`n"
            }
            $headerText += "`r`n"
            $headerBytes = [Text.Encoding]::ASCII.GetBytes($headerText)
            $Stream.Write($headerBytes, 0, $headerBytes.Length)
            if ($Method -cne 'HEAD' -and $Body.Length -gt 0) {
                $Stream.Write($Body, 0, $Body.Length)
            }
            $Stream.Flush()
            Write-OriginLog (
                "response completed: $Method $StatusCode, $($Body.Length) body bytes")
        }

        try {
            $listener.Start()
            [IO.File]::WriteAllText($ReadyPath, 'ready', [Text.UTF8Encoding]::new($false))
            Write-OriginLog 'listening on 127.0.0.1:443'
            while ($true) {
                if (-not $listener.Pending()) {
                    Start-Sleep -Milliseconds 100
                    continue
                }
                $client = $listener.AcceptTcpClient()
                $stream = $null
                $reader = $null
                try {
                    $stream = [Net.Security.SslStream]::new($client.GetStream(), $false)
                    $stream.AuthenticateAsServer(
                        $certificate,
                        $false,
                        [Security.Authentication.SslProtocols]::Tls12,
                        $false)
                    $reader = [IO.StreamReader]::new(
                        $stream,
                        [Text.Encoding]::ASCII,
                        $false,
                        4096,
                        $true)
                    $requestLine = $reader.ReadLine()
                    if ([string]::IsNullOrWhiteSpace($requestLine)) {
                        continue
                    }
                    $parts = $requestLine.Split(' ')
                    if ($parts.Count -lt 2) {
                        continue
                    }
                    $method = $parts[0].ToUpperInvariant()
                    $requestTarget = $parts[1]
                    $hostHeader = $null
                    $userAgent = $null
                    do {
                        $line = $reader.ReadLine()
                        if ($line -like 'Host:*') {
                            $hostHeader = $line.Substring(5).Trim().Split(':')[0]
                        } elseif ($line -like 'User-Agent:*') {
                            $userAgent = $line.Substring(11).Trim()
                        }
                    } while (-not [string]::IsNullOrEmpty($line))
                    if ([string]::IsNullOrWhiteSpace($hostHeader)) {
                        throw 'The request did not contain a Host header.'
                    }
                    $requestUri = [Uri]::new("https://$hostHeader$requestTarget")
                    $absoluteUrl = "https://$hostHeader$($requestUri.AbsolutePath)"
                    Write-OriginLog "$method $absoluteUrl"

                    $legacyFeedRequest = (
                        ($hostHeader -ceq 'updates.resticpal.com' -and
                         $requestUri.AbsolutePath -in @(
                             '/appcast.xml', '/appcast.xml.signature')) -or
                        ($hostHeader -ceq 'github.com' -and
                         $requestUri.AbsolutePath -in @(
                             '/theatrus/resticpal/releases/latest/download/appcast.xml',
                             '/theatrus/resticpal/releases/latest/download/appcast.xml.signature')))
                    $v2FeedRequest = (
                        ($hostHeader -ceq 'updates.resticpal.com' -and
                         $requestUri.AbsolutePath -in @(
                             '/appcast-v2.xml', '/appcast-v2.xml.signature')) -or
                        ($hostHeader -ceq 'github.com' -and
                         $requestUri.AbsolutePath -in @(
                             '/theatrus/resticpal/releases/latest/download/appcast-v2.xml',
                             '/theatrus/resticpal/releases/latest/download/appcast-v2.xml.signature')))
                    $candidateFeedRequest = (
                        $legacyFeedRequest -or
                        ($v2FeedRequest -and
                         [string]::IsNullOrWhiteSpace($ProbeAppCastPath)))

                    if (-not (Test-Path -LiteralPath $FeedGatePath -PathType Leaf) -and
                        $candidateFeedRequest) {
                        Write-OriginLog 'signed feed held until automatic-install setup completes'
                        Write-Response $stream $method 503 'Service Unavailable' `
                            @{ 'Content-Type' = 'text/plain'; 'Retry-After' = '1' } `
                            ([Text.Encoding]::UTF8.GetBytes('feed not enabled'))
                    } elseif ($legacyFeedRequest -and
                        $requestUri.AbsolutePath.EndsWith(
                            'appcast.xml', [StringComparison]::Ordinal)) {
                        Write-OriginLog "served signed appcast to $userAgent"
                        $body = [IO.File]::ReadAllBytes($AppCastPath)
                        Write-Response $stream $method 200 'OK' `
                            @{ 'Content-Type' = 'text/xml; charset=utf-8' } $body
                    } elseif ($legacyFeedRequest -and
                        $requestUri.AbsolutePath.EndsWith(
                            'appcast.xml.signature', [StringComparison]::Ordinal)) {
                        $body = [IO.File]::ReadAllBytes($AppCastSignaturePath)
                        Write-Response $stream $method 200 'OK' `
                            @{ 'Content-Type' = 'application/octet-stream' } $body
                    } elseif ($v2FeedRequest -and
                        $requestUri.AbsolutePath.EndsWith(
                            'appcast-v2.xml', [StringComparison]::Ordinal)) {
                        $servingProbe = -not [string]::IsNullOrWhiteSpace($ProbeAppCastPath)
                        Write-OriginLog $(if ($servingProbe) {
                            "served candidate probe appcast to $userAgent"
                        } else {
                            "served signed appcast to $userAgent"
                        })
                        $body = [IO.File]::ReadAllBytes($(if ($servingProbe) {
                            $ProbeAppCastPath
                        } else {
                            $AppCastPath
                        }))
                        Write-Response $stream $method 200 'OK' `
                            @{ 'Content-Type' = 'text/xml; charset=utf-8' } $body
                    } elseif ($v2FeedRequest -and
                        $requestUri.AbsolutePath.EndsWith(
                            'appcast-v2.xml.signature', [StringComparison]::Ordinal)) {
                        if (-not [string]::IsNullOrWhiteSpace($ProbeAppCastSignaturePath)) {
                            Write-OriginLog "served candidate probe appcast signature to $userAgent"
                        }
                        $body = [IO.File]::ReadAllBytes($(
                            if ([string]::IsNullOrWhiteSpace($ProbeAppCastSignaturePath)) {
                                $AppCastSignaturePath
                            } else {
                                $ProbeAppCastSignaturePath
                            }))
                        Write-Response $stream $method 200 'OK' `
                            @{ 'Content-Type' = 'application/octet-stream' } $body
                    } elseif ($absoluteUrl -ceq $EnclosureUrl `
                        -and $hostHeader -ceq 'github.com') {
                        Write-OriginLog "redirect $RedirectUrl"
                        Write-Response $stream $method 302 'Found' `
                            @{ 'Location' = $RedirectUrl } ([byte[]]::new(0))
                    } elseif ($absoluteUrl -ceq $EnclosureUrl `
                        -or $absoluteUrl -ceq $RedirectUrl) {
                        # Deliberately omit Content-Disposition. The opaque
                        # redirect recreates the filename loss that broke old
                        # NetSparkle updates.
                        $body = [IO.File]::ReadAllBytes($CandidateMsiPath)
                        Write-Response $stream $method 200 'OK' `
                            @{ 'Content-Type' = 'application/octet-stream' } $body
                    } elseif (-not [string]::IsNullOrWhiteSpace($ProbePayloadPath) `
                        -and $absoluteUrl -ceq $ProbePayloadUrl) {
                        Write-OriginLog "served candidate probe payload to $userAgent"
                        $body = [IO.File]::ReadAllBytes($ProbePayloadPath)
                        Write-Response $stream $method 200 'OK' `
                            @{ 'Content-Type' = 'application/octet-stream' } $body
                    } else {
                        Write-Response $stream $method 404 'Not Found' `
                            @{ 'Content-Type' = 'text/plain' } `
                            ([Text.Encoding]::UTF8.GetBytes('not found'))
                    }
                } catch {
                    Write-OriginLog "request failed: $($_.Exception.Message)"
                } finally {
                    Write-OriginLog 'request cleanup started'
                    if ($null -ne $reader) {
                        $reader.Dispose()
                        Write-OriginLog 'request reader disposed'
                    }
                    if ($null -ne $stream) {
                        $stream.Dispose()
                        Write-OriginLog 'request TLS stream disposed'
                    }
                    $client.Dispose()
                    Write-OriginLog 'request TCP client disposed'
                }
            }
        } finally {
            $listener.Stop()
        }
    } -ArgumentList @(
        $certificateThumbprint,
        $appCast,
        $appCastSignature,
        $candidateMsi,
        $EnclosureUrl,
        $redirectUrl,
        $originLog,
        $originReady,
        $originFeedGate,
        $(if ($bridgeMode) { $probeAppCast } else { $null }),
        $(if ($bridgeMode) { $probeAppCastSignature } else { $null }),
        $(if ($bridgeMode) { $probePayload } else { $null }),
        $(if ($bridgeMode) { $ExpectedProbePayloadUrl } else { $null })
    )
    Wait-Path $originReady ([TimeSpan]::FromSeconds(15))
    return $redirectUrl
}

function Export-GuestDiagnostics {
    try {
        Export-InstallerDiagnostics
    } catch {
        $_ | Out-String | Set-Content -LiteralPath (
            Join-Path $localArtifactRoot 'installer-diagnostics-error.txt') -Encoding UTF8
    }
    $eventStart = $startedAt.LocalDateTime.AddMinutes(-1)
    $events = @(
        Get-WinEvent -FilterHashtable @{
            LogName = 'Application'
            ProviderName = 'MsiInstaller'
            StartTime = $eventStart
        } -ErrorAction SilentlyContinue
        Get-WinEvent -FilterHashtable @{
            LogName = 'System'
            ProviderName = 'Service Control Manager'
            StartTime = $eventStart
        } -ErrorAction SilentlyContinue
    ) | Sort-Object TimeCreated |
        Select-Object TimeCreated, LogName, ProviderName, Id, LevelDisplayName, Message
    @($events) | ConvertTo-Json -Depth 4 |
        Set-Content -LiteralPath (Join-Path $localArtifactRoot 'windows-events.json') -Encoding UTF8
    Get-NetTCPConnection -ErrorAction SilentlyContinue |
        Where-Object RemotePort -eq 443 |
        Select-Object State, LocalAddress, LocalPort, RemoteAddress, RemotePort, OwningProcess |
        ConvertTo-Json -Depth 3 |
        Set-Content -LiteralPath (Join-Path $localArtifactRoot 'tcp-443.json') -Encoding UTF8
    foreach ($request in @(
        @{ Name = 'service-status'; Command = @{ type = 'get_status' } },
        @{ Name = 'service-update-settings'; Command = @{ type = 'get_update_settings' } },
        @{ Name = 'service-diagnostics'; Command = @{ type = 'get_diagnostics'; limit = 100 } }
    )) {
        try {
            Invoke-ResticPalRequest $request.Command |
                ConvertTo-Json -Depth 12 |
                Set-Content -LiteralPath (
                    Join-Path $localArtifactRoot "$($request.Name).json") -Encoding UTF8
        } catch {
            $_ | Out-String | Set-Content -LiteralPath (
                Join-Path $localArtifactRoot "$($request.Name)-error.txt") -Encoding UTF8
        }
    }
}

try {
    New-Item -ItemType Directory -Path $localArtifactRoot -Force | Out-Null
    Remove-Item -LiteralPath $originFeedGate -Force -ErrorAction SilentlyContinue
    Copy-Item -LiteralPath $PublishedClientMsiPath -Destination $publishedMsi
    Copy-Item -LiteralPath $CandidateMsiPath -Destination $candidateMsi
    Copy-Item -LiteralPath $AppCastPath -Destination $appCast
    Copy-Item -LiteralPath $AppCastSignaturePath -Destination $appCastSignature
    if ($bridgeMode) {
        Copy-Item -LiteralPath $ProbeAppCastPath -Destination $probeAppCast
        Copy-Item -LiteralPath $ProbeAppCastSignaturePath -Destination $probeAppCastSignature
        Copy-Item -LiteralPath $ProbePayloadPath -Destination $probePayload
    }
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'The Windows Sandbox update test must run as an administrator.'
    }
    if ($null -ne (Get-Service -Name ResticPal -ErrorAction SilentlyContinue)) {
        throw 'A ResticPal service already exists in the disposable guest.'
    }

    $publishedHash = (Get-FileHash -LiteralPath $publishedMsi -Algorithm SHA256).Hash.ToLowerInvariant()
    $candidateHash = (Get-FileHash -LiteralPath $candidateMsi -Algorithm SHA256).Hash.ToLowerInvariant()
    $appCastHash = (Get-FileHash -LiteralPath $appCast -Algorithm SHA256).Hash.ToLowerInvariant()
    $appCastSignatureHash = (
        Get-FileHash -LiteralPath $appCastSignature -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($publishedHash -cne $ExpectedPublishedSha256.ToLowerInvariant()) {
        throw 'The copied published-client MSI hash changed before guest installation.'
    }
    if ([uint64](Get-Item -LiteralPath $publishedMsi).Length -ne $PublishedReleaseAssetLength) {
        throw 'The copied published-client MSI length changed before guest installation.'
    }
    if ($candidateHash -cne $ExpectedCandidateSha256.ToLowerInvariant()) {
        throw 'The copied candidate MSI hash changed before guest installation.'
    }
    if ($appCastHash -cne $ExpectedAppCastSha256.ToLowerInvariant() `
        -or $appCastSignatureHash -cne $ExpectedAppCastSignatureSha256.ToLowerInvariant()) {
        throw 'The copied signed appcast pair changed before guest use.'
    }
    if ($bridgeMode) {
        $probeAppCastHash = (
            Get-FileHash -LiteralPath $probeAppCast -Algorithm SHA256).Hash.ToLowerInvariant()
        $probeAppCastSignatureHash = (
            Get-FileHash -LiteralPath $probeAppCastSignature -Algorithm SHA256).Hash.ToLowerInvariant()
        $probePayloadHash = (
            Get-FileHash -LiteralPath $probePayload -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($probeAppCastHash -cne $ExpectedProbeAppCastSha256.ToLowerInvariant() -or
            $probeAppCastSignatureHash -cne $ExpectedProbeAppCastSignatureSha256.ToLowerInvariant() -or
            $probePayloadHash -cne $ExpectedProbePayloadSha256.ToLowerInvariant() -or
            [uint64](Get-Item -LiteralPath $probePayload).Length -ne $ExpectedProbePayloadLength) {
            throw 'The copied candidate-tray probe bytes changed before guest use.'
        }
    }
    $verification.published_sha256 = $publishedHash
    $verification.candidate_sha256 = $candidateHash

    $originalHostsBytes = [IO.File]::ReadAllBytes($hostsPath)
    $certificate = New-SelfSignedCertificate `
        -DnsName @(
            'updates.resticpal.com',
            'github.com',
            'objects.githubusercontent.com'
        ) `
        -CertStoreLocation 'Cert:\LocalMachine\My' `
        -KeyAlgorithm RSA `
        -KeyLength 2048 `
        -HashAlgorithm SHA256 `
        -NotAfter ([DateTime]::Now.AddDays(2))
    $certificateThumbprint = $certificate.Thumbprint
    $rootStore = [Security.Cryptography.X509Certificates.X509Store]::new(
        [Security.Cryptography.X509Certificates.StoreName]::Root,
        [Security.Cryptography.X509Certificates.StoreLocation]::LocalMachine)
    try {
        $rootStore.Open([Security.Cryptography.X509Certificates.OpenFlags]::ReadWrite)
        $rootStore.Add($certificate)
    } finally {
        $rootStore.Close()
    }
    [IO.File]::AppendAllText(
        $hostsPath,
        "`r`n127.0.0.1 updates.resticpal.com github.com objects.githubusercontent.com`r`n",
        [Text.Encoding]::ASCII)
    if (-not $automaticMode) {
        [IO.File]::WriteAllText(
            $originFeedGate,
            'enabled',
            [Text.UTF8Encoding]::new($false))
    }
    $redirectUrl = Start-LocalUpdateOrigin
    $verification.redirect_url = $redirectUrl

    $onboardingMarker = Join-Path $env:LOCALAPPDATA 'resticpal\onboarding-shown-v1'
    New-Item -ItemType Directory -Path (Split-Path -Parent $onboardingMarker) -Force | Out-Null
    [IO.File]::WriteAllText($onboardingMarker, 'update qualification', [Text.UTF8Encoding]::new($false))

    Write-TestProgress "Installing the actually published resticpal $ExpectedPublishedVersion client."
    $timing.published_install_requested_at = [DateTimeOffset]::UtcNow.ToString('o')
    Invoke-Installer "/i `"$publishedMsi`" /qn /norestart /l*v `"$baselineInstallLog`"" `
        'Published-client installation'
    $timing.published_install_completed_at = [DateTimeOffset]::UtcNow.ToString('o')
    Wait-InstalledVersion $ExpectedPublishedVersion ([TimeSpan]::FromSeconds(45))
    $baselineService = Wait-ServiceRunning ([TimeSpan]::FromSeconds(45))
    $baselineServiceProcessId = [uint32]$baselineService.ProcessId
    if ($baselineServiceProcessId -eq 0) {
        throw 'The published-client service is Running but has no process ID.'
    }
    $baselineServiceProcess = Get-Process -Id $baselineServiceProcessId -ErrorAction Stop
    $installedTray = Wait-InteractiveProcess 'resticpal-tray' ([TimeSpan]::FromSeconds(45))
    $verification.baseline_service_process_id = $baselineServiceProcessId

    $expectedPublishedFileVersion = "$ExpectedPublishedVersion.0"
    foreach ($payload in @(
        @{ Name = 'service'; FileName = 'resticpal-service.exe' },
        @{ Name = 'tray'; FileName = 'resticpal-tray.exe' },
        @{ Name = 'ui'; FileName = 'resticpal-ui.exe' }
    )) {
        $publishedFileVersion = (Get-Item -LiteralPath (
            Join-Path $installRoot $payload.FileName)).VersionInfo.FileVersion
        if ($publishedFileVersion -cne $expectedPublishedFileVersion) {
            throw ("The published $($payload.Name) file version is $publishedFileVersion; " +
                   "expected $expectedPublishedFileVersion.")
        }
        $verification["published_$($payload.Name)_file_version"] = $publishedFileVersion
    }

    if ($automaticMode) {
        Stop-Process -Id $installedTray.Id -Force
        Wait-ProcessExit $installedTray ([TimeSpan]::FromSeconds(15))
    } else {
        $baselineTray = $installedTray
        $baselineTrayId = $baselineTray.Id
        $verification.published_tray_process_id = $baselineTrayId
    }

    # The pre-created first-run marker keeps onboarding from racing this
    # focused update test. Close any unexpected UI, then open updates.
    Get-Process -Name 'resticpal-ui' -ErrorAction SilentlyContinue |
        Where-Object SessionId -eq $interactiveSessionId |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Process -FilePath (Join-Path $installRoot 'resticpal-ui.exe') -ArgumentList '--updates'
    $updateUi = Wait-InteractiveProcess 'resticpal-ui' ([TimeSpan]::FromSeconds(45))
    $verification.published_ui_process_id = $updateUi.Id
    if ($automaticMode) {
        $initialUpdateSettings = Invoke-ResticPalRequest @{ type = 'get_update_settings' }
        if ($initialUpdateSettings.type -cne 'update_settings' `
            -or $initialUpdateSettings.configuration.automatic_install -ne $false) {
            throw 'The clean published client did not begin with automatic_install=false.'
        }
        Wait-AutomationTextContains `
            $updateUi `
            'UpdateStatusDescription' `
            'signed update feed could not be checked' `
            ([TimeSpan]::FromSeconds(45)) | Out-Null
        Enable-AutomaticUpdatesThroughUi $updateUi ([TimeSpan]::FromSeconds(15))
        $automaticConfiguration = Wait-AutomaticUpdatesEnabled ([TimeSpan]::FromSeconds(30))
        if ($automaticSettingUiActions -ne 1) {
            throw 'The harness did not enable automatic installation with exactly one UI toggle action.'
        }
        $verification.automatic_install_enabled = $true
        $verification.automatic_install_enabled_via = 'published-client-ui-and-service-protocol'
        $verification.automatic_install_locked = if (
            $null -eq $automaticConfiguration.PSObject.Properties['automatic_install_locked']) {
            $false
        } else {
            [bool]$automaticConfiguration.automatic_install_locked
        }
        $verification.published_protocol_version = [uint32]$protocolVersion
        # Newer clients may perform one forced check immediately after this
        # toggle changes. Keep the feed closed until that handler has settled,
        # so only the deliberately restarted tray can dispatch the candidate.
        Start-Sleep -Seconds 2
        Wait-AutomationTextContains `
            $updateUi `
            'UpdateStatusDescription' `
            'signed update feed could not be checked' `
            ([TimeSpan]::FromSeconds(15)) | Out-Null
        Assert-NoAutomaticUpdateConfirmation

        Start-InstallerProcessMonitor
        $installerRequestedAt = Get-Date
        if (-not $bridgeMode) {
            [IO.File]::WriteAllText(
                $originFeedGate,
                'enabled',
                [Text.UTF8Encoding]::new($false))
            $timing.signed_feed_enabled_at = [DateTimeOffset]::UtcNow.ToString('o')
            $timing.automatic_tray_dispatch_requested_at = [DateTimeOffset]::UtcNow.ToString('o')
        }
        $baselineTray = Start-Process `
            -FilePath (Join-Path $installRoot 'resticpal-tray.exe') `
            -RedirectStandardOutput $automaticTrayStdout `
            -RedirectStandardError $automaticTrayStderr `
            -PassThru
        $baselineTrayId = $baselineTray.Id
        $verification.published_tray_process_id = $baselineTrayId
        if ($bridgeMode) {
            $verification.update_dispatcher = 'qualification-harness-via-published-service-ipc'
            $timing.automatic_bridge_dispatch_requested_at = [DateTimeOffset]::UtcNow.ToString('o')
            $dispatchResponse = Invoke-ResticPalRequest @{
                type = 'install_update'
                package = @{
                    version = $ExpectedCandidateVersion
                    url = $EnclosureUrl
                    signature = $EnclosureSignature
                    length = [uint64](Get-Item -LiteralPath $candidateMsi).Length
                }
            }
            if ($dispatchResponse.type -cne 'accepted') {
                throw ('The published service did not accept the qualification bridge package: ' +
                       ($dispatchResponse | ConvertTo-Json -Compress -Depth 6))
            }
            $verification.dispatch_bridge = [ordered]@{
                reason = 'published-v1.0.6-tray-error-pipe-busy'
                protocol_version = [uint32]$protocolVersion
                request_type = 'install_update'
                response_type = 'accepted'
                appcast_sha256 = $ExpectedAppCastSha256.ToLowerInvariant()
                appcast_signature_sha256 = $ExpectedAppCastSignatureSha256.ToLowerInvariant()
                package = [ordered]@{
                    version = $ExpectedCandidateVersion
                    url = $EnclosureUrl
                    signature = $EnclosureSignature
                    length = [uint64](Get-Item -LiteralPath $candidateMsi).Length
                }
            }
        } else {
            $verification.update_dispatcher = 'published-client-tray'
        }

        $installerLaunchTimeout = [TimeSpan]::FromMinutes($InstallerLaunchTimeoutMinutes)
        $installerLaunchDeadline = [DateTime]::UtcNow + $installerLaunchTimeout
        $automaticStagedPath = Join-Path (
            Join-Path $env:ProgramData 'ResticPal\Updates') (
                "resticpal-$ExpectedCandidateVersion-x64.msi")
        Write-TestProgress (
            "Automatic installation is enabled. Waiting up to $InstallerLaunchTimeoutMinutes " +
            'minutes for the published service to launch the signed package as LocalSystem.')
        $candidateInstallerProcess = Wait-CandidateInstallerProcessStart `
            $automaticStagedPath `
            $installerLaunchTimeout `
            0 `
            $baselineServiceProcessId `
            -RequireSilent
        $stagedUpdate = Find-StagedUpdate `
            ([TimeSpan]::FromSeconds(60)) `
            (Split-Path -Parent $automaticStagedPath) `
            $automaticStagedPath
    } else {
        $checkButton = Wait-AutomationElementEnabled `
            $updateUi 'CheckForUpdatesButton' ([TimeSpan]::FromSeconds(45))
        Invoke-AutomationElement $checkButton
        Wait-AutomationTextContains `
            $updateUi `
            'UpdateStatusDescription' `
            "resticpal $ExpectedCandidateVersion is available" `
            ([TimeSpan]::FromSeconds(45)) | Out-Null

        $downloadButton = Wait-AutomationElementEnabled `
            $updateUi 'DownloadUpdateButton' ([TimeSpan]::FromSeconds(15))
        Invoke-AutomationElement $downloadButton
        $confirmDownload = Wait-AutomationElementByName 'Download' ([TimeSpan]::FromSeconds(15))
        $downloadConfirmationActions++
        Invoke-AutomationElement $confirmDownload
        Wait-AutomationTextContains `
            $updateUi `
            'UpdateStatusDescription' `
            'is downloaded and its Ed25519 signature is valid' `
            ([TimeSpan]::FromMinutes(3)) | Out-Null

        $stagedUpdate = Find-StagedUpdate ([TimeSpan]::FromSeconds(30))
        $installButton = Wait-AutomationElementEnabled `
            $updateUi 'InstallUpdateButton' ([TimeSpan]::FromSeconds(15))
        Invoke-AutomationElement $installButton
        $confirmInstall = Wait-AutomationElementByName `
            'Install update' ([TimeSpan]::FromSeconds(15))
        $knownInstallerCommandPaths = @(Get-ChildItem -LiteralPath $env:TEMP `
            -File `
            -Recurse `
            -Force `
            -ErrorAction SilentlyContinue |
            Where-Object Extension -ieq '.cmd' |
            ForEach-Object FullName)
        Start-InstallerProcessMonitor
        $installerRequestedAt = Get-Date
        $timing.candidate_install_confirmed_at = [DateTimeOffset]::UtcNow.ToString('o')
        $installConfirmationActions++
        Invoke-AutomationElement $confirmInstall

        $installerLaunchTimeout = [TimeSpan]::FromMinutes($InstallerLaunchTimeoutMinutes)
        $installerLaunchDeadline = [DateTime]::UtcNow + $installerLaunchTimeout
        Write-TestProgress (
            "Confirmed the prompted install. Waiting up to $InstallerLaunchTimeoutMinutes " +
            'minutes for the old UI to close and Windows Installer to begin.')
        Wait-ProcessExit $updateUi $installerLaunchTimeout
        $timing.published_ui_exited_at = [DateTimeOffset]::UtcNow.ToString('o')
        Write-TestProgress 'The published update UI exited.'

        $remainingLaunchTime = $installerLaunchDeadline - [DateTime]::UtcNow
        if ($remainingLaunchTime -le [TimeSpan]::Zero) {
            throw 'The old update UI consumed the entire candidate-installer launch timeout.'
        }
        $candidateInstallerProcess = Wait-CandidateInstallerProcessStart `
            $stagedUpdate.path $remainingLaunchTime ([uint32]$interactiveSessionId)
    }

    $stagedUpdate | ConvertTo-Json -Depth 3 |
        Set-Content -LiteralPath (Join-Path $localArtifactRoot 'staged-update.json') -Encoding UTF8
    if (-not [string]::Equals(
        $stagedUpdate.extension,
        '.msi',
        [StringComparison]::OrdinalIgnoreCase)) {
        throw "The exact candidate was staged without an .msi extension: $($stagedUpdate.path)"
    }
    Write-TestProgress "The exact candidate was staged as $($stagedUpdate.path)"

    $candidateInstallerProcessId = [uint32]$candidateInstallerProcess.ProcessId
    $verification.candidate_installer_process_id = $candidateInstallerProcessId
    $verification.candidate_installer_parent_process_id =
        [uint32]$candidateInstallerProcess.ParentProcessId
    $verification.candidate_installer_session_id = [uint32]$candidateInstallerProcess.SessionId
    $verification.candidate_installer_owner = [string]$candidateInstallerProcess.Owner
    $verification.candidate_installer_command_line = [string]$candidateInstallerProcess.CommandLine
    $verification.candidate_installer_silent = $automaticMode
    $verification.msi_files_in_use_prompt_handled = $false
    $verification.automatic_installer_dialog_observed = $false
    $timing.candidate_installer_process_started_at = if (
        $null -eq $candidateInstallerProcess.CreationDate) {
        [DateTimeOffset]::UtcNow.ToString('o')
    } else {
        ([DateTimeOffset]$candidateInstallerProcess.CreationDate).ToUniversalTime().ToString('o')
    }
    Write-TestProgress (
        "The published client launched msiexec process $candidateInstallerProcessId with the exact " +
        "candidate path at $($timing.candidate_installer_process_started_at).")

    $remainingLaunchTime = $installerLaunchDeadline - [DateTime]::UtcNow
    if ($remainingLaunchTime -le [TimeSpan]::Zero) {
        throw 'The client launched msiexec only after consuming the transaction-start timeout.'
    }
    $candidateTransaction = Wait-MsiTransactionStart `
        $stagedUpdate.path `
        $installerRequestedAt `
        $remainingLaunchTime `
        $candidateInstallerProcessId
    $candidateProcessMatch = [Regex]::Match(
        [string]$candidateTransaction.Message,
        'Client Process Id:\s*(?<processId>\d+)\.?\s*$')
    if (-not $candidateProcessMatch.Success) {
        throw 'The candidate Windows Installer transaction did not identify its client process ID.'
    }
    $transactionProcessId = [uint32]$candidateProcessMatch.Groups['processId'].Value
    if ($transactionProcessId -ne $candidateInstallerProcessId) {
        throw ("The candidate transaction reports client process $transactionProcessId, " +
               "not traced msiexec process $candidateInstallerProcessId.")
    }
    $timing.candidate_installer_started_at = (
        [DateTimeOffset]$candidateTransaction.TimeCreated).ToUniversalTime().ToString('o')
    Write-TestProgress (
        "Windows Installer began the candidate transaction at " +
        "$($timing.candidate_installer_started_at). Waiting up to " +
        "$InstallerCompletionTimeoutMinutes minutes for completion.")

    Wait-InstalledVersion `
        $ExpectedCandidateVersion `
        ([TimeSpan]::FromMinutes($InstallerCompletionTimeoutMinutes)) `
        $candidateInstallerProcessId
    $timing.candidate_version_observed_at = [DateTimeOffset]::UtcNow.ToString('o')
    Write-TestProgress "Windows reports installed version $ExpectedCandidateVersion."
    Wait-ProcessExit $updateUi ([TimeSpan]::FromSeconds(60))
    $verification.published_ui_exited = $true
    if (-not $timing.Contains('published_ui_exited_at')) {
        $timing.published_ui_exited_at = [DateTimeOffset]::UtcNow.ToString('o')
    }
    Wait-ProcessExit $baselineServiceProcess ([TimeSpan]::FromSeconds(60))
    $upgradedService = Wait-ServiceRunning ([TimeSpan]::FromSeconds(60))
    $upgradedServiceProcessId = [uint32]$upgradedService.ProcessId
    if ($upgradedServiceProcessId -eq 0 `
        -or $upgradedServiceProcessId -eq $baselineServiceProcessId) {
        throw ("The service process was not replaced during the upgrade " +
               "(published=$baselineServiceProcessId, upgraded=$upgradedServiceProcessId).")
    }
    if ($automaticMode) {
        $script:protocolVersion = $null
        $null = Wait-AutomaticUpdatesEnabled ([TimeSpan]::FromSeconds(30))
        $verification.automatic_install_persisted_after_upgrade = $true
        $verification.upgraded_protocol_version = [uint32]$protocolVersion
        $verification.upgraded_service_protocol_version = [uint32]$protocolVersion
    }

    Wait-ProcessExit $baselineTray ([TimeSpan]::FromSeconds(60))
    $upgradedTray = Wait-SingleReplacementTray `
        $baselineTrayId ([TimeSpan]::FromSeconds(60))
    $trayProcesses = @(Get-Process -Name 'resticpal-tray' -ErrorAction SilentlyContinue |
        Where-Object SessionId -eq $interactiveSessionId)
    if ($trayProcesses.Count -ne 1 -or $trayProcesses[0].Id -ne $upgradedTray.Id) {
        throw "The upgrade did not leave exactly one replacement tray in the interactive session."
    }
    if ($bridgeMode) {
        if ([uint32]$protocolVersion -ne 4) {
            throw "The upgraded service exposed protocol $protocolVersion instead of protocol 4."
        }
        $candidateServiceProcess = Get-Process -Id $upgradedServiceProcessId -ErrorAction Stop
        $probeNotBefore = [DateTimeOffset]($candidateServiceProcess.StartTime.ToUniversalTime())
        $probeDiagnostics = Wait-CandidateTrayProbeFailure `
            $probeNotBefore `
            ([TimeSpan]::FromMinutes(5))
        $null = Wait-AutomaticUpdatesEnabled ([TimeSpan]::FromSeconds(30))
        if ([uint32]$protocolVersion -ne 4) {
            throw 'The upgraded service protocol changed during the candidate-tray probe.'
        }
        Start-Sleep -Milliseconds 500

        $probeOrigin = Get-Content -LiteralPath $originLog -Raw
        $expectedProbeUserAgent = "resticpal/$ExpectedCandidateVersion"
        foreach ($observation in @(
            "served candidate probe appcast to $expectedProbeUserAgent",
            "served candidate probe appcast signature to $expectedProbeUserAgent",
            "served candidate probe payload to $expectedProbeUserAgent")) {
            if (-not $probeOrigin.Contains($observation)) {
                throw "The candidate-tray probe origin did not record: $observation"
            }
        }

        $probeFinalPath = Join-Path (
            Join-Path $env:ProgramData 'ResticPal\Updates') (
                "resticpal-$ExpectedProbeVersion-x64.msi")
        $probePartialPath = "$probeFinalPath.partial"
        if ((Test-Path -LiteralPath $probeFinalPath) -or
            (Test-Path -LiteralPath $probePartialPath)) {
            throw 'The invalid candidate-tray probe left staged update bytes behind.'
        }
        $probeStagingEntries = @(Get-ChildItem -LiteralPath (
                Join-Path $env:ProgramData 'ResticPal\Updates') -Force -ErrorAction Stop |
            Where-Object Name -Like "resticpal-$ExpectedProbeVersion-x64.msi*" |
            ForEach-Object Name)
        if ($probeStagingEntries.Count -ne 0) {
            throw ('The invalid candidate-tray probe left unexpected staged entries: ' +
                   ($probeStagingEntries -join ', '))
        }
        Stop-InstallerProcessMonitor -RequireHealthy
        $probeInstallerStarts = @()
        $probeStartedAt = [DateTimeOffset]$probeDiagnostics.started_at
        if (Test-Path -LiteralPath $installerProcessEvents -PathType Leaf) {
            $probeInstallerStarts = @(Get-Content -LiteralPath $installerProcessEvents |
                Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
                ForEach-Object { $_ | ConvertFrom-Json } |
                Where-Object {
                    $_.event -ceq 'process_start' -and
                    $_.name -ieq 'msiexec.exe' -and
                    [DateTimeOffset]$_.observed_at -ge $probeStartedAt -and
                    ([uint32]$_.parent_process_id -eq $upgradedServiceProcessId -or
                     [string]$_.command_line -like "*$probeFinalPath*")
                })
        }
        if ($probeInstallerStarts.Count -ne 0) {
            throw 'The invalid candidate-tray probe started Windows Installer.'
        }
        $currentService = Get-CimInstance Win32_Service -Filter "Name='ResticPal'"
        $currentTrays = @(Get-Process -Name 'resticpal-tray' -ErrorAction SilentlyContinue |
            Where-Object SessionId -eq $interactiveSessionId)
        if ([uint32]$currentService.ProcessId -ne $upgradedServiceProcessId -or
            $currentTrays.Count -ne 1 -or $currentTrays[0].Id -ne $upgradedTray.Id) {
            throw 'The candidate-tray probe replaced or stopped the upgraded service or tray.'
        }
        $verification.upgraded_tray_protocol_version = 4
        $verification.candidate_tray_probe = [ordered]@{
            protocol_version = 4
            probe_version = $ExpectedProbeVersion
            appcast_sha256 = $ExpectedProbeAppCastSha256.ToLowerInvariant()
            appcast_signature_sha256 = $ExpectedProbeAppCastSignatureSha256.ToLowerInvariant()
            payload = [ordered]@{
                name = [IO.Path]::GetFileName($probePayload)
                url = $ExpectedProbePayloadUrl
                length = [uint64]$ExpectedProbePayloadLength
                sha256 = $ExpectedProbePayloadSha256.ToLowerInvariant()
                expected_signature = $ExpectedProbePackageSignature
            }
            requests = [ordered]@{
                appcast = [ordered]@{
                    url = 'https://updates.resticpal.com/appcast-v2.xml'
                    user_agent = $expectedProbeUserAgent
                }
                appcast_signature = [ordered]@{
                    url = 'https://updates.resticpal.com/appcast-v2.xml.signature'
                    user_agent = $expectedProbeUserAgent
                }
                payload = [ordered]@{
                    url = $ExpectedProbePayloadUrl
                    user_agent = $expectedProbeUserAgent
                }
            }
            diagnostics = @(
                [ordered]@{
                    code = 'update.started'
                    observed_at = $probeDiagnostics.started_at
                },
                [ordered]@{
                    code = 'update.failed'
                    failure_code = 'update_signature_invalid'
                    observed_at = $probeDiagnostics.failed_at
                })
            final_path = $probeFinalPath
            final_exists = $false
            partial_path = $probePartialPath
            partial_exists = $false
            staging_entries = @($probeStagingEntries)
            msiexec_process_count = 0
            tray_process_id = [uint32]$upgradedTray.Id
            service_process_id = [uint32]$upgradedServiceProcessId
        }
    }

    $expectedFileVersion = "$ExpectedCandidateVersion.0"
    $installedFileVersions = [ordered]@{}
    foreach ($payload in @(
        @{ Name = 'service'; FileName = 'resticpal-service.exe' },
        @{ Name = 'tray'; FileName = 'resticpal-tray.exe' },
        @{ Name = 'ui'; FileName = 'resticpal-ui.exe' }
    )) {
        $payloadPath = Join-Path $installRoot $payload.FileName
        $fileVersion = (Get-Item -LiteralPath $payloadPath).VersionInfo.FileVersion
        if ($fileVersion -cne $expectedFileVersion) {
            throw ("The installed $($payload.Name) file version is $fileVersion; " +
                   "expected $expectedFileVersion.")
        }
        $installedFileVersions[$payload.Name] = $fileVersion
    }
    $verification.installed_version = Get-InstalledVersion
    $verification.installed_ui_file_version = $installedFileVersions.ui
    $verification.installed_service_file_version = $installedFileVersions.service
    $verification.installed_tray_file_version = $installedFileVersions.tray
    $verification.upgraded_service_process_id = $upgradedServiceProcessId
    $verification.upgraded_tray_process_id = $upgradedTray.Id
    $verification.published_tray_exited = $true
    $verification.tray_process_count = $trayProcesses.Count
    $verification.service_identity = $upgradedService.StartName
    $verification.service_state = (Get-Service -Name ResticPal).Status.ToString()
    $verification.download_confirmation_actions = $downloadConfirmationActions
    $verification.install_confirmation_actions = $installConfirmationActions
    $verification.automatic_setting_ui_actions = $automaticSettingUiActions
    $verification.installer_dialog_interventions = if ($msiFilesInUseHandled) { 1 } else { 0 }
    $remainingUiProcesses = @(Get-Process -Name 'resticpal-ui' -ErrorAction SilentlyContinue |
        Where-Object SessionId -eq $interactiveSessionId)
    $verification.interactive_ui_process_count = $remainingUiProcesses.Count
    if ($remainingUiProcesses.Count -ne 0) {
        throw 'The published UI was not closed and replaced on disk by the silent upgrade.'
    }

    Stop-InstallerProcessMonitor -RequireHealthy

    if ($automaticMode) {
        Assert-NoAutomaticUpdateConfirmation
        Assert-NoAutomaticConsentEvent
        $verification.uac_consent_events = 0
        $verification.no_uac_prompt = $true
        if ($downloadConfirmationActions -ne 0 `
            -or $installConfirmationActions -ne 0 `
            -or $msiFilesInUseHandled `
            -or $verification.automatic_installer_dialog_observed) {
            throw 'Automatic mode required a user confirmation or installer-dialog intervention.'
        }
        $silentInstallLog = Join-Path $env:ProgramData 'ResticPal\Updates\install.log'
        if (-not (Test-Path -LiteralPath $silentInstallLog -PathType Leaf) `
            -or (Get-Item -LiteralPath $silentInstallLog).Length -eq 0) {
            throw 'The LocalSystem updater did not leave its silent MSI transaction log.'
        }
        $verification.silent_install_log = $silentInstallLog
        $verification.no_user_confirmation_or_dialog_intervention = $true
    }

    $requests = Get-Content -LiteralPath $originLog -Raw
    if ($automaticMode) {
        if (-not $requests.Contains(
            'signed feed held until automatic-install setup completes')) {
            throw 'Automatic mode did not prove that the signed feed stayed gated during setup.'
        }
        $verification.signed_feed_gated_during_setup = $true
        if ($bridgeMode) {
            if (Test-Path -LiteralPath $originFeedGate -PathType Leaf) {
                throw 'The legacy feed was exposed during the one-time service-bridge qualification.'
            }
            $verification.signed_appcast_fetched_by_published_tray = $false
            $verification.prepared_signed_appcast_metadata_dispatched_by_qualification_harness = $true
        } else {
            if (-not $requests.Contains(
                "served signed appcast to resticpal/$ExpectedPublishedVersion")) {
                throw 'The published tray did not fetch the signed appcast after the gate opened.'
            }
            $verification.signed_appcast_fetched_by_published_tray = $true
        }
    }
    $expectedRequests = if ($bridgeMode) {
        @(
            $EnclosureUrl,
            'https://updates.resticpal.com/appcast-v2.xml',
            'https://updates.resticpal.com/appcast-v2.xml.signature',
            $ExpectedProbePayloadUrl)
    } elseif ($publishedUsesV2Feed) {
        @(
            'https://updates.resticpal.com/appcast-v2.xml',
            'https://updates.resticpal.com/appcast-v2.xml.signature',
            $EnclosureUrl)
    } else {
        @(
            'https://updates.resticpal.com/appcast.xml',
            'https://updates.resticpal.com/appcast.xml.signature',
            $EnclosureUrl)
    }
    foreach ($expectedRequest in $expectedRequests) {
        if (-not $requests.Contains($expectedRequest)) {
            throw "The local update origin did not observe $expectedRequest."
        }
    }
    if ($EnclosureUrl.StartsWith('https://github.com/', [StringComparison]::Ordinal) `
        -and -not $requests.Contains("GET $redirectUrl") `
        -and -not $requests.Contains("HEAD $redirectUrl")) {
        throw 'The published client did not follow the simulated opaque GitHub redirect.'
    }

    Write-Host (
        "$InstallationMode update succeeded: $ExpectedPublishedVersion -> " +
        "$ExpectedCandidateVersion; staged path ended in .msi, service is " +
        'LocalSystem/Running, and tray restarted.')
    $status = 'passed'
    $exitCode = 0
} catch {
    $errorMessage = $_.Exception.Message
    Write-Error -ErrorRecord $_ -ErrorAction Continue
} finally {
    try {
        Stop-InstallerProcessMonitor
    } catch {
        $monitorError = "Could not stop the installer process monitor: $($_.Exception.Message)"
        $errorMessage = if ([string]::IsNullOrWhiteSpace($errorMessage)) {
            $monitorError
        } else {
            "$errorMessage $monitorError"
        }
    }
    try {
        Export-GuestDiagnostics
    } catch {
        $diagnosticError = "Could not export guest diagnostics: $($_.Exception.Message)"
        $errorMessage = if ([string]::IsNullOrWhiteSpace($errorMessage)) {
            $diagnosticError
        } else {
            "$errorMessage $diagnosticError"
        }
    }

    if ($null -ne $serverJob) {
        Stop-Job -Job $serverJob -ErrorAction SilentlyContinue
        Receive-Job -Job $serverJob -ErrorAction Continue 2>&1 |
            Out-String |
            Set-Content -LiteralPath (Join-Path $localArtifactRoot 'update-origin-job.log') -Encoding UTF8
        Remove-Job -Job $serverJob -Force -ErrorAction SilentlyContinue
    }
    if ($null -ne $originalHostsBytes) {
        [IO.File]::WriteAllBytes($hostsPath, $originalHostsBytes)
    }
    if (-not [string]::IsNullOrWhiteSpace($certificateThumbprint)) {
        Remove-Item -LiteralPath "Cert:\LocalMachine\Root\$certificateThumbprint" `
            -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath "Cert:\LocalMachine\My\$certificateThumbprint" `
            -Force -ErrorAction SilentlyContinue
    }

    if ($transcriptStarted) {
        Stop-Transcript | Out-Null
    }
    try {
        if (Test-Path -LiteralPath $localArtifactRoot) {
            Copy-Item -LiteralPath $localArtifactRoot `
                -Destination $exportedArtifactRoot `
                -Recurse `
                -Force
        }
    } catch {
        $exportError = "Could not export guest artifacts: $($_.Exception.Message)"
        $errorMessage = if ([string]::IsNullOrWhiteSpace($errorMessage)) {
            $exportError
        } else {
            "$errorMessage $exportError"
        }
        $status = 'failed'
        $exitCode = 1
    }

    $result = [ordered]@{
        schema = if ($bridgeMode) { 2 } else { 1 }
        qualification = if ($bridgeMode) {
            'previous-published-service-automatic-update-bridge'
        } elseif ($automaticMode) {
            'previous-published-client-automatic-update'
        } else {
            'previous-published-client-prompted-update'
        }
        installation_mode = $installationModeName
        status = $status
        exit_code = $exitCode
        error = $errorMessage
        started_at = $startedAt.ToString('o')
        finished_at = [DateTimeOffset]::UtcNow.ToString('o')
        computer_name = $env:COMPUTERNAME
        windows_build = [Environment]::OSVersion.Version.ToString()
        published_version = $ExpectedPublishedVersion
        published_release = [ordered]@{
            tag = "v$ExpectedPublishedVersion"
            asset_name = $PublishedReleaseAssetName
            asset_length = $PublishedReleaseAssetLength
            asset_sha256 = $ExpectedPublishedSha256.ToLowerInvariant()
            asset_url = $PublishedReleaseAssetUrl
        }
        candidate_version = $ExpectedCandidateVersion
        appcast_sha256 = $ExpectedAppCastSha256.ToLowerInvariant()
        appcast_signature_sha256 = $ExpectedAppCastSignatureSha256.ToLowerInvariant()
        enclosure_url = $EnclosureUrl
        timing = $timing
        installer_diagnostics = [ordered]@{
            process_events = Join-Path $exportedArtifactRoot 'installer-process-events.jsonl'
            final_processes = Join-Path $exportedArtifactRoot 'installer-process-final.json'
            launcher_files = Join-Path $exportedArtifactRoot 'netsparkle-launchers.json'
        }
        staged_update = $stagedUpdate
        verification = $verification
        test_artifacts = $exportedArtifactRoot
        transcript = $transcriptPath
    }
    $json = $result | ConvertTo-Json -Depth 6
    [IO.File]::WriteAllText($temporaryResultPath, $json, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temporaryResultPath -Destination $resultPath -Force

    if (-not $KeepOpen) {
        Start-Process -FilePath "$env:SystemRoot\System32\shutdown.exe" `
            -ArgumentList '/s /t 3 /f' `
            -WindowStyle Hidden
    }
}

exit $exitCode
