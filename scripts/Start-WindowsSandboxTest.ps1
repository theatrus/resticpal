[CmdletBinding()]
param(
    [string] $MsiPath,

    [ValidateRange(2048, 32768)]
    [int] $MemoryInMB = 8192,

    [ValidateRange(1, 60)]
    [int] $TimeoutMinutes = 15,

    [switch] $EnableNetworking,
    [switch] $KeepOpen,
    [switch] $GenerateOnly
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$sandboxExecutable = Join-Path $env:SystemRoot 'System32\WindowsSandbox.exe'
if (-not $GenerateOnly -and -not (Test-Path -LiteralPath $sandboxExecutable -PathType Leaf)) {
    throw 'Windows Sandbox is unavailable. Run scripts\Enable-WindowsSandbox.ps1 as administrator, restart Windows, and try again.'
}

if ([string]::IsNullOrWhiteSpace($MsiPath)) {
    $candidates = @(Get-ChildItem `
        -LiteralPath (Join-Path $repositoryRoot 'artifacts\installer\output') `
        -Filter 'resticpal-*-x64.msi' `
        -File `
        -ErrorAction SilentlyContinue)
    if ($candidates.Count -ne 1) {
        throw 'Pass -MsiPath when the installer output directory does not contain exactly one MSI.'
    }
    $MsiPath = $candidates[0].FullName
}
$resolvedMsiPath = (Resolve-Path -LiteralPath $MsiPath).Path

$runId = '{0}-{1}' -f (Get-Date -Format 'yyyyMMdd-HHmmss'), ([Guid]::NewGuid().ToString('N').Substring(0, 8))
$runRoot = Join-Path $repositoryRoot "artifacts\windows-sandbox\$runId"
New-Item -ItemType Directory -Path $runRoot -Force | Out-Null
$sandboxMsiPath = Join-Path $runRoot 'resticpal.msi'
Copy-Item -LiteralPath $resolvedMsiPath -Destination $sandboxMsiPath
$stagedServicePath = Join-Path $repositoryRoot 'artifacts\installer\stage\resticpal-service.exe'
if (Test-Path -LiteralPath $stagedServicePath -PathType Leaf) {
    Copy-Item -LiteralPath $stagedServicePath -Destination (Join-Path $runRoot 'resticpal-service.exe')
}

$networking = if ($EnableNetworking) { 'Enable' } else { 'Disable' }
$keepOpenArgument = if ($KeepOpen) { ' -KeepOpen' } else { '' }
$escapedRepositoryRoot = [Security.SecurityElement]::Escape($repositoryRoot)
$escapedRunRoot = [Security.SecurityElement]::Escape($runRoot)
$configuration = @"
<Configuration>
  <VGpu>Disable</VGpu>
  <Networking>$networking</Networking>
  <AudioInput>Disable</AudioInput>
  <VideoInput>Disable</VideoInput>
  <PrinterRedirection>Disable</PrinterRedirection>
  <ClipboardRedirection>Disable</ClipboardRedirection>
  <MemoryInMB>$MemoryInMB</MemoryInMB>
  <MappedFolders>
    <MappedFolder>
      <HostFolder>$escapedRepositoryRoot</HostFolder>
      <SandboxFolder>C:\ResticPalSource</SandboxFolder>
      <ReadOnly>true</ReadOnly>
    </MappedFolder>
    <MappedFolder>
      <HostFolder>$escapedRunRoot</HostFolder>
      <SandboxFolder>C:\ResticPalRun</SandboxFolder>
      <ReadOnly>false</ReadOnly>
    </MappedFolder>
  </MappedFolders>
  <LogonCommand>
    <Command>powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File C:\ResticPalSource\scripts\Invoke-WindowsSandboxTest.ps1 -MsiPath C:\ResticPalRun\resticpal.msi -ResultRoot C:\ResticPalRun$keepOpenArgument</Command>
  </LogonCommand>
</Configuration>
"@
$configurationPath = Join-Path $runRoot 'resticpal-test.wsb'
[IO.File]::WriteAllText($configurationPath, $configuration, [Text.UTF8Encoding]::new($false))

if ($GenerateOnly) {
    Write-Host "Generated $configurationPath"
    Get-Item -LiteralPath $configurationPath
    return
}

$activeSandboxProcesses = @(Get-Process `
    -Name WindowsSandboxRemoteSession, WindowsSandboxServer `
    -ErrorAction SilentlyContinue)
if ($activeSandboxProcesses.Count -gt 0) {
    throw 'Another Windows Sandbox session is active. Close it before starting an automated test.'
}

Write-Host "Starting disposable Windows test VM. Results will be written to $runRoot"
$null = Start-Process `
    -FilePath $sandboxExecutable `
    -ArgumentList "`"$configurationPath`""

$resultPath = Join-Path $runRoot 'result.json'
$deadline = [DateTime]::UtcNow.AddMinutes($TimeoutMinutes)
$startupDeadline = [DateTime]::UtcNow.AddSeconds(120)
$guestLogPath = Join-Path $runRoot 'guest.log'
while (-not (Test-Path -LiteralPath $resultPath -PathType Leaf)) {
    if (-not (Test-Path -LiteralPath $guestLogPath -PathType Leaf) -and
        [DateTime]::UtcNow -ge $startupDeadline) {
        Get-Process `
            -Name WindowsSandboxRemoteSession, WindowsSandboxServer `
            -ErrorAction SilentlyContinue |
            Stop-Process -Force -ErrorAction SilentlyContinue
        throw "Windows Sandbox did not reach its configured logon command within 120 seconds. See $runRoot"
    }
    if ([DateTime]::UtcNow -ge $deadline) {
        Get-Process `
            -Name WindowsSandboxRemoteSession, WindowsSandboxServer `
            -ErrorAction SilentlyContinue |
            Stop-Process -Force -ErrorAction SilentlyContinue
        throw "Windows Sandbox test timed out after $TimeoutMinutes minutes. See $runRoot"
    }
    Start-Sleep -Seconds 1
}

$result = Get-Content -LiteralPath $resultPath -Raw | ConvertFrom-Json
if (-not $KeepOpen) {
    $shutdownDeadline = [DateTime]::UtcNow.AddSeconds(15)
    do {
        $activeSandboxProcesses = @(Get-Process `
            -Name WindowsSandboxRemoteSession, WindowsSandboxServer `
            -ErrorAction SilentlyContinue)
        if ($activeSandboxProcesses.Count -gt 0) {
            Start-Sleep -Milliseconds 250
        }
    } while ($activeSandboxProcesses.Count -gt 0 -and [DateTime]::UtcNow -lt $shutdownDeadline)
}
$summary = [pscustomobject]@{
    Status = $result.status
    ExitCode = $result.exit_code
    WindowsBuild = $result.windows_build
    RunDirectory = $runRoot
    Transcript = Join-Path $runRoot 'guest.log'
    TestArtifacts = Join-Path $runRoot 'test-artifacts'
}
$summary | Format-List

if ($result.status -ne 'passed' -or $result.exit_code -ne 0) {
    throw "Windows Sandbox test failed: $($result.error). See $(Join-Path $runRoot 'guest.log')"
}
