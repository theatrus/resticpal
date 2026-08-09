# resticpal

resticpal is an early-stage, Windows-focused companion for [restic](https://restic.net/). It will provide machine-wide file backups through a Windows service, a low-resource notification-area process, and an on-demand WinUI 3 application.

The agreed product and architecture direction is recorded in [DESIGN.md](DESIGN.md).

## Repository layout

- `crates/resticpal-core`: testable policy, scheduling, status, and restic invocation logic
- `crates/resticpal-protocol`: versioned and length-bounded local IPC messages
- `crates/resticpal-windows`: named-pipe, DPAPI, and user-profile Windows integrations
- `apps/resticpal-service`: Rust Windows service host
- `apps/resticpal-tray`: low-resource native Rust/Win32 tray host
- `apps/ResticPal.UI`: on-demand .NET 10 / WinUI 3 application
- `installer`: WiX 6 per-machine x64 MSI authoring
- `scripts`: verified restic acquisition plus local-repository and installed-service test harnesses
- `config`: example human-editable configuration

## Current implementation slice

The first slice establishes:

- typed local and managed configuration layers;
- per-field policy locking and recommended-value behavior;
- daily and resume-after-wake scheduling decisions;
- default battery and metered-network policy;
- append-only command authorization;
- shell-free restic backup invocation construction with opaque secret references;
- service control handling for stop, shutdown, resume, power, and time changes;
- an RAII Windows system-required power request;
- machine configuration loading with a friendly invalid/unconfigured state;
- authenticated, size- and time-bounded local IPC using client token impersonation and a protected DACL;
- a native tray icon whose status and run-now command come from the service;
- an on-demand WinUI application that reads service status and sends run-now requests;
- direct restic process execution in a kill-on-close Windows Job Object;
- bounded JSON progress parsing, cancellation, and sanitized outcomes;
- a system-required wake lock that automatically expires at the configured safety timeout;
- DPAPI credential encryption bound to the service identity, with protected service/admin ACLs and atomic credential-file replacement;
- deadline-driven startup and resume catch-up execution through the same service scheduler used by manual runs;
- Windows power, network-availability, and metered-network gates, with local repositories exempt from network checks;
- bounded exponential retry after failures and durable last-success state for restart-safe daily deadlines;
- elevated, typed backup-source configuration over the protected service IPC, with per-field managed-policy locks;
- atomic local TOML replacement and live scheduler updates after accepted configuration changes;
- Windows profile discovery for existing Desktop, Documents, Pictures, Videos, and Music folders;
- a WinUI backup-sources page for discovery, folder picking, removal, and exclusion editing;
- elevated repository configuration over service IPC, including arbitrary restic repository URLs, append-only mode, and bounded advanced options;
- transactional DPAPI credential provisioning and rotation with opaque, collision-resistant references and redacted status responses;
- a policy-aware WinUI repository page for local/network, S3-compatible, and advanced restic backends;
- asynchronous create/connect repository flows with a hard timeout, service-owned restic execution, and append-only initialization enforcement;
- a durable repository-validation gate tied to connection fields and credential references, preventing first use and later backups after unverified changes;
- elevated, per-field policy-aware schedule configuration with atomic persistence and immediate scheduler reevaluation;
- a WinUI Schedule page for interval, wake grace, wake-lock timeout, battery, and metered-network behavior;
- bounded SQLite run history containing only timestamps, outcomes, aggregate counts, sanitized codes, and snapshot identifiers;
- read-only bounded history IPC for interactive users and a native WinUI History page;
- a build-validated per-machine x64 MSI containing the release service, tray, self-contained WinUI application, and pinned restic 0.19.1;
- virtual-service-account registration, machine-data ACL authoring, service recovery, tray logon registration, and data-preserving uninstall behavior;
- an elevated installed-service harness that drives configuration, credential provisioning, repository initialization, append-only validation, VSS backup, restart persistence, and uninstall through the production named pipe.

Credential provisioning through installer bootstrap/enrollment, automatic discovery for newly created profiles, direct-file configuration watching, and enrollment are not wired up yet. The development MSI supplies the pinned sibling `restic.exe`, but its service-account/VSS lifecycle still requires qualification from an elevated shell and across the supported Windows matrix. Cancellation currently terminates the contained process job; graceful restic shutdown before escalation remains to be added. IPC currently uses bounded one-request/response connections; a later status-subscription channel will provide push updates.

## Build and test

The repository pins Rust 1.97 and .NET SDK 10.0.302.

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
dotnet build ResticPal.slnx --configuration Debug
```

An opt-in integration test exercises the service executor against a disposable local restic repository. The helper uses `restic.exe` from `PATH` when available; otherwise it downloads the pinned official Windows x64 test binary to a unique temporary directory and verifies its SHA-256 before use. It creates both its source data and encrypted repository under a temporary directory and removes all downloaded and generated files when it finishes:

```powershell
.\scripts\Test-LocalRestic.ps1
```

That non-elevated test verifies that the production invocation requests VSS, then removes only `--use-fs-snapshot` before starting restic. Run the exact production VSS path from an elevated developer shell or qualified service test environment:

```powershell
.\scripts\Test-LocalRestic.ps1 -UseVss
```

Pass `-ResticPath C:\path\to\restic.exe` to test a specific binary. The lifecycle covers missing-repository detection, initialization, password validation, append-only backups, snapshots/check inspection, and a second changed backup. Neither test writes a restic binary or repository into the working tree.

Build the development MSI with WiX 6. The script derives the package version from `Cargo.toml`, creates release Rust and self-contained WinUI payloads, downloads and verifies the pinned restic binary when necessary, embeds notices, and runs Windows Installer ICE validation:

```powershell
.\scripts\Build-Installer.ps1
```

Validate the resulting MSI without elevation by performing an administrative extraction and running its packaged binaries:

```powershell
.\scripts\Test-InstallerPackage.ps1
```

From an elevated PowerShell session on a machine without an existing ResticPal install or data directory, run the destructive-but-self-cleaning installed-service test:

```powershell
.\scripts\Test-InstalledResticPal.ps1
```

The harness refuses to overwrite existing ResticPal state. It installs the MSI silently, verifies the virtual service account and tray registration, configures a disposable local repository through protocol v2, performs a real VSS backup, restarts the service, verifies durable history, uninstalls, proves machine data survived uninstall, and then removes only the synthetic data it created. Pass `-KeepInstalled` to retain the test installation for manual UI/tray inspection.

Run a non-service smoke test for the service host:

```powershell
cargo run -p resticpal-service -- --console --config config/resticpal.example.toml
```

The tray binary creates a native notification-area icon and waits in its Windows message loop:

```powershell
cargo run -p resticpal-tray
```

## Repository modes

`standard` repositories may eventually run configured client-side retention and maintenance. `append_only` repositories permit backup and approved inspection operations but reject prune, forget, rewrite, migration, destructive repair, and key-removal operations. Actual append-only protection must also be enforced by the storage service, proxy, or S3 policy/immutability configuration.

## Backup history

The service keeps the newest 200 backup attempts in `state.db` next to the machine configuration. Clients may request at most 100 records at once; the WinUI page requests the newest 50. Records intentionally exclude backup paths, filenames, repository URLs, credentials, raw restic output, and exception text. Database or history-write failures are reported locally but never fail or block a backup.

## License

BSD 2-Clause. Copyright (c) 2026 Yann Ramin. See [LICENSE](LICENSE).
