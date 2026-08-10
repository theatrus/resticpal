[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string] $MsiPath,

    [Parameter(Mandatory)]
    [string] $ResultRoot,

    [string] $UpgradeFromMsiPath,

    [switch] $KeepOpen
)

$ErrorActionPreference = 'Stop'
$startedAt = [DateTimeOffset]::UtcNow
$resultPath = Join-Path $ResultRoot 'result.json'
$temporaryResultPath = Join-Path $ResultRoot 'result.json.tmp'
$transcriptPath = Join-Path $ResultRoot 'guest.log'
$localTestRoot = 'C:\ResticPalTest'
$localMsiPath = Join-Path $localTestRoot 'resticpal.msi'
$localUpgradeFromMsiPath = Join-Path $localTestRoot 'resticpal-upgrade-from.msi'
$localTestArtifactRoot = Join-Path $localTestRoot 'artifacts'
$exportedTestArtifactRoot = Join-Path $ResultRoot 'test-artifacts'
$status = 'failed'
$exitCode = 1
$errorMessage = $null
$transcriptStarted = $false

function Export-GuestDiagnostics {
    New-Item -ItemType Directory -Path $localTestArtifactRoot -Force | Out-Null
    $eventStart = $startedAt.LocalDateTime.AddMinutes(-1)
    $systemEvents = @(Get-WinEvent -FilterHashtable @{
        LogName = 'System'
        ProviderName = 'Service Control Manager'
        StartTime = $eventStart
    } -ErrorAction SilentlyContinue)
    $applicationEvents = @(Get-WinEvent -FilterHashtable @{
        LogName = 'Application'
        ProviderName = @('Application Error', 'Windows Error Reporting')
        StartTime = $eventStart
    } -ErrorAction SilentlyContinue)
    $codeIntegrityEvents = @(Get-WinEvent -FilterHashtable @{
        LogName = 'Microsoft-Windows-CodeIntegrity/Operational'
        StartTime = $eventStart
    } -ErrorAction SilentlyContinue)
    $events = @($systemEvents + $applicationEvents + $codeIntegrityEvents) |
        Sort-Object TimeCreated |
        Select-Object TimeCreated, LogName, ProviderName, Id, LevelDisplayName, Message
    $events |
        ConvertTo-Json -Depth 4 |
        Set-Content -LiteralPath (Join-Path $localTestArtifactRoot 'windows-events.json') -Encoding UTF8
}

