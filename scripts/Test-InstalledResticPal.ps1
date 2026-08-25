[CmdletBinding()]
param(
    [string] $MsiPath,
    [string] $UpgradeFromMsiPath,
    [switch] $KeepInstalled,
    [string] $ArtifactRoot
)

$ErrorActionPreference = 'Stop'
$testStartedAt = Get-Date
Add-Type -AssemblyName UIAutomationClient
Add-Type -AssemblyName UIAutomationTypes
Add-Type -AssemblyName System.Windows.Forms
Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Diagnostics;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Security.Principal;
using System.Text;

public static class ResticPalNativeTest
{
    private delegate bool EnumThreadWindowsCallback(IntPtr window, IntPtr parameter);

    [StructLayout(LayoutKind.Sequential)]
    private struct SidAndAttributes
    {
        public IntPtr sid;
        public uint attributes;
    }

    [DllImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool OpenProcessToken(
        IntPtr process,
        uint desiredAccess,
        out IntPtr token);

    [DllImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetTokenInformation(
        IntPtr token,
        int informationClass,
        out int information,
        int informationLength,
        out int returnLength);

    [DllImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool DuplicateTokenEx(
        IntPtr existingToken,
        uint desiredAccess,
        IntPtr attributes,
        int impersonationLevel,
        int tokenType,
        out IntPtr duplicateToken);

    [DllImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CreateRestrictedToken(
        IntPtr existingToken,
        uint flags,
        uint disabledSidCount,
        ref SidAndAttributes disabledSids,
        uint deletedPrivilegeCount,
        IntPtr deletedPrivileges,
        uint restrictedSidCount,
        IntPtr restrictedSids,
        out IntPtr restrictedToken);

    [DllImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetTokenInformation(
        IntPtr token,
        int informationClass,
        IntPtr information,
        uint informationLength);

    [DllImport("advapi32.dll", EntryPoint = "GetTokenInformation", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetTokenInformationBuffer(
        IntPtr token,
        int informationClass,
        IntPtr information,
        int informationLength,
        out int returnLength);

    [DllImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool ImpersonateLoggedOnUser(IntPtr token);

    [DllImport("advapi32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool RevertToSelf();

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr CreateNamedPipeW(
        string name,
        uint openMode,
        uint pipeMode,
        uint maximumInstances,
        uint outputBufferSize,
        uint inputBufferSize,
        uint defaultTimeout,
        IntPtr securityAttributes);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetComputerNameExW(
        int nameType,
        StringBuilder computerName,
        ref uint bufferLength);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CloseHandle(IntPtr handle);

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

    [DllImport("user32.dll")]
    private static extern IntPtr GetWindowDpiAwarenessContext(IntPtr window);

    [DllImport("user32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool AreDpiAwarenessContextsEqual(
        IntPtr first,
        IntPtr second);

    [DllImport("user32.dll")]
    public static extern uint GetDpiForWindow(IntPtr window);

    public static bool IsPerMonitorV2(IntPtr window)
    {
        return window != IntPtr.Zero && AreDpiAwarenessContextsEqual(
            GetWindowDpiAwarenessContext(window),
            new IntPtr(-4));
    }

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

    public static int ProbeFirstNamedPipeInstance(string pipeName)
    {
        IntPtr handle = CreateNamedPipeW(
            pipeName,
            0x00080003,
            0,
            255,
            4096,
            4096,
            0,
            IntPtr.Zero);
        if (handle != new IntPtr(-1))
        {
            CloseHandle(handle);
            throw new InvalidOperationException(
                "An outside process unexpectedly became the first service pipe instance.");
        }
        return Marshal.GetLastWin32Error();
    }

    public static string PhysicalDnsHostName()
    {
        uint capacity = 64;
        while (capacity <= 1024)
        {
            uint length = capacity;
            var name = new StringBuilder((int)capacity);
            if (GetComputerNameExW(5, name, ref length))
            {
                string result = name.ToString();
                if (result.Length == 0)
                {
                    throw new InvalidOperationException(
                        "Windows returned an empty physical DNS hostname.");
                }
                return result;
            }

            int error = Marshal.GetLastWin32Error();
            if (error != 234 || length <= capacity)
            {
                throw new Win32Exception(error,
                    "Could not determine restic's physical DNS host identity.");
            }
            capacity = length;
        }
        throw new InvalidOperationException(
            "The physical DNS hostname exceeded the safe Windows host-name limit.");
    }

    public static void VerifyReadWithNonElevatedExplorerToken(
        int explorerProcessId,
        string expectedUserSid,
        string restoredPath,
        string expectedSha256)
    {
        IntPtr explorerToken = IntPtr.Zero;
        IntPtr callerToken = IntPtr.Zero;
        IntPtr interactiveToken = IntPtr.Zero;
        IntPtr administratorsSid = IntPtr.Zero;
        IntPtr mediumIntegritySid = IntPtr.Zero;
        IntPtr integrityLabel = IntPtr.Zero;
        try
        {
            using (Process explorer = Process.GetProcessById(explorerProcessId))
            {
                if (!OpenProcessToken(explorer.Handle, 0x008B, out explorerToken))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(),
                        "Could not inspect the interactive Explorer token.");
                }
            }

            int elevated;
            int returnedLength;
            if (!GetTokenInformation(explorerToken, 20, out elevated, sizeof(int),
                    out returnedLength))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(),
                    "Could not determine whether the interactive Explorer token is elevated.");
            }
            if (elevated != 0)
            {
                using (Process currentProcess = Process.GetCurrentProcess())
                {
                    if (!OpenProcessToken(currentProcess.Handle, 0x008B,
                            out callerToken))
                    {
                        throw new Win32Exception(Marshal.GetLastWin32Error(),
                            "Could not inspect the caller's primary token.");
                    }
                }

                SecurityIdentifier administratorIdentity = new SecurityIdentifier(
                    WellKnownSidType.BuiltinAdministratorsSid, null);
                byte[] administratorBytes = new byte[administratorIdentity.BinaryLength];
                administratorIdentity.GetBinaryForm(administratorBytes, 0);
                administratorsSid = Marshal.AllocHGlobal(administratorBytes.Length);
                Marshal.Copy(administratorBytes, 0, administratorsSid,
                    administratorBytes.Length);
                SidAndAttributes denyAdministrators = new SidAndAttributes();
                denyAdministrators.sid = administratorsSid;
                denyAdministrators.attributes = 0;
                // Restrict the calling process's own primary token. Same-user
                // impersonation works even when Application Guard does not
                // grant its administrator SeImpersonatePrivilege.
                if (!CreateRestrictedToken(callerToken, 0x00000001, 1,
                        ref denyAdministrators, 0, IntPtr.Zero, 0, IntPtr.Zero,
                        out interactiveToken))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(),
                        "Could not derive a same-user non-administrator caller token.");
                }

                SecurityIdentifier mediumIdentity =
                    new SecurityIdentifier("S-1-16-8192");
                byte[] mediumBytes = new byte[mediumIdentity.BinaryLength];
                mediumIdentity.GetBinaryForm(mediumBytes, 0);
                mediumIntegritySid = Marshal.AllocHGlobal(mediumBytes.Length);
                Marshal.Copy(mediumBytes, 0, mediumIntegritySid, mediumBytes.Length);
                SidAndAttributes mediumLabel = new SidAndAttributes();
                mediumLabel.sid = mediumIntegritySid;
                mediumLabel.attributes = 0x00000020;
                int labelLength = Marshal.SizeOf(typeof(SidAndAttributes)) +
                    mediumBytes.Length;
                integrityLabel = Marshal.AllocHGlobal(labelLength);
                Marshal.StructureToPtr(mediumLabel, integrityLabel, false);
                if (!SetTokenInformation(interactiveToken, 25, integrityLabel,
                        (uint)labelLength))
                {
                    throw new Win32Exception(Marshal.GetLastWin32Error(),
                        "Could not lower the restricted Explorer token to medium integrity.");
                }
            }
            else if (!DuplicateTokenEx(explorerToken, 0x02000000, IntPtr.Zero, 2, 1,
                    out interactiveToken))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(),
                    "Could not duplicate the interactive user's standard token.");
            }

            AssertMediumIntegrity(interactiveToken);
            if (!ImpersonateLoggedOnUser(interactiveToken))
            {
                int impersonationError = Marshal.GetLastWin32Error();
                throw new Win32Exception(impersonationError,
                    "Could not impersonate the same-user non-administrator token " +
                    "(Win32 " + impersonationError + ": " +
                    new Win32Exception(impersonationError).Message + ").");
            }
            try
            {
                using (WindowsIdentity identity = WindowsIdentity.GetCurrent())
                {
                    if (identity.User == null ||
                        !String.Equals(identity.User.Value, expectedUserSid,
                            StringComparison.Ordinal))
                    {
                        throw new InvalidOperationException(
                            "The restored-file probe impersonated the wrong user.");
                    }
                    if (new WindowsPrincipal(identity).IsInRole(
                            WindowsBuiltInRole.Administrator))
                    {
                        throw new InvalidOperationException(
                            "The restored-file probe retained administrator access.");
                    }
                    AssertMediumIntegrity(identity.Token);

                    // Open the handle only AFTER impersonation: NTFS therefore
                    // evaluates this real read against the standard-user token.
                    using (FileStream restored = new FileStream(restoredPath,
                        FileMode.Open, FileAccess.Read, FileShare.Read))
                    using (SHA256 sha256 = SHA256.Create())
                    {
                        string observed = BitConverter.ToString(
                            sha256.ComputeHash(restored)).Replace("-", String.Empty);
                        if (!String.Equals(observed, expectedSha256,
                                StringComparison.OrdinalIgnoreCase))
                        {
                            throw new InvalidOperationException(
                                "The non-administrator restored-file SHA-256 differs.");
                        }
                    }
                }
            }
            finally
            {
                if (!RevertToSelf())
                {
                    int revertError = Marshal.GetLastWin32Error();
                    throw new Win32Exception(revertError,
                        "Could not revert the restored-file probe impersonation " +
                        "(Win32 " + revertError + ": " +
                        new Win32Exception(revertError).Message + ").");
                }
            }
        }
        finally
        {
            if (interactiveToken != IntPtr.Zero) CloseHandle(interactiveToken);
            if (callerToken != IntPtr.Zero) CloseHandle(callerToken);
            if (explorerToken != IntPtr.Zero) CloseHandle(explorerToken);
            if (integrityLabel != IntPtr.Zero) Marshal.FreeHGlobal(integrityLabel);
            if (mediumIntegritySid != IntPtr.Zero) Marshal.FreeHGlobal(mediumIntegritySid);
            if (administratorsSid != IntPtr.Zero) Marshal.FreeHGlobal(administratorsSid);
        }
    }

    private static void AssertMediumIntegrity(IntPtr token)
    {
        int integrityLength;
        GetTokenInformationBuffer(token, 25, IntPtr.Zero, 0, out integrityLength);
        if (integrityLength <= 0)
        {
            throw new Win32Exception(Marshal.GetLastWin32Error(),
                "Could not determine the standard-token integrity buffer length.");
        }
        IntPtr observedIntegrity = Marshal.AllocHGlobal(integrityLength);
        try
        {
            if (!GetTokenInformationBuffer(token, 25, observedIntegrity,
                    integrityLength, out integrityLength))
            {
                throw new Win32Exception(Marshal.GetLastWin32Error(),
                    "Could not inspect the effective standard-token integrity level.");
            }
            SidAndAttributes observedLabel = (SidAndAttributes)
                Marshal.PtrToStructure(observedIntegrity, typeof(SidAndAttributes));
            string integritySid = new SecurityIdentifier(observedLabel.sid).Value;
            int integrityRid = int.Parse(integritySid.Substring(
                integritySid.LastIndexOf('-') + 1));
            if (integrityRid > 8192)
            {
                throw new InvalidOperationException(
                    "The restored-file probe retained elevated integrity " + integritySid + ".");
            }
        }
        finally
        {
            Marshal.FreeHGlobal(observedIntegrity);
        }
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
$cacheRoot = Join-Path $dataRoot 'Cache'
$startMenuShortcut = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\resticpal.lnk'
$onboardingMarker = Join-Path $env:LOCALAPPDATA 'resticpal\onboarding-shown-v1'
$interactiveSessionId = [Diagnostics.Process]::GetCurrentProcess().SessionId
$resticHostName = [ResticPalNativeTest]::PhysicalDnsHostName()
Write-Host (
    "Restic snapshot identity uses physical DNS hostname '$resticHostName'; " +
    "the NetBIOS COMPUTERNAME is '$env:COMPUTERNAME'."
)
$e2eRoot = Join-Path $env:SystemDrive (
    'ResticPal-Installed-E2E-' + [Guid]::NewGuid().ToString('N'))
$sourceRoot = Join-Path $e2eRoot 'Source'
$backupRoot = Join-Path $e2eRoot 'Repository'
$restoreRoot = Join-Path $e2eRoot 'Restores'
if ([string]::IsNullOrWhiteSpace($ArtifactRoot)) {
    $ArtifactRoot = Join-Path $repositoryRoot 'artifacts\installer\e2e'
}
$artifactRoot = [IO.Path]::GetFullPath($ArtifactRoot)
$timestamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$installLog = Join-Path $artifactRoot "install-$timestamp.log"
$baselineInstallLog = Join-Path $artifactRoot "baseline-install-$timestamp.log"
$uninstallLog = Join-Path $artifactRoot "uninstall-$timestamp.log"
$script:requestId = 0L
$protocolVersion = 5
$installedByTest = $false
$installedPackagePath = $null
$onboardingMarkerCreatedByTest = $false
$testReachedPersistenceCheck = $false
$candidateFileVersion = $null
$cacheStateAfterFirstBackup = $null
$appendOnlyFileRestoreDestination = $null
$appendOnlyFolderRestoreDestination = $null

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

function Export-SettingsProcessSnapshot([string] $Path) {
    $processes = @(
        Get-Process -Name 'resticpal-ui' -ErrorAction SilentlyContinue |
            Where-Object SessionId -eq $interactiveSessionId |
            ForEach-Object {
                $_.Refresh()
                $cimProcess = Get-CimInstance Win32_Process -Filter "ProcessId=$($_.Id)" -ErrorAction SilentlyContinue
                $imagePath = try { $_.Path } catch { $null }
                $fileVersion = if (-not [string]::IsNullOrWhiteSpace($imagePath) -and
                    (Test-Path -LiteralPath $imagePath -PathType Leaf)) {
                    [Diagnostics.FileVersionInfo]::GetVersionInfo($imagePath).FileVersion
                } else {
                    $null
                }
                $startTime = try {
                    $_.StartTime.ToUniversalTime().ToString('O')
                } catch {
                    $null
                }
                [pscustomobject]@{
                    process_id = $_.Id
                    session_id = $_.SessionId
                    start_time = $startTime
                    main_window_handle = $_.MainWindowHandle.ToInt64()
                    main_window_title = $_.MainWindowTitle
                    image_path = $imagePath
                    image_file_version = $fileVersion
                    command_line = $cimProcess.CommandLine
                    executable_path = $cimProcess.ExecutablePath
                }
            }
    )
    $processes | ConvertTo-Json -Depth 5 | Set-Content -LiteralPath $Path -Encoding UTF8
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

function Wait-AutomationElementEnabled(
    [Diagnostics.Process] $Process,
    [string] $AutomationId,
    [TimeSpan] $Timeout
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        try {
            $element = Wait-AutomationElement $Process $AutomationId ([TimeSpan]::FromSeconds(2))
            if ($element.Current.IsEnabled) {
                return $element
            }
        } catch {
            # Retry when a WinUI layout pass replaces the automation element.
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for enabled UI element $AutomationId."
}

function Wait-AutomationListRow(
    [Diagnostics.Process] $Process,
    [string] $ListAutomationId,
    [string] $ExpectedName,
    [TimeSpan] $Timeout
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    $rowCondition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::ControlTypeProperty,
        [Windows.Automation.ControlType]::ListItem
    )
    do {
        try {
            $list = Wait-AutomationElement $Process $ListAutomationId ([TimeSpan]::FromSeconds(2))
            $rows = $list.FindAll([Windows.Automation.TreeScope]::Descendants, $rowCondition)
            foreach ($row in $rows) {
                $names = @(
                    $row.Current.Name
                    $row.FindAll(
                        [Windows.Automation.TreeScope]::Descendants,
                        [Windows.Automation.Condition]::TrueCondition
                    ) | ForEach-Object { $_.Current.Name }
                )
                if (@($names | Where-Object {
                    [string]::Equals($_, $ExpectedName, [StringComparison]::OrdinalIgnoreCase)
                }).Count -gt 0) {
                    return $row
                }
            }
        } catch {
            # Retry if a WinUI list refresh replaces an automation element.
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for '$ExpectedName' in UI list $ListAutomationId."
}

function Wait-AutomationElementByName([string] $Name, [TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    $condition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::NameProperty,
        $Name
    )
    do {
        $window = [Windows.Automation.AutomationElement]::RootElement.FindFirst(
            [Windows.Automation.TreeScope]::Descendants,
            $condition
        )
        if ($null -ne $window) {
            return $window
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    $windowCondition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::ControlTypeProperty,
        [Windows.Automation.ControlType]::Window
    )
    $visibleWindowNames = @(
        [Windows.Automation.AutomationElement]::RootElement.FindAll(
            [Windows.Automation.TreeScope]::Children,
            $windowCondition
        ) | ForEach-Object { $_.Current.Name } | Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
    )
    throw "Timed out waiting for automation element '$Name'. Visible windows: $($visibleWindowNames -join '; ')"
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
                ) |
                    ForEach-Object { $_.Current.Name }
            ) -join ' '
            $lastText = $text
            if ($text.IndexOf($ExpectedText, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
                return $text
            }
        } catch {
            # Retry while the async command updates the WinUI InfoBar content.
        }
        Start-Sleep -Milliseconds 250
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for '$ExpectedText' in UI element $AutomationId. Observed: $lastText"
}

function Assert-AutomationTextDoesNotContain(
    [Diagnostics.Process] $Process,
    [string] $AutomationId,
    [string] $ForbiddenText
) {
    $processCondition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::ProcessIdProperty,
        $Process.Id
    )
    $idCondition = [Windows.Automation.PropertyCondition]::new(
        [Windows.Automation.AutomationElement]::AutomationIdProperty,
        $AutomationId
    )
    $condition = [Windows.Automation.AndCondition]::new($processCondition, $idCondition)
    $element = [Windows.Automation.AutomationElement]::RootElement.FindFirst(
        [Windows.Automation.TreeScope]::Descendants,
        $condition
    )
    if ($null -eq $element) {
        return
    }
    $text = @(
        $element.Current.Name
        $element.FindAll(
            [Windows.Automation.TreeScope]::Descendants,
            [Windows.Automation.Condition]::TrueCondition
        ) |
            ForEach-Object { $_.Current.Name }
    ) -join ' '
    if ($text.IndexOf($ForbiddenText, [StringComparison]::OrdinalIgnoreCase) -ge 0) {
        throw "UI element $AutomationId unexpectedly retained '$ForbiddenText': $text"
    }
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

function Get-ResticPalIpcTimeoutDiagnostics {
    $serviceDescription = try {
        $service = Get-Service -Name 'ResticPal' -ErrorAction Stop
        "ResticPal service=$($service.Status)"
    } catch {
        "ResticPal service unavailable=$($_.Exception.Message)"
    }

    $processDescriptions = @(
        Get-Process -Name 'resticpal-service', 'restic', 'resticpal-ui' -ErrorAction SilentlyContinue |
            ForEach-Object {
                "$($_.ProcessName)(pid=$($_.Id),cpu=$($_.CPU),threads=$($_.Threads.Count))"
            }
    )
    $processDescription = if ($processDescriptions.Count -eq 0) {
        '(none)'
    } else {
        $processDescriptions -join ', '
    }

    return "$serviceDescription; relevant processes=$processDescription"
}

function Read-Exact(
    [IO.Stream] $Stream,
    [int] $Count,
    [string] $CommandName,
    [string] $Stage,
    [TimeSpan] $Timeout
) {
    $buffer = [byte[]]::new($Count)
    $offset = 0
    $deadline = [DateTime]::UtcNow + $Timeout
    while ($offset -lt $Count) {
        $remaining = $deadline - [DateTime]::UtcNow
        $remainingMilliseconds = [Math]::Max(
            1,
            [Math]::Min([int]::MaxValue, [int] [Math]::Ceiling($remaining.TotalMilliseconds))
        )
        $readTask = $Stream.ReadAsync($buffer, $offset, $Count - $offset)
        if ($remaining.TotalMilliseconds -le 0 -or -not $readTask.Wait($remainingMilliseconds)) {
            # Closing the asynchronous handle cancels its outstanding read and
            # prevents a stalled service from keeping the test alive forever.
            $Stream.Dispose()
            $diagnostics = Get-ResticPalIpcTimeoutDiagnostics
            throw [TimeoutException]::new(
                "Timed out after $([int] $Timeout.TotalSeconds)s waiting for " +
                "resticpal IPC command '$CommandName' at '$Stage' " +
                "($offset of $Count bytes received). $diagnostics"
            )
        }
        $read = $readTask.GetAwaiter().GetResult()
        if ($read -eq 0) {
            throw (
                "The resticpal service closed its named pipe during command " +
                "'$CommandName' at '$Stage' before completing its response."
            )
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
        "ResticPal.v$protocolVersion",
        [IO.Pipes.PipeDirection]::InOut,
        [IO.Pipes.PipeOptions]::Asynchronous
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

        $responseLengthBytes = Read-Exact `
            $client `
            4 `
            ([string] $Command.type) `
            'response header' `
            ([TimeSpan]::FromSeconds(15))
        if (-not [BitConverter]::IsLittleEndian) {
            [Array]::Reverse($responseLengthBytes)
        }
        $responseLength = [BitConverter]::ToUInt32($responseLengthBytes, 0)
        if ($responseLength -eq 0 -or $responseLength -gt 1024 * 1024) {
            throw "Invalid incoming IPC frame length: $responseLength"
        }
        $responseBytes = Read-Exact `
            $client `
            ([int] $responseLength) `
            ([string] $Command.type) `
            'response body' `
            ([TimeSpan]::FromSeconds(15))
        $response = $utf8.GetString($responseBytes) | ConvertFrom-Json
    } finally {
        $client.Dispose()
    }

    if ($response.protocol_version -ne $protocolVersion -or $response.request_id -ne $script:requestId) {
        throw 'The service returned a mismatched IPC response.'
    }
    return $response.payload
}

function Assert-ServicePipeContinuouslyOwned([string] $Phase) {
    $pipeName = "\\.\pipe\ResticPal.v$protocolVersion"
    for ($attempt = 0; $attempt -lt 8; $attempt++) {
        $errorCode = [ResticPalNativeTest]::ProbeFirstNamedPipeInstance($pipeName)
        if ($errorCode -ne 5) {
            throw ("The $Phase LocalSystem service pipe was not continuously protected " +
                   "by its first instance (Win32 error $errorCode).")
        }
        if ($attempt -lt 7) {
            $status = Invoke-ResticPalRequest @{ type = 'get_status' }
            if ($status.type -ne 'status') {
                throw "The $Phase pipe continuity probe interrupted productive service IPC."
            }
        }
    }
    Write-Host "The $Phase LocalSystem service continuously owned its protocol-v$protocolVersion pipe."
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

function Wait-RestoreQueryPage(
    [UInt64] $QueryId,
    [string] $ExpectedKind,
    [UInt32] $Offset,
    [UInt16] $Limit,
    [TimeSpan] $Timeout
) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $payload = Invoke-ResticPalRequest @{
            type = 'get_restore_query'
            query_id = $QueryId
            offset = $Offset
            limit = $Limit
        }
        if ($payload.type -ne 'restore_query') {
            throw (
                "Restore $ExpectedKind query $QueryId was rejected: " +
                "$($payload.code) $($payload.message)"
            )
        }
        $result = $payload.result
        if ([UInt64] $result.query_id -ne $QueryId -or $result.kind -ne $ExpectedKind) {
            throw "The service returned the wrong restore query or query kind for $QueryId."
        }
        switch ($result.state) {
            'running' {
                Start-Sleep -Milliseconds 100
                continue
            }
            'succeeded' {
                [object[]] $items = if ($ExpectedKind -eq 'snapshots') {
                    @($result.snapshots | Where-Object { $null -ne $_ })
                } else {
                    @($result.entries | Where-Object { $null -ne $_ })
                }
                if ($items.Count -gt $Limit `
                    -or [UInt64] $Offset + [UInt64] $items.Count -gt [UInt64] $result.total) {
                    throw "Restore query $QueryId exceeded its requested page bounds."
                }
                return $result
            }
            'failed' {
                throw "Restore $ExpectedKind query $QueryId failed: $($result.message)"
            }
            'cancelled' {
                throw "Restore $ExpectedKind query $QueryId was unexpectedly cancelled."
            }
            default {
                throw "Restore query $QueryId returned an unknown state: $($result.state)"
            }
        }
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for restore $ExpectedKind query $QueryId."
}

function Get-RestoreQueryItems(
    [UInt64] $QueryId,
    [string] $ExpectedKind,
    [UInt16] $PageSize = 1
) {
    $offset = [UInt32] 0
    $expectedTotal = $null
    do {
        $page = Wait-RestoreQueryPage `
            $QueryId `
            $ExpectedKind `
            $offset `
            $PageSize `
            ([TimeSpan]::FromMinutes(2))
        if ($null -eq $expectedTotal) {
            $expectedTotal = [UInt64] $page.total
            if ($expectedTotal -gt 1024) {
                throw "Disposable restore $ExpectedKind query returned too many results."
            }
        } elseif ([UInt64] $page.total -ne $expectedTotal) {
            throw "Restore query $QueryId changed its result total between pages."
        }
        [object[]] $items = if ($ExpectedKind -eq 'snapshots') {
            @($page.snapshots | Where-Object { $null -ne $_ })
        } else {
            @($page.entries | Where-Object { $null -ne $_ })
        }
        if ($items.Count -eq 0 -and [UInt64] $offset -lt $expectedTotal) {
            throw "Restore query $QueryId returned an empty page before its result total."
        }
        foreach ($item in $items) {
            $item
        }
        $previousOffset = $offset
        $offset += [UInt32] $items.Count
        if ([UInt64] $previousOffset -lt $expectedTotal -and $offset -le $previousOffset) {
            throw "Restore query $QueryId did not advance its pagination offset."
        }
    } while ([UInt64] $offset -lt $expectedTotal)
}

function Get-RestoreSnapshots {
    $started = Invoke-ResticPalRequest @{ type = 'begin_restore_snapshot_query' }
    if ($started.type -ne 'restore_query_started' -or [UInt64] $started.query_id -eq 0) {
        throw "Snapshot browsing did not start: $($started.code) $($started.message)"
    }
    return [pscustomobject]@{
        query_id = [UInt64] $started.query_id
        snapshots = @(Get-RestoreQueryItems ([UInt64] $started.query_id) 'snapshots')
    }
}

function Get-RestoreDirectory([string] $SnapshotId, [string] $Path) {
    $started = Invoke-ResticPalRequest @{
        type = 'begin_restore_directory_query'
        snapshot_id = $SnapshotId
        path = $Path
    }
    if ($started.type -ne 'restore_query_started' -or [UInt64] $started.query_id -eq 0) {
        throw "Browsing snapshot directory $Path did not start: $($started.code) $($started.message)"
    }
    return @(Get-RestoreQueryItems ([UInt64] $started.query_id) 'directory')
}

function Find-RestoreSourceFile([string] $SnapshotId, [string] $FileName) {
    $pending = [Collections.Generic.Queue[object]]::new()
    $visited = [Collections.Generic.HashSet[string]]::new(
        [StringComparer]::OrdinalIgnoreCase
    )
    $pending.Enqueue([pscustomobject]@{ path = '/'; directory_entry = $null })

    while ($pending.Count -gt 0) {
        $directory = $pending.Dequeue()
        if (-not $visited.Add([string] $directory.path)) {
            continue
        }
        if ($visited.Count -gt 32) {
            throw 'The disposable snapshot exceeded the bounded lazy-directory traversal.'
        }
        $entries = @(Get-RestoreDirectory $SnapshotId ([string] $directory.path))
        foreach ($entry in $entries) {
            if ($entry.node_type -eq 'file' `
                -and [string] $entry.name -ceq $FileName) {
                return [pscustomobject]@{
                    file_entry = $entry
                    directory_entry = $directory.directory_entry
                    visited_directories = $visited.Count
                }
            }
            if ($entry.node_type -eq 'directory') {
                if ([string]::IsNullOrWhiteSpace([string] $entry.path) `
                    -or -not ([string] $entry.path).StartsWith('/')) {
                    throw 'The restore browser returned an invalid absolute snapshot path.'
                }
                $pending.Enqueue([pscustomobject]@{
                    path = [string] $entry.path
                    directory_entry = $entry
                })
            }
        }
    }
    throw "Lazy repository browsing could not find the known snapshot file $FileName."
}

function Wait-RestoreCompletion([UInt64] $JobId, [TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $payload = Invoke-ResticPalRequest @{ type = 'get_restore_status' }
        if ($payload.type -ne 'restore_status') {
            throw "Restore status was rejected: $($payload.code) $($payload.message)"
        }
        $status = $payload.status
        if ($null -eq $status.job_id -or [UInt64] $status.job_id -ne $JobId) {
            throw "The service returned a different restore job than the requested job $JobId."
        }
        switch ($status.state) {
            'running' {
                Start-Sleep -Milliseconds 100
                continue
            }
            'succeeded' {
                if ([string]::IsNullOrWhiteSpace([string] $status.destination)) {
                    throw "Restore job $JobId succeeded without returning its destination."
                }
                return $status
            }
            'failed' {
                throw "Restore job $JobId failed: $($status.message)"
            }
            'cancelled' {
                throw "Restore job $JobId was unexpectedly cancelled."
            }
            default {
                throw "Restore job $JobId returned an unknown state: $($status.state)"
            }
        }
    } while ([DateTime]::UtcNow -lt $deadline)
    throw "Timed out waiting for LocalSystem restore job $JobId."
}

function Get-InheritedRestoreAclRules([Security.AccessControl.FileSystemSecurity] $Acl) {
    return @(
        $Acl.GetAccessRules(
            $false,
            $true,
            [Security.Principal.SecurityIdentifier]
        ) |
            ForEach-Object {
                '{0}|{1}|{2}|{3}|{4}' -f `
                    $_.IdentityReference.Value, `
                    [int] $_.AccessControlType, `
                    [int64] $_.FileSystemRights, `
                    [int] $_.InheritanceFlags, `
                    [int] $_.PropagationFlags
            } |
            Sort-Object
    )
}

function Assert-NormalRestoreInheritance([string] $RestoredRoot) {
    $probeRoot = Join-Path $restoreRoot (
        'ResticPal-Acl-Probe-' + [Guid]::NewGuid().ToString('N'))
    $probeCreated = $false
    try {
        New-Item -ItemType Directory -Path $probeRoot -ErrorAction Stop | Out-Null
        $probeCreated = $true
        $restoredAcl = Get-Acl -LiteralPath $RestoredRoot
        if ($restoredAcl.AreAccessRulesProtected) {
            throw "The completed restore root still blocks normal inherited access: $RestoredRoot"
        }
        $expectedInherited = @(Get-InheritedRestoreAclRules (
            Get-Acl -LiteralPath $probeRoot))
        $actualInherited = @(Get-InheritedRestoreAclRules $restoredAcl)
        if ($expectedInherited.Count -ne $actualInherited.Count `
            -or -not [string]::Equals(
                [string]::Join("`n", $expectedInherited),
                [string]::Join("`n", $actualInherited),
                [StringComparison]::Ordinal)) {
            throw ('The completed restore root inherited different access rules from ' +
                   "an ordinary destination sibling: $RestoredRoot")
        }
    } finally {
        if ($probeCreated) {
            Remove-Item -LiteralPath $probeRoot -Force -ErrorAction Stop
            if (Test-Path -LiteralPath $probeRoot) {
                throw "The exact empty restore ACL sibling was not removed: $probeRoot"
            }
        }
    }
}

function Assert-NonElevatedRestoreRead([string] $RestoredFile, [string] $ExpectedHash) {
    $explorers = @(Get-Process -Name explorer -ErrorAction Stop |
        Where-Object SessionId -eq $interactiveSessionId)
    if ($explorers.Count -ne 1) {
        throw ('Expected exactly one interactive Explorer for non-elevated restore ' +
               "verification; found $($explorers.Count).")
    }
    $expectedUserSid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    [ResticPalNativeTest]::VerifyReadWithNonElevatedExplorerToken(
        [int] $explorers[0].Id,
        $expectedUserSid,
        $RestoredFile,
        $ExpectedHash)
}

function Invoke-VerifiedRestore(
    [string] $SnapshotId,
    [string] $SnapshotPath,
    [string] $ExpectedRelativePath,
    [string] $OriginalFile
) {
    $started = Invoke-ResticPalRequest @{
        type = 'start_restore'
        snapshot_id = $SnapshotId
        path = $SnapshotPath
        destination = $restoreRoot
    }
    if ($started.type -ne 'restore_started' -or [UInt64] $started.job_id -eq 0) {
        throw "LocalSystem restore did not start: $($started.code) $($started.message)"
    }
    $completed = Wait-RestoreCompletion ([UInt64] $started.job_id) ([TimeSpan]::FromMinutes(2))
    $destination = [IO.Path]::GetFullPath([string] $completed.destination)
    $expectedParent = [IO.Path]::GetFullPath($restoreRoot).TrimEnd('\')
    $actualParent = [IO.Path]::GetDirectoryName($destination).TrimEnd('\')
    if (-not $actualParent.Equals($expectedParent, [StringComparison]::OrdinalIgnoreCase)) {
        throw "The service restored outside a unique direct destination child: $destination"
    }
    if (-not (Test-Path -LiteralPath $destination -PathType Container)) {
        throw "The reported restore destination was not created: $destination"
    }
    $restoredFile = Join-Path $destination $ExpectedRelativePath
    if (-not (Test-Path -LiteralPath $restoredFile -PathType Leaf)) {
        throw "The requested restored file is missing: $restoredFile"
    }
    $originalInfo = Get-Item -LiteralPath $OriginalFile
    $restoredInfo = Get-Item -LiteralPath $restoredFile
    $originalHash = (Get-FileHash -LiteralPath $OriginalFile -Algorithm SHA256).Hash
    $restoredHash = (Get-FileHash -LiteralPath $restoredFile -Algorithm SHA256).Hash
    if ($originalInfo.Length -ne $restoredInfo.Length -or $originalHash -cne $restoredHash) {
        throw "The LocalSystem restored bytes differ from the original file: $restoredFile"
    }
    Assert-NormalRestoreInheritance $destination
    Assert-NonElevatedRestoreRead $restoredFile $originalHash
    return $destination
}

function Write-RandomTestFile([string] $Path, [int] $SizeInMiB) {
    $buffer = [byte[]]::new(1024 * 1024)
    $random = [Security.Cryptography.RandomNumberGenerator]::Create()
    $stream = [IO.File]::Open(
        $Path,
        [IO.FileMode]::Create,
        [IO.FileAccess]::Write,
        [IO.FileShare]::None
    )
    try {
        for ($index = 0; $index -lt $SizeInMiB; $index++) {
            $random.GetBytes($buffer)
            $stream.Write($buffer, 0, $buffer.Length)
        }
    } finally {
        $stream.Dispose()
        $random.Dispose()
    }
}

function Assert-MachineOnlyAcl(
    [string] $Path,
    [switch] $RequireProtected,
    [switch] $RequireInheritedRules,
    [switch] $RequireDirectoryInheritance
) {
    $acl = Get-Acl -LiteralPath $Path
    if ($RequireProtected -and -not $acl.AreAccessRulesProtected) {
        throw "$Path does not have a protected DACL."
    }
    if ($RequireInheritedRules -and $acl.AreAccessRulesProtected) {
        throw "$Path unexpectedly blocks the cache root's inherited DACL."
    }

    $systemSid = 'S-1-5-18'
    $administratorsSid = 'S-1-5-32-544'
    $allowedSids = @($systemSid, $administratorsSid)
    $ownerSid = $acl.GetOwner([Security.Principal.SecurityIdentifier]).Value
    if ($allowedSids -notcontains $ownerSid) {
        throw "$Path has untrusted owner SID $ownerSid."
    }

    $rules = @($acl.GetAccessRules(
        $true,
        $true,
        [Security.Principal.SecurityIdentifier]
    ))
    if ($rules.Count -ne 2) {
        throw "$Path has $($rules.Count) access rules instead of the two trusted machine rules."
    }

    $observedSids = @()
    foreach ($rule in $rules) {
        $sid = $rule.IdentityReference.Value
        if ($allowedSids -notcontains $sid) {
            throw "$Path grants access to untrusted SID $sid."
        }
        if ($rule.AccessControlType -ne [Security.AccessControl.AccessControlType]::Allow) {
            throw "$Path contains a deny rule for trusted SID $sid instead of the expected allow rule."
        }
        if (($rule.FileSystemRights -band [Security.AccessControl.FileSystemRights]::FullControl) `
            -ne [Security.AccessControl.FileSystemRights]::FullControl) {
            throw "$Path does not grant full control to trusted SID $sid."
        }
        if ($RequireInheritedRules -and -not $rule.IsInherited) {
            throw "$Path has a non-inherited access rule for trusted SID $sid."
        }
        if ($RequireProtected -and $rule.IsInherited) {
            throw "$Path has an inherited access rule despite its protected DACL."
        }
        if ($RequireDirectoryInheritance) {
            $expectedInheritance = `
                [Security.AccessControl.InheritanceFlags]::ContainerInherit -bor `
                [Security.AccessControl.InheritanceFlags]::ObjectInherit
            if (($rule.InheritanceFlags -band $expectedInheritance) -ne $expectedInheritance) {
                throw "$Path does not propagate its trusted rule for SID $sid to files and directories."
            }
        }
        $observedSids += $sid
    }
    foreach ($sid in $allowedSids) {
        if ($observedSids -notcontains $sid) {
            throw "$Path is missing the required access rule for SID $sid."
        }
    }
}

function Get-ResticCacheState {
    if (-not (Test-Path -LiteralPath $cacheRoot -PathType Container)) {
        throw "The installed service did not create its restic cache at $cacheRoot."
    }
    $cacheEntries = @(Get-ChildItem -LiteralPath $cacheRoot -Force)
    if ($cacheEntries.Count -eq 0) {
        throw 'The installed service created an empty restic cache.'
    }

    $cacheTag = Join-Path $cacheRoot 'CACHEDIR.TAG'
    if (-not (Test-Path -LiteralPath $cacheTag -PathType Leaf)) {
        throw 'The restic cache is missing CACHEDIR.TAG.'
    }
    $cacheTagInfo = Get-Item -LiteralPath $cacheTag
    if ($cacheTagInfo.Length -eq 0) {
        throw 'The restic cache has an empty CACHEDIR.TAG.'
    }

    $repositoryDirectories = @(
        Get-ChildItem -LiteralPath $cacheRoot -Directory -Force |
            Where-Object Name -Match '^[0-9a-f]{64}$'
    )
    if ($repositoryDirectories.Count -ne 1) {
        throw "Expected one repository cache directory, found $($repositoryDirectories.Count)."
    }
    $repositoryDirectory = $repositoryDirectories[0]
    $repositoryVersion = Join-Path $repositoryDirectory.FullName 'version'
    if (-not (Test-Path -LiteralPath $repositoryVersion -PathType Leaf)) {
        throw 'The repository cache is missing its version file.'
    }
    $repositoryFiles = @(
        Get-ChildItem -LiteralPath $repositoryDirectory.FullName -File -Recurse -Force
    )
    if ($repositoryFiles.Count -eq 0) {
        throw 'The repository cache directory contains no cached metadata.'
    }

    Assert-MachineOnlyAcl `
        $cacheRoot `
        -RequireProtected `
        -RequireDirectoryInheritance
    Assert-MachineOnlyAcl $cacheTag -RequireInheritedRules
    Assert-MachineOnlyAcl $repositoryDirectory.FullName -RequireInheritedRules
    Assert-MachineOnlyAcl $repositoryVersion -RequireInheritedRules

    $cacheRootInfo = Get-Item -LiteralPath $cacheRoot
    $repositoryVersionInfo = Get-Item -LiteralPath $repositoryVersion
    return [pscustomobject]@{
        root_creation_ticks = $cacheRootInfo.CreationTimeUtc.Ticks
        tag_sha256 = (Get-FileHash -LiteralPath $cacheTag -Algorithm SHA256).Hash
        tag_length = $cacheTagInfo.Length
        repository_name = $repositoryDirectory.Name
        repository_creation_ticks = $repositoryDirectory.CreationTimeUtc.Ticks
        repository_version_sha256 = (
            Get-FileHash -LiteralPath $repositoryVersion -Algorithm SHA256
        ).Hash
        repository_version_length = $repositoryVersionInfo.Length
        cached_file_count = $repositoryFiles.Count
    }
}

function Assert-ResticCacheReused(
    [pscustomobject] $Expected,
    [string] $LifecycleStage
) {
    $actual = Get-ResticCacheState
    foreach ($property in @(
        'root_creation_ticks',
        'tag_sha256',
        'tag_length',
        'repository_name',
        'repository_creation_ticks',
        'repository_version_sha256',
        'repository_version_length'
    )) {
        if ($actual.$property -cne $Expected.$property) {
            throw "The restic cache changed $property instead of being reused $LifecycleStage."
        }
    }
    Write-Host (
        "The LocalSystem restic cache repository $($actual.repository_name) was reused " +
        "$LifecycleStage with $($actual.cached_file_count) cached files."
    )
}

function Assert-SnapshotCached([pscustomobject] $CacheState, [string] $SnapshotId) {
    if ($SnapshotId -notmatch '^[0-9a-f]{64}$') {
        throw "The backup returned an invalid full snapshot identifier: $SnapshotId"
    }
    $snapshotPath = Join-Path `
        (Join-Path `
            (Join-Path $cacheRoot $CacheState.repository_name) `
            'snapshots') `
        (Join-Path $SnapshotId.Substring(0, 2) $SnapshotId)
    if (-not (Test-Path -LiteralPath $snapshotPath -PathType Leaf)) {
        throw "The LocalSystem cache does not contain completed snapshot $SnapshotId."
    }
    Assert-MachineOnlyAcl $snapshotPath -RequireInheritedRules
}

function Assert-InternalDataExcluded([string] $SnapshotId, [string] $SentinelName) {
    $previousResticEnvironment = @(
        [Environment]::GetEnvironmentVariables('Process').GetEnumerator() |
            Where-Object {
                ([string] $_.Key).StartsWith(
                    'RESTIC_',
                    [StringComparison]::OrdinalIgnoreCase
                )
            } |
            ForEach-Object {
                [pscustomobject]@{
                    name = [string] $_.Key
                    value = [string] $_.Value
                }
            }
    )
    try {
        foreach ($variable in $previousResticEnvironment) {
            [Environment]::SetEnvironmentVariable($variable.name, $null, 'Process')
        }
        [Environment]::SetEnvironmentVariable(
            'RESTIC_PASSWORD',
            'resticpal-e2e-disposable-password',
            'Process'
        )
        $listing = @(
            & (Join-Path $installRoot 'restic.exe') `
                --repo $backupRoot `
                --no-cache `
                ls $SnapshotId 2>&1 |
                ForEach-Object ToString
        )
        $listingExitCode = $LASTEXITCODE
    } finally {
        $currentResticEnvironmentNames = @(
            [Environment]::GetEnvironmentVariables('Process').Keys |
                Where-Object {
                    ([string] $_).StartsWith(
                        'RESTIC_',
                        [StringComparison]::OrdinalIgnoreCase
                    )
                }
        )
        foreach ($name in $currentResticEnvironmentNames) {
            [Environment]::SetEnvironmentVariable([string] $name, $null, 'Process')
        }
        foreach ($variable in $previousResticEnvironment) {
            [Environment]::SetEnvironmentVariable(
                $variable.name,
                $variable.value,
                'Process'
            )
        }
    }
    if ($listingExitCode -ne 0) {
        throw "Listing installed-service snapshot $SnapshotId failed with exit code $listingExitCode."
    }
    $listingText = $listing -join "`n"
    foreach ($expectedSourceFile in @('document.txt', 'vss-exclusive-open.txt')) {
        if ($listingText.IndexOf($expectedSourceFile, [StringComparison]::Ordinal) -lt 0) {
            throw "Snapshot $SnapshotId is missing expected source file $expectedSourceFile."
        }
    }
    if ($listingText.IndexOf($SentinelName, [StringComparison]::Ordinal) -ge 0) {
        throw "Snapshot $SnapshotId included resticpal internal data $SentinelName."
    }
    Write-Host 'The production backup excluded resticpal internal data from an explicitly configured source.'
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
if (Test-Path -LiteralPath $e2eRoot) {
    throw "The disposable source/repository directory already exists: $e2eRoot"
}
if (Test-Path -LiteralPath $onboardingMarker -PathType Leaf) {
    throw "The current user already has a first-run marker; use Windows Sandbox for a clean onboarding test: $onboardingMarker"
}
New-Item -ItemType Directory -Path $artifactRoot -Force | Out-Null

# Validate restricted-token impersonation before paying for MSI installation or
# creating restore fixtures. The actual restored bytes are checked again later.
$tokenLaunchProbe = Join-Path $env:SystemRoot `
    'System32\WindowsPowerShell\v1.0\powershell.exe'
Assert-NonElevatedRestoreRead `
    $tokenLaunchProbe `
    ((Get-FileHash -LiteralPath $tokenLaunchProbe -Algorithm SHA256).Hash)
Write-Host 'The same-user non-administrator restored-file impersonation verifier is working.'

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
        $baselineTrayProcess = Wait-InteractiveProcess 'resticpal-tray' ([TimeSpan]::FromSeconds(30))
        $baselineUiProcess = Wait-InteractiveProcess 'resticpal-ui' ([TimeSpan]::FromSeconds(30))
        Write-Host (
            "Baseline tray $($baselineTrayProcess.Id) and Settings $($baselineUiProcess.Id) " +
            'are live for the major-upgrade application-shutdown check.'
        )
        $upgradeSentinel = Join-Path $dataRoot 'upgrade-sentinel.txt'
        Set-Content -LiteralPath $upgradeSentinel -Value 'preserve across major upgrade' -NoNewline
        New-Item -ItemType Directory -Path $cacheRoot -Force | Out-Null
        $upgradeCacheSentinel = Join-Path $cacheRoot 'upgrade-cache-sentinel.txt'
        $upgradeCacheSentinelContent = (
            'preserve cache across major upgrade ' + [Guid]::NewGuid().ToString('N')
        )
        Set-Content `
            -LiteralPath $upgradeCacheSentinel `
            -Value $upgradeCacheSentinelContent `
            -NoNewline
        Write-Host "Upgrading the baseline installation to $resolvedMsiPath"
    }
    Write-Host "Installing $resolvedMsiPath"
    Invoke-Installer "/i `"$resolvedMsiPath`" /qn /norestart /l*v `"$installLog`"" 'Installation'
    $installedByTest = $true
    $installedPackagePath = $resolvedMsiPath
    if ($null -ne $resolvedUpgradeFromMsiPath) {
        if (-not (Test-Path -LiteralPath $upgradeSentinel -PathType Leaf)) {
            throw 'The major upgrade did not preserve existing machine data.'
        }
        if (-not (Test-Path -LiteralPath $upgradeCacheSentinel -PathType Leaf)) {
            throw 'The major upgrade did not preserve the existing restic cache sentinel.'
        }
        $preservedCacheSentinelContent = Get-Content `
            -LiteralPath $upgradeCacheSentinel `
            -Raw
        if (-not $preservedCacheSentinelContent.Equals(
                $upgradeCacheSentinelContent,
                [StringComparison]::Ordinal)) {
            throw 'The major upgrade changed the existing restic cache sentinel.'
        }
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
    foreach ($fileName in @(
        'resticpal-service.exe',
        'resticpal-tray.exe',
        'resticpal-ui.exe',
        'restic.exe',
        'coreclr.dll',
        'hostfxr.dll',
        'hostpolicy.dll'
    )) {
        if (-not (Test-Path -LiteralPath (Join-Path $installRoot $fileName) -PathType Leaf)) {
            throw "Installed payload is missing $fileName"
        }
    }
    $candidateFileVersion = [Diagnostics.FileVersionInfo]::GetVersionInfo(
        (Join-Path $installRoot 'resticpal-ui.exe')
    ).FileVersion
    if ([string]::IsNullOrWhiteSpace($candidateFileVersion)) {
        throw 'The installed candidate Settings executable has no file version.'
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
    Assert-ServicePipeContinuouslyOwned 'freshly installed'
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
    try {
        Wait-AutomationElement $uiProcess 'SettingsItem' ([TimeSpan]::FromSeconds(30)) | Out-Null
    } catch {
        Export-SettingsProcessSnapshot (Join-Path $artifactRoot 'settings-processes-on-first-launch-failure.json')
        throw
    }
    $mutexProbeCreatedNew = $false
    $mutexProbe = [Threading.Mutex]::new(
        $false,
        'Local\ResticPal.Settings',
        [ref]$mutexProbeCreatedNew)
    try {
        if ($mutexProbeCreatedNew) {
            throw 'The primary settings process did not retain its single-instance mutex.'
        }
    } finally {
        $mutexProbe.Dispose()
    }

    $duplicateExitTimer = [Diagnostics.Stopwatch]::StartNew()
    $duplicateUiProcess = Start-Process `
        -FilePath (Join-Path $installRoot 'resticpal-ui.exe') `
        -ArgumentList '--setup' `
        -PassThru
    if (-not $duplicateUiProcess.WaitForExit(30000)) {
        throw 'A duplicate settings launch did not yield to the existing window.'
    }
    $duplicateExitTimer.Stop()
    Write-Host "Duplicate settings process exited after $($duplicateExitTimer.ElapsedMilliseconds) ms."
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
    if (-not [ResticPalNativeTest]::IsPerMonitorV2($trayWindow)) {
        throw 'The installed tray hidden window is not Per-Monitor-v2 DPI aware.'
    }
    $trayDpi = [ResticPalNativeTest]::GetDpiForWindow($trayWindow)
    if ($trayDpi -lt 96) {
        throw "The installed tray hidden window reported invalid DPI $trayDpi."
    }
    Write-Host "The tray hidden window is Per-Monitor-v2 aware at $trayDpi DPI."

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
    $trayMenuWindow = Wait-NativeWindowForProcess `
        $trayProcess `
        '#32768' `
        ([TimeSpan]::FromSeconds(10))
    if (-not [ResticPalNativeTest]::IsPerMonitorV2($trayMenuWindow)) {
        throw 'The tray action menu is not Per-Monitor-v2 DPI aware.'
    }
    $trayMenuDpi = [ResticPalNativeTest]::GetDpiForWindow($trayMenuWindow)
    if ($trayMenuDpi -lt 96) {
        throw "The tray action menu reported invalid DPI $trayMenuDpi."
    }
    Write-Host "A tray right click opened a Per-Monitor-v2 action menu at $trayMenuDpi DPI."
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
    Wait-AutomationElement $startMenuUiProcess 'RestoreItem' ([TimeSpan]::FromSeconds(10)) | Out-Null
    Write-Host 'The WinUI Settings page exposes restore, enrollment, and signed-update controls.'

    $sourcesItem = Wait-AutomationElement $startMenuUiProcess 'SourcesItem' ([TimeSpan]::FromSeconds(10))
    $sourcesSelection = $sourcesItem.GetCurrentPattern(
        [Windows.Automation.SelectionItemPattern]::Pattern
    )
    $sourcesSelection.Select()
    $addSource = Wait-AutomationElementEnabled $startMenuUiProcess 'AddSourceButton' ([TimeSpan]::FromSeconds(10))
    $addSourceInvoke = $addSource.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern)
    $addSourceInvoke.Invoke()
    try {
        Wait-AutomationElementByName 'Choose a folder to back up' ([TimeSpan]::FromSeconds(5)) | Out-Null
    } catch {
        $pickerError = $_.Exception.Message
        $messageText = try {
            $messageBar = Wait-AutomationElement $startMenuUiProcess 'MessageBar' ([TimeSpan]::FromSeconds(2))
            $messageTextCondition = [Windows.Automation.PropertyCondition]::new(
                [Windows.Automation.AutomationElement]::ControlTypeProperty,
                [Windows.Automation.ControlType]::Text
            )
            @(
                $messageBar.FindAll([Windows.Automation.TreeScope]::Descendants, $messageTextCondition) |
                    ForEach-Object { $_.Current.Name } |
                    Where-Object { -not [string]::IsNullOrWhiteSpace($_) }
            ) -join ' '
        } catch {
            '(no error message was shown)'
        }
        throw "$pickerError resticpal message: $messageText"
    }
    [Windows.Forms.SendKeys]::SendWait('{ESC}')
    Wait-AutomationElementEnabled $startMenuUiProcess 'AddSourceButton' ([TimeSpan]::FromSeconds(10)) | Out-Null
    Write-Host 'The backup-source Add folder action opened the native Windows App SDK picker.'
    Stop-Process -Id $startMenuUiProcess.Id -Force
    $startMenuUiProcess.WaitForExit(10000) | Out-Null
    Start-Process -FilePath (Join-Path $installRoot 'resticpal-ui.exe') -ArgumentList '--updates'
    $updatesUiProcess = Wait-InteractiveProcess 'resticpal-ui' ([TimeSpan]::FromSeconds(30))
    Wait-AutomationElementOnscreen $updatesUiProcess 'CheckForUpdatesButton' ([TimeSpan]::FromSeconds(30)) | Out-Null
    Write-Host 'The --updates launch opens the signed-update controls in the visible viewport.'

    $updateSettings = Invoke-ResticPalRequest @{ type = 'get_update_settings' }
    if ($updateSettings.type -ne 'update_settings' `
        -or $updateSettings.configuration.automatic_install `
        -or $updateSettings.configuration.automatic_install_locked) {
        throw 'Automatic update installation was not disabled by default.'
    }
    Assert-Accepted @{ type = 'update_update_settings'; automatic_install = $true }
    $updateSettings = Invoke-ResticPalRequest @{ type = 'get_update_settings' }
    if (-not $updateSettings.configuration.automatic_install `
        -or $updateSettings.configuration.automatic_install_locked) {
        throw 'Automatic update installation was not enabled through IPC.'
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

    # With sources present and an unverified repository, the overview must
    # expose the service's waiting state instead of presenting stale setup or
    # protected copy. Clear the sources again before initialization so the
    # scheduler cannot race the remainder of repository setup.
    Assert-Accepted @{
        type = 'update_backup_sources'
        paths = @($sourceRoot)
        exclusions = @()
    }
    $overviewItem = Wait-AutomationElement $updatesUiProcess 'OverviewItem' ([TimeSpan]::FromSeconds(10))
    $overviewSelection = $overviewItem.GetCurrentPattern(
        [Windows.Automation.SelectionItemPattern]::Pattern
    )
    $overviewSelection.Select()
    Wait-AutomationTextContains `
        $updatesUiProcess `
        'StatusCardTitle' `
        'Repository setup required' `
        ([TimeSpan]::FromSeconds(10)) | Out-Null
    Write-Host 'The overview status card displayed the repository-validation waiting state.'
    Assert-Accepted @{
        type = 'update_backup_sources'
        paths = @()
        exclusions = @()
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

    $repositoryItem = Wait-AutomationElement $updatesUiProcess 'RepositoryItem' ([TimeSpan]::FromSeconds(10))
    $repositorySelection = $repositoryItem.GetCurrentPattern(
        [Windows.Automation.SelectionItemPattern]::Pattern
    )
    $repositorySelection.Select()
    Wait-AutomationElementEnabled $updatesUiProcess 'ValidateRepositoryButton' ([TimeSpan]::FromSeconds(10)) | Out-Null
    $saveRepository = Wait-AutomationElement $updatesUiProcess 'SaveRepositoryButton' ([TimeSpan]::FromSeconds(10))
    if ($saveRepository.Current.IsEnabled) {
        throw 'Loading the service-saved repository incorrectly marked it as an unsaved UI edit.'
    }
    Write-Host 'A service-loaded repository can be tested immediately without an unnecessary save.'

    $lockedSourcePath = Join-Path $sourceRoot 'vss-exclusive-open.txt'
    $internalExclusionSentinelName = 'must-never-be-backed-up.txt'
    Set-Content `
        -LiteralPath (Join-Path $dataRoot $internalExclusionSentinelName) `
        -Value 'resticpal service data is always excluded from backup sources' `
        -NoNewline
    Set-Content `
        -LiteralPath $lockedSourcePath `
        -Value 'This file must be read from the LocalSystem VSS snapshot.' `
        -NoNewline
    $lockedSourceStream = [IO.File]::Open(
        $lockedSourcePath,
        [IO.FileMode]::Open,
        [IO.FileAccess]::ReadWrite,
        [IO.FileShare]::None
    )
    $programDataExclusions = @(
        Get-ChildItem -LiteralPath $env:ProgramData -Force |
            Where-Object {
                -not $_.FullName.Equals($dataRoot, [StringComparison]::OrdinalIgnoreCase)
            } |
            ForEach-Object FullName
    )
    if ($programDataExclusions.Count -gt 500) {
        throw "The Sandbox ProgramData fixture needs too many exclusions: $($programDataExclusions.Count)."
    }
    try {
        Assert-Accepted @{
            type = 'update_backup_sources'
            # ProgramData is a legitimate broad ancestor. Every sibling is
            # excluded by the fixture, leaving the mandatory ResticPal rule as
            # the only reason the protected data-root sentinel is absent.
            paths = @($sourceRoot, $env:ProgramData)
            exclusions = $programDataExclusions
        }
        $automaticRun = Wait-Backup ([TimeSpan]::FromMinutes(3))
    } finally {
        $lockedSourceStream.Dispose()
    }
    Write-Host (
        "Initial configuration automatically started append-only snapshot " +
        "$($automaticRun.snapshot_id) while a source file was exclusively open."
    )
    $cacheStateAfterFirstBackup = Get-ResticCacheState
    Assert-SnapshotCached $cacheStateAfterFirstBackup $automaticRun.snapshot_id
    Assert-InternalDataExcluded $automaticRun.snapshot_id $internalExclusionSentinelName
    Write-Host (
        "The protected LocalSystem cache contains repository " +
        "$($cacheStateAfterFirstBackup.repository_name) and the first VSS-backed snapshot."
    )

    $preUiSnapshotQuery = Get-RestoreSnapshots
    $preUiMatchingSnapshots = @(
        $preUiSnapshotQuery.snapshots |
            Where-Object { $_.id -ceq $automaticRun.snapshot_id }
    )
    if ($preUiMatchingSnapshots.Count -ne 1) {
        $returnedSnapshots = @($preUiSnapshotQuery.snapshots | ForEach-Object {
            "$($_.id) (host=$($_.hostname))"
        }) -join ', '
        throw (
            "The LocalSystem restore listing omitted completed snapshot " +
            "$($automaticRun.snapshot_id); physical DNS hostname='$resticHostName', " +
            "NetBIOS COMPUTERNAME='$env:COMPUTERNAME', returned snapshots=" +
            $(if ($returnedSnapshots.Length -eq 0) { '(none)' } else { $returnedSnapshots })
        )
    }
    if (-not ([string] $preUiMatchingSnapshots[0].hostname).Equals(
            $resticHostName,
            [StringComparison]::OrdinalIgnoreCase)) {
        throw (
            "The service exposed snapshot host '$($preUiMatchingSnapshots[0].hostname)' " +
            "instead of the local physical DNS hostname '$resticHostName'."
        )
    }
    Write-Host (
        "The service identified completed snapshot $($automaticRun.snapshot_id) " +
        "under restic-compatible physical DNS hostname '$resticHostName'."
    )

    $restoreNavigation = Wait-AutomationElement `
        $updatesUiProcess `
        'RestoreItem' `
        ([TimeSpan]::FromSeconds(10))
    $restoreNavigation.GetCurrentPattern(
        [Windows.Automation.SelectionItemPattern]::Pattern
    ).Select()
    $restoreToggle = Wait-AutomationElementEnabled `
        $updatesUiProcess `
        'RestoreEnabledToggle' `
        ([TimeSpan]::FromSeconds(30))
    $restoreToggleState = $restoreToggle.GetCurrentPattern(
        [Windows.Automation.TogglePattern]::Pattern
    ).Current.ToggleState
    if ($restoreToggleState -ne [Windows.Automation.ToggleState]::On) {
        throw 'The installed recovery page did not show standalone restore as enabled.'
    }
    Wait-AutomationTextContains `
        $updatesUiProcess `
        'RestoreSnapshotSummary' `
        'available backup' `
        ([TimeSpan]::FromSeconds(30)) | Out-Null
    Wait-AutomationElementEnabled `
        $updatesUiProcess `
        'RestoreDatePicker' `
        ([TimeSpan]::FromSeconds(30)) | Out-Null
    Wait-AutomationElementEnabled `
        $updatesUiProcess `
        'RestoreSnapshotPicker' `
        ([TimeSpan]::FromSeconds(10)) | Out-Null
    Wait-AutomationElementEnabled `
        $updatesUiProcess `
        'RefreshRestoreButton' `
        ([TimeSpan]::FromSeconds(30)) | Out-Null
    $restoreSourceRow = Wait-AutomationListRow `
        $updatesUiProcess `
        'RestoreEntriesList' `
        ([IO.Path]::GetFileName($sourceRoot)) `
        ([TimeSpan]::FromSeconds(15))
    $openSourceCondition = [Windows.Automation.AndCondition]::new(
        [Windows.Automation.PropertyCondition]::new(
            [Windows.Automation.AutomationElement]::ControlTypeProperty,
            [Windows.Automation.ControlType]::Button
        ),
        [Windows.Automation.PropertyCondition]::new(
            [Windows.Automation.AutomationElement]::NameProperty,
            'Open'
        )
    )
    $openSource = $restoreSourceRow.FindFirst(
        [Windows.Automation.TreeScope]::Descendants,
        $openSourceCondition
    )
    if ($null -eq $openSource) {
        throw 'The recovery browser did not expose an Open action for the backup source.'
    }
    $openSource.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern).Invoke()
    Wait-AutomationListRow `
        $updatesUiProcess `
        'RestoreEntriesList' `
        'document.txt' `
        ([TimeSpan]::FromSeconds(30)) | Out-Null
    Wait-AutomationElementEnabled `
        $updatesUiProcess `
        'RefreshRestoreButton' `
        ([TimeSpan]::FromSeconds(30)) | Out-Null
    Write-Host (
        'The installed WinUI recovery page populated its date and snapshot pickers, ' +
        'opened the real backup source, and displayed document.txt.'
    )

    $restoreSettings = Invoke-ResticPalRequest @{ type = 'get_restore_settings' }
    if ($restoreSettings.type -ne 'restore_settings' `
        -or -not $restoreSettings.configuration.enabled `
        -or $restoreSettings.configuration.enabled_locked `
        -or $restoreSettings.configuration.managed) {
        throw 'Standalone installed-service restore was not enabled and administrator-editable by default.'
    }
    $snapshotQuery = Get-RestoreSnapshots
    $matchingSnapshots = @(
        $snapshotQuery.snapshots |
            Where-Object { $_.id -ceq $automaticRun.snapshot_id }
    )
    if ($matchingSnapshots.Count -ne 1) {
        throw 'Snapshot browsing did not return the exact completed append-only backup.'
    }
    foreach ($snapshot in $snapshotQuery.snapshots) {
        if ([string] $snapshot.id -notmatch '^[0-9a-f]{64}$' `
            -or -not ([string] $snapshot.hostname).Equals(
                $resticHostName,
                [StringComparison]::OrdinalIgnoreCase)) {
            throw "Snapshot browsing exposed an ambiguous identifier or another computer's backup."
        }
    }
    $oversizedPage = Invoke-ResticPalRequest @{
        type = 'get_restore_query'
        query_id = [UInt64] $snapshotQuery.query_id
        offset = 0
        limit = 101
    }
    if ($oversizedPage.type -ne 'rejected' `
        -or $oversizedPage.code -ne 'invalid_restore_query_limit') {
        throw 'The installed restore browser accepted an IPC page larger than 100 entries.'
    }
    $outOfRangePage = Invoke-ResticPalRequest @{
        type = 'get_restore_query'
        query_id = [UInt64] $snapshotQuery.query_id
        offset = [UInt32] ($snapshotQuery.snapshots.Count + 1)
        limit = 1
    }
    if ($outOfRangePage.type -ne 'rejected' `
        -or $outOfRangePage.code -ne 'invalid_restore_query_offset') {
        throw 'The installed restore browser accepted a result offset beyond its snapshot inventory.'
    }
    $restoreSource = Find-RestoreSourceFile $automaticRun.snapshot_id 'document.txt'
    if ($null -eq $restoreSource.directory_entry `
        -or $restoreSource.directory_entry.node_type -ne 'directory') {
        throw 'Lazy snapshot browsing did not identify the source directory around document.txt.'
    }
    New-Item -ItemType Directory -Path $restoreRoot -Force | Out-Null
    $restoreParentSentinel = Join-Path $restoreRoot 'preserve-parent.txt'
    $restoreParentSentinelContent = 'Existing destination-parent content must never be overwritten.'
    Set-Content `
        -LiteralPath $restoreParentSentinel `
        -Value $restoreParentSentinelContent `
        -NoNewline

    Assert-Accepted @{ type = 'update_restore_settings'; enabled = $false }
    $disabledRestore = Invoke-ResticPalRequest @{ type = 'get_restore_settings' }
    if ($disabledRestore.type -ne 'restore_settings' `
        -or $disabledRestore.configuration.enabled `
        -or $disabledRestore.configuration.enabled_locked) {
        throw 'The standalone administrator could not disable restore.'
    }
    $disabledCommands = @(
        @{ type = 'begin_restore_snapshot_query' },
        @{
            type = 'begin_restore_directory_query'
            snapshot_id = $automaticRun.snapshot_id
            path = '/'
        },
        @{
            type = 'get_restore_query'
            query_id = [UInt64] $snapshotQuery.query_id
            offset = 0
            limit = 1
        },
        @{
            type = 'cancel_restore_query'
            query_id = [UInt64] $snapshotQuery.query_id
        },
        @{
            type = 'start_restore'
            snapshot_id = $automaticRun.snapshot_id
            path = [string] $restoreSource.file_entry.path
            destination = $restoreRoot
        },
        @{ type = 'get_restore_status' },
        @{ type = 'cancel_restore' }
    )
    foreach ($command in $disabledCommands) {
        $denied = Invoke-ResticPalRequest $command
        if ($denied.type -ne 'rejected' -or $denied.code -ne 'restore_disabled') {
            throw "Disabled restore did not reject '$($command.type)' with restore_disabled."
        }
    }
    if (@(Get-ChildItem -LiteralPath $restoreRoot -Directory -Force).Count -ne 0 `
        -or (Get-Content -LiteralPath $restoreParentSentinel -Raw) -cne $restoreParentSentinelContent) {
        throw 'A disabled restore operation created output or changed the destination parent.'
    }
    Assert-Accepted @{ type = 'update_restore_settings'; enabled = $true }
    $reenabledRestore = Invoke-ResticPalRequest @{ type = 'get_restore_settings' }
    if (-not $reenabledRestore.configuration.enabled `
        -or $reenabledRestore.configuration.enabled_locked) {
        throw 'The standalone administrator could not re-enable restore.'
    }
    $oldQuery = Invoke-ResticPalRequest @{
        type = 'get_restore_query'
        query_id = [UInt64] $snapshotQuery.query_id
        offset = 0
        limit = 1
    }
    if ($oldQuery.type -ne 'rejected' -or $oldQuery.code -ne 'restore_query_not_found') {
        throw 'Disabling restore did not clear an existing sensitive snapshot query.'
    }
    $staleSnapshotGrant = Invoke-ResticPalRequest @{
        type = 'begin_restore_directory_query'
        snapshot_id = $automaticRun.snapshot_id
        path = '/'
    }
    if ($staleSnapshotGrant.type -ne 'rejected' `
        -or $staleSnapshotGrant.code -ne 'restore_snapshot_not_authorized') {
        throw 'Disabling restore did not clear authorization for a previously browsed snapshot.'
    }

    $refreshedSnapshots = Get-RestoreSnapshots
    if (@($refreshedSnapshots.snapshots |
            Where-Object { $_.id -ceq $automaticRun.snapshot_id }).Count -ne 1) {
        throw 'The re-enabled restore browser could not reauthorize its own append-only snapshot.'
    }
    $foreignSnapshotId = if ($automaticRun.snapshot_id -ceq ('f' * 64)) {
        'e' * 64
    } else {
        'f' * 64
    }
    $foreignSnapshot = Invoke-ResticPalRequest @{
        type = 'begin_restore_directory_query'
        snapshot_id = $foreignSnapshotId
        path = '/'
    }
    if ($foreignSnapshot.type -ne 'rejected' `
        -or $foreignSnapshot.code -ne 'restore_snapshot_not_authorized') {
        throw 'The restore browser accepted a snapshot outside its hostname-filtered inventory.'
    }
    $unlistedDirectory = Invoke-ResticPalRequest @{
        type = 'begin_restore_directory_query'
        snapshot_id = $automaticRun.snapshot_id
        path = '/resticpal-unlisted-directory'
    }
    if ($unlistedDirectory.type -ne 'rejected' `
        -or $unlistedDirectory.code -ne 'restore_path_not_authorized') {
        throw 'The restore browser accepted a directory not learned from a parent listing.'
    }
    $unlistedRestore = Invoke-ResticPalRequest @{
        type = 'start_restore'
        snapshot_id = $automaticRun.snapshot_id
        path = '/resticpal-unlisted-file.txt'
        destination = $restoreRoot
    }
    if ($unlistedRestore.type -ne 'rejected' `
        -or $unlistedRestore.code -ne 'restore_path_not_authorized' `
        -or @(Get-ChildItem -LiteralPath $restoreRoot -Directory -Force).Count -ne 0) {
        throw 'The restore engine accepted or created output for an unbrowsed snapshot path.'
    }
    $restoreSource = Find-RestoreSourceFile $automaticRun.snapshot_id 'document.txt'
    if ($null -eq $restoreSource.directory_entry) {
        throw 'Re-enabling restore did not rebuild the authorized source-directory inventory.'
    }

    $originalDocument = Join-Path $sourceRoot 'document.txt'
    $appendOnlyFileRestoreDestination = Invoke-VerifiedRestore `
        $automaticRun.snapshot_id `
        ([string] $restoreSource.file_entry.path) `
        'document.txt' `
        $originalDocument
    $restoredSourceFolderName = [string] $restoreSource.directory_entry.name
    $appendOnlyFolderRestoreDestination = Invoke-VerifiedRestore `
        $automaticRun.snapshot_id `
        ([string] $restoreSource.directory_entry.path) `
        (Join-Path $restoredSourceFolderName 'document.txt') `
        $originalDocument
    if ($appendOnlyFileRestoreDestination.Equals(
            $appendOnlyFolderRestoreDestination,
            [StringComparison]::OrdinalIgnoreCase)) {
        throw 'Sequential restores reused their destination child instead of creating unique output.'
    }
    $restoredLockedFile = Join-Path `
        (Join-Path $appendOnlyFolderRestoreDestination $restoredSourceFolderName) `
        'vss-exclusive-open.txt'
    if (-not (Test-Path -LiteralPath $restoredLockedFile -PathType Leaf)) {
        throw 'A directory restore omitted the file captured through the filesystem snapshot.'
    }
    if (@(Get-ChildItem -LiteralPath $restoreRoot -Directory -Force).Count -ne 2 `
        -or (Get-Content -LiteralPath $restoreParentSentinel -Raw) -cne $restoreParentSentinelContent) {
        throw 'Restore overwrote or deleted content outside its unique destination child.'
    }
    Assert-ResticCacheReused $cacheStateAfterFirstBackup 'for append-only snapshot browsing and verified restores'
    Write-Host (
        "Append-only snapshot $($automaticRun.snapshot_id) restored one exact file and its " +
        "directory into separate verified destinations after $($restoreSource.visited_directories) lazy directory queries."
    )
    Assert-ServicePipeContinuouslyOwned 'post-repository-browsing'

    # Give the manual run enough real work that the one-second active-backup
    # refresh cadence must render its running state before completion.
    Write-RandomTestFile (Join-Path $sourceRoot 'manual-status-transition.bin') 256
    $overviewSelection.Select()
    $runBackupButton = Wait-AutomationElementEnabled $updatesUiProcess 'RunBackupButton' ([TimeSpan]::FromSeconds(10))
    $runBackupInvoke = $runBackupButton.GetCurrentPattern([Windows.Automation.InvokePattern]::Pattern)
    $runBackupInvoke.Invoke()
    # The overview card is the acknowledgement surface. The shared bottom
    # InfoBar is reserved for rejection/error messages so an accepted request
    # cannot leave a stale "Backup requested" toast behind.
    Wait-AutomationTextContains $updatesUiProcess 'StatusCardTitle' 'Backup requested' ([TimeSpan]::FromSeconds(10)) | Out-Null
    Wait-AutomationTextContains $updatesUiProcess 'StatusCardTitle' 'Backup in progress' ([TimeSpan]::FromSeconds(30)) | Out-Null
    Assert-AutomationTextDoesNotContain $updatesUiProcess 'MessageBar' 'Backup requested'
    $run = Wait-Backup ([TimeSpan]::FromMinutes(3)) $automaticRun.snapshot_id
    Wait-AutomationTextContains $updatesUiProcess 'StatusCardTitle' 'Protected' ([TimeSpan]::FromSeconds(15)) | Out-Null
    Assert-ResticCacheReused $cacheStateAfterFirstBackup 'for the subsequent manual backup'
    Assert-SnapshotCached $cacheStateAfterFirstBackup $run.snapshot_id
    Assert-InternalDataExcluded $run.snapshot_id $internalExclusionSentinelName
    Write-Host "Run backup now rendered its acknowledgement, running, and completed states for append-only snapshot $($run.snapshot_id)."

    $pagedSnapshots = Get-RestoreSnapshots
    foreach ($expectedSnapshot in @($automaticRun.snapshot_id, $run.snapshot_id)) {
        if (@($pagedSnapshots.snapshots | Where-Object { $_.id -ceq $expectedSnapshot }).Count -ne 1) {
            throw "One-item snapshot pagination omitted append-only snapshot $expectedSnapshot."
        }
    }
    Write-Host 'Asynchronous repository snapshot browsing returned both append-only backups across one-item pages.'

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
    if ($diagnosticJson.Contains($sourceRoot) `
        -or $diagnosticJson.Contains($backupRoot) `
        -or $diagnosticJson.Contains($dataRoot)) {
        throw 'Operational diagnostics disclosed a source or repository path.'
    }
    Assert-ResticCacheReused $cacheStateAfterFirstBackup 'for the standard backup and retention pass'
    Assert-SnapshotCached $cacheStateAfterFirstBackup $standardRun.snapshot_id
    Assert-InternalDataExcluded $standardRun.snapshot_id $internalExclusionSentinelName
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
    if (-not $updatesAfterRestart.configuration.automatic_install `
        -or $updatesAfterRestart.configuration.automatic_install_locked) {
        throw 'The automatic-update setting did not survive the installed service restart.'
    }
    $restoreAfterRestart = Invoke-ResticPalRequest @{ type = 'get_restore_settings' }
    if ($restoreAfterRestart.type -ne 'restore_settings' `
        -or -not $restoreAfterRestart.configuration.enabled `
        -or $restoreAfterRestart.configuration.enabled_locked) {
        throw 'The administrator-enabled restore setting did not survive the installed service restart.'
    }
    Assert-ServicePipeContinuouslyOwned 'restarted'
    foreach ($restoredDestination in @(
        $appendOnlyFileRestoreDestination,
        $appendOnlyFolderRestoreDestination
    )) {
        if (-not (Test-Path -LiteralPath $restoredDestination -PathType Container)) {
            throw "Restart removed successful restore output: $restoredDestination"
        }
    }
    Assert-ResticCacheReused $cacheStateAfterFirstBackup 'after the service restart'
    Assert-SnapshotCached $cacheStateAfterFirstBackup $standardRun.snapshot_id
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
        if ([string]::IsNullOrWhiteSpace($candidateFileVersion)) {
            throw 'The installed candidate file version was not captured before uninstall.'
        }
        $candidateVersionPattern = [Regex]::Escape($candidateFileVersion)
        $candidateCrashEvents = @(Get-WinEvent -FilterHashtable @{
            LogName = 'Application'
            ProviderName = 'Application Error'
            Id = 1000
            StartTime = $testStartedAt
        } -ErrorAction SilentlyContinue | Where-Object {
            $_.Message -match (
                'Faulting application name: resticpal-(?:ui|tray|service)\.exe, ' +
                "version: $candidateVersionPattern,")
        })
        if ($candidateCrashEvents.Count -gt 0) {
            $crashSummary = @($candidateCrashEvents | ForEach-Object {
                $firstLine = ($_.Message -split '\r?\n')[0]
                "[$($_.TimeCreated.ToString('O'))] $firstLine"
            }) -join '; '
            throw "A candidate resticpal process crashed during the installed lifecycle: $crashSummary"
        }
        if (-not (Test-Path -LiteralPath $dataRoot)) {
            throw 'Uninstall removed machine backup data instead of preserving it.'
        }
        if ($testReachedPersistenceCheck) {
            Assert-ResticCacheReused $cacheStateAfterFirstBackup 'after uninstall'
            Assert-SnapshotCached $cacheStateAfterFirstBackup $standardRun.snapshot_id
            foreach ($restoredDestination in @(
                $appendOnlyFileRestoreDestination,
                $appendOnlyFolderRestoreDestination
            )) {
                if (-not (Test-Path -LiteralPath $restoredDestination -PathType Container)) {
                    throw "Uninstall removed user-requested restore output: $restoredDestination"
                }
            }
            Write-Host 'Install, backup, verified restore, restart, persistence, and uninstall checks passed.'
        }
        if ($onboardingMarkerCreatedByTest) {
            Remove-Item -LiteralPath $onboardingMarker -Force
        }
        Remove-Item -LiteralPath $dataRoot -Recurse -Force
        if (Test-Path -LiteralPath $e2eRoot) {
            $resolvedE2eRoot = [IO.Path]::GetFullPath($e2eRoot)
            $expectedE2eParent = [IO.Path]::GetFullPath("$env:SystemDrive\")
            $actualE2eParent = [IO.Path]::GetDirectoryName($resolvedE2eRoot)
            $actualE2eName = [IO.Path]::GetFileName($resolvedE2eRoot)
            if (-not $actualE2eParent.TrimEnd('\').Equals(
                    $expectedE2eParent.TrimEnd('\'),
                    [StringComparison]::OrdinalIgnoreCase) `
                -or $actualE2eName -notmatch '^ResticPal-Installed-E2E-[0-9a-f]{32}$') {
                throw "Refusing to remove an unsafe installed-test directory: $resolvedE2eRoot"
            }
            Remove-Item -LiteralPath $resolvedE2eRoot -Recurse -Force
            if (Test-Path -LiteralPath $resolvedE2eRoot) {
                throw "The disposable backup and restore test directory was not removed: $resolvedE2eRoot"
            }
        }
    }
}
