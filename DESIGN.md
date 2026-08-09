# resticpal product and architecture design

Status: initial agreed direction, 2026-08-08

This document records the initial product requirements and architecture decisions for **resticpal**, a friendly, Windows-focused wrapper around [restic](https://restic.net/). It is intended to be the source of truth for the first implementation. Items explicitly marked "open" or "proposed" still require validation.

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

### `resticpal-tray.exe`

The persistent tray process uses native Win32 notification-area APIs from Rust. It should do no polling when an IPC subscription is healthy. It displays a small native menu, notifications, and the current health icon. Opening resticpal launches or activates the on-demand WinUI process.

The tray runs in each interactive user session. It may view machine backup status and request a one-run backup, deferral, or cancellation. Permanent configuration changes require elevation and service-side authorization.

### `resticpal-ui.exe`

The C# WinUI 3 application provides the modern settings and status experience. It is not kept alive merely to own a tray icon. Initial pages are:

- Overview
- Backup sources and exclusions
- Repository
- Schedule, power, and network policy
- History and diagnostics
- Enrollment and managed-policy state
- Updates and application settings

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

## IPC and authorization

Service-to-client communication uses a versioned protocol over Windows named pipes.

- The pipe security descriptor permits interactive users to read status.
- The service inspects the connecting process token rather than trusting identity fields supplied in a message.
- Administrative mutations require an elevated administrator token.
- Ordinary users may request `run now`, defer one run, or cancel one run unless the applicable action is locked by managed policy.
- The protocol exposes typed operations, not arbitrary executable paths, environment variables, or restic arguments.
- Progress is pushed by subscription; clients do not continuously poll.
- Messages have explicit protocol versions and bounded sizes.

## Backup scope

The primary use case is file-level backup of user data. This is not a bare-metal Windows recovery product.

### Default source discovery

The service discovers existing and newly created local user profiles using Windows profile and known-folder information rather than assuming `C:\Users`. New profiles automatically receive the default folder template.

The proposed default template includes:

- Desktop
- Documents
- Pictures
- Videos
- Music
- selected application/browser data where it is safe and useful

Downloads and cloud-placeholder content are opt-in by default. All defaults can be changed locally or by managed policy. The UI supports adding/removing arbitrary paths and editing exclusions.

### VSS and metadata

Backups use restic's Windows filesystem snapshot support by default so that locked files can be read from VSS. Restic's supported Windows metadata, including security descriptors and NTFS metadata, should be enabled/preserved where available. VSS improves consistency but does not turn the product into an application-aware or bare-metal backup system.

## Scheduling, wake, power, and network

The scheduler is deadline-based rather than a single fixed wall-clock task.

- Default cadence: once per day.
- If a deadline is missed because the machine is asleep or off, the backup becomes overdue.
- On resume, an overdue backup waits through a default five-minute grace period before starting.
- Network-restored, schedule, startup, and resume triggers coalesce into a single pending run.
- Only one repository-mutating restic operation may run at a time.
- Battery operation is allowed by default and configurable per field.
- Metered-network operation is allowed by default and configurable per field.
- A run waits while its required repository is unreachable and retries with bounded exponential backoff.
- A successful manual run satisfies the same backup deadline unless policy says otherwise.

While backup work is active, the service obtains a Windows system-required power request. The power request is always released when the run ends and has a hard two-hour safety limit. If a backup exceeds two hours, the power request expires but the backup itself may continue; backup runtime limits are a separate policy.

Cancellation first requests graceful termination and then escalates after a bounded shutdown period. An interrupted run is recorded distinctly from a backup failure.

## Repository support

The application bundles restic and supports both creating a new repository and connecting to an existing repository.

### Backends

The UI offers friendly, typed setup for common backends and an advanced form for the remaining backends supported by the bundled restic version. At minimum the configuration model must represent:

- local and network paths;
- REST server URLs;
- SFTP;
- S3 and S3-compatible endpoints, bucket/prefix, region, bucket lookup style, and supported backend options;
- Azure, Google Cloud Storage, Backblaze B2, and other native restic backends;
- rclone-backed repositories.

Repository URLs and non-secret options may live in policy. Passwords, access keys, secret keys, tokens, and private key material live only in the protected credential store and are referenced by opaque secret IDs.

The server may supply an S3 bucket configuration and encrypted credentials during enrollment or a later policy refresh. Managed settings remain typed and allowlisted. The server cannot send arbitrary restic command-line arguments.

### Bundled restic

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

Retention is configurable in the file and UI. The precise default prune cadence is still open; pruning should be scheduled separately from ordinary backup deadlines because it may be expensive.

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

When a server maintains a repository that receives append-only client backups, its retention design should follow restic's append-only guidance, including careful use of time-window retention to avoid an untrusted client manipulating which snapshots appear newest.

## Configuration model

### Files and state

Machine state lives under `%ProgramData%\ResticPal` with service/admin-only ACLs where appropriate.

| Item | Proposed location | Notes |
| --- | --- | --- |
| Local configuration | `config.toml` | Human-editable, no secrets |
| Managed policy cache | `managed-policy.json` | Signed, versioned, last-known-good |
| Credential store | implementation-private | DPAPI/CNG protected and service-only |
| Scheduler checkpoint | `state.json` | Last successful completion for restart-safe deadlines |
| Run history | `state.db` | Bounded SQLite history |
| Logs | `Logs\` | Structured, rotated, sanitized |
| Bundled tools | under installation directory | Administrator/service write only |

Direct edits to `config.toml` are watched, parsed, validated, and applied atomically. Invalid edits do not replace the last valid configuration and produce a clear local status error. The UI writes configuration through the service, not directly.

### Effective policy and per-field locks

Effective configuration is calculated from:

1. product defaults;
2. valid local administrator configuration;
3. the last valid signed managed policy;
4. transient one-run choices such as an allowed deferral.

Managed policy marks individual fields as locked. A managed value overrides local configuration only for that field. The UI explains the source of each managed value and disables editing only where locked. Unknown fields are rejected or safely ignored according to schema-version rules; they never become raw restic arguments.

Loss of server connectivity does not stop backups. The service continues with its last valid policy and reports that management connectivity is stale. Only a signed explicit disable policy may stop managed backups.

## Credentials and device keys

- Repository secrets are encrypted at rest using Windows DPAPI or a non-exportable Windows CNG key, scoped so only the service identity can use them.
- Protected files and registry entries also have service/admin-only ACLs; encryption does not replace access control.
- The service generates its device key locally during enrollment and sends only the public portion to the server.
- Bootstrapped secrets are encrypted for the enrolled device in addition to transport security.
- Secrets are redacted from logs, status, crash information, command lines, configuration files, and UI diagnostics.
- A local administrator may unenroll with an elevation prompt. Unenrollment removes management credentials and device enrollment state, records a local audit event, and does not delete repositories or existing backups.

## Enrollment and remote policy

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

The tray and UI expose:

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

## Remote status reporting

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

The WinUI application integrates NetSparkleUpdater. Until releases have Authenticode code signing:

- updates are user-selected and user-initiated;
- update metadata and packages require Ed25519 verification;
- the UI warns that Windows may display an unsigned-publisher or SmartScreen prompt;
- applying an update requires UAC elevation;
- an update never replaces binaries during an active backup; it waits or asks the user to cancel/defer;
- the elevated updater stops the service, replaces the application and bundled restic atomically, restarts the service, and reports the outcome;
- rollback or repair behavior must be designed before automatic installation is considered.

## Installer and deployment

- Per-machine x64 WiX MSI.
- Target Windows 10 and Windows 11; the exact minimum Windows 10 build will be validated with the first WinUI/installer prototype.
- Installs and configures the Windows service and service SID/account.
- Applies restrictive ACLs to binaries and `%ProgramData%\ResticPal`.
- Registers the lightweight tray process for interactive user logon.
- Installs the on-demand WinUI application, updater, and pinned restic binary.
- Offers initial local setup or optional enrollment by bootstrap URL.
- Supports repair/uninstall without deleting repositories. Removal of local configuration and credentials must be an explicit choice.

## Logging and audit

- Structured logs with stable event IDs and correlation/run IDs.
- Rotating local files plus important service lifecycle/failure events in Windows Event Log.
- Bounded run history in SQLite.
- Audit events for configuration changes, policy revisions, enrollment/unenrollment, credentials changes, update attempts, manual run/cancel/defer actions, and service-account fallback.
- Path and secret redaction occurs before persistence, not only in the UI.

## Security boundaries

- Only the service launches restic.
- Executable paths are fixed and ACL-protected.
- Remote and local configuration are mapped to typed allowlisted arguments/options.
- No shell command construction is used.
- All paths, URLs, options, environment names, sizes, and message lengths are validated.
- Managed repository configuration is powerful: an enrolled server can direct readable files to a repository it controls. Enrollment therefore represents an explicit administrator trust decision and must be clearly presented.
- Append-only mode is enforced both by resticpal command policy and, for meaningful ransomware resistance, by the storage system.
- Update signatures and policy signatures use distinct keys and trust purposes.

## Initial delivery milestones

1. **Windows feasibility spike**
   - Service installation and virtual-account identity.
   - Named-pipe authorization.
   - VSS backup of known folders on Windows 10 and Windows 11.
   - Sleep/resume event handling and two-hour power request.
   - Measure idle service/tray resource use.

2. **Local vertical slice**
   - Rust service and tray.
   - On-demand WinUI overview/settings shell.
   - Bundled restic, one repository, source selection, daily/wake scheduling, progress, cancellation, and history.
   - DPAPI/CNG secret storage.

3. **Repository and policy breadth**
   - Create/connect flows and typed common backends.
   - S3-compatible configuration.
   - Standard retention and append-only/server-maintained mode.
   - Configuration file reload and per-field managed locks.

4. **Enrollment and reporting**
   - Bootstrap flow, device identity, signed policy cache, encrypted secret bootstrap, metadata-file mode, status reports, schemas, fixtures, and protocol tests.

5. **Packaging and updates**
   - WiX MSI, startup registration, upgrade/repair/uninstall behavior, NetSparkle appcast verification, and elevated atomic updater.

## Required test themes

- Scheduler tests for shutdown, sleep, resume, clock changes, daylight-saving transitions, duplicate triggers, retry/backoff, battery, metered networks, and cancellation.
- Policy tests for precedence, field locks, stale/offline policy, signature failure, rollback/replay attempts, schema changes, and atomic recovery.
- Command-construction tests proving remote configuration cannot introduce arbitrary arguments or executables.
- Append-only tests proving resticpal never schedules or exposes destructive maintenance commands and behaves correctly when the backend rejects deletes/overwrites.
- S3 tests for endpoint variants, regions, bucket addressing, temporary credentials, rotation, and redaction.
- Service security tests for pipe ACLs, impersonation/token checks, service binary/config ACLs, and privilege use.
- Update tests for signature failure, interrupted download/install, an active backup, rollback/repair, and restic version replacement.
- Resource tests for long idle periods, repeated UI open/close, disconnected server/network, and large progress streams.

## Open implementation decisions

- Whether the virtual service account is sufficient for reliable VSS and arbitrary configured user paths; LocalSystem is the fallback.
- Exact local and remote schema shapes and protocol serialization.
- Concrete idle memory/CPU targets after the feasibility prototype.
- Default prune cadence in standard/client-maintained mode.
- Local history retention and notification thresholds.
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
