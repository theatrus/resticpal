# Disposable Windows test VM

Windows Sandbox provides a clean, disposable Windows VM for the installed-service test. It is intended for local and agent-driven qualification, not CI. The VM uses the host's Windows build, so it is useful for clean-machine coverage but does not replace eventual Windows 10/11 matrix testing.

## One-time host setup

The host must run Windows Pro, Enterprise, or Education with hardware virtualization enabled. From an elevated PowerShell session, run:

```powershell
.\scripts\Enable-WindowsSandbox.ps1
```

Restart Windows if the script requests it. The setup only enables the built-in `Containers-DisposableClientVM` Windows feature; it does not download or maintain a separate Windows image.

## Run the installed-service test

Build the MSI on the host, then start the VM from a normal PowerShell session:

```powershell
.\scripts\Build-Installer.ps1
.\scripts\Start-WindowsSandboxTest.ps1
```

The launcher blocks until the guest reports a result, making it suitable for agentic shell work. On Windows 11 24H2 and newer it prefers the Windows Sandbox CLI, sharing the test payload and starting the guest harness directly in the signed-in Sandbox administrator session. This avoids depending on `.wsb` logon-command startup while preserving the normal Windows Installer execution context. Older hosts fall back to launching the generated `.wsb` file. The lifecycle verifies the current-session tray process, all-users logon registration, first-run bootstrap/local setup, all-users Start Menu launch, real local restic backups in append-only and standard modes, restart persistence, process/registration cleanup, and data-preserving uninstall. Each run gets a directory under `artifacts\windows-sandbox`. That directory contains the generated `.wsb` configuration, a copy of the tested MSI, `result.json`, a guest transcript, and the MSI lifecycle logs. Failed installer runs also export relevant Windows events and service-startup probes.

The source tree is mounted read-only. The guest can write only to its run-specific artifact directory. Networking, clipboard sharing, device input, printers, and vGPU are disabled by default. Pass `-EnableNetworking` only for a test that needs network access, or `-KeepOpen` to leave the VM open for interactive inspection after the test completes.

Useful options:

```powershell
.\scripts\Start-WindowsSandboxTest.ps1 -MsiPath C:\path\to\resticpal.msi
.\scripts\Start-WindowsSandboxTest.ps1 -MsiPath C:\path\to\new.msi -UpgradeFromMsiPath C:\path\to\old.msi
.\scripts\Start-WindowsSandboxTest.ps1 -MemoryInMB 4096 -TimeoutMinutes 20
.\scripts\Start-WindowsSandboxTest.ps1 -GenerateOnly
.\scripts\Start-WindowsSandboxTest.ps1 -UseLegacyLauncher
```

Closing Windows Sandbox discards its system disk. Test logs remain in the mapped host artifact directory.

Only one automated Sandbox session may be active at a time. The launcher refuses to start while another session is open. If the VM does not reach its logon command within 120 seconds, or if the overall test times out, the launcher closes the disposable Sandbox processes it started.

## Qualify an update before publishing its feed

The ordinary `-UpgradeFromMsiPath` lifecycle proves MSI major-upgrade behavior by invoking the new installer directly. It does not prove that an already-published NetSparkle client preserves the `.msi` extension after a redirected download. Use the dedicated update test for that release-blocking check.

First download the old installer from its published GitHub release and obtain a manually dispatched, Authenticode-signed candidate. Stage the new release before preparing either appcast name. The stage contains only the exact MSI, checksum, license, and notices; it must contain no appcast assets. This lets the deployed legacy hook populate and verify the direct package mirror while leaving the currently live feed untouched.

```powershell
New-Item -ItemType Directory -Path artifacts\published\v1.0.6 -Force | Out-Null
gh release download v1.0.6 `
    --repo theatrus/resticpal `
    --pattern resticpal-1.0.6-x64.msi `
    --dir artifacts\published\v1.0.6

