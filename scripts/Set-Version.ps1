[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string] $Version
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$utf8NoBom = New-Object Text.UTF8Encoding($false)

function Replace-ExactlyOnce(
    [string] $Path,
    [string] $Pattern,
    [string] $Replacement
) {
    $text = [IO.File]::ReadAllText($Path)
    $regex = New-Object Text.RegularExpressions.Regex($Pattern)
    if ($regex.Matches($text).Count -ne 1) {
        throw "Expected exactly one version field matching '$Pattern' in $Path."
    }
    $updated = $regex.Replace($text, $Replacement, 1)
    [IO.File]::WriteAllText($Path, $updated, $utf8NoBom)
}

$cargoManifest = Join-Path $repositoryRoot 'Cargo.toml'
Replace-ExactlyOnce `
    -Path $cargoManifest `
    -Pattern '(?m)^version\s*=\s*"\d+\.\d+\.\d+"\s*$' `
    -Replacement "version = `"$Version`""

$projectPath = Join-Path $repositoryRoot 'apps\ResticPal.UI\ResticPal.UI.csproj'
foreach ($field in @('Version', 'InformationalVersion')) {
    Replace-ExactlyOnce `
        -Path $projectPath `
        -Pattern "<$field>[^<]+</$field>" `
        -Replacement "<$field>$Version</$field>"
}
$fourPartVersion = "$Version.0"
foreach ($field in @('AssemblyVersion', 'FileVersion')) {
    Replace-ExactlyOnce `
        -Path $projectPath `
        -Pattern "<$field>[^<]+</$field>" `
        -Replacement "<$field>$fourPartVersion</$field>"
}

$applicationManifest = Join-Path $repositoryRoot 'apps\ResticPal.UI\app.manifest'
Replace-ExactlyOnce `
    -Path $applicationManifest `
    -Pattern '<assemblyIdentity version="[^"]+" name="resticpal-ui\.app" />' `
    -Replacement "<assemblyIdentity version=`"$fourPartVersion`" name=`"resticpal-ui.app`" />"

Push-Location $repositoryRoot
try {
    & cargo metadata --format-version 1 --offline | Out-Null
    if ($LASTEXITCODE -ne 0) {
        throw "Updating Cargo.lock failed with exit code $LASTEXITCODE."
    }
    & (Join-Path $PSScriptRoot 'Test-VersionConsistency.ps1')
} finally {
    Pop-Location
}

Write-Host "resticpal product version is now $Version."
