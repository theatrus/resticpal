# Runs the client<->server enrollment flow end to end against a real
# resticpal-server: build the server, mint a one-time bootstrap URL, start the
# server on loopback, then drive the service's real ManagementClient through
# enrollment, an authenticated manifest fetch, a status report, and a replay
# rejection (the ignored test real_server_enrollment_manifest_and_status_lifecycle).
#
# Requires a checkout of https://github.com/theatrus/resticpal-server. Locally:
#   ./scripts/Test-ServerEnrollment.ps1 -ServerRepoPath ..\resticpal-server
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string] $ServerRepoPath,
    [int] $Port = 8787,

    # Keep schema v1 as the cross-repository CI default while independently
    # deployed servers catch up; use v3 to qualify managed restore explicitly.
    [ValidateRange(1, 3)]
    [int] $ManagedPolicySchemaVersion = 1
)

$ErrorActionPreference = 'Stop'

$serverRepo = (Resolve-Path -LiteralPath $ServerRepoPath).Path
$clientRepo = Split-Path -Parent $PSScriptRoot

function New-RandomToken {
    $bytes = [byte[]]::new(32)
    $generator = [Security.Cryptography.RandomNumberGenerator]::Create()
    try {
        $generator.GetBytes($bytes)
    } finally {
        $generator.Dispose()
    }
    return ([Convert]::ToBase64String($bytes)).TrimEnd('=').Replace('+', '-').Replace('/', '_')
}

# Feeds stdin from a BOM-less file rather than a PowerShell pipe: Windows
# PowerShell pipes prepend a UTF-8 BOM to native stdin, which would silently
# become part of the token, round-trip into the bootstrap URL, and break the
# client's Authorization header.
function Invoke-ServerCli {
    param(
        [string] $Executable,
        [string[]] $Arguments,
        [string] $StdinLine,
        [string] $Label
    )
    $stdinPath = [IO.Path]::GetTempFileName()
    $stdoutPath = [IO.Path]::GetTempFileName()
    $stderrPath = [IO.Path]::GetTempFileName()
    try {
        [IO.File]::WriteAllText($stdinPath, $StdinLine + "`n")
        $process = Start-Process -FilePath $Executable -ArgumentList $Arguments `
            -RedirectStandardInput $stdinPath -RedirectStandardOutput $stdoutPath `
            -RedirectStandardError $stderrPath -PassThru -Wait -NoNewWindow
        if ($process.ExitCode -ne 0) {
            Get-Content -LiteralPath $stderrPath | Write-Host
            throw "resticpal-server $Label failed with exit code $($process.ExitCode)"
        }
        $line = Get-Content -LiteralPath $stdoutPath | Where-Object { $_ } | Select-Object -Last 1
        if (-not $line) { throw "resticpal-server $Label produced no output" }
        return $line.Trim()
    } finally {
        Remove-Item -LiteralPath $stdinPath, $stdoutPath, $stderrPath -Force -ErrorAction SilentlyContinue
    }
}

Write-Host "Building resticpal-server from $serverRepo..."
Push-Location $serverRepo
try {
    cargo build --locked
    if ($LASTEXITCODE -ne 0) { throw 'resticpal-server build failed' }
} finally {
    Pop-Location
}
$serverExe = Join-Path $serverRepo 'target\debug\resticpal-server.exe'
if (-not (Test-Path -LiteralPath $serverExe -PathType Leaf)) {
    throw "The server build did not produce $serverExe"
}

# Fresh key material and tokens for this run only; nothing here outlives the
# temporary stage directory.
$keygen = & $serverExe keygen
if ($LASTEXITCODE -ne 0) { throw 'resticpal-server keygen failed' }
$privateKey = @($keygen) | Where-Object { $_ -like 'private=*' } | ForEach-Object { $_.Substring('private='.Length) }
if (-not $privateKey) { throw 'keygen did not print a private key' }

$bootstrapToken = New-RandomToken
$env:RESTICPAL_SERVER_SIGNING_KEY = $privateKey
$env:RESTICPAL_E2E_RESTIC_PASSWORD = 'e2e-repository-password'
$bootstrapHash = Invoke-ServerCli -Executable $serverExe -Arguments @('hash-token') -StdinLine $bootstrapToken -Label 'hash-token'
$adminHash = Invoke-ServerCli -Executable $serverExe -Arguments @('hash-token') -StdinLine (New-RandomToken) -Label 'hash-token'