function Export-ServiceStartupProbes {
    $probeOutputPath = Join-Path $localTestArtifactRoot 'service-startup-probes.json'
    $sourceExecutable = Join-Path $ResultRoot 'resticpal-service.exe'
    if (-not (Test-Path -LiteralPath $sourceExecutable -PathType Leaf)) {
        @([pscustomobject]@{
            name = 'probe-setup'
            error = "The staged service executable was not available at $sourceExecutable."
        }) | ConvertTo-Json -Depth 3 | Set-Content -LiteralPath $probeOutputPath -Encoding UTF8
        return
    }

    $probeRoot = 'C:\ResticPalServiceProbe'
    $probeExecutable = Join-Path $probeRoot 'resticpal-service.exe'
    $dataRoot = Join-Path $env:ProgramData 'ResticPal'
    New-Item -ItemType Directory -Path $probeRoot -Force | Out-Null
    Copy-Item -LiteralPath $sourceExecutable -Destination $probeExecutable -Force

    $startupErrorPath = Join-Path $dataRoot 'service-startup-errors.log'
    $consoleOutputPath = Join-Path $probeRoot 'console.stdout.log'
    $consoleErrorPath = Join-Path $probeRoot 'console.stderr.log'
    $consoleProcess = Start-Process -FilePath $probeExecutable `
        -ArgumentList @('--console', '--config', (Join-Path $probeRoot 'config.toml')) `
        -RedirectStandardOutput $consoleOutputPath `
        -RedirectStandardError $consoleErrorPath `
        -WindowStyle Hidden `
        -Wait `
        -PassThru
    Remove-Item -LiteralPath $startupErrorPath -Force -ErrorAction SilentlyContinue
    $directOutputPath = Join-Path $probeRoot 'direct.stdout.log'
    $directErrorPath = Join-Path $probeRoot 'direct.stderr.log'
    $directProcess = Start-Process -FilePath $probeExecutable `
        -RedirectStandardOutput $directOutputPath `
        -RedirectStandardError $directErrorPath `
        -WindowStyle Hidden `
        -Wait `
        -PassThru
    $preflight = [ordered]@{
        console_exit_code = $consoleProcess.ExitCode
        console_stdout = Get-Content -LiteralPath $consoleOutputPath -Raw -ErrorAction SilentlyContinue
        console_stderr = Get-Content -LiteralPath $consoleErrorPath -Raw -ErrorAction SilentlyContinue
        direct_exit_code = $directProcess.ExitCode
        direct_stdout = Get-Content -LiteralPath $directOutputPath -Raw -ErrorAction SilentlyContinue
        direct_stderr = Get-Content -LiteralPath $directErrorPath -Raw -ErrorAction SilentlyContinue
        direct_startup_error = Get-Content -LiteralPath $startupErrorPath -Raw -ErrorAction SilentlyContinue
    }
    $preflight |
        ConvertTo-Json -Depth 4 |
        Set-Content -LiteralPath (Join-Path $localTestArtifactRoot 'service-process-preflight.json') -Encoding UTF8

    function Remove-ProbeService {
        $service = Get-Service -Name ResticPal -ErrorAction SilentlyContinue
        if ($service -and $service.Status -ne [ServiceProcess.ServiceControllerStatus]::Stopped) {
            Stop-Service -Name ResticPal -Force -ErrorAction SilentlyContinue
        }
        Get-Process -Name 'resticpal-service' -ErrorAction SilentlyContinue |
            Stop-Process -Force -ErrorAction SilentlyContinue
        & "$env:SystemRoot\System32\sc.exe" delete ResticPal 2>&1 | Out-Null
        $deadline = [DateTime]::UtcNow.AddSeconds(10)
        while ($null -ne (Get-Service -Name ResticPal -ErrorAction SilentlyContinue)) {
            if ([DateTime]::UtcNow -ge $deadline) {
                break
            }
            Start-Sleep -Milliseconds 250
        }
    }

    Remove-ProbeService

    $probes = @(
        @{ Name = 'local-system'; Account = 'LocalSystem'; UnrestrictedSid = $false },
        @{ Name = 'virtual-account'; Account = 'NT SERVICE\ResticPal'; UnrestrictedSid = $false },
        @{ Name = 'virtual-account-unrestricted-sid'; Account = 'NT SERVICE\ResticPal'; UnrestrictedSid = $true }
    )
    $results = foreach ($probe in $probes) {
        $record = [ordered]@{
            name = $probe.Name
            account = $probe.Account
            unrestricted_sid = $probe.UnrestrictedSid
            create_exit_code = $null
            create_output = $null
            sid_exit_code = $null
            sid_output = $null
            acl_exit_code = $null
            acl_output = $null
            start_exit_code = $null
            start_output = $null
            service_state = $null
            process = $null
            startup_error = $null
        }
        try {
            $createOutput = & "$env:SystemRoot\System32\sc.exe" create ResticPal `
                binPath= $probeExecutable `
                start= demand `
                obj= $probe.Account 2>&1 | Out-String
            $record.create_exit_code = $LASTEXITCODE
            $record.create_output = $createOutput.Trim()
            if ($LASTEXITCODE -ne 0) {
                [pscustomobject]$record
                continue
            }

            New-Item -ItemType Directory -Path $dataRoot -Force | Out-Null
            if ($probe.UnrestrictedSid) {
                $sidOutput = & "$env:SystemRoot\System32\sc.exe" sidtype ResticPal unrestricted 2>&1 | Out-String
                $record.sid_exit_code = $LASTEXITCODE
                $record.sid_output = $sidOutput.Trim()
            }
            if ($probe.Account -like 'NT SERVICE\*') {
                $aclOutput = & "$env:SystemRoot\System32\icacls.exe" $dataRoot `
                    /grant 'NT SERVICE\ResticPal:(OI)(CI)F' /T /C 2>&1 | Out-String
                $record.acl_exit_code = $LASTEXITCODE
                $record.acl_output = $aclOutput.Trim()
            }

            Remove-Item -LiteralPath $startupErrorPath -Force -ErrorAction SilentlyContinue
            $startOutput = & "$env:SystemRoot\System32\sc.exe" start ResticPal 2>&1 | Out-String
            $record.start_exit_code = $LASTEXITCODE
            $record.start_output = $startOutput.Trim()
            Start-Sleep -Seconds 2
            $service = Get-CimInstance Win32_Service -Filter "Name='ResticPal'" -ErrorAction SilentlyContinue
            if ($service) {
                $record.service_state = $service.State
            }
            $process = Get-CimInstance Win32_Process -Filter "Name='resticpal-service.exe'" -ErrorAction SilentlyContinue |
                Select-Object -First 1 ProcessId, ExecutablePath, CommandLine
            if ($process) {
                $record.process = $process
            }
            if (Test-Path -LiteralPath $startupErrorPath -PathType Leaf) {
                $record.startup_error = Get-Content -LiteralPath $startupErrorPath -Raw
            }
        } finally {
            Remove-ProbeService
        }
        [pscustomobject]$record
    }

    @($results) |
        ConvertTo-Json -Depth 6 |
        Set-Content -LiteralPath $probeOutputPath -Encoding UTF8
}

try {
    New-Item -ItemType Directory -Path $ResultRoot -Force | Out-Null
    New-Item -ItemType Directory -Path $localTestRoot -Force | Out-Null
    Copy-Item -LiteralPath $MsiPath -Destination $localMsiPath
    if (-not [string]::IsNullOrWhiteSpace($UpgradeFromMsiPath)) {
        Copy-Item -LiteralPath $UpgradeFromMsiPath -Destination $localUpgradeFromMsiPath
    }
    Start-Transcript -LiteralPath $transcriptPath -Force | Out-Null
    $transcriptStarted = $true

    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = [Security.Principal.WindowsPrincipal]::new($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'The Windows Sandbox logon account is not running as an administrator.'
    }

    $testScript = 'C:\ResticPalSource\scripts\Test-InstalledResticPal.ps1'
    $testArguments = @{
        MsiPath = $localMsiPath
        ArtifactRoot = $localTestArtifactRoot
    }
    if (-not [string]::IsNullOrWhiteSpace($UpgradeFromMsiPath)) {
        $testArguments.UpgradeFromMsiPath = $localUpgradeFromMsiPath
    }
    & $testScript @testArguments
    $status = 'passed'
    $exitCode = 0
} catch {
    $errorMessage = $_.Exception.Message
    Write-Error -ErrorRecord $_ -ErrorAction Continue
} finally {
    try {
        Export-GuestDiagnostics
        if ($status -eq 'failed') {
            Export-ServiceStartupProbes
        }
    } catch {
        $diagnosticError = "Could not export guest diagnostics: $($_.Exception.Message)"
        $errorMessage = if ([string]::IsNullOrWhiteSpace($errorMessage)) {
            $diagnosticError
        } else {
            "$errorMessage $diagnosticError"
        }
    }

    if ($transcriptStarted) {
        Stop-Transcript | Out-Null
    }

    try {
        if (Test-Path -LiteralPath $localTestArtifactRoot) {
            Copy-Item -LiteralPath $localTestArtifactRoot -Destination $exportedTestArtifactRoot -Recurse -Force
        }
    } catch {
        $exportError = "Could not export guest test artifacts: $($_.Exception.Message)"
        $errorMessage = if ([string]::IsNullOrWhiteSpace($errorMessage)) {
            $exportError
        } else {
            "$errorMessage $exportError"
        }
        $status = 'failed'
        $exitCode = 1
    }

    $result = [ordered]@{
        status = $status
        exit_code = $exitCode
        error = $errorMessage
        started_at = $startedAt.ToString('o')
        finished_at = [DateTimeOffset]::UtcNow.ToString('o')
        computer_name = $env:COMPUTERNAME
        windows_build = [Environment]::OSVersion.Version.ToString()
        test_artifacts = $exportedTestArtifactRoot
        transcript = $transcriptPath
    }
    $json = $result | ConvertTo-Json -Depth 4
    [IO.File]::WriteAllText($temporaryResultPath, $json, [Text.UTF8Encoding]::new($false))
    Move-Item -LiteralPath $temporaryResultPath -Destination $resultPath -Force

    if (-not $KeepOpen) {
        Start-Process -FilePath "$env:SystemRoot\System32\shutdown.exe" `
            -ArgumentList '/s /t 3 /f' `
            -WindowStyle Hidden
    }
}

exit $exitCode
