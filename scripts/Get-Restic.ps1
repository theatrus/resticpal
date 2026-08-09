[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $DestinationPath
)

$ErrorActionPreference = 'Stop'
$resticVersion = '0.19.1'
$resticArchiveSha256 = 'da948ad707ed690426473aaba2046cd61f8f90f6f0e7dab6be0d5796531de67d'
$resticExecutableSha256 = 'b0dd1fd21eea5d8fe1325f55f7118213c21f36de8a261e04c0624a5ab9fd7830'
$temporaryRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
$downloadRoot = Join-Path $temporaryRoot ("resticpal-restic-download-{0}" -f [Guid]::NewGuid().ToString('N'))

try {
    New-Item -ItemType Directory -Path $downloadRoot | Out-Null
    $archiveName = "restic_{0}_windows_amd64.zip" -f $resticVersion
    $archivePath = Join-Path $downloadRoot $archiveName
    $downloadUri = "https://github.com/restic/restic/releases/download/v{0}/{1}" -f $resticVersion, $archiveName
    Write-Host "Downloading restic $resticVersion for windows/amd64..."
    Invoke-WebRequest -UseBasicParsing -Uri $downloadUri -OutFile $archivePath

    $actualHash = (Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actualHash -ne $resticArchiveSha256) {
        throw "Downloaded restic archive has unexpected SHA-256: $actualHash"
    }

    Expand-Archive -LiteralPath $archivePath -DestinationPath $downloadRoot
    $sourcePath = Join-Path $downloadRoot ("restic_{0}_windows_amd64.exe" -f $resticVersion)
    $resolvedDestination = [IO.Path]::GetFullPath($DestinationPath)
    $destinationDirectory = Split-Path -Parent $resolvedDestination
    New-Item -ItemType Directory -Path $destinationDirectory -Force | Out-Null
    Copy-Item -LiteralPath $sourcePath -Destination $resolvedDestination -Force

    $executableHash = (Get-FileHash -LiteralPath $resolvedDestination -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($executableHash -ne $resticExecutableSha256) {
        throw "Extracted restic executable has unexpected SHA-256: $executableHash"
    }
    $versionOutput = & $resolvedDestination version
    if ($LASTEXITCODE -ne 0 -or $versionOutput -notmatch "^restic $([Regex]::Escape($resticVersion)) ") {
        throw "The verified restic executable did not report version $resticVersion."
    }
    Write-Host ($versionOutput -join [Environment]::NewLine)
} finally {
    if (Test-Path -LiteralPath $downloadRoot) {
        $resolvedDownloadRoot = [IO.Path]::GetFullPath($downloadRoot)
        $resolvedTemporaryRoot = $temporaryRoot.TrimEnd('\')
        if (-not $resolvedDownloadRoot.StartsWith($resolvedTemporaryRoot + '\', [StringComparison]::OrdinalIgnoreCase)) {
            throw "Refusing to remove a download outside the temporary directory: $resolvedDownloadRoot"
        }
        Remove-Item -LiteralPath $resolvedDownloadRoot -Recurse -Force
    }
}