$stage = Join-Path ([IO.Path]::GetTempPath()) ("resticpal-e2e-server-{0}" -f [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stage | Out-Null
$configPath = Join-Path $stage 'server.toml'
$serverLog = Join-Path $stage 'server.log'
$serverErrLog = Join-Path $stage 'server.err.log'
$server = $null
$failed = $false

try {
    # Keep legacy transport coverage available while allowing the complete
    # signed schema-v3 restore grant to cross the real server/client boundary.
    $policyDocument = [ordered]@{
        schema_version = $ManagedPolicySchemaVersion
        revision = "e2e-ci-schema-$ManagedPolicySchemaVersion"
    }
    if ($ManagedPolicySchemaVersion -ge 2) {
        $policyDocument.updates = @{
            automatic_install = @{ value = $false; locked = $true }
        }
    }
    if ($ManagedPolicySchemaVersion -ge 3) {
        $policyDocument.restore = @{
            enabled = @{ value = $true; locked = $true }
        }
    }
    Set-Content `
        -LiteralPath (Join-Path $stage 'policy.json') `
        -Encoding ascii `
        -Value ($policyDocument | ConvertTo-Json -Depth 6 -Compress)
    Set-Content -LiteralPath $configPath -Encoding ascii -Value @"
listen = "127.0.0.1:$Port"
public_base_url = "http://127.0.0.1:$Port/"
database_path = "data/e2e.db"
signing_key_env = "RESTICPAL_SERVER_SIGNING_KEY"
admin_token_sha256 = "$adminHash"

[[devices]]
id = "e2e-device"
policy_path = "policy.json"
sequence = 1

[[enrollments]]
id = "e2e-device-setup"
device_id = "e2e-device"
token_sha256 = "$bootstrapHash"

[enrollments.secret_env]
RESTIC_PASSWORD = "RESTICPAL_E2E_RESTIC_PASSWORD"
"@

    Write-Host "Starting resticpal-server on 127.0.0.1:$Port..."
    $server = Start-Process -FilePath $serverExe -ArgumentList @('serve', $configPath) `
        -RedirectStandardOutput $serverLog -RedirectStandardError $serverErrLog `
        -PassThru -WindowStyle Hidden
    $ready = $false
    for ($attempt = 0; $attempt -lt 60; $attempt++) {
        if ($server.HasExited) { break }
        try {
            $health = Invoke-RestMethod -Uri "http://127.0.0.1:$Port/healthz" -TimeoutSec 2
            if ($health.status -eq 'ok') { $ready = $true; break }
        } catch {
            Start-Sleep -Milliseconds 500
        }
    }
    if (-not $ready) { throw 'resticpal-server did not answer /healthz in time' }

    $bootstrapUrl = Invoke-ServerCli -Executable $serverExe `
        -Arguments @('bootstrap-url', $configPath, 'e2e-device-setup') `
        -StdinLine $bootstrapToken -Label 'bootstrap-url'

    Write-Host "Running the client enrollment end-to-end test with managed-policy schema v$ManagedPolicySchemaVersion..."
    $env:RESTICPAL_TEST_BOOTSTRAP_URL = $bootstrapUrl
    $env:RESTICPAL_TEST_POLICY_SCHEMA_VERSION = [string] $ManagedPolicySchemaVersion
    Push-Location $clientRepo
    try {
        cargo test -p resticpal-service --locked -- --ignored --exact `
            management::tests::real_server_enrollment_manifest_and_status_lifecycle
        if ($LASTEXITCODE -ne 0) { throw 'the client enrollment end-to-end test failed' }
    } finally {
        Pop-Location
    }
    Write-Host (
        "OK: schema-v$ManagedPolicySchemaVersion enrollment, manifest fetch, " +
        'managed permission verification, status report, and replay rejection all passed.'
    )
} catch {
    $failed = $true
    throw
} finally {
    if ($server -and -not $server.HasExited) {
        Stop-Process -Id $server.Id -Force
        $server.WaitForExit()
    }
    if ($failed) {
        foreach ($log in @($serverLog, $serverErrLog)) {
            if (Test-Path -LiteralPath $log) {
                Write-Host "--- $log"
                Get-Content -LiteralPath $log | Write-Host
            }
        }
    }
    foreach ($name in @(
        'RESTICPAL_TEST_BOOTSTRAP_URL',
        'RESTICPAL_TEST_POLICY_SCHEMA_VERSION',
        'RESTICPAL_SERVER_SIGNING_KEY',
        'RESTICPAL_E2E_RESTIC_PASSWORD'
    )) {
        Remove-Item -LiteralPath "Env:\$name" -ErrorAction SilentlyContinue
    }
    if (Test-Path -LiteralPath $stage) {
        Remove-Item -LiteralPath $stage -Recurse -Force
    }
}
