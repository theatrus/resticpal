[CmdletBinding()]
param(
    [string] $MsiPath,

    [string] $UpgradeFromMsiPath,

    [ValidateRange(2048, 32768)]
    [int] $MemoryInMB = 8192,

    [ValidateRange(1, 60)]
    [int] $TimeoutMinutes = 15,

    [switch] $EnableNetworking,
    [switch] $KeepOpen,
    [switch] $GenerateOnly,
    [switch] $UseLegacyLauncher
)

$ErrorActionPreference = 'Stop'
$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$sandboxExecutable = Join-Path $env:SystemRoot 'System32\WindowsSandbox.exe'
$sandboxCliCommand = Get-Command wsb -CommandType Application -ErrorAction SilentlyContinue
$useSandboxCli = -not $GenerateOnly -and -not $UseLegacyLauncher -and $null -ne $sandboxCliCommand
if (-not $GenerateOnly -and -not $useSandboxCli -and -not (Test-Path -LiteralPath $sandboxExecutable -PathType Leaf)) {
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
$resolvedUpgradeFromMsiPath = if ([string]::IsNullOrWhiteSpace($UpgradeFromMsiPath)) {
    $null
} else {
    (Resolve-Path -LiteralPath $UpgradeFromMsiPath).Path
}

$runId = '{0}-{1}' -f (Get-Date -Format 'yyyyMMdd-HHmmss'), ([Guid]::NewGuid().ToString('N').Substring(0, 8))
$runRoot = Join-Path $repositoryRoot "artifacts\windows-sandbox\$runId"
New-Item -ItemType Directory -Path $runRoot -Force | Out-Null
$sandboxMsiPath = Join-Path $runRoot 'resticpal.msi'
Copy-Item -LiteralPath $resolvedMsiPath -Destination $sandboxMsiPath
$upgradeArgument = ''
if ($null -ne $resolvedUpgradeFromMsiPath) {
    $sandboxUpgradeMsiPath = Join-Path $runRoot 'resticpal-upgrade-from.msi'
    Copy-Item -LiteralPath $resolvedUpgradeFromMsiPath -Destination $sandboxUpgradeMsiPath
    $upgradeArgument = ' -UpgradeFromMsiPath C:\ResticPalRun\resticpal-upgrade-from.msi'
}
$stagedServicePath = Join-Path $repositoryRoot 'artifacts\installer\stage\resticpal-service.exe'
if (Test-Path -LiteralPath $stagedServicePath -PathType Leaf) {
    Copy-Item -LiteralPath $stagedServicePath -Destination (Join-Path $runRoot 'resticpal-service.exe')
}

$networking = if ($EnableNetworking) { 'Enable' } else { 'Disable' }
$keepOpenArgument = if ($KeepOpen) { ' -KeepOpen' } else { '' }
$runtimeConfiguration = @"
<Configuration>
  <VGpu>Disable</VGpu>
  <Networking>$networking</Networking>
  <AudioInput>Disable</AudioInput>
  <VideoInput>Disable</VideoInput>
  <PrinterRedirection>Disable</PrinterRedirection>
  <ClipboardRedirection>Disable</ClipboardRedirection>
  <MemoryInMB>$MemoryInMB</MemoryInMB>
</Configuration>
"@
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
    <Command>powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File C:\ResticPalSource\scripts\Invoke-WindowsSandboxTest.ps1 -MsiPath C:\ResticPalRun\resticpal.msi -ResultRoot C:\ResticPalRun$upgradeArgument$keepOpenArgument</Command>
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

if ($null -ne $sandboxCliCommand) {
    $listOutput = & $sandboxCliCommand.Source list --raw 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "Could not query running Windows Sandbox environments: $($listOutput | Out-String)"
    }
    $runningSandboxes = $listOutput | Out-String | ConvertFrom-Json
    if (@($runningSandboxes.WindowsSandboxEnvironments).Count -gt 0) {
        throw 'Another Windows Sandbox session is active. Close it before starting an automated test.'
    }
} else {
    $activeSandboxProcesses = @(Get-Process `
        -Name WindowsSandboxRemoteSession, WindowsSandboxServer `
        -ErrorAction SilentlyContinue)
    if ($activeSandboxProcesses.Count -gt 0) {
        throw 'Another Windows Sandbox session is active. Close it before starting an automated test.'
    }
}

Write-Host "Starting disposable Windows test VM. Results will be written to $runRoot"
$sandboxId = $null
$guestExecutionJob = $null
$sandboxCliTranscript = Join-Path $runRoot 'sandbox-cli-exec.log'

function Stop-AutomatedSandbox {
    if ($useSandboxCli -and -not [string]::IsNullOrWhiteSpace($sandboxId)) {
        & $sandboxCliCommand.Source stop --id $sandboxId --raw 2>&1 | Out-Null
        return
    }
    Get-Process `
        -Name WindowsSandboxRemoteSession, WindowsSandboxServer `
        -ErrorAction SilentlyContinue |
        Stop-Process -Force -ErrorAction SilentlyContinue
}

