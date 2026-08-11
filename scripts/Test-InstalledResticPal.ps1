[CmdletBinding()]
param(
    [string] $MsiPath,
    [string] $UpgradeFromMsiPath,
    [switch] $KeepInstalled,
    [string] $ArtifactRoot
)

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -TypeDefinition @'
using System;
using System.Diagnostics;
using System.Runtime.InteropServices;
using System.Text;

public static class ResticPalNativeTest
{
    private delegate bool EnumThreadWindowsCallback(IntPtr window, IntPtr parameter);

    [DllImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool EnumThreadWindows(
        uint threadId,
        EnumThreadWindowsCallback callback,
        IntPtr parameter);

    [DllImport("user32.dll", CharSet = CharSet.Unicode)]
    private static extern int GetClassName(IntPtr window, StringBuilder className, int maxCount);

    [DllImport("user32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    public static extern bool PostMessage(
        IntPtr window,
        uint message,
        IntPtr wParam,
        IntPtr lParam);

    [DllImport("user32.dll")]
    public static extern uint GetWindowThreadProcessId(IntPtr window, out uint processId);

    public static IntPtr FindWindowForProcess(int processId, string expectedClassName)
    {
        using (Process process = Process.GetProcessById(processId))
        {
            foreach (ProcessThread thread in process.Threads)
            {
                IntPtr found = IntPtr.Zero;
                EnumThreadWindows((uint)thread.Id, (window, parameter) =>
                {
                    var className = new StringBuilder(256);
                    if (GetClassName(window, className, className.Capacity) > 0 &&
                        string.Equals(className.ToString(), expectedClassName, StringComparison.Ordinal))
                    {
                        found = window;
                        return false;
                    }
                    return true;
                }, IntPtr.Zero);
                if (found != IntPtr.Zero)
                {
                    return found;
                }
            }
        }
        return IntPtr.Zero;
    }
}
'@
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$isAdministrator = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator
)
if (-not $isAdministrator) {
    throw 'Run this end-to-end test from an elevated PowerShell session.'
}

if ([string]::IsNullOrWhiteSpace($MsiPath)) {
    $candidates = @(Get-ChildItem -LiteralPath (Join-Path $repositoryRoot 'artifacts\installer\output') -Filter 'resticpal-*-x64.msi' -File -ErrorAction SilentlyContinue)
    if ($candidates.Count -ne 1) {
        throw 'Pass -MsiPath when the installer output directory does not contain exactly one MSI.'
    }
    $MsiPath = $candidates[0].FullName
}
$resolvedMsiPath = (Resolve-Path -LiteralPath $MsiPath).Path
$resolvedUpgradeFromMsiPath = if ([string]::IsNullOrWhiteSpace($UpgradeFromMsiPath)) {
    $null
} else {
    (Resolve-Path -LiteralPath $UpgradeFromMsiPath).Path
}
$installRoot = Join-Path $env:ProgramFiles 'resticpal'
$dataRoot = Join-Path $env:ProgramData 'ResticPal'
$startMenuShortcut = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\resticpal.lnk'
$onboardingMarker = Join-Path $env:LOCALAPPDATA 'resticpal\onboarding-shown-v1'
$interactiveSessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
$e2eRoot = Join-Path $dataRoot 'E2E'
$sourceRoot = Join-Path $e2eRoot 'Source'
$backupRoot = Join-Path $e2eRoot 'Repository'
if ([string]::IsNullOrWhiteSpace($ArtifactRoot)) {
    $ArtifactRoot = Join-Path $repositoryRoot 'artifacts\installer\e2e'
}
$artifactRoot = [IO.Path]::GetFullPath($ArtifactRoot)
$timestamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$installLog = Join-Path $artifactRoot "install-$timestamp.log"
$baselineInstallLog = Join-Path $artifactRoot "baseline-install-$timestamp.log"
$uninstallLog = Join-Path $artifactRoot "uninstall-$timestamp.log"
$script:requestId = 0L
$protocolVersion = 3
$installedByTest = $false
$installedPackagePath = $null
$onboardingMarkerCreatedByTest = $false
$testReachedPersistenceCheck = $false

function Invoke-Installer([string] $Arguments, [string] $Action) {
    $process = Start-Process -FilePath "$env:SystemRoot\System32\msiexec.exe" `
        -ArgumentList $Arguments `
        -WindowStyle Hidden `
        -Wait `
        -PassThru
    if ($process.ExitCode -ne 0) {
        throw "$Action failed with Windows Installer exit code $($process.ExitCode)."
    }
}

function Wait-InteractiveProcess([string] $Name, [TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $process = Get-Process -Name $Name -ErrorAction SilentlyContinue |
            Where-Object SessionId -eq $interactiveSessionId |
            Select-Object -First 1
        if ($null -ne $process) {
            return $process
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for $Name in interactive session $interactiveSessionId."
}

function Wait-Path([string] $Path, [TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        if (Test-Path -LiteralPath $Path) {
            return
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for $Path."
}

function Wait-AutomationElement(
    [Diagnostics.Process] $Process,
    [string] $AutomationId,
    [TimeSpan] $Timeout
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $Process.Refresh()
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

function Wait-AutomationElementOnscreen(
    [Diagnostics.Process] $Process,
    [string] $AutomationId,
    [TimeSpan] $Timeout
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        try {
            $element = Wait-AutomationElement $Process $AutomationId ([TimeSpan]::FromSeconds(2))
            if (-not $element.Current.IsOffscreen) {
                return $element
            }
        } catch {
            # Retry when a WinUI layout pass replaces the automation element.
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for UI element $AutomationId to come on-screen."
}

function Wait-NativeWindowForProcess(
    [Diagnostics.Process] $Process,
    [string] $ClassName,
    [TimeSpan] $Timeout
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $Process.Refresh()
        if ($Process.HasExited) {
            throw "$($Process.ProcessName) exited while waiting for native window $ClassName."
        }
        $window = [ResticPalNativeTest]::FindWindowForProcess($Process.Id, $ClassName)
        if ($window -ne [IntPtr]::Zero) {
            return $window
        }
        Start-Sleep -Milliseconds 100
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for $($Process.ProcessName) native window $ClassName."
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

function Invoke-ResticPalRequest([hashtable] $Command) {
    $script:requestId += 1
    $request = [ordered]@{
        protocol_version = $protocolVersion
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
        'ResticPal.v3',
        [IO.Pipes.PipeDirection]::InOut,
        [IO.Pipes.PipeOptions]::None
    )
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

    if ($response.protocol_version -ne $protocolVersion -or $response.request_id -ne $script:requestId) {
        throw 'The service returned a mismatched IPC response.'
    }
    return $response.payload
}

function Assert-Accepted([hashtable] $Command) {
    $payload = Invoke-ResticPalRequest $Command
    if ($payload.type -ne 'accepted') {
        throw "The service rejected '$($Command.type)': $($payload.code) $($payload.message)"
    }
}

function Wait-RepositoryOperation([string] $Operation, [TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $payload = Invoke-ResticPalRequest @{ type = 'get_repository' }
        if ($payload.type -ne 'repository') {
            throw 'The service did not return repository status.'
        }
        $operationStatus = $payload.configuration.operation_status
        if ($operationStatus.state -eq 'succeeded' -and $operationStatus.operation -eq $Operation) {
            return
        }
        if ($operationStatus.state -eq 'failed') {
            throw "Repository $Operation failed: $($operationStatus.code)"
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for repository $Operation."
}

function Wait-Backup([TimeSpan] $Timeout, [string] $PreviousSnapshotId = '') {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $payload = Invoke-ResticPalRequest @{ type = 'get_run_history'; limit = 10 }
        if ($payload.type -ne 'run_history') {
            throw 'The service did not return backup history.'
        }
        if ($payload.runs.Count -gt 0) {
            $run = $payload.runs[0]
            if (-not [string]::IsNullOrWhiteSpace($PreviousSnapshotId) -and $run.snapshot_id -eq $PreviousSnapshotId) {
                Start-Sleep -Milliseconds 500
                continue
            }
            if ($run.outcome -ne 'succeeded') {
                throw "Installed-service backup failed: $($run.outcome) $($run.error_code)"
            }
            if ([string]::IsNullOrWhiteSpace($run.snapshot_id)) {
                throw 'Installed-service backup succeeded without a snapshot identifier.'
            }
            return $run
        }
        Start-Sleep -Milliseconds 500
    } while ([DateTime]::UtcNow -lt $deadline)
    throw 'Timed out waiting for the installed-service backup.'
}

function Wait-DiagnosticEvents([string[]] $EventIds, [TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $payload = Invoke-ResticPalRequest @{ type = 'get_diagnostics'; limit = 100 }
        if ($payload.type -ne 'diagnostics') {
            throw 'The service did not return operational diagnostics.'
        }
        $observed = @($payload.entries.event_id)
        if (@($EventIds | Where-Object { $observed -notcontains $_ }).Count -eq 0) {
            return $payload
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for diagnostic events: $($EventIds -join ', ')"
}

if ($null -ne (Get-Service -Name ResticPal -ErrorAction SilentlyContinue)) {
    throw 'A ResticPal service already exists; refusing to modify an existing installation.'
}
if (Test-Path -LiteralPath $installRoot) {
    throw "The install directory already exists: $installRoot"
}
if (Test-Path -LiteralPath $dataRoot) {
    throw "The data directory already exists: $dataRoot"
}
if (Test-Path -LiteralPath $onboardingMarker -PathType Leaf) {
    throw "The current user already has a first-run marker; use Windows Sandbox for a clean onboarding test: $onboardingMarker"
}
New-Item -ItemType Directory -Path $artifactRoot -Force | Out-Null

try {
    if ($null -ne $resolvedUpgradeFromMsiPath) {
        Write-Host "Installing upgrade baseline $resolvedUpgradeFromMsiPath"
        Invoke-Installer "/i `"$resolvedUpgradeFromMsiPath`" /qn /norestart /l*v `"$baselineInstallLog`"" 'Baseline installation'
        $installedByTest = $true
        $installedPackagePath = $resolvedUpgradeFromMsiPath
        $baselineService = Get-Service -Name ResticPal
        $baselineService.WaitForStatus(
            [ServiceProcess.ServiceControllerStatus]::Running,
            [TimeSpan]::FromSeconds(30)
        )
        $upgradeSentinel = Join-Path $dataRoot 'upgrade-sentinel.txt'
        Set-Content -LiteralPath $upgradeSentinel -Value 'preserve across major upgrade' -NoNewline
        Write-Host "Upgrading the baseline installation to $resolvedMsiPath"
    }
    Write-Host "Installing $resolvedMsiPath"
    Invoke-Installer "/i `"$resolvedMsiPath`" /qn /norestart /l*v `"$installLog`"" 'Installation'
    $installedByTest = $true
    $installedPackagePath = $resolvedMsiPath
    if ($null -ne $resolvedUpgradeFromMsiPath -and -not (Test-Path -LiteralPath $upgradeSentinel -PathType Leaf)) {
        throw 'The major upgrade did not preserve existing machine data.'
    }

    $service = Get-Service -Name ResticPal
    $service.WaitForStatus([ServiceProcess.ServiceControllerStatus]::Running, [TimeSpan]::FromSeconds(30))
    $serviceConfiguration = Get-CimInstance Win32_Service -Filter "Name='ResticPal'"
    if ($serviceConfiguration.StartName -ne 'LocalSystem') {
        throw "Unexpected service identity: $($serviceConfiguration.StartName)"
    }
    if ($serviceConfiguration.StartMode -ne 'Auto') {
        throw "Unexpected service start mode: $($serviceConfiguration.StartMode)"
    }
    foreach ($fileName in @('resticpal-service.exe', 'resticpal-tray.exe', 'resticpal-ui.exe', 'restic.exe')) {
        if (-not (Test-Path -LiteralPath (Join-Path $installRoot $fileName) -PathType Leaf)) {
            throw "Installed payload is missing $fileName"
        }
    }
    $runValue = Get-ItemPropertyValue -LiteralPath 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Run' -Name ResticPal
    if ($runValue -notlike '*resticpal-tray.exe*') {
        throw 'The tray logon registration is missing or invalid.'
    }
    $trayProcess = Wait-InteractiveProcess 'resticpal-tray' ([TimeSpan]::FromSeconds(30))
    Write-Host "Tray process $($trayProcess.Id) started in the installing user's session."

    Wait-Path $startMenuShortcut ([TimeSpan]::FromSeconds(10))

    $status = Invoke-ResticPalRequest @{ type = 'get_status' }
    if ($status.type -ne 'status' -or $status.status.state.state -ne 'unconfigured') {
        throw 'A fresh installed service did not report the expected unconfigured state.'
    }
    $setupUiLaunchedAfterUpgrade = $false
    $existingUiProcess = Get-Process -Name 'resticpal-ui' -ErrorAction SilentlyContinue |
        Where-Object SessionId -eq $interactiveSessionId |
        Select-Object -First 1
    if ($null -ne $resolvedUpgradeFromMsiPath `
        -and $null -eq $existingUiProcess `
        -and (Test-Path -LiteralPath $onboardingMarker -PathType Leaf)) {
        Start-Process -FilePath (Join-Path $installRoot 'resticpal-ui.exe') -ArgumentList '--setup'
        $setupUiLaunchedAfterUpgrade = $true
    }
    $uiProcess = Wait-InteractiveProcess 'resticpal-ui' ([TimeSpan]::FromSeconds(30))
    Wait-Path $onboardingMarker ([TimeSpan]::FromSeconds(30))
    $onboardingMarkerCreatedByTest = $true
    Start-Sleep -Seconds 2
    $onboardingUiProcesses = @(Get-Process -Name 'resticpal-ui' -ErrorAction SilentlyContinue |
        Where-Object SessionId -eq $interactiveSessionId)
    if ($onboardingUiProcesses.Count -ne 1) {
        throw "Expected one first-run settings process, found $($onboardingUiProcesses.Count)."
    }

    $duplicateUiProcess = Start-Process `
        -FilePath (Join-Path $installRoot 'resticpal-ui.exe') `
        -ArgumentList '--setup' `
        -PassThru
    if (-not $duplicateUiProcess.WaitForExit(10000)) {
        throw 'A duplicate settings launch did not yield to the existing window.'
    }
    $remainingUiProcesses = @(Get-Process -Name 'resticpal-ui' -ErrorAction SilentlyContinue |
        Where-Object SessionId -eq $interactiveSessionId)
    if ($remainingUiProcesses.Count -ne 1 -or $remainingUiProcesses[0].Id -ne $uiProcess.Id) {
        throw 'The settings single-instance boundary did not preserve the first-run window.'
    }
    if ($setupUiLaunchedAfterUpgrade) {
        Write-Host "Upgrade preserved the first-run marker; explicit setup process $($uiProcess.Id) opened correctly."
    } else {
        Write-Host "First-run setup process $($uiProcess.Id) opened for bootstrap or local configuration."
    }
    Stop-Process -Id $uiProcess.Id -Force
    $uiProcess.WaitForExit(10000) | Out-Null

    $trayProcess.Refresh()
    if ($trayProcess.HasExited) {
        throw "The installed tray exited before click testing with code $($trayProcess.ExitCode)."
    }
    $trayWindow = [ResticPalNativeTest]::FindWindowForProcess(
        $trayProcess.Id,
        'ResticPalTrayWindow'
    )
    if ($trayWindow -eq [IntPtr]::Zero) {
        throw 'The installed tray hidden window was not found.'
    }
    [uint32] $trayWindowProcessId = 0
    [void] [ResticPalNativeTest]::GetWindowThreadProcessId(
        $trayWindow,
        [ref] $trayWindowProcessId
    )
    if ($trayWindowProcessId -ne $trayProcess.Id) {
        throw "The tray window belongs to unexpected process $trayWindowProcessId."
    }

    if (-not [ResticPalNativeTest]::PostMessage(
        $trayWindow,
        0x8001,
        [IntPtr]::Zero,
        [IntPtr] 0x0202
    )) {
        throw 'Posting the tray left-click callback failed.'
    }
    $leftClickUiProcess = Wait-InteractiveProcess 'resticpal-ui' ([TimeSpan]::FromSeconds(30))
    Wait-AutomationElement $leftClickUiProcess 'SettingsItem' ([TimeSpan]::FromSeconds(30)) | Out-Null
    Write-Host "A single tray left click opened settings as process $($leftClickUiProcess.Id)."
    Stop-Process -Id $leftClickUiProcess.Id -Force
    $leftClickUiProcess.WaitForExit(10000) | Out-Null

    if (-not [ResticPalNativeTest]::PostMessage(
        $trayWindow,
        0x8001,
        [IntPtr]::Zero,
        [IntPtr] 0x0205
    )) {
        throw 'Posting the tray right-click callback failed.'
    }
    Wait-NativeWindowForProcess $trayProcess '#32768' ([TimeSpan]::FromSeconds(10)) | Out-Null
    Write-Host 'A tray right click opened the native action menu.'
    if (-not [ResticPalNativeTest]::PostMessage(
        $trayWindow,
        0x001F,
        [IntPtr]::Zero,
        [IntPtr]::Zero
    )) {
        throw 'Closing the tray action menu failed.'
    }

    Start-Process -FilePath $startMenuShortcut
    $startMenuUiProcess = Wait-InteractiveProcess 'resticpal-ui' ([TimeSpan]::FromSeconds(30))
    Write-Host "The all-users Start Menu shortcut opened settings as process $($startMenuUiProcess.Id)."
    $settingsItem = Wait-AutomationElement $startMenuUiProcess 'SettingsItem' ([TimeSpan]::FromSeconds(30))
    $settingsSelection = $settingsItem.GetCurrentPattern(
        [Windows.Automation.SelectionItemPattern]::Pattern
    )
    $settingsSelection.Select()
    Wait-AutomationElement $startMenuUiProcess 'ManagementStatusTitle' ([TimeSpan]::FromSeconds(10)) | Out-Null
    Wait-AutomationElement $startMenuUiProcess 'CheckForUpdatesButton' ([TimeSpan]::FromSeconds(10)) | Out-Null
    Wait-AutomationElement $startMenuUiProcess 'AutomaticUpdatesToggle' ([TimeSpan]::FromSeconds(10)) | Out-Null
    Write-Host 'The WinUI Settings page exposes enrollment and signed-update controls.'
    Stop-Process -Id $startMenuUiProcess.Id -Force
    $startMenuUiProcess.WaitForExit(10000) | Out-Null
    Start-Process -FilePath (Join-Path $installRoot 'resticpal-ui.exe') -ArgumentList '--updates'
    $updatesUiProcess = Wait-InteractiveProcess 'resticpal-ui' ([TimeSpan]::FromSeconds(30))
    Wait-AutomationElementOnscreen $updatesUiProcess 'CheckForUpdatesButton' ([TimeSpan]::FromSeconds(30)) | Out-Null
    Write-Host 'The --updates launch opens the signed-update controls in the visible viewport.'

    $updateSettings = Invoke-ResticPalRequest @{ type = 'get_update_settings' }
    if ($updateSettings.type -ne 'update_settings' -or $updateSettings.configuration.automatic_install) {
        throw 'Automatic update installation was not disabled by default.'
    }
    $invalidUpdate = Invoke-ResticPalRequest @{
        type = 'install_update'
        package = @{
            version = '99.0.0'
            url = 'https://example.test/resticpal-99.0.0-x64.msi'
            signature = ('A' * 88)
            length = 1024
        }
    }
    if ($invalidUpdate.type -ne 'rejected' -or $invalidUpdate.code -ne 'update_metadata_invalid') {
        throw 'The service accepted update metadata outside the pinned GitHub release path.'
    }
    Assert-Accepted @{ type = 'update_update_settings'; automatic_install = $true }
    $updateSettings = Invoke-ResticPalRequest @{ type = 'get_update_settings' }
    if (-not $updateSettings.configuration.automatic_install) {
        throw 'Automatic update installation was not enabled through IPC.'
    }

    New-Item -ItemType Directory -Path $sourceRoot -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $sourceRoot 'document.txt') -Value 'resticpal installed-service end-to-end data' -NoNewline

    Assert-Accepted @{
        type = 'update_repository'
        display_name = 'Disposable installed-service repository'
        url = $backupRoot
        mode = 'standard'
        options = @{}
        secret_updates = @(@{
            action = 'set'
            variable = 'RESTIC_PASSWORD'
            value = 'resticpal-e2e-disposable-password'
        })
    }
    Assert-Accepted @{ type = 'initialize_repository' }
    Wait-RepositoryOperation 'initialize' ([TimeSpan]::FromMinutes(2))

    Assert-Accepted @{
        type = 'update_repository'
        display_name = $null
        url = $null
        mode = 'append_only'
        options = $null
        secret_updates = @()
    }
    Assert-Accepted @{ type = 'validate_repository' }
    Wait-RepositoryOperation 'validate' ([TimeSpan]::FromMinutes(2))

    Assert-Accepted @{
        type = 'update_backup_sources'
        paths = @($sourceRoot)
        exclusions = @()
    }
    $runRequest = Invoke-ResticPalRequest @{ type = 'run_backup_now' }
    if ($runRequest.type -ne 'accepted' -and -not (
        $runRequest.type -eq 'rejected' -and $runRequest.code -eq 'already_running'
    )) {
        throw "The service rejected 'run_backup_now': $($runRequest.code) $($runRequest.message)"
    }
    $run = Wait-Backup ([TimeSpan]::FromMinutes(3))
    Write-Host "Append-only backup snapshot $($run.snapshot_id) completed through the installed service."

    $appendOnlyRetention = Invoke-ResticPalRequest @{ type = 'get_retention' }
    if ($appendOnlyRetention.type -ne 'retention' -or $appendOnlyRetention.configuration.repository_mode -ne 'append_only') {
        throw 'The installed service did not report server-managed append-only retention.'
    }
    $appendOnlyUpdate = Invoke-ResticPalRequest @{
        type = 'update_retention'
        daily = 14
        weekly = $null
        monthly = $null
        yearly = $null
        prune_interval_days = $null
    }
    if ($appendOnlyUpdate.type -ne 'rejected' -or $appendOnlyUpdate.code -ne 'retention_managed_by_server') {
        throw 'The installed service allowed local retention changes in append-only mode.'
    }

    Assert-Accepted @{
        type = 'update_repository'
        display_name = $null
        url = $null
        mode = 'standard'
        options = $null
        secret_updates = @()
    }
    Set-Content -LiteralPath (Join-Path $sourceRoot 'second-document.txt') -Value 'standard retention end-to-end data' -NoNewline
    Assert-Accepted @{ type = 'run_backup_now' }
    $standardRun = Wait-Backup ([TimeSpan]::FromMinutes(3)) $run.snapshot_id
    $retention = Invoke-ResticPalRequest @{ type = 'get_retention' }
    if ($retention.type -ne 'retention' `
        -or $retention.configuration.repository_mode -ne 'standard' `
        -or $null -eq $retention.configuration.last_retention `
        -or $null -eq $retention.configuration.last_prune `
        -or $null -ne $retention.configuration.last_error) {
        throw 'Standard-mode retention and prune state was not recorded after backup.'
    }
    $diagnostics = Wait-DiagnosticEvents @('retention.succeeded', 'backup.succeeded') ([TimeSpan]::FromSeconds(10))
    $diagnosticJson = $diagnostics | ConvertTo-Json -Compress -Depth 12
    if ($diagnosticJson.Contains($sourceRoot) -or $diagnosticJson.Contains($backupRoot)) {
        throw 'Operational diagnostics disclosed a source or repository path.'
    }
    Write-Host "Standard backup snapshot $($standardRun.snapshot_id) completed with local retention and prune."

    Restart-Service -Name ResticPal -Force
    (Get-Service -Name ResticPal).WaitForStatus(
        [ServiceProcess.ServiceControllerStatus]::Running,
        [TimeSpan]::FromSeconds(30)
    )
    $historyAfterRestart = Invoke-ResticPalRequest @{ type = 'get_run_history'; limit = 1 }
    if ($historyAfterRestart.runs.Count -ne 1 -or $historyAfterRestart.runs[0].snapshot_id -ne $standardRun.snapshot_id) {
        throw 'Backup history did not survive the installed service restart.'
    }
    $retentionAfterRestart = Invoke-ResticPalRequest @{ type = 'get_retention' }
    if ($null -eq $retentionAfterRestart.configuration.last_retention `
        -or $null -eq $retentionAfterRestart.configuration.last_prune) {
        throw 'Retention state did not survive the installed service restart.'
    }
    $updatesAfterRestart = Invoke-ResticPalRequest @{ type = 'get_update_settings' }
    if (-not $updatesAfterRestart.configuration.automatic_install) {
        throw 'The automatic-update setting did not survive the installed service restart.'
    }
    $testReachedPersistenceCheck = $true
} finally {
    if ($installedByTest -and -not $KeepInstalled) {
        Write-Host 'Uninstalling the end-to-end package...'
        Invoke-Installer "/x `"$installedPackagePath`" /qn /norestart /l*v `"$uninstallLog`"" 'Uninstallation'
        if ($null -ne (Get-Service -Name ResticPal -ErrorAction SilentlyContinue)) {
            throw 'The ResticPal service still exists after uninstall.'
        }
        if (Test-Path -LiteralPath $installRoot) {
            throw 'The resticpal install directory still exists after uninstall.'
        }
        if (Test-Path -LiteralPath $startMenuShortcut) {
            throw 'The all-users Start Menu shortcut still exists after uninstall.'
        }
        $remainingRunKey = Get-ItemProperty `
            -LiteralPath 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Run' `
            -ErrorAction SilentlyContinue
        $remainingRunValue = $remainingRunKey.ResticPal
        if ($null -ne $remainingRunValue) {
            throw 'The all-users tray logon registration still exists after uninstall.'
        }
        foreach ($processName in @('resticpal-tray', 'resticpal-ui')) {
            $remainingProcess = Get-Process -Name $processName -ErrorAction SilentlyContinue |
                Where-Object SessionId -eq $interactiveSessionId
            if ($null -ne $remainingProcess) {
                throw "$processName is still running in the interactive session after uninstall."
            }
        }
        if (-not (Test-Path -LiteralPath $dataRoot)) {
            throw 'Uninstall removed machine backup data instead of preserving it.'
        }
        if ($testReachedPersistenceCheck) {
            Write-Host 'Install, backup, restart, persistence, and uninstall checks passed.'
        }
        if ($onboardingMarkerCreatedByTest) {
            Remove-Item -LiteralPath $onboardingMarker -Force
        }
        Remove-Item -LiteralPath $dataRoot -Recurse -Force
    }
}
