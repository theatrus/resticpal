[CmdletBinding()]
param(
    [string] $ResticPath,
    [switch] $UseVss
)

$ErrorActionPreference = 'Stop'
$downloadRoot = $null
$previousResticPath = [Environment]::GetEnvironmentVariable('RESTICPAL_TEST_RESTIC', 'Process')
$hadPreviousResticPath = Test-Path -LiteralPath Env:RESTICPAL_TEST_RESTIC
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path

try {
    if ([string]::IsNullOrWhiteSpace($ResticPath)) {
        $installedRestic = Get-Command restic.exe -CommandType Application -ErrorAction SilentlyContinue
        if ($null -ne $installedRestic) {
            $ResticPath = $installedRestic.Source
        }
    }

    if ([string]::IsNullOrWhiteSpace($ResticPath)) {
        if (-not [Environment]::Is64BitOperatingSystem) {
            throw 'The automatic test download currently supports only Windows x64.'
        }

        $temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
        $downloadRoot = Join-Path $temporaryRoot ("resticpal-real-restic-{0}" -f [Guid]::NewGuid().ToString('N'))
        New-Item -ItemType Directory -Path $downloadRoot | Out-Null
        $ResticPath = Join-Path $downloadRoot 'restic.exe'
        & (Join-Path $PSScriptRoot 'Get-Restic.ps1') -DestinationPath $ResticPath
    }

    $resolvedResticPath = (Resolve-Path -LiteralPath $ResticPath).Path
    if (-not (Test-Path -LiteralPath $resolvedResticPath -PathType Leaf)) {
        throw "Restic executable is not a file: $resolvedResticPath"
    }

    $versionOutput = & $resolvedResticPath version
    if ($LASTEXITCODE -ne 0) {
        throw "Unable to execute restic at $resolvedResticPath"
    }
    Write-Host ($versionOutput -join [Environment]::NewLine)

    $testName = if ($UseVss) {
        'executor::tests::real_restic_vss_local_repository_lifecycle'
    } else {
        'executor::tests::real_restic_local_repository_lifecycle_without_vss'
    }

    [Environment]::SetEnvironmentVariable('RESTICPAL_TEST_RESTIC', $resolvedResticPath, 'Process')
    Push-Location $repositoryRoot
    try {
        & cargo test -p resticpal-service $testName -- --ignored --exact --nocapture
        if ($LASTEXITCODE -ne 0) {
            throw "Local restic integration test failed with exit code $LASTEXITCODE"
        }
    } finally {
        Pop-Location
    }
} finally {
    if ($hadPreviousResticPath) {
        [Environment]::SetEnvironmentVariable('RESTICPAL_TEST_RESTIC', $previousResticPath, 'Process')
    } else {
        [Environment]::SetEnvironmentVariable('RESTICPAL_TEST_RESTIC', $null, 'Process')
    }

    if ($null -ne $downloadRoot -and (Test-Path -LiteralPath $downloadRoot)) {
        $resolvedDownloadRoot = [IO.Path]::GetFullPath($downloadRoot)
        $resolvedTemporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath()).TrimEnd('\')
        if (-not $resolvedDownloadRoot.StartsWith($resolvedTemporaryRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to remove a download outside the temporary directory: $resolvedDownloadRoot"
        }
        Remove-Item -LiteralPath $resolvedDownloadRoot -Recurse -Force
    }
}