if ($useSandboxCli) {
    try {
        $startOutput = & $sandboxCliCommand.Source start --config $runtimeConfiguration --raw 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "Windows Sandbox CLI could not start a guest: $($startOutput | Out-String)"
        }
        $startResult = $startOutput | Out-String | ConvertFrom-Json
        $sandboxId = $startResult.Id
        if ([string]::IsNullOrWhiteSpace($sandboxId)) {
            throw 'Windows Sandbox CLI did not return a guest ID.'
        }

        $shareOutput = & $sandboxCliCommand.Source share `
            --id $sandboxId `
            --host-path $repositoryRoot `
            --sandbox-path 'C:\ResticPalSource' `
            --raw 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "Could not share the source tree with Windows Sandbox: $($shareOutput | Out-String)"
        }
        $shareOutput = & $sandboxCliCommand.Source share `
            --id $sandboxId `
            --host-path $runRoot `
            --sandbox-path 'C:\ResticPalRun' `
            --allow-write `
            --raw 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "Could not share the result directory with Windows Sandbox: $($shareOutput | Out-String)"
        }

        $null = Start-Process `
            -FilePath $sandboxCliCommand.Source `
            -ArgumentList @('connect', '--id', $sandboxId) `
            -WindowStyle Hidden
        $loginDeadline = [DateTime]::UtcNow.AddSeconds(60)
        do {
            Start-Sleep -Seconds 2
            $previousErrorPreference = $ErrorActionPreference
            try {
                $ErrorActionPreference = 'Continue'
                $loginOutput = & $sandboxCliCommand.Source exec `
                    --id $sandboxId `
                    --run-as ExistingLogin `
                    --command 'cmd.exe /d /c exit 0' `
                    --raw 2>&1
                $loginExitCode = $LASTEXITCODE
            } finally {
                $ErrorActionPreference = $previousErrorPreference
            }
        } while ($loginExitCode -ne 0 -and [DateTime]::UtcNow -lt $loginDeadline)
        if ($loginExitCode -ne 0) {
            throw "Windows Sandbox did not establish its interactive administrator session: $($loginOutput | Out-String)"
        }

        $guestCommand = "powershell.exe -NoLogo -NoProfile -ExecutionPolicy Bypass -File C:\ResticPalSource\scripts\Invoke-WindowsSandboxTest.ps1 -MsiPath C:\ResticPalRun\resticpal.msi -ResultRoot C:\ResticPalRun$upgradeArgument -KeepOpen"
        $guestExecutionJob = Start-Job -ScriptBlock {
            param($Executable, $SandboxId, $Command)
            $output = & $Executable exec `
                --id $SandboxId `
                --run-as ExistingLogin `
                --working-directory 'C:\ResticPalSource' `
                --command $Command `
                --raw 2>&1
            $exitCode = $LASTEXITCODE
            [pscustomobject]@{
                ExitCode = $exitCode
                Output = $output | Out-String
            }
        } -ArgumentList $sandboxCliCommand.Source, $sandboxId, $guestCommand
    } catch {
        Stop-AutomatedSandbox
        throw
    }
} else {
    $null = Start-Process `
        -FilePath $sandboxExecutable `
        -ArgumentList "`"$configurationPath`""
}

$resultPath = Join-Path $runRoot 'result.json'
$deadline = [DateTime]::UtcNow.AddMinutes($TimeoutMinutes)
$startupDeadline = [DateTime]::UtcNow.AddSeconds(120)
$guestLogPath = Join-Path $runRoot 'guest.log'
while (-not (Test-Path -LiteralPath $resultPath -PathType Leaf)) {
    if (-not (Test-Path -LiteralPath $guestLogPath -PathType Leaf) -and
        [DateTime]::UtcNow -ge $startupDeadline) {
        Stop-AutomatedSandbox
        throw "Windows Sandbox did not start the guest test within 120 seconds. See $runRoot"
    }
    if ($null -ne $guestExecutionJob -and $guestExecutionJob.State -in @('Completed', 'Failed', 'Stopped')) {
        $executionResult = Receive-Job -Job $guestExecutionJob -ErrorAction SilentlyContinue
        $executionResult | Format-List | Out-String | Set-Content -LiteralPath $sandboxCliTranscript -Encoding UTF8
        Remove-Job -Job $guestExecutionJob -Force -ErrorAction SilentlyContinue
        $guestExecutionJob = $null
        Stop-AutomatedSandbox
        throw "Windows Sandbox stopped the guest test before it wrote a result. See $sandboxCliTranscript"
    }
    if ([DateTime]::UtcNow -ge $deadline) {
        Stop-AutomatedSandbox
        throw "Windows Sandbox test timed out after $TimeoutMinutes minutes. See $runRoot"
    }
    Start-Sleep -Seconds 1
}

$result = Get-Content -LiteralPath $resultPath -Raw | ConvertFrom-Json
if ($null -ne $guestExecutionJob) {
    $null = Wait-Job -Job $guestExecutionJob -Timeout 10
    $executionResult = Receive-Job -Job $guestExecutionJob -ErrorAction SilentlyContinue
    $executionResult | Format-List | Out-String | Set-Content -LiteralPath $sandboxCliTranscript -Encoding UTF8
    Stop-Job -Job $guestExecutionJob -ErrorAction SilentlyContinue
    Remove-Job -Job $guestExecutionJob -Force -ErrorAction SilentlyContinue
    $guestExecutionJob = $null
}
if ($useSandboxCli -and -not $KeepOpen) {
    Stop-AutomatedSandbox
} elseif (-not $KeepOpen) {
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
