[CmdletBinding()]
param(
    [string] $MsiPath
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
if ([string]::IsNullOrWhiteSpace($MsiPath)) {
    $candidates = @(Get-ChildItem -LiteralPath (Join-Path $repositoryRoot 'artifacts\installer\output') -Filter 'resticpal-*-x64.msi' -File -ErrorAction SilentlyContinue)
    if ($candidates.Count -ne 1) {
        throw 'Pass -MsiPath when the installer output directory does not contain exactly one MSI.'
    }
    $MsiPath = $candidates[0].FullName
}
$resolvedMsiPath = (Resolve-Path -LiteralPath $MsiPath).Path
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
        'Property Id="ARPPRODUCTICON" Value="ResticPalIcon"',
        '<CustomTable Id="Wix4SecureObject">'
    )) {
        if (-not $decompiledPackage.Contains($requiredAuthoring)) {
            throw "MSI database is missing required authoring: $requiredAuthoring"
        }
    }
    if (-not $decompiledPackage.Contains('<Data Column="User" Value="NT AUTHORITY\SYSTEM" />')) {
        throw 'MSI database does not contain the LocalSystem application-data ACL entry.'
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