gh workflow run ci.yml --ref main
# Wait for the manual run to pass, then stage only its signed package assets.
# No appcast or appcast signature is published in this phase.
.\scripts\Publish-Release.ps1 `
    -RunId <signed-run-id> `
    -ReleaseNotesPath C:\path\to\release-notes.md `
    -Stage

# Wait for updates.resticpal.com/releases/v1.0.7/resticpal-1.0.7-x64.msi
# to return the staged MSI directly with the signed artifact's exact length
# and SHA-256. Preparation repeats that check before signing the canonical
# appcast-v2.xml and creating byte-identical appcast.xml aliases.
.\scripts\Publish-Release.ps1 -RunId <signed-run-id>
```

Run the prompted update through the unmodified published client (prompted is the
launcher default):

```powershell
.\scripts\Start-WindowsSandboxUpdateTest.ps1 `
    -PublishedClientMsiPath artifacts\published\v1.0.6\resticpal-1.0.6-x64.msi `
    -ExpectedPublishedVersion 1.0.6 `
    -CandidateMsiPath artifacts\release\v1.0.7\ci-artifact\artifacts\installer\output\resticpal-1.0.7-x64.msi `
    -ExpectedCandidateVersion 1.0.7 `
    -AppCastPath artifacts\release\v1.0.7\feed\appcast-v2.xml `
    -AppCastSignaturePath artifacts\release\v1.0.7\feed\appcast-v2.xml.signature
```

Every release now requires a complementary automatic qualification with the same
official previous-client MSI and prepared candidate:

```powershell
.\scripts\Start-WindowsSandboxUpdateTest.ps1 `
    -InstallationMode Automatic `
    -PublishedClientMsiPath artifacts\published\v1.0.6\resticpal-1.0.6-x64.msi `
    -ExpectedPublishedVersion 1.0.6 `
    -CandidateMsiPath artifacts\release\v1.0.7\ci-artifact\artifacts\installer\output\resticpal-1.0.7-x64.msi `
    -ExpectedCandidateVersion 1.0.7 `
    -AppCastPath artifacts\release\v1.0.7\feed\appcast-v2.xml `
    -AppCastSignaturePath artifacts\release\v1.0.7\feed\appcast-v2.xml.signature `
    -ProbeAppCastPath artifacts\release\v1.0.7\probe\appcast-v2-probe.xml `
    -ProbeAppCastSignaturePath artifacts\release\v1.0.7\probe\appcast-v2-probe.xml.signature `
    -ProbePayloadPath artifacts\release\v1.0.7\probe\resticpal-1.0.8-x64.msi
```

The launcher queries the official GitHub API for the stable `v<old-version>` release and requires the local old MSI to match that release's exact asset name, SHA-256 digest, length, and download URL. The guest redirects only its own update hostnames to a loopback HTTPS origin trusted by an ephemeral Sandbox-only certificate, then serves the exact prepared appcast and candidate. A GitHub test enclosure, when explicitly used for compatibility testing, is redirected to an extensionless object URL without `Content-Disposition`.

The published client must validate the appcast, download and validate the MSI, and expose its prompted install flow. Before clicking Install, the harness enumerates every same-length file below the interactive user's temporary directory, identifies the actual staged file by the candidate SHA-256, records its path, and only then requires that path to end in `.msi`. If Windows Installer presents the signed package's native FilesInUse prompt, the harness verifies the exact dialog and candidate `msiexec` process, chooses automatic application close/restart, invokes OK, and records that prompted step. It then requires the old tray to exit, the LocalSystem service process ID to change, the service to return to `Running`, all three resticpal executable file versions to match the candidate, and exactly one replacement tray to remain in the interactive session.

The default installer-launch budget is seven minutes. On ordinary v2 transitions this spans the tray's five-minute retry after a transient service/pipe conflict. The one-time v1.0.6 bridge does not claim that the old tray retried; its evidence names the harness-to-published-service dispatcher explicitly.

