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
    [ValidateRange(1, 15)]
    [int] $InstallerLaunchTimeoutMinutes,

    [Parameter(Mandatory)]
    [ValidateRange(1, 20)]
    [int] $InstallerCompletionTimeoutMinutes,

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
$baselineInstallLog = Join-Path $localArtifactRoot 'published-client-install.log'
$originLog = Join-Path $localArtifactRoot 'update-origin.log'
$originReady = Join-Path $localArtifactRoot 'update-origin.ready'
$installerMonitorReady = Join-Path $localArtifactRoot 'installer-process-monitor.ready'
$installerMonitorStop = Join-Path $localArtifactRoot 'installer-process-monitor.stop'
$installerProcessEvents = Join-Path $localArtifactRoot 'installer-process-events.jsonl'
$installRoot = Join-Path $env:ProgramFiles 'resticpal'
$hostsPath = Join-Path $env:SystemRoot 'System32\drivers\etc\hosts'
$interactiveSessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
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
    [TimeSpan] $Timeout
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    $nextProgress = [DateTime]::UtcNow
    do {
        $matches = @(Get-CimInstance Win32_Process -Filter "Name='msiexec.exe'" |
            Where-Object {
                [uint32]$_.SessionId -eq [uint32]$interactiveSessionId `
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
            return $matches[0]
        }
        if ([DateTime]::UtcNow -ge $nextProgress) {
            Write-TestProgress 'Waiting for NetSparkle to launch msiexec with the exact candidate path.'
            $nextProgress = [DateTime]::UtcNow.AddSeconds(15)
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "NetSparkle did not launch msiexec with the exact candidate path within $Timeout."
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
            $initialProcesses = @(Get-CimInstance Win32_Process |
                Where-Object Name -in @('cmd.exe', 'msiexec.exe') |
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

            Register-WmiEvent `
                -Query ("SELECT * FROM Win32_ProcessStartTrace WHERE " +
                        "ProcessName='cmd.exe' OR ProcessName='msiexec.exe'") `
                -SourceIdentifier $startSource | Out-Null
            Register-WmiEvent `
                -Query ("SELECT * FROM Win32_ProcessStopTrace WHERE " +
                        "ProcessName='cmd.exe' OR ProcessName='msiexec.exe'") `
                -SourceIdentifier $stopSource | Out-Null
            [IO.File]::WriteAllText($ReadyPath, 'ready', [Text.UTF8Encoding]::new($false))

            while (-not (Test-Path -LiteralPath $StopPath -PathType Leaf)) {
                $event = Wait-Event -Timeout 1
                if ($null -eq $event) {
                    continue
                }
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
}

function Stop-InstallerProcessMonitor {
    if ($null -eq $script:installerMonitorJob) {
        return
    }
    [IO.File]::WriteAllText(
        $installerMonitorStop,
        'stop',
        [Text.UTF8Encoding]::new($false))
    $null = Wait-Job -Job $script:installerMonitorJob -Timeout 5
    if ($script:installerMonitorJob.State -notin @('Completed', 'Failed', 'Stopped')) {
        Stop-Job -Job $script:installerMonitorJob -ErrorAction SilentlyContinue
    }
    Receive-Job -Job $script:installerMonitorJob -ErrorAction Continue 2>&1 |
        Out-String |
        Set-Content -LiteralPath (
            Join-Path $localArtifactRoot 'installer-process-monitor-job.log') -Encoding UTF8
    Remove-Job -Job $script:installerMonitorJob -Force -ErrorAction SilentlyContinue
    $script:installerMonitorJob = $null
}

function Export-InstallerDiagnostics {
    $processes = @(Get-CimInstance Win32_Process |
        Where-Object Name -in @(
            'cmd.exe',
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

function Find-StagedUpdate([TimeSpan] $Timeout) {
    $expectedLength = (Get-Item -LiteralPath $candidateMsi).Length
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $files = @(Get-ChildItem -LiteralPath $env:TEMP `
            -File `
            -Recurse `
            -Force `
            -ErrorAction SilentlyContinue |
            Where-Object Length -eq $expectedLength |
            Sort-Object LastWriteTimeUtc -Descending)
        $matches = @()
        foreach ($file in $files) {
            $hash = (Get-FileHash -LiteralPath $file.FullName -Algorithm SHA256).Hash.ToLowerInvariant()
            if ($hash -ceq $ExpectedCandidateSha256.ToLowerInvariant()) {
                $matches += [pscustomobject]@{
                    path = $file.FullName
                    file_name = $file.Name
                    extension = [IO.Path]::GetExtension($file.FullName)
                    length = $file.Length
                    sha256 = $hash
                    same_length_files_examined = $files.Count
                    hash_matches = 1
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
    throw 'No temporary file matched the signed candidate MSI length and SHA-256.'
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
            $ReadyPath
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
                    do {
                        $line = $reader.ReadLine()
                        if ($line -like 'Host:*') {
                            $hostHeader = $line.Substring(5).Trim().Split(':')[0]
                        }
                    } while (-not [string]::IsNullOrEmpty($line))
                    if ([string]::IsNullOrWhiteSpace($hostHeader)) {
                        throw 'The request did not contain a Host header.'
                    }
                    $requestUri = [Uri]::new("https://$hostHeader$requestTarget")
                    $absoluteUrl = "https://$hostHeader$($requestUri.AbsolutePath)"
                    Write-OriginLog "$method $absoluteUrl"

                    if ($hostHeader -ceq 'updates.resticpal.com' `
                        -and $requestUri.AbsolutePath -ceq '/appcast.xml') {
                        $body = [IO.File]::ReadAllBytes($AppCastPath)
                        Write-Response $stream $method 200 'OK' `
                            @{ 'Content-Type' = 'text/xml; charset=utf-8' } $body
                    } elseif ($hostHeader -ceq 'updates.resticpal.com' `
                        -and $requestUri.AbsolutePath -ceq '/appcast.xml.signature') {
                        $body = [IO.File]::ReadAllBytes($AppCastSignaturePath)
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
                    } else {
                        Write-Response $stream $method 404 'Not Found' `
                            @{ 'Content-Type' = 'text/plain' } `
                            ([Text.Encoding]::UTF8.GetBytes('not found'))
                    }
                } catch {
                    Write-OriginLog "request failed: $($_.Exception.Message)"
                } finally {
                    if ($null -ne $reader) { $reader.Dispose() }
                    if ($null -ne $stream) { $stream.Dispose() }
                    $client.Dispose()
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
        $originReady
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
}

try {
    New-Item -ItemType Directory -Path $localArtifactRoot -Force | Out-Null
    Copy-Item -LiteralPath $PublishedClientMsiPath -Destination $publishedMsi
    Copy-Item -LiteralPath $CandidateMsiPath -Destination $candidateMsi
    Copy-Item -LiteralPath $AppCastPath -Destination $appCast
    Copy-Item -LiteralPath $AppCastSignaturePath -Destination $appCastSignature
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
    $baselineTray = Wait-InteractiveProcess 'resticpal-tray' ([TimeSpan]::FromSeconds(45))
    $baselineTrayId = $baselineTray.Id
    $verification.baseline_service_process_id = $baselineServiceProcessId
    $verification.published_tray_process_id = $baselineTrayId

    # The pre-created first-run marker keeps onboarding from racing this
    # focused update test. Close any unexpected UI, then open updates.
    Get-Process -Name 'resticpal-ui' -ErrorAction SilentlyContinue |
        Where-Object SessionId -eq $interactiveSessionId |
        Stop-Process -Force -ErrorAction SilentlyContinue
    Start-Process -FilePath (Join-Path $installRoot 'resticpal-ui.exe') -ArgumentList '--updates'
    $updateUi = Wait-InteractiveProcess 'resticpal-ui' ([TimeSpan]::FromSeconds(45))
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
    Invoke-AutomationElement $confirmDownload
    Wait-AutomationTextContains `
        $updateUi `
        'UpdateStatusDescription' `
        'is downloaded and its Ed25519 signature is valid' `
        ([TimeSpan]::FromMinutes(3)) | Out-Null

    $stagedUpdate = Find-StagedUpdate ([TimeSpan]::FromSeconds(30))
    $stagedUpdate | ConvertTo-Json -Depth 3 |
        Set-Content -LiteralPath (Join-Path $localArtifactRoot 'staged-update.json') -Encoding UTF8
    if (-not [string]::Equals(
        $stagedUpdate.extension,
        '.msi',
        [StringComparison]::OrdinalIgnoreCase)) {
        throw "NetSparkle staged the exact candidate without an .msi extension: $($stagedUpdate.path)"
    }
    Write-TestProgress "NetSparkle staged the exact candidate as $($stagedUpdate.path)"

    $installButton = Wait-AutomationElementEnabled `
        $updateUi 'InstallUpdateButton' ([TimeSpan]::FromSeconds(15))
    Invoke-AutomationElement $installButton
    $confirmInstall = Wait-AutomationElementByName 'Install update' ([TimeSpan]::FromSeconds(15))
    $verification.published_ui_process_id = $updateUi.Id
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
    Invoke-AutomationElement $confirmInstall

    $installerLaunchTimeout = [TimeSpan]::FromMinutes($InstallerLaunchTimeoutMinutes)
    $installerLaunchDeadline = [DateTime]::UtcNow + $installerLaunchTimeout
    Write-TestProgress (
        "Confirmed the prompted install. Waiting up to $InstallerLaunchTimeoutMinutes minutes " +
        'for the old UI to close and the candidate Windows Installer transaction to begin.')
    Wait-ProcessExit $updateUi $installerLaunchTimeout
    $timing.published_ui_exited_at = [DateTimeOffset]::UtcNow.ToString('o')
    Write-TestProgress 'The published update UI exited.'

    $remainingLaunchTime = $installerLaunchDeadline - [DateTime]::UtcNow
    if ($remainingLaunchTime -le [TimeSpan]::Zero) {
        throw 'The old update UI consumed the entire candidate-installer launch timeout.'
    }
    $candidateInstallerProcess = Wait-CandidateInstallerProcessStart `
        $stagedUpdate.path $remainingLaunchTime
    $candidateInstallerProcessId = [uint32]$candidateInstallerProcess.ProcessId
    $verification.candidate_installer_process_id = $candidateInstallerProcessId
    $verification.msi_files_in_use_prompt_handled = $false
    $timing.candidate_installer_process_started_at = if (
        $null -eq $candidateInstallerProcess.CreationDate) {
        [DateTimeOffset]::UtcNow.ToString('o')
    } else {
        ([DateTimeOffset]$candidateInstallerProcess.CreationDate).ToUniversalTime().ToString('o')
    }
    Write-TestProgress (
        "NetSparkle launched msiexec process $candidateInstallerProcessId with the exact " +
        "candidate path at $($timing.candidate_installer_process_started_at).")

    $remainingLaunchTime = $installerLaunchDeadline - [DateTime]::UtcNow
    if ($remainingLaunchTime -le [TimeSpan]::Zero) {
        throw 'NetSparkle launched msiexec only after consuming the transaction-start timeout.'
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
               "not traced NetSparkle msiexec process $candidateInstallerProcessId.")
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
    Wait-ProcessExit $baselineServiceProcess ([TimeSpan]::FromSeconds(60))
    $upgradedService = Wait-ServiceRunning ([TimeSpan]::FromSeconds(60))
    $upgradedServiceProcessId = [uint32]$upgradedService.ProcessId
    if ($upgradedServiceProcessId -eq 0 `
        -or $upgradedServiceProcessId -eq $baselineServiceProcessId) {
        throw ("The service process was not replaced during the upgrade " +
               "(published=$baselineServiceProcessId, upgraded=$upgradedServiceProcessId).")
    }

    Wait-ProcessExit $baselineTray ([TimeSpan]::FromSeconds(60))
    $upgradedTray = Wait-SingleReplacementTray `
        $baselineTrayId ([TimeSpan]::FromSeconds(60))
    $trayProcesses = @(Get-Process -Name 'resticpal-tray' -ErrorAction SilentlyContinue |
        Where-Object SessionId -eq $interactiveSessionId)
    if ($trayProcesses.Count -ne 1 -or $trayProcesses[0].Id -ne $upgradedTray.Id) {
        throw "The upgrade did not leave exactly one replacement tray in the interactive session."
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

    $requests = Get-Content -LiteralPath $originLog -Raw
    foreach ($expectedRequest in @(
        'https://updates.resticpal.com/appcast.xml',
        'https://updates.resticpal.com/appcast.xml.signature',
        $EnclosureUrl
    )) {
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
        "Prompted update succeeded: $ExpectedPublishedVersion -> $ExpectedCandidateVersion; " +
        "staged path ended in .msi, service is LocalSystem/Running, and tray restarted.")
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
        schema = 1
        qualification = 'previous-published-client-prompted-update'
        installation_mode = 'prompted'
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
