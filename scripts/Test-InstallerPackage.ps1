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
        'Account="NT SERVICE\ResticPal"',
        'Start="install" Stop="both" Remove="uninstall"',
        'Software\Microsoft\Windows\CurrentVersion\Run',
        '<CustomTable Id="Wix4SecureObject">'
    )) {
        if (-not $decompiledPackage.Contains($requiredAuthoring)) {
            throw "MSI database is missing required authoring: $requiredAuthoring"
        }
    }
    if (-not $decompiledPackage.Contains('<Data Column="User" Value="NT SERVICE\ResticPal" />')) {
        throw 'MSI database does not contain the virtual service account ProgramData ACL entry.'
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
        'restic.exe',
        'LICENSE-resticpal.txt',
        'THIRD-PARTY-NOTICES.md'
    )) {
        if (-not (Test-Path -LiteralPath (Join-Path $installImage $fileName) -PathType Leaf)) {
            throw "Administrative image is missing $fileName"
        }
    }

    $resticPath = Join-Path $installImage 'restic.exe'
    $resticHash = (Get-FileHash -LiteralPath $resticPath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($resticHash -ne $resticExecutableSha256) {
        throw "Administrative-image restic executable has unexpected SHA-256: $resticHash"
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
