[CmdletBinding()]
param(
    [string] $MsiPath
)

$ErrorActionPreference = 'Stop'
if (-not ('ResticPalPeResource' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.ComponentModel;
using System.Runtime.InteropServices;
using System.Text;

public static class ResticPalPeResource
{
    private const uint LoadLibraryAsDataFile = 0x00000002;
    private const uint LoadLibraryAsImageResource = 0x00000020;
    private static readonly IntPtr ApplicationManifestResource = new IntPtr(1);
    private static readonly IntPtr ManifestResourceType = new IntPtr(24);

    [DllImport("kernel32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern IntPtr LoadLibraryEx(
        string fileName,
        IntPtr file,
        uint flags);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr FindResource(
        IntPtr module,
        IntPtr name,
        IntPtr type);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern uint SizeofResource(IntPtr module, IntPtr resource);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr LoadResource(IntPtr module, IntPtr resource);

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr LockResource(IntPtr resourceData);

    [DllImport("kernel32.dll")]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool FreeLibrary(IntPtr module);

    public static string ReadApplicationManifest(string path)
    {
        IntPtr module = LoadLibraryEx(
            path,
            IntPtr.Zero,
            LoadLibraryAsDataFile | LoadLibraryAsImageResource);
        if (module == IntPtr.Zero)
        {
            throw new Win32Exception(
                Marshal.GetLastWin32Error(),
                "Could not load the packaged tray as a resource image.");
        }

        try
        {
            IntPtr resource = FindResource(
                module,
                ApplicationManifestResource,
                ManifestResourceType);
            if (resource == IntPtr.Zero)
            {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "The packaged tray has no RT_MANIFEST resource 1.");
            }

            uint size = SizeofResource(module, resource);
            if (size == 0 || size > 1024 * 1024)
            {
                throw new InvalidOperationException(
                    "The packaged tray application manifest has an invalid size.");
            }

            IntPtr loaded = LoadResource(module, resource);
            IntPtr bytes = loaded == IntPtr.Zero ? IntPtr.Zero : LockResource(loaded);
            if (bytes == IntPtr.Zero)
            {
                throw new Win32Exception(
                    Marshal.GetLastWin32Error(),
                    "Could not read the packaged tray application manifest.");
            }

            var managed = new byte[size];
            Marshal.Copy(bytes, managed, 0, managed.Length);
            return Encoding.UTF8.GetString(managed).TrimEnd('\0');
        }
        finally
        {
            FreeLibrary(module);
        }
    }
}
'@
}
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
if ([string]::IsNullOrWhiteSpace($MsiPath)) {
    $candidates = @(Get-ChildItem -LiteralPath (Join-Path $repositoryRoot 'artifacts\installer\output') -Filter 'resticpal-*-x64.msi' -File -ErrorAction SilentlyContinue)
    if ($candidates.Count -ne 1) {
        throw 'Pass -MsiPath when the installer output directory does not contain exactly one MSI.'
    }
    $MsiPath = $candidates[0].FullName
}
$resolvedMsiPath = (Resolve-Path -LiteralPath $MsiPath).Path
$versionMatch = [Regex]::Match(
    [IO.Path]::GetFileName($resolvedMsiPath),
    '^resticpal-(?<version>\d+\.\d+\.\d+)-x64\.msi$')
if (-not $versionMatch.Success) {
    throw "The MSI file name does not contain the expected product version: $resolvedMsiPath"
}
$productVersion = $versionMatch.Groups['version'].Value
$fileVersion = "$productVersion.0"
$artifactRoot = [IO.Path]::GetFullPath((Join-Path $repositoryRoot 'artifacts'))
$testRoot = Join-Path $artifactRoot ("installer\package-test-{0}" -f [Guid]::NewGuid().ToString('N'))
$adminImageRoot = Join-Path $testRoot 'image'
$logPath = Join-Path $testRoot 'admin-image.log'
$resticExecutableSha256 = 'b0dd1fd21eea5d8fe1325f55f7118213c21f36de8a261e04c0624a5ab9fd7830'

try {
    New-Item -ItemType Directory -Path $testRoot | Out-Null
    & wix msi validate $resolvedMsiPath -pdb ([IO.Path]::ChangeExtension($resolvedMsiPath, '.wixpdb'))
    if ($LASTEXITCODE -ne 0) {
        throw "MSI validation failed with exit code $LASTEXITCODE"
    }

    $decompiledPath = Join-Path $testRoot 'package.wxs'
    & wix msi decompile $resolvedMsiPath -o $decompiledPath | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "MSI decompilation failed with exit code $LASTEXITCODE"
    }
    $decompiledPackage = Get-Content -LiteralPath $decompiledPath -Raw
    foreach ($requiredAuthoring in @(
        'Account="LocalSystem"',
        'Start="install" Stop="both" Remove="uninstall"',
        'Software\Microsoft\Windows\CurrentVersion\Run',
        'Property Id="RESTICPAL_BOOTSTRAP_URL"',
        'Secure="yes" Hidden="yes"',
        'Name="BootstrapUrl"',
        'Value="[RESTICPAL_BOOTSTRAP_URL]"',
        'Property Id="ARPPRODUCTICON" Value="ResticPalIcon.ico"',
        'Property Id="MSIDISABLERMRESTART" Value="1"',
        'Shortcut Id="ResticPalSettingsShortcut"',
        'Directory="ProgramMenuFolder"',
        'File Id="TrayExecutable"',
        'CustomAction Id="LaunchTrayAfterInstall"',
        'DllEntry="WixUnelevatedShellExec"',
        'Condition="NOT (REMOVE ~= &quot;ALL&quot;)"',
        '<CustomTable Id="Wix4CloseApplication">',
        '<Data Column="CloseApplication" Value="CloseResticPalTray" />',
        '<Data Column="CloseApplication" Value="CloseResticPalUi" />',
        '<CustomTable Id="Wix4SecureObject">'
    )) {
        if (-not $decompiledPackage.Contains($requiredAuthoring)) {
            throw "MSI database is missing required authoring: $requiredAuthoring"
        }
    }
    if (-not $decompiledPackage.Contains('<Data Column="User" Value="NT AUTHORITY\SYSTEM" />')) {
        throw 'MSI database does not contain the LocalSystem application-data ACL entry.'
    }

    # Keep RemoveExistingProducts after the candidate payload is installed.
    # An early major-upgrade removal can delete a higher-patch self-contained
    # .NET runtime after MSI has already declined to copy the candidate's lower
    # versioned shared components, leaving resticpal-ui without hostfxr/coreclr.
    $installer = New-Object -ComObject WindowsInstaller.Installer
    $database = $null
    $view = $null
    $record = $null
    try {
        $database = $installer.GetType().InvokeMember(
            'OpenDatabase', 'InvokeMethod', $null, $installer, @($resolvedMsiPath, 0))
        $view = $database.GetType().InvokeMember(
            'OpenView',
            'InvokeMethod',
            $null,
            $database,
            'SELECT `Sequence` FROM `InstallExecuteSequence` WHERE `Action` = ''RemoveExistingProducts''')
        $view.GetType().InvokeMember('Execute', 'InvokeMethod', $null, $view, $null) | Out-Null
        $record = $view.GetType().InvokeMember('Fetch', 'InvokeMethod', $null, $view, $null)
        if ($null -eq $record -or $record.IntegerData(1) -ne 6501) {
            $observedSequence = if ($null -eq $record) { '<missing>' } else { $record.IntegerData(1) }
            throw "RemoveExistingProducts must run immediately after InstallExecute (6501); got $observedSequence."
        }

        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($record) | Out-Null
        $record = $null
        $view.GetType().InvokeMember('Close', 'InvokeMethod', $null, $view, $null) | Out-Null
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($view) | Out-Null
        $view = $null

        $view = $database.GetType().InvokeMember(
            'OpenView',
            'InvokeMethod',
            $null,
            $database,
            'SELECT `Sequence` FROM `InstallExecuteSequence` WHERE `Action` = ''Wix4CloseApplications_X64''')
        $view.GetType().InvokeMember('Execute', 'InvokeMethod', $null, $view, $null) | Out-Null
        $record = $view.GetType().InvokeMember('Fetch', 'InvokeMethod', $null, $view, $null)
        $closeApplicationsSequence = if ($null -eq $record) { $null } else { $record.IntegerData(1) }
        if (
            $null -eq $closeApplicationsSequence -or
            $closeApplicationsSequence -le 1500 -or
            $closeApplicationsSequence -ge 3500
        ) {
            $observedSequence = if ($null -eq $closeApplicationsSequence) {
                '<missing>'
            } else {
                $closeApplicationsSequence
            }
            throw "CloseApplications must run after InstallInitialize (1500) and before RemoveFiles (3500); got $observedSequence."
        }
    } finally {
        if ($null -ne $record) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($record) | Out-Null
        }
        if ($null -ne $view) {
            $view.GetType().InvokeMember('Close', 'InvokeMethod', $null, $view, $null) | Out-Null
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($view) | Out-Null
        }
        if ($null -ne $database) {
            [Runtime.InteropServices.Marshal]::FinalReleaseComObject($database) | Out-Null
        }
        [Runtime.InteropServices.Marshal]::FinalReleaseComObject($installer) | Out-Null
    }

    $arguments = "/a `"$resolvedMsiPath`" TARGETDIR=`"$adminImageRoot`" /qn /norestart /l*v `"$logPath`""
    $process = Start-Process -FilePath "$env:SystemRoot\System32\msiexec.exe" `
        -ArgumentList $arguments `
        -WindowStyle Hidden `
        -Wait `
        -PassThru
    if ($process.ExitCode -ne 0) {
        throw "MSI administrative install failed with exit code $($process.ExitCode)"
    }

    $installImage = Join-Path $adminImageRoot 'PFiles64\resticpal'
    foreach ($fileName in @(
        'resticpal-service.exe',
        'resticpal-tray.exe',
        'resticpal-ui.exe',
        'coreclr.dll',
        'hostfxr.dll',
        'hostpolicy.dll',
        'App.xbf',
        'MainWindow.xbf',
        'resources.pri',
        'resticpal.ico',
        'resticpal-logo.png',
        'restic.exe',
        'LICENSE-resticpal.txt',
        'THIRD-PARTY-NOTICES.md'
    )) {
        if (-not (Test-Path -LiteralPath (Join-Path $installImage $fileName) -PathType Leaf)) {
            throw "Administrative image is missing $fileName"
        }
    }

    foreach ($fileName in @('resticpal-service.exe', 'resticpal-tray.exe', 'resticpal-ui.exe')) {
        $versionInfo = [Diagnostics.FileVersionInfo]::GetVersionInfo(
            (Join-Path $installImage $fileName))
        $versionMismatch = (
            $versionInfo.FileVersion -cne $fileVersion -or
            $versionInfo.ProductVersion -cne $productVersion
        )
        if ($versionMismatch) {
            throw ("$fileName version mismatch: file=$($versionInfo.FileVersion), " +
                   "product=$($versionInfo.ProductVersion), expected=$productVersion")
        }
    }

    $trayPath = Join-Path $installImage 'resticpal-tray.exe'
    $trayManifest = [ResticPalPeResource]::ReadApplicationManifest($trayPath)
    $trayManifestDocument = [xml] $trayManifest
    $trayAssemblyIdentity = $trayManifestDocument.SelectSingleNode(
        "/*[local-name()='assembly']/*[local-name()='assemblyIdentity']")
    if (
        $null -eq $trayAssemblyIdentity -or
        $trayAssemblyIdentity.GetAttribute('processorArchitecture') -cne 'amd64'
    ) {
        throw 'The packaged x64 tray manifest definition identity must use processorArchitecture="amd64".'
    }
    foreach ($requiredManifestEntry in @(
        "version=`"$fileVersion`"",
        '<requestedExecutionLevel level="asInvoker" uiAccess="false" />',
        '>true/pm</dpiAware>',
        '>PerMonitorV2, PerMonitor</dpiAwareness>',
        'name="Microsoft.Windows.Common-Controls"',
        'version="6.0.0.0"'
    )) {
        if (-not $trayManifest.Contains($requiredManifestEntry)) {
            throw "The packaged tray manifest is missing: $requiredManifestEntry"
        }
    }
    if ($trayManifest.Contains('level="requireAdministrator"')) {
        throw 'The packaged tray must remain asInvoker so all-users login never prompts for elevation.'
    }

    $resticPath = Join-Path $installImage 'restic.exe'
    # restic ships unsigned from upstream, so the release build signs it along
    # with everything else in the payload -- an unsigned executable doing the
    # actual backups would be the one thing a user runs without a publisher
    # identity. Signing rewrites the file, so the pinned upstream hash no
    # longer matches, and asserting it unconditionally would fail every signed
    # build.
    #
    # Both states are still checked, just differently. Unsigned: the bytes must
    # be exactly what upstream published. Signed: it must carry a valid
    # signature naming us. The supply-chain guarantee is not weakened -- the
    # hash is verified against upstream at download time in Get-Restic.ps1,
    # before anything touches the file.
    $resticHash = (Get-FileHash -LiteralPath $resticPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($resticHash -eq $resticExecutableSha256) {
        Write-Host 'Packaged restic matches the pinned upstream hash (unsigned build).'
    }
    else {
        $signature = Get-AuthenticodeSignature -LiteralPath $resticPath
        if ($signature.Status -ne 'Valid') {
            throw ("Packaged restic is neither the pinned upstream build " +
                   "($resticExecutableSha256) nor validly signed; got hash " +
                   "$resticHash and signature status $($signature.Status).")
        }
        if ($signature.SignerCertificate.Subject -notmatch 'StackFoundry LLC') {
            throw ("Packaged restic is signed by an unexpected publisher: " +
                   $signature.SignerCertificate.Subject)
        }
        Write-Host 'Packaged restic is signed by StackFoundry LLC (signed build).'
    }
    & $resticPath version
    if ($LASTEXITCODE -ne 0) {
        throw 'Administrative-image restic executable did not start.'
    }

    $servicePath = Join-Path $installImage 'resticpal-service.exe'
    & $servicePath --console --config (Resolve-Path -LiteralPath (Join-Path $repositoryRoot 'config\resticpal.example.toml')).Path
    if ($LASTEXITCODE -ne 0) {
        throw 'Administrative-image service console smoke test failed.'
    }
    Write-Host 'MSI validation, administrative extraction, payload integrity, and packaged binary smoke tests passed.'
} finally {
    if (Test-Path -LiteralPath $testRoot) {
        $resolvedTestRoot = [IO.Path]::GetFullPath($testRoot)
        $resolvedArtifactRoot = $artifactRoot.TrimEnd('\')
        if (-not $resolvedTestRoot.StartsWith($resolvedArtifactRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to remove a package test outside the artifact directory: $resolvedTestRoot"
        }
        Remove-Item -LiteralPath $resolvedTestRoot -Recurse -Force
    }
}
