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

The launcher blocks until the guest reports a result, making it suitable for agentic shell work. On Windows 11 24H2 and newer it prefers the Windows Sandbox CLI, sharing the test payload and starting the guest harness directly in the signed-in Sandbox administrator session. This avoids depending on `.wsb` logon-command startup while preserving the normal Windows Installer execution context. Older hosts fall back to launching the generated `.wsb` file. Each run gets a directory under `artifacts\windows-sandbox`. That directory contains the generated `.wsb` configuration, a copy of the tested MSI, `result.json`, a guest transcript, and the MSI lifecycle logs. Failed installer runs also export relevant Windows events and service-startup probes.

The source tree is mounted read-only. The guest can write only to its run-specific artifact directory. Networking, clipboard sharing, device input, printers, and vGPU are disabled by default. Pass `-EnableNetworking` only for a test that needs network access, or `-KeepOpen` to leave the VM open for interactive inspection after the test completes.

Useful options:

```powershell
.\scripts\Start-WindowsSandboxTest.ps1 -MsiPath C:\path\to\resticpal.msi
.\scripts\Start-WindowsSandboxTest.ps1 -MemoryInMB 4096 -TimeoutMinutes 20
.\scripts\Start-WindowsSandboxTest.ps1 -GenerateOnly
.\scripts\Start-WindowsSandboxTest.ps1 -UseLegacyLauncher
```

Closing Windows Sandbox discards its system disk. Test logs remain in the mapped host artifact directory.

Only one automated Sandbox session may be active at a time. The launcher refuses to start while another session is open. If the VM does not reach its logon command within 120 seconds, or if the overall test times out, the launcher closes the disposable Sandbox processes it started.

## Sandbox connection failures

A Sandbox window that reports a lost connection before `result.json` is written is a host/Sandbox failure, not a resticpal test result. Check the host System event log for `Microsoft-Windows-DriverFrameworks-UserMode` events. Events 10111, 10120, or 10121 naming `RdpIdd.dll` indicate that the Microsoft Remote Display Adapter's user-mode driver crashed. Restart the host and install available Windows updates before retrying.

When Sandbox remains unreliable, the guarded host harness provides the same installed-service, local-repository, restart, and uninstall coverage from an elevated shell:

```powershell
.\scripts\Test-InstalledResticPal.ps1
```

It refuses to run if a ResticPal service, install directory, or data directory already exists.
