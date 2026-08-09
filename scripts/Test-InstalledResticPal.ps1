[CmdletBinding()]
param(
    [string] $MsiPath,
    [switch] $KeepInstalled
)

$ErrorActionPreference = 'Stop'
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
$installRoot = Join-Path $env:ProgramFiles 'resticpal'
$dataRoot = Join-Path $env:ProgramData 'ResticPal'
$e2eRoot = Join-Path $dataRoot 'E2E'
$sourceRoot = Join-Path $e2eRoot 'Source'
$backupRoot = Join-Path $e2eRoot 'Repository'
$artifactRoot = Join-Path $repositoryRoot 'artifacts\installer\e2e'
$timestamp = Get-Date -Format 'yyyyMMdd-HHmmss'
$installLog = Join-Path $artifactRoot "install-$timestamp.log"
$uninstallLog = Join-Path $artifactRoot "uninstall-$timestamp.log"
$script:requestId = 0L
$installedByTest = $false
$testReachedPersistenceCheck = $false

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

function Read-Exact([IO.Stream] $Stream, [int] $Count) {
    $buffer = [byte[]]::new($Count)
    $offset = 0
    while ($offset -lt $Count) {
        $read = $Stream.Read($buffer, $offset, $Count - $offset)
        if ($read -eq 0) {
            throw 'The resticpal service closed the named pipe before completing a frame.'
        }
        $offset += $read
    }
    return ,$buffer
}

function Invoke-ResticPalRequest([hashtable] $Command) {
    $script:requestId += 1
    $request = [ordered]@{
        protocol_version = 2
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
        'ResticPal.v2',
        [IO.Pipes.PipeDirection]::InOut,
        [IO.Pipes.PipeOptions]::None
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

        $responseLengthBytes = Read-Exact $client 4
        if (-not [BitConverter]::IsLittleEndian) {
            [Array]::Reverse($responseLengthBytes)
        }
        $responseLength = [BitConverter]::ToUInt32($responseLengthBytes, 0)
        if ($responseLength -eq 0 -or $responseLength -gt 1024 * 1024) {
            throw "Invalid incoming IPC frame length: $responseLength"
        }
        $responseBytes = Read-Exact $client ([int]$responseLength)
        $response = $utf8.GetString($responseBytes) | ConvertFrom-Json
    } finally {
        $client.Dispose()
    }

    if ($response.protocol_version -ne 2 -or $response.request_id -ne $script:requestId) {
        throw 'The service returned a mismatched IPC response.'
    }
    return $response.payload
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

function Wait-Backup([TimeSpan] $Timeout) {
    $deadline = [DateTime]::UtcNow + $Timeout
    do {
        $payload = Invoke-ResticPalRequest @{ type = 'get_run_history'; limit = 10 }
        if ($payload.type -ne 'run_history') {
            throw 'The service did not return backup history.'
        }
        if ($payload.runs.Count -gt 0) {
            $run = $payload.runs[0]
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

if ($null -ne (Get-Service -Name ResticPal -ErrorAction SilentlyContinue)) {
    throw 'A ResticPal service already exists; refusing to modify an existing installation.'
}
if (Test-Path -LiteralPath $installRoot) {
    throw "The install directory already exists: $installRoot"
}
if (Test-Path -LiteralPath $dataRoot) {
    throw "The data directory already exists: $dataRoot"
}
New-Item -ItemType Directory -Path $artifactRoot -Force | Out-Null

try {
    Write-Host "Installing $resolvedMsiPath"
    Invoke-Installer "/i `"$resolvedMsiPath`" /qn /norestart /l*v `"$installLog`"" 'Installation'
    $installedByTest = $true

    $service = Get-Service -Name ResticPal
    $service.WaitForStatus([ServiceProcess.ServiceControllerStatus]::Running, [TimeSpan]::FromSeconds(30))
    $serviceConfiguration = Get-CimInstance Win32_Service -Filter "Name='ResticPal'"
    if ($serviceConfiguration.StartName -ne 'NT SERVICE\ResticPal') {
        throw "Unexpected service identity: $($serviceConfiguration.StartName)"
    }
    if ($serviceConfiguration.StartMode -ne 'Auto') {
        throw "Unexpected service start mode: $($serviceConfiguration.StartMode)"
    }
    foreach ($fileName in @('resticpal-service.exe', 'resticpal-tray.exe', 'resticpal-ui.exe', 'restic.exe')) {
        if (-not (Test-Path -LiteralPath (Join-Path $installRoot $fileName) -PathType Leaf)) {
            throw "Installed payload is missing $fileName"
        }
    }
    $runValue = Get-ItemPropertyValue -LiteralPath 'HKLM:\Software\Microsoft\Windows\CurrentVersion\Run' -Name ResticPal
    if ($runValue -notlike '*resticpal-tray.exe*') {
        throw 'The tray logon registration is missing or invalid.'
    }

    $status = Invoke-ResticPalRequest @{ type = 'get_status' }
    if ($status.type -ne 'status' -or $status.status.state.state -ne 'unconfigured') {
        throw 'A fresh installed service did not report the expected unconfigured state.'
    }

    New-Item -ItemType Directory -Path $sourceRoot -Force | Out-Null
    Set-Content -LiteralPath (Join-Path $sourceRoot 'document.txt') -Value 'resticpal installed-service end-to-end data' -NoNewline

    Assert-Accepted @{
        type = 'update_backup_sources'
        paths = @($sourceRoot)
        exclusions = @()
    }
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

    Assert-Accepted @{ type = 'run_backup_now' }
    $run = Wait-Backup ([TimeSpan]::FromMinutes(3))
    Write-Host "Backup snapshot $($run.snapshot_id) completed through the installed service."

    Restart-Service -Name ResticPal -Force
    (Get-Service -Name ResticPal).WaitForStatus(
        [ServiceProcess.ServiceControllerStatus]::Running,
        [TimeSpan]::FromSeconds(30)
    )
    $historyAfterRestart = Invoke-ResticPalRequest @{ type = 'get_run_history'; limit = 1 }
    if ($historyAfterRestart.runs.Count -ne 1 -or $historyAfterRestart.runs[0].snapshot_id -ne $run.snapshot_id) {
        throw 'Backup history did not survive the installed service restart.'
    }
    $testReachedPersistenceCheck = $true
} finally {
    if ($installedByTest -and -not $KeepInstalled) {
        Write-Host 'Uninstalling the end-to-end package...'
        Invoke-Installer "/x `"$resolvedMsiPath`" /qn /norestart /l*v `"$uninstallLog`"" 'Uninstallation'
        if ($null -ne (Get-Service -Name ResticPal -ErrorAction SilentlyContinue)) {
            throw 'The ResticPal service still exists after uninstall.'
        }
        if (Test-Path -LiteralPath $installRoot) {
            throw 'The resticpal install directory still exists after uninstall.'
        }
        if (-not (Test-Path -LiteralPath $dataRoot)) {
            throw 'Uninstall removed machine backup data instead of preserving it.'
        }
        if ($testReachedPersistenceCheck) {
            Write-Host 'Install, backup, restart, persistence, and uninstall checks passed.'
        }
        Remove-Item -LiteralPath $dataRoot -Recurse -Force
    }
}
