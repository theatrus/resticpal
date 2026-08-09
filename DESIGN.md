# resticpal product and architecture design

Status: living design and implementation record, updated 2026-08-08

This document records the product requirements, architecture decisions, and current implementation boundary for **resticpal**, a friendly, Windows-focused wrapper around [restic](https://restic.net/). It is the source of truth for the first implementation. Items explicitly marked "open", "proposed", "planned", or "not yet implemented" still require work or validation.

## Implementation status

The repository now contains a buildable x64 Windows vertical slice using Rust 1.97 and .NET 10. The automated baseline is 109 Rust tests plus a warning-free WinUI build. It is a development build, not an installable or production-qualified release.

| Area | Implemented | Remaining |
| --- | --- | --- |
| Core model | Typed local/managed configuration layers, per-field resolution and locks, validation bounds, deadline scheduling, and append-only command authorization | Signed managed-policy ingestion, policy freshness/replay handling, and standard-mode retention execution |
| Windows service | SCM control handling, startup/resume catch-up, power/network gates, retry backoff, restic process containment, cancellation, timed wake lock, DPAPI repository credentials, recoverable atomic UI-driven configuration, repository create/validate, scheduler checkpoint, bounded SQLite history, and bounded shutdown outcome draining | Installer-created service identity/ACL validation, direct-file watching, structured logs/audit, graceful cancellation before escalation, and production VSS testing |
| Local IPC | Protocol v2, 1 MiB bounded frames, bounded per-connection I/O, protected named pipe, client-token authorization, ordinary-user status/history/run/cancel/defer, and elevated configuration operations | Long-lived status/progress subscriptions and compatibility/evolution policy beyond v2 |
| Tray | Native Win32 notification icon, current status tooltip, run/cancel action, and elevated UI launch | Push-driven live icon updates, deferral UI, notifications, richer health icons, and startup registration |
| WinUI application | Overview, backup sources, repository, schedule/power/network, and bounded backup-history pages | Diagnostics/logs, enrollment/managed-policy, updates/settings, accessibility and Windows 10 qualification |
| Remote management | Typed managed-policy data model and lock-aware service/UI paths | Enrollment, device keys, signed policy cache, credential bootstrap, metadata/API schemas, and status delivery |
| Distribution | x64 project targets and a NetSparkleUpdater package reference | Bundled pinned `restic.exe`, WiX MSI, service/tray registration, update UI/appcast verification, elevated updater, repair, and uninstall |

Current durable state consists of atomic `config.toml`, DPAPI-protected credential files, `state.json` for scheduler/repository-verification state, and lazy `state.db` backup history. The history retains the newest 200 attempts and exposes at most 100 per IPC request; the WinUI page requests 50.

## Product summary

resticpal provides reliable, low-maintenance, file-level backups of user data on Windows 10 and Windows 11. A system service performs backups even when no user is signed in. A lightweight tray process reports protection status, and an on-demand modern Windows UI provides setup and administration.

The first release is Windows x64. A macOS implementation and a restore UI may come later.

## Goals

- Bundle and safely invoke a pinned version of `restic.exe`.
- Back up selected user and system paths from a machine-wide configuration.
- Run backups from a Windows service without requiring an interactive login.
- Handle laptops correctly: catch up after resume, wait through a short grace period, and prevent sleep while a backup is active for up to two hours.
- Keep idle CPU, memory, disk, and network use low.
- Present clear backup state, history, errors, and progress in the notification area and an on-demand settings application.
- Support every backend restic supports, including an S3 or S3-compatible bucket supplied by managed configuration.
- Support repositories where the backup client has append-only access and a separate server performs retention and pruning.
- Allow local administrator configuration and optional enrollment into a remotely managed policy.
- Secure repository and enrollment credentials with Windows facilities.
- Support user-initiated updates with NetSparkleUpdater and cryptographically signed update metadata and packages.

## Non-goals for the first release

- A restore browser or restore workflow.
- Bare-metal recovery, Windows installation imaging, or guaranteed application-consistent database backup.
- A management server implementation. The repository will define the client protocol, schemas, and fixtures needed to integrate with one.
- macOS support.
- ARM64 packages.
- Fully unattended updates while releases lack an Authenticode code-signing certificate.

## Process architecture

The steady-state design has two small persistent processes and launches richer UI only when requested.

```text
Windows Service Control Manager
        |
        +-- resticpal-service.exe       Rust; machine-wide, privileged
                |
                +-- restic.exe          bundled; only while a command runs
                |
                +-- authenticated named-pipe IPC
                          |
User session              +-- resticpal-tray.exe     Rust/Win32; always present
                                      |
                                      +-- resticpal-ui.exe
                                          C# / WinUI 3; on demand

On demand: resticpal-updater.exe       elevated update/bootstrap helper
```

The service, tray, and WinUI projects in this diagram are implemented. The updater is still a planned process, and packaging does not yet install or register any of the components.

### `resticpal-service.exe`

The Rust Windows service is the sole owner of:

- effective configuration and policy enforcement;
- credentials and device identity;
- scheduling, wake/resume, network, and power decisions;
- source discovery and path validation;
- restic command construction and execution;
- backup cancellation and timeout handling;
- local history and logs;
- remote enrollment, policy refresh, and status delivery;
- update coordination with the UI/updater.

No tray or UI process may invoke restic directly.

The current service implements configuration/policy resolution, scheduling, system-condition gates, restic execution, repository setup/validation, DPAPI credentials, scheduler state, and backup history. Remote enrollment/reporting, structured logs, and update coordination remain planned. The service expects a fixed sibling `restic.exe`, but the repository does not yet package that binary.

### `resticpal-tray.exe`

The persistent tray process uses native Win32 notification-area APIs from Rust. The current message-loop implementation fetches status at startup and when its menu or an action needs it; it has no background polling timer. It displays a native menu, opens the WinUI process through UAC, and requests run-now or cancellation. Push updates, notifications, deferral, and richer state-specific icons await the IPC subscription channel.

The installed design runs the tray in each interactive user session. It may view machine backup status and request a one-run backup, deferral, or cancellation. Permanent configuration changes require elevation and service-side authorization. Logon registration is not implemented yet.

### `resticpal-ui.exe`

The C# WinUI 3 application provides the modern settings and status experience. It is not kept alive merely to own a tray icon. Implemented pages are:

- Overview
- Backup sources and exclusions
- Repository
- Schedule, power, and network policy
- Backup history

Planned pages are:

- Diagnostics and local logs
- Enrollment and managed-policy state
- Updates and application settings

The current unpackaged x64 application uses Windows App SDK 2.3.1, targets .NET 10, and requires administrator elevation for the whole process because its primary job is machine configuration. A later pass may split read-only status/history from elevated mutations if avoiding the prompt materially improves daily use.

### Resource policy

- Persistent components must be event-driven.
- Progress events are rate-limited before crossing IPC.
- Remote heartbeats are infrequent and backed off on failure.
- Repository-wide statistics and integrity scans must never run merely to render the tray UI.
- Concrete memory and CPU budgets are open and will be set after the first vertical prototype is measured on Windows 10 and Windows 11.

## Windows service identity and access

The preferred identity is a dedicated virtual service account, `NT SERVICE\ResticPal`, with a service SID, the minimum required Windows privileges, and narrowly ACLed application state. An early compatibility spike must confirm that this identity can:

- read configured user data without weakening user-file ACLs;
- enable the filesystem/backup privileges restic expects;
- create and use VSS snapshots;
- access local, network, and supported remote repositories;
- start the bundled restic child process with the intended restricted environment.

If reliable file access or VSS cannot be achieved with the virtual account, the installer may offer or use LocalSystem as a documented fallback. LocalSystem is highly privileged, so the service command surface, binaries, configuration, named pipes, and update path must be tightly ACLed and validated.

The service host and service-control handling exist, but the repository has no installer and has not yet validated the virtual account against real user ACLs, VSS, network shares, or cloud credentials on the supported Windows matrix. DPAPI data is intentionally bound to whichever identity runs the service, so changing that identity requires an explicit credential migration or reprovisioning design.

## IPC and authorization

Service-to-client communication uses a versioned protocol over Windows named pipes. The implemented protocol is v2 over `\\.\pipe\ResticPal.v2` with one bounded request and response per connection.

- The pipe security descriptor permits interactive users to read status.
- The service inspects the connecting process token rather than trusting identity fields supplied in a message.
- Administrative mutations require an elevated administrator token.
- Ordinary users may request `run now`, defer one run, or cancel one run unless the applicable action is locked by managed policy.
- The protocol exposes typed operations, not arbitrary executable paths, environment variables, or restic arguments.
- Progress is currently returned in status snapshots. A future subscription channel will push rate-limited status/progress changes without continuous polling.
- Messages have explicit protocol versions, reject unknown fields within a version, and have bounded sizes and per-connection I/O time.

The pipe DACL, connecting-token impersonation, elevated-administrator checks, frame bounds, request IDs, and exact protocol-version checks are implemented and covered by Windows tests. Configuration reads are currently administrator-only because they expose machine configuration such as source paths and repository URLs; ordinary users receive only the redacted canonical status and sanitized history.

## Backup scope

The primary use case is file-level backup of user data. This is not a bare-metal Windows recovery product.

### Default source discovery

The service currently discovers existing local user profiles through the Windows profile list rather than assuming `C:\Users`, then offers existing Desktop, Documents, Pictures, Videos, and Music directories. Automatic discovery when a new profile is created is planned; today an administrator reruns discovery from the UI.

The implemented discovery set includes:

- Desktop
- Documents
- Pictures
- Videos
- Music

Downloads, application/browser data, and cloud-placeholder handling are not automatically selected. All configured paths can be changed locally or eventually by managed policy. The implemented UI supports discovery, folder picking, adding/removing arbitrary absolute paths, and editing exclusions.

### VSS and metadata

The backup invocation enables restic's Windows filesystem snapshot support by default (`--use-fs-snapshot`) so locked files can be read through VSS. Restic's supported Windows metadata, including security descriptors and NTFS metadata, should be enabled/preserved where available. Real service-account VSS behavior and metadata restoration still require Windows 10/11 integration testing. VSS improves consistency but does not turn the product into an application-aware or bare-metal backup system.

## Scheduling, wake, power, and network

The scheduler is deadline-based rather than a single fixed wall-clock task.

- Default cadence: once per day.
- If a deadline is missed because the machine is asleep or off, the backup becomes overdue.
- On resume, an overdue backup waits through a default five-minute grace period before starting.
- Manual, schedule, startup, resume, power, time-change, and periodic condition reevaluations coalesce into a single pending run. A dedicated network-change notification is not wired yet; blocked network conditions are reevaluated every minute.
- Only one repository-mutating restic operation may run at a time.
- Battery operation is allowed by default and configurable per field.
- Metered-network operation is allowed by default and configurable per field.
- A run waits while its required repository is unreachable and retries with bounded exponential backoff.
- A successful manual run satisfies the same backup deadline unless policy says otherwise.

While backup work is active, the service obtains a Windows system-required power request. The power request is always released when the run ends and has a hard two-hour safety limit. If a backup exceeds two hours, the power request expires but the backup itself may continue; backup runtime limits are a separate policy.

Cancellation currently terminates the contained Windows Job Object and is recorded distinctly from a backup failure. Graceful restic shutdown followed by bounded escalation remains to be implemented.

## Repository support

The service supports creating a new repository and connecting to an existing repository through service-owned, fixed restic operations. Bundling `restic.exe` is a packaging task that is not complete; development builds look for a sibling `restic.exe`.

### Backends

The implemented UI offers friendly local/network and S3-compatible choices plus an advanced restic repository URL and bounded key/value options for the remaining backends. The configuration and credential models represent:

- local and network paths;
- REST server URLs;
- SFTP;
- S3 and S3-compatible endpoints, bucket/prefix, region, bucket lookup style, and supported backend options;
- Azure, Google Cloud Storage, Backblaze B2, and other native restic backends;
- rclone-backed repositories.

The first use of a configured repository, and any later change that affects connectivity, requires durable verification. Backups remain blocked until the service successfully runs the allowlisted connection test or repository initialization. Initialization has a hard timeout and is prohibited in append-only mode.

Repository URLs and non-secret options may live in policy. Passwords, access keys, secret keys, tokens, and private key material live only in the protected credential store and are referenced by opaque secret IDs.

The server may supply an S3 bucket configuration and encrypted credentials during enrollment or a later policy refresh. Managed settings remain typed and allowlisted. The server cannot send arbitrary restic command-line arguments.

### Bundled restic

The command builder, executor boundary, sibling-binary lookup, bounded JSON parsing, and secret injection are implemented. Release pinning, binary verification, version capture, and distribution are not.

- Ship a pinned, verified `restic.exe` with the application.
- Disable or never expose `restic self-update`; restic updates arrive as part of a resticpal release.
- Capture the restic version in every run record and remote health report.
- Prefer restic's JSON output where available and treat console text as untrusted diagnostic data.
- Pass secrets through protected files, inherited environment, or standard input as supported; never place secrets in process command lines or logs.
- Run with a minimal explicit environment.

## Repository maintenance modes

Every repository has an explicit maintenance mode. It is part of effective policy and can be locked by the management server.

### Standard/client-maintained mode

In standard mode, resticpal may apply local retention and maintenance policy. The agreed default retention is:

- 7 daily snapshots
- 5 weekly snapshots
- 12 monthly snapshots
- 3 yearly snapshots

Retention will be configurable in the file and UI. The precise default prune cadence is still open; pruning should be scheduled separately from ordinary backup deadlines because it may be expensive.

The retention values and policy-resolution model exist, but retention editing, `forget`, `prune`, and maintenance scheduling are not implemented. Consequently the current standard mode performs backups only.

### Append-only/server-maintained mode

In append-only mode, the backup client is intentionally not a repository administrator.

- resticpal may create backups and perform explicitly approved read-only inspection.
- resticpal must never invoke local retention or destructive/rewriting maintenance, including `forget`, `prune`, `rewrite`, repository migration, destructive repair, key removal, or equivalent future commands.
- Repository initialization is disabled unless a separate full-access provisioning flow is explicitly configured.
- The retention UI displays **Managed by server** and does not offer a local prune action.
- Any retention values received by the client are informational unless the repository is later switched to standard mode with appropriate credentials.
- Remote status reports include the configured repository maintenance mode.
- The server or another isolated maintenance host uses separate full-access credentials to perform pruning and other administration.

The client-side command restriction is defense in depth, not proof of append-only storage. Real protection must also be enforced by the repository service, proxy, S3 IAM/bucket policy, object immutability/versioning controls, or another storage-side mechanism. resticpal must label the mode as configured; it must not claim to have verified backend immutability unless a future backend-specific verification exists.

The append-only authorization matrix is implemented in the core command builder and tested against initialization, forget, prune, rewrite, migration, destructive repair, and key removal. The repository UI labels append-only retention as server-managed, and the service rejects initialization in this mode.

When a server maintains a repository that receives append-only client backups, its retention design should follow restic's append-only guidance, including careful use of time-window retention to avoid an untrusted client manipulating which snapshots appear newest.

## Configuration model

### Files and state

Machine state lives under `%ProgramData%\ResticPal` with service/admin-only ACLs where appropriate.

| Item | Location | Current status |
| --- | --- | --- |
| Local configuration | `config.toml` | Implemented; typed TOML with no secrets, loaded at startup and replaced atomically for accepted UI/service changes |
| Managed policy cache | `managed-policy.json` | Planned; signed, versioned, last-known-good |
| Credential store | implementation-private `Credentials\` | Implemented for repository secrets with service-identity DPAPI, opaque references, restricted ACLs, and atomic replacement |
| Scheduler checkpoint | `state.json` | Implemented; last success plus repository-validation identity/time for restart-safe scheduling |
| Run history | `state.db` | Implemented lazily; SQLite, newest 200 attempts, sanitized and non-blocking |
| Logs | `Logs\` | Planned; structured, rotated, sanitized |
| Bundled tools | under installation directory | Planned; administrator/service write only |

The UI writes configuration through elevated typed service operations, not directly. Those writes validate a candidate effective configuration before atomically replacing `config.toml` and immediately reevaluating the scheduler. A directly edited file is validated at service startup, but live file watching and last-known-good recovery for invalid edits are not implemented yet.

### Effective policy and per-field locks

Effective configuration is calculated from:

1. product defaults;
2. valid local administrator configuration;
3. the last valid signed managed policy;
4. transient one-run choices such as an allowed deferral.

Managed policy marks individual fields as locked. A managed value overrides local configuration only for that field. The complete UI will explain the source of each managed value and disable editing only where locked. Unknown fields are rejected or safely ignored according to schema-version rules; they never become raw restic arguments.

Loss of server connectivity does not stop backups. The service continues with its last valid policy and reports that management connectivity is stale. Only a signed explicit disable policy may stop managed backups.

The typed precedence/lock resolver and lock-aware service/UI mutation paths are implemented and tested. The running service currently resolves product defaults plus local configuration with no managed document; signed policy loading, freshness, caching, and runtime revision changes belong to the enrollment milestone.

## Credentials and device keys

Repository credential storage is implemented with DPAPI under the service identity. Credential values cross only the protected administrator IPC request, are written under opaque collision-resistant references, are zeroized where practical, and never enter TOML or response views. Rotation commits the new configuration before retiring superseded credential files. Device enrollment keys and bootstrapped-secret decryption are not implemented.

The target credential/device-key model is:

- Repository secrets are encrypted at rest using Windows DPAPI or a non-exportable Windows CNG key, scoped so only the service identity can use them.
- Protected files and registry entries also have service/admin-only ACLs; encryption does not replace access control.
- The service generates its device key locally during enrollment and sends only the public portion to the server.
- Bootstrapped secrets are encrypted for the enrolled device in addition to transport security.
- Secrets are redacted from logs, status, crash information, command lines, configuration files, and UI diagnostics.
- A local administrator may unenroll with an elevation prompt. Unenrollment removes management credentials and device enrollment state, records a local audit event, and does not delete repositories or existing backups.

## Enrollment and remote policy

This section remains a proposed protocol. No bootstrap URL consumption, device identity, signed policy cache, policy/status HTTP client, metadata-file adapter, schemas, or fixtures are implemented yet.

The installer offers an optional bootstrap URL field. Interactive entry is preferred because one-time URLs and tokens can leak through process listings, shell history, response logs, or MSI logs when passed on a command line.

### Trust model

- Bootstrap uses HTTPS and a time-limited, one-time signed URL.
- The bootstrap descriptor pins the server's Ed25519 policy-signing public key or fingerprint.
- Policy documents are independently signed and verified even though they are transported over TLS.
- The service rejects expired, replayed, incorrectly signed, downgraded, or schema-incompatible policy documents.
- The last valid policy is retained atomically.
- Client status requests are authenticated with the enrolled device identity.

### Proposed enrollment sequence

1. The installer stores the bootstrap URL in a service-only staging location.
2. On first start, the service consumes the URL and fetches the bootstrap descriptor.
3. The service generates a device key pair.
4. The service enrolls with its public key, hostname, app/restic version, OS build, architecture, and a nonce.
5. The server returns a device ID, policy/status endpoints, a signed initial policy, and any secrets encrypted for the device.
6. The service verifies and commits enrollment, then erases the one-time bootstrap material.
7. Future policy fetches use conditional requests such as ETag/version and retain the last-known-good policy when offline.

The protocol should permit a metadata-file-only integration as well as a conventional API. This repository will provide versioned JSON Schemas, an OpenAPI description for optional endpoints, signature test vectors, and example fixtures; it will not implement the server.

## Backup state and local status

The canonical state machine includes:

- `unconfigured`
- `idle` / protected
- `waiting`, with a reason such as wake grace, network, policy backoff, or power policy
- `running`, with phases such as preparing VSS, scanning, uploading, finalizing, retention, and checking
- `succeeded`
- `succeeded_with_warnings`
- `failed`
- `cancelled`
- `paused` or administratively disabled
- `service_unavailable` in clients that cannot reach the service

The implemented canonical service state includes all entries above except client-only `service_unavailable`. Waiting reasons currently cover wake grace, network, battery, metered network, policy backoff, and repository validation. Running status begins in preparation and moves to uploading when JSON progress arrives; finer scanning/finalizing/retention/checking phase reporting is still planned.

The complete tray and UI status surface will expose:

- current state, phase, start time, and bounded progress;
- last attempt and last successful backup;
- next deadline and any current blocker;
- files and bytes processed/uploaded when restic reports them;
- duration, outcome, warnings, and snapshot ID;
- configured repository display name and maintenance mode;
- recent bounded history;
- sanitized errors and a route to detailed local logs;
- resticpal and bundled-restic versions;
- enrollment, policy revision, and update state.

Tray notifications should be useful rather than noisy: notify on an initial failure, repeated/stale protection, user action required, and recovery after a failure streak. Exact notification thresholds are proposed behavior and should be user configurable.

The current local status protocol exposes redacted state, repository name/mode, last attempt/success, deadline/blocker, and bounded progress. The overview and tray reduce that to a friendly health summary and current progress. The WinUI History page exposes timestamps, duration, outcome, aggregate file/byte statistics, sanitized error code, and snapshot ID for the newest 50 records. Version, enrollment, update, detailed diagnostic, and notification surfaces remain planned.

## Remote status reporting

Remote status reporting is not implemented. The following remains the agreed reporting contract for the enrollment milestone.

Remote reporting occurs only when enrolled. The default cadence is:

- immediately for important state transitions;
- every five minutes while a backup is running;
- every six hours while idle;
- exponential backoff with jitter after delivery failures.

Report delivery never blocks or fails a backup.

Reports may contain:

- stable random device ID;
- hostname/display name;
- app, bundled-restic, OS, and architecture versions;
- current policy revision and effective-configuration health;
- configured repository identifier and maintenance mode, but no credentials;
- current state, phase, timestamps, progress summary, and blockers;
- last attempt and success timestamps;
- result, duration, aggregate file/byte statistics, warning/error code, and snapshot ID;
- last successful management contact;
- update availability/version.

Reports must not contain repository passwords, cloud credentials, enrollment tokens, source paths, filenames, raw restic output, or full exception/log dumps. A future detailed diagnostic upload must be explicit, previewable, and opt-in.

## Updates

The WinUI project references NetSparkleUpdater 3.1.0, but no appcast, update-check UI, verification flow, or updater helper is wired yet. The intended behavior, until releases have Authenticode code signing, is:

- updates are user-selected and user-initiated;
- update metadata and packages require Ed25519 verification;
- the UI warns that Windows may display an unsigned-publisher or SmartScreen prompt;
- applying an update requires UAC elevation;
- an update never replaces binaries during an active backup; it waits or asks the user to cancel/defer;
- the elevated updater stops the service, replaces the application and bundled restic atomically, restarts the service, and reports the outcome;
- rollback or repair behavior must be designed before automatic installation is considered.

## Installer and deployment

Installer and deployment work has not started. The following is the target packaging behavior:

The development WinUI project currently declares `10.0.17763.0` (Windows 10 version 1809) as its target-platform minimum. That is a build setting, not yet a qualified support promise.

- Per-machine x64 WiX MSI.
- Target Windows 10 and Windows 11; the exact minimum Windows 10 build will be validated with the first WinUI/installer prototype.
- Installs and configures the Windows service and service SID/account.
- Applies restrictive ACLs to binaries and `%ProgramData%\ResticPal`.
- Registers the lightweight tray process for interactive user logon.
- Installs the on-demand WinUI application, updater, and pinned restic binary.
- Offers initial local setup or optional enrollment by bootstrap URL.
- Supports repair/uninstall without deleting repositories. Removal of local configuration and credentials must be an explicit choice.

## Logging and audit

- Bounded run history is implemented in SQLite, retaining the newest 200 attempts and returning at most 100 records per local IPC request.
- Run records persist only timestamps, outcomes, aggregate counts, allowlisted error/snapshot identifiers, and no source paths, filenames, repository URLs, raw output, credentials, or exception text. Identifiers are allowlisted again when read so a corrupted database cannot surface path-like data through IPC.
- History I/O failure is logged to the service diagnostic stream and never blocks or fails a backup.
- Structured rotating logs, stable event IDs, correlation/run IDs, Windows Event Log integration, and audit events are planned.

## Security boundaries

The service-only execution boundary, shell-free command construction, typed option validation, bounded IPC/progress parsing, DPAPI credential references, and append-only operation checks are implemented. Filesystem ACL protection for installed binaries/configuration and update/policy signature boundaries still depend on the installer, enrollment, and updater milestones.

- Only the service launches restic.
- Installed executable paths will be fixed and ACL-protected; the current executor already fixes restic to the sibling application path.
- Remote and local configuration are mapped to typed allowlisted arguments/options.
- No shell command construction is used.
- All paths, URLs, options, environment names, sizes, and message lengths are validated.
- Managed repository configuration is powerful: an enrolled server can direct readable files to a repository it controls. Enrollment therefore represents an explicit administrator trust decision and must be clearly presented.
- Append-only mode is enforced both by resticpal command policy and, for meaningful ransomware resistance, by the storage system.
- Update signatures and policy signatures will use distinct keys and trust purposes.

## Initial delivery milestones

1. **Windows feasibility spike — in progress**
   - Service installation and virtual-account identity.
   - Named-pipe authorization.
   - VSS backup of known folders on Windows 10 and Windows 11.
   - Sleep/resume event handling and two-hour power request.
   - Measure idle service/tray resource use.

   The service host, named-pipe authorization, resume event, and timed power request exist. A real-restic disposable local-repository lifecycle is covered by an opt-in non-elevated test, and an exact VSS variant is available for elevated/service qualification. Installation/identity, successful elevated VSS qualification, OS-matrix qualification, and resource measurement remain.

2. **Local vertical slice — functional development slice**
   - Rust service and tray.
   - On-demand WinUI overview/settings shell.
   - Bundled restic, one repository, source selection, daily/wake scheduling, progress, cancellation, and history.
   - DPAPI/CNG secret storage.

   All listed application behaviors are implemented except release bundling of restic and production installation/qualification. Cancellation is currently immediate Job Object termination rather than graceful-first shutdown.

3. **Repository and policy breadth — partially implemented**
   - Create/connect flows and typed common backends.
   - S3-compatible configuration.
   - Standard retention and append-only/server-maintained mode.
   - Configuration file reload and per-field managed locks.

   Create/connect, S3/advanced configuration, append-only enforcement, typed policy precedence, and per-field lock enforcement are implemented. Standard retention execution/UI, live file reload, and signed managed-policy ingestion remain.

4. **Enrollment and reporting — not started**
   - Bootstrap flow, device identity, signed policy cache, encrypted secret bootstrap, metadata-file mode, status reports, schemas, fixtures, and protocol tests.

5. **Packaging and updates — package reference only**
   - WiX MSI, startup registration, upgrade/repair/uninstall behavior, NetSparkle appcast verification, and elevated atomic updater.

## Required test themes

The 109-test automated Rust baseline covers scheduling/deadlines/resume/power/network decisions, retry/cancellation state, policy precedence and locks, command construction, append-only authorization, configuration bounds, named-pipe framing/ACL/token checks, DPAPI persistence/rotation/redaction, repository validation/restart behavior, executor JSON/progress/timeouts, and SQLite history retention/redaction/restart behavior. Two additional ignored, opt-in real-restic tests create disposable local repositories: the normal developer variant removes only the VSS flag after asserting the production builder supplied it, while the exact production variant requires an elevated token with VSS access. The lifecycle verifies missing-repository detection, initialization, wrong-password rejection, probe, append-only backup, snapshots, check, and changed second-backup behavior. A PowerShell helper locates or checksum-verifies a pinned restic test binary without writing it into the repository. The WinUI project is build-validated but does not yet have an automated UI test suite.

The following themes remain required as their corresponding product areas land:

- Scheduler tests for shutdown races, daylight-saving transitions, dedicated network-change events, and graceful cancellation escalation.
- Policy tests for stale/offline signed policy, signature failure, rollback/replay attempts, schema evolution, and atomic cache recovery.
- End-to-end command tests proving enrolled configuration cannot introduce arbitrary arguments or executables.
- Append-only integration tests for storage-side delete/overwrite rejection.
- S3 integration tests for endpoint variants, regions, bucket addressing, temporary credentials, and rotation.
- Service security tests for installed binary/config ACLs, virtual-account privileges, and VSS access.
- Update tests for signature failure, interrupted download/install, an active backup, rollback/repair, and restic version replacement.
- Resource tests for long idle periods, repeated UI open/close, disconnected server/network, and large progress streams.

## Open implementation decisions

- Whether the virtual service account is sufficient for reliable VSS and arbitrary configured user paths; LocalSystem is the fallback.
- Remote schema shapes, signature envelopes, and API/metadata serialization.
- Local IPC subscription framing and compatibility policy beyond protocol v2.
- Concrete idle memory/CPU targets after the feasibility prototype.
- Default prune cadence in standard/client-maintained mode.
- Notification thresholds.
- Exact minimum supported Windows 10 build.

## License

resticpal will use the BSD 2-Clause license with:

```text
Copyright (c) 2026 Yann Ramin
```

The bundled restic binary and all other third-party components retain their own copyright notices and licenses and will be included in third-party notices.

## References

- [restic project](https://github.com/restic/restic)
- [restic Windows VSS backup documentation](https://restic.readthedocs.io/en/stable/040_backup.html)
- [restic repository backends and S3 configuration](https://restic.readthedocs.io/en/stable/030_preparing_a_new_repo.html)
- [restic append-only maintenance guidance](https://restic.readthedocs.io/en/latest/060_forget.html)
- [Windows App SDK platform overview](https://learn.microsoft.com/en-us/windows/apps/develop/platform/)
- [NetSparkleUpdater](https://github.com/NetSparkleUpdater/NetSparkle)
