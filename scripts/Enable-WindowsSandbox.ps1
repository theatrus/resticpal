[CmdletBinding()]
param(
    [switch] $Restart
)

$ErrorActionPreference = 'Stop'
$identity = [Security.Principal.WindowsIdentity]::GetCurrent()
$principal = [Security.Principal.WindowsPrincipal]::new($identity)
if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
    throw 'Run this one-time setup script from an elevated PowerShell session.'
}

$featureName = 'Containers-DisposableClientVM'
$feature = Get-WindowsOptionalFeature -Online -FeatureName $featureName
if ($feature.State -eq 'Enabled') {
    Write-Host 'Windows Sandbox is already enabled.'
    return
}

$computer = Get-CimInstance Win32_ComputerSystem
$processor = Get-CimInstance Win32_Processor | Select-Object -First 1
if (-not $computer.HypervisorPresent -and -not $processor.VirtualizationFirmwareEnabled) {
    throw 'Hardware virtualization is disabled. Enable Intel VT-x or AMD-V in firmware before enabling Windows Sandbox.'
}

Write-Host 'Enabling Windows Sandbox...'
$result = Enable-WindowsOptionalFeature -Online -FeatureName $featureName -All -NoRestart
if ($result.RestartNeeded) {
    if ($Restart) {
        Write-Host 'Restarting Windows to finish enabling Windows Sandbox...'
        Restart-Computer -Force
    } else {
        Write-Host 'Windows Sandbox was enabled. Restart Windows before starting a test VM.'
    }
} else {
    Write-Host 'Windows Sandbox is enabled and ready.'
}
