[CmdletBinding()]
param(
    [string] $Version,
    [string] $ResticPath
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
if ([string]::IsNullOrWhiteSpace($Version)) {
    $manifestText = Get-Content -LiteralPath (Join-Path $repositoryRoot 'Cargo.toml') -Raw
    $versionMatch = [Regex]::Match($manifestText, '(?m)^version\s*=\s*"(?<version>\d+\.\d+\.\d+)"\s*$')
    if (-not $versionMatch.Success) {
        throw 'Unable to determine the workspace version from Cargo.toml.'
    }
    $Version = $versionMatch.Groups['version'].Value
}
if ($Version -notmatch '^\d+\.\d+\.\d+$') {
    throw "Installer version must have three numeric parts: $Version"
}
$artifactRoot = Join-Path $repositoryRoot 'artifacts\installer'
$stageRoot = Join-Path $artifactRoot 'stage'
$intermediateRoot = Join-Path $artifactRoot 'obj'
$outputRoot = Join-Path $artifactRoot 'output'
$msiPath = Join-Path $outputRoot ("resticpal-{0}-x64.msi" -f $Version)

function Reset-GeneratedDirectory([string] $Path) {
    $resolvedRepositoryRoot = [IO.Path]::GetFullPath($repositoryRoot).TrimEnd('\')
    $resolvedPath = [IO.Path]::GetFullPath($Path)
    $resolvedArtifactRoot = [IO.Path]::GetFullPath((Join-Path $resolvedRepositoryRoot 'artifacts')).TrimEnd('\')
    if (-not $resolvedPath.StartsWith($resolvedArtifactRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
        throw "Refusing to reset a directory outside the artifact root: $resolvedPath"
    }
    if (Test-Path -LiteralPath $resolvedPath) {
        Remove-Item -LiteralPath $resolvedPath -Recurse -Force
    }
    New-Item -ItemType Directory -Path $resolvedPath | Out-Null
}

foreach ($tool in @('cargo', 'dotnet', 'wix')) {
    if ($null -eq (Get-Command $tool -ErrorAction SilentlyContinue)) {
        throw "Required build tool is unavailable: $tool"
    }
}
if (-not [Environment]::Is64BitOperatingSystem) {
    throw 'The installer build currently supports only Windows x64.'
}

Reset-GeneratedDirectory $stageRoot
Reset-GeneratedDirectory $intermediateRoot
Reset-GeneratedDirectory $outputRoot

Push-Location $repositoryRoot
try {
    & cargo build --release -p resticpal-service -p resticpal-tray
    if ($LASTEXITCODE -ne 0) {
        throw "Rust release build failed with exit code $LASTEXITCODE"
    }

    & dotnet publish apps/ResticPal.UI/ResticPal.UI.csproj `
        --configuration Release `
        --runtime win-x64 `
        --self-contained true `
        -p:SatelliteResourceLanguages=en-US `
        --output $stageRoot
    if ($LASTEXITCODE -ne 0) {
        throw "WinUI publish failed with exit code $LASTEXITCODE"
    }

    # These three Windows App SDK satellite assemblies contain language metadata
    # that is not representable in MSI's numeric File.Language column. The UI is
    # currently English-only, so omit them rather than suppressing ICE03 globally.
    foreach ($culture in @('gd-gb', 'mi-NZ', 'ug-CN')) {
        $cultureDirectory = Join-Path $stageRoot $culture
        if (Test-Path -LiteralPath $cultureDirectory) {
            Remove-Item -LiteralPath $cultureDirectory -Recurse -Force
        }
    }
    Get-ChildItem -LiteralPath $stageRoot -Filter '*.pdb' -File -Recurse |
        Remove-Item -Force
    Copy-Item -LiteralPath (Join-Path $repositoryRoot 'target\release\resticpal-service.exe') -Destination $stageRoot
    Copy-Item -LiteralPath (Join-Path $repositoryRoot 'target\release\resticpal-tray.exe') -Destination $stageRoot

    $stagedRestic = Join-Path $stageRoot 'restic.exe'
    if ([string]::IsNullOrWhiteSpace($ResticPath)) {
        & (Join-Path $PSScriptRoot 'Get-Restic.ps1') -DestinationPath $stagedRestic
    } else {
        $resolvedRestic = (Resolve-Path -LiteralPath $ResticPath).Path
        Copy-Item -LiteralPath $resolvedRestic -Destination $stagedRestic
        & $stagedRestic version
        if ($LASTEXITCODE -ne 0) {
            throw "Unable to execute the staged restic binary: $stagedRestic"
        }
    }

    Copy-Item -LiteralPath (Join-Path $repositoryRoot 'LICENSE') -Destination (Join-Path $stageRoot 'LICENSE-resticpal.txt')
    Copy-Item -LiteralPath (Join-Path $repositoryRoot 'THIRD-PARTY-NOTICES.md') -Destination $stageRoot

    $dotnetRoot = Split-Path -Parent (Get-Command dotnet).Source
    $licenseRoot = Join-Path $stageRoot 'licenses'
    New-Item -ItemType Directory -Path $licenseRoot | Out-Null
    Copy-Item -LiteralPath (Join-Path $dotnetRoot 'LICENSE.txt') -Destination (Join-Path $licenseRoot 'dotnet-LICENSE.txt')
    Copy-Item -LiteralPath (Join-Path $dotnetRoot 'ThirdPartyNotices.txt') -Destination (Join-Path $licenseRoot 'dotnet-ThirdPartyNotices.txt')

    $globalPackages = ((& dotnet nuget locals global-packages --list) -replace '^global-packages:\s*', '').Trim()
    $windowsAppSdkRoot = Join-Path $globalPackages 'microsoft.windowsappsdk\2.3.1'
    Copy-Item -LiteralPath (Join-Path $windowsAppSdkRoot 'license.txt') -Destination (Join-Path $licenseRoot 'windows-app-sdk-LICENSE.txt')
    Copy-Item -LiteralPath (Join-Path $windowsAppSdkRoot 'NOTICE.txt') -Destination (Join-Path $licenseRoot 'windows-app-sdk-NOTICE.txt')

    $cargoMetadata = & cargo metadata --format-version 1 --locked
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo metadata failed with exit code $LASTEXITCODE"
    }
    $cargoMetadata | Set-Content -LiteralPath (Join-Path $licenseRoot 'rust-package-metadata.json') -Encoding utf8

    & wix build installer/Product.wxs `
        -arch x64 `
        -ext WixToolset.Util.wixext `
        -d "StageDir=$stageRoot" `
        -d "BrandIcon=$(Join-Path $repositoryRoot 'assets\resticpal.ico')" `
        -d "ProductVersion=$Version" `
        -intermediatefolder $intermediateRoot `
        -out $msiPath
    if ($LASTEXITCODE -ne 0) {
        throw "WiX build failed with exit code $LASTEXITCODE"
    }

    & wix msi validate $msiPath -pdb ([IO.Path]::ChangeExtension($msiPath, '.wixpdb'))
    if ($LASTEXITCODE -ne 0) {
        throw "MSI validation failed with exit code $LASTEXITCODE"
    }
} finally {
    Pop-Location
}

Write-Host "Built and validated $msiPath"
Get-Item -LiteralPath $msiPath