Automatic mode does not click Check, Download, Install, a FilesInUse dialog, or any UAC prompt. It opens the official previous client's Updates page and enables `automatic_install` with the real UI toggle. On ordinary transitions, the published tray must fetch the gated signed v2 appcast and hand its exact package metadata to the LocalSystem service without another user action.

The v1.0.6→v1.0.7 transition is a one-time exception. Published v1.0.6 lacks the named-pipe busy retry, so the harness leaves the feed closed to that tray and dispatches the exact prepared enclosure through the published protocol-v3 service IPC. The service must return `accepted`, stage the exact MSI, and perform the same silent LocalSystem install. After upgrade, the candidate protocol-v4 tray must fetch the production-signed probe appcast and its sentinel payload using `resticpal/1.0.7`, then reject the deliberately invalid package signature. The harness requires `update.started` followed by `update.failed/update_signature_invalid`, an empty enumeration of every matching probe staging entry (including final and partial names), zero probe `msiexec` starts, and unchanged upgraded tray/service process IDs. No future transition may use this bridge.

The service-owned path must be
`C:\ProgramData\ResticPal\Updates\resticpal-<version>-x64.msi`. The harness hashes
that actual file, requires the `.msi` extension, and binds the resulting Windows
Installer transaction to the same path. It also requires the client `msiexec`
process to run in session 0 as `NT AUTHORITY\SYSTEM`, be a direct child of the
published service process, and include both `/qn` and `/norestart`. Any visible
Download or Install confirmation, candidate FilesInUse dialog, UAC consent
process, transient candidate UI process start, or harness dialog intervention
fails the automatic qualification. A
pass additionally proves that the old UI, tray, and service processes exit; all
three installed executable versions change to the candidate; the service returns
as LocalSystem; the automatic setting survives the upgrade; and exactly one
replacement tray remains in the interactive session.

Results are written under `artifacts\windows-sandbox-update`. Keep both resulting `result.json` files, their `guest.log` files, `test-artifacts\staged-update.json`, origin request logs, and MSI logs with the release evidence. Both the prompted and automatic v1.0.6→v1.0.7 results are release blockers. Finalization binds their hashes plus normalized evidence into the prepared schema-5 manifest before uploading either signed name pair:

```powershell
.\scripts\Publish-Release.ps1 `
    -RunId <signed-run-id> `
    -ReleaseNotesPath C:\path\to\release-notes.md `
    -UpdateQualificationPath artifacts\windows-sandbox-update\<prompted-run-id>\result.json `
    -AutomaticUpdateQualificationPath artifacts\windows-sandbox-update\<automatic-run-id>\result.json `
    -Finalize
```

The Sandbox test does not mutate either public appcast. Until qualification-bound finalization succeeds, the staged GitHub release has no appcast assets and the old live legacy feed remains unchanged. Finalization uploads the final checksum and canonical signed `appcast-v2.xml` pair first, then its byte-identical legacy signature and `appcast.xml` last. That last upload exposes the qualified update through the legacy GitHub fallback; the following release edit makes the deployed hook advance the legacy live feed. The hook does not mirror the v2 name, so new clients temporarily receive the same signed v2 bytes from their GitHub fallback after the primary returns 404.

## Sandbox connection failures

A Sandbox window that reports a lost connection before `result.json` is written is a host/Sandbox failure, not a resticpal test result. Check the host System event log for `Microsoft-Windows-DriverFrameworks-UserMode` events. Events 10111, 10120, or 10121 naming `RdpIdd.dll` indicate that the Microsoft Remote Display Adapter's user-mode driver crashed. Restart the host and install available Windows updates before retrying.

When Sandbox remains unreliable, the guarded host harness provides the same installed-service, local-repository, restart, and uninstall coverage from an elevated shell:

```powershell
.\scripts\Test-InstalledResticPal.ps1
```

It refuses to run if a ResticPal service, install directory, or data directory already exists.
