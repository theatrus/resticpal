# resticpal product and architecture design

Status: living design and implementation record, updated 2026-08-24

This document records the product requirements, architecture decisions, and current implementation boundary for **resticpal**, a friendly, Windows-focused wrapper around [restic](https://restic.net/). It is the source of truth for the first implementation. Items explicitly marked "open", "proposed", "planned", or "not yet implemented" still require work or validation.

## Implementation status

The repository now contains a buildable x64 Windows vertical slice using Rust 1.97 and .NET 10, plus a WiX 6 development MSI. The active automated baseline is 216 passing client Rust tests, 58 passing WinUI-independent .NET logic tests, plus 19 passing tests in the adjacent `resticpal-server` repository and warning-free .NET builds. The MSI is build/ICE/admin-image validated, and its current-session tray launch, single-instance first-run setup, all-users Start Menu/logon integration, control-level Settings and `--updates` checks, installed LocalSystem service, VSS backup path, UI-triggered backup acknowledgement, 200%-scaled Per-Monitor-v2 tray/menu behavior, and v1.0.6-to-v1.0.7 major-upgrade path pass in a disposable Windows 11 Sandbox; the wider Windows 10/11 matrix is not yet production-qualified.

| Area | Implemented | Remaining |
| --- | --- | --- |
| Core model | Typed local/managed configuration layers, per-field resolution and locks, validation bounds, deadline scheduling, standard-mode retention/prune cadence, append-only command authorization, managed-policy v1/v2 compatibility, versioned manifest and enrollment payloads, strict Ed25519 envelopes, X25519/HKDF/ChaCha20-Poly1305 secret bootstrap, freshness, and replay checks | Future schema evolution and graceful-first maintenance cancellation |
| Windows service | SCM control handling, startup/resume catch-up, power/network gates, retry backoff, restic process containment, cancellation, timed wake lock, DPAPI repository and enrollment credentials, recoverable atomic UI-driven configuration, repository create/validate, scheduler/retention checkpoint, bounded SQLite history with per-item partial-backup detail, bounded redacted diagnostic logs, bounded shutdown outcome draining, plain/signed manifest fetching, last-known-good cache, runtime policy application, bounded status delivery, one-time enrollment/rotation, unenrollment, prompt-free pinned-signature MSI download/install, and Sandbox-qualified LocalSystem/VSS execution | Windows 10/11 matrix ACL/VSS/update qualification, direct-file watching, diagnostic export/audit, and graceful cancellation before escalation |
| Local IPC | Protocol v4, 1 MiB bounded frames, bounded per-connection I/O, protected named pipe, client-token authorization, ordinary-user status/sanitized-history/run/cancel/defer, administrator-only source-failure detail and configuration/enrollment operations, effective automatic-update settings with lock state, signed package handoff, and bounded update preparation that refuses active work | Long-lived status/progress subscriptions and compatibility/evolution policy beyond v4 |
| Tray | Per-Monitor-v2 native Win32 notification icon and action menu, DPI-metric icon selection, single instance per session, current status tooltip, run/cancel action, elevated UI launch, immediate post-install startup, all-users logon registration, bounded first-run setup retry, and native six-hour detached-Ed25519-signed update checks with either daily-bounded notifications or prompt-free service installation | Push-driven live backup icon updates, deferral UI, richer health icons, and multi-session qualification |
| WinUI application | First-run bootstrap/local setup, Overview, backup sources, repository, schedule/power/network, standard/server-managed retention, bounded backup history with on-demand unreadable-file detail, redacted diagnostics, managed enrollment/rotation/unenrollment, lock-aware automatic-update control, silent service handoff, and strict signed prompted-update controls opened directly from tray notifications, with installed control-level regression coverage for Settings and `--updates` | Broader accessibility and Windows 10 qualification |
| Remote management | Plain HTTP/HTTPS manifest mode without reporting; signed HTTPS manifest mode with pinned Ed25519 key, atomic cache, replay/freshness checks, authenticated status delivery; one-time signed enrollment with encrypted credentials; adjacent server with signed manifests, bounded SQLite status, and admin-only maintenance jobs | Conditional requests, enrollment audit/rate limits, and production deployment hardening |
| Distribution | Published release line with synchronized product metadata, per-machine x64 WiX MSI, pinned restic 0.19.1, statically linked/versioned Rust payload, self-contained/versioned WinUI payload, LocalSystem service/recovery authoring, ProgramData and bootstrap registry ACL authoring, optional hidden bootstrap property, immediate tray launch, reentrancy-safe single-instance first-run setup, all-users tray logon and Start Menu integration, data-preserving uninstall, v1.0.6-to-v1.0.7 major-upgrade qualification, notices, disposable Sandbox E2E harness, release-tag/manual Azure Authenticode signing, strict Ed25519 NetSparkle client, locally held release key, bridge-first dual-named signed-appcast/release tooling, direct legacy-client MSI bridge, and GitHub CI package artifacts with checksums | Windows 10/11 installer/VSS/update matrix, optional interactive installer bootstrap dialog, complete license generation, rollback recovery, and wider upgrade/repair matrix |

Current durable state consists of atomic `config.toml`, DPAPI-protected credential files, `state.json` for scheduler/retention/repository-verification state, lazy `state.db` backup history, and rotating structured service logs. The history retains the newest 200 attempts and exposes at most 100 sanitized summaries per IPC request; the WinUI page requests 50. A warning run retains at most 100 unique, safe source-item paths of at most 4 KiB each plus an omitted count. Those paths are returned only by an explicit elevated local detail request. Diagnostics rotate at 1 MiB with three archives and expose at most 200 entries per elevated IPC request; the WinUI page requests 100.

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
- Support prompted and administrator-enabled unattended updates with cryptographically signed metadata and packages.

## Non-goals for the first release

- A restore browser or restore workflow.
- Bare-metal recovery, Windows installation imaging, or guaranteed application-consistent database backup.
- A large multi-tenant backup SaaS. The optional companion server remains small, self-hostable, and separable from standalone/plain-file operation.
- macOS support.
- ARM64 packages.

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

On demand: signed MSI major upgrade    launched by elevated WinUI or LocalSystem service
```

The service, tray, and WinUI projects in this diagram are implemented. The development MSI packages them with restic and authors service/tray registration. The tray verifies signed appcasts; administrators may retain the explicit NetSparkle UI flow or opt into a service-owned, prompt-free signed MSI path. Interrupted upgrades, rollback, and the wider Windows OS matrix still require qualification.

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
- update settings, signature verification, download, and Windows Installer coordination.

No tray or UI process may invoke restic directly.

The current service implements configuration/policy resolution, scheduling, system-condition gates, restic execution, repository setup/validation, DPAPI credentials, a persistent protected restic metadata cache, scheduler and retention state, backup history, bounded redacted structured diagnostics, managed-manifest fetching/caching, runtime policy refresh, authenticated status delivery, one-time managed enrollment/rotation/unenrollment, and bounded backup-safe update coordination. When automatic installation is enabled, it downloads only the exact versioned GitHub release MSI named by the signed feed, bounds its length, verifies the pinned Ed25519 package signature, stages it under protected ProgramData, and runs Windows Installer silently as LocalSystem. The service expects a fixed sibling `restic.exe`; the development MSI supplies a checksum-verified pinned binary at that location.

### `resticpal-tray.exe`

The persistent tray process uses native Win32 notification-area APIs from Rust. Its required as-invoker application manifest declares Per-Monitor-v2 awareness (with the older per-monitor fallback), so the hidden owner window and native action menu scale at the taskbar monitor's DPI without a login-time elevation prompt. The shell icon is loaded from the multi-frame ICO through the current small-icon metric instead of forcing one 32-pixel frame. A single left click opens the WinUI settings/status application, while a right click opens the native action menu; duplicate left-click notifications within the user's Windows double-click interval are coalesced before elevation. Backup status is fetched only at startup and when its menu or an action needs it. Separate low-frequency update work verifies the signed feed at login and every six hours; when automatic installation is enabled it hands typed package metadata to the service and retries a busy service after five minutes without prompting. A bounded one-second timer is active only during the first two minutes of an unconfigured startup so transient service validation cannot suppress onboarding. Push-driven backup state, deferral UI, and richer state-specific icons await the IPC subscription channel.

The installed design runs one tray instance in each interactive user session. It may view machine backup status and request a one-run backup, deferral, or cancellation. Permanent configuration changes require elevation and service-side authorization. The MSI starts the tray in the installing user's normal desktop token after a successful install and registers it under the machine Run key for every user's future logon; upgrade, repair, and simultaneous multi-session behavior still need qualification.

### `resticpal-ui.exe`

The C# WinUI 3 application provides the modern settings and status experience. It is not kept alive merely to own a tray icon. Implemented pages are:

- Overview
- Backup sources and exclusions
- Repository
- Schedule, power, and network policy
- Retention
- Backup history
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

The preferred feasibility candidate was a dedicated virtual service account, `NT SERVICE\ResticPal`, with a service SID, the minimum required Windows privileges, and narrowly ACLed application state. The clean-machine Sandbox spike confirmed that the service can start and access its ACLed repository under that identity, but restic cannot use VSS: Windows rejects a VSS requester that is not LocalSystem or a member of Administrators/Backup Operators by default. The same identity also cannot reliably read arbitrary user profiles without weakening their ACLs.

The x64 v1 installer therefore uses the documented fallback, LocalSystem. This supplies the filesystem access and VSS requester rights required for machine-wide user-data backup. `%ProgramData%\ResticPal`, the installed binaries, bootstrap registry state, named-pipe command surface, typed restic invocation boundary, and future update path must remain tightly ACLed and validated because this identity is highly privileged.

DPAPI data is intentionally bound to the service identity. The LocalSystem decision was made before release; any future identity change will require explicit credential migration or reprovisioning. Network-share access under the machine identity and the complete Windows 10/11 ACL/VSS matrix still require qualification.

## IPC and authorization

Service-to-client communication uses a versioned protocol over Windows named pipes. The implemented protocol is v4 over `\\.\pipe\ResticPal.v4` with one bounded request and response per connection.

- The pipe security descriptor permits interactive users to read status.
- The service inspects the connecting process token rather than trusting identity fields supplied in a message.
- Administrative mutations require an elevated administrator token.
- Ordinary users may request `run now`, defer one run, or cancel one run unless the applicable action is locked by managed policy.
- The protocol exposes typed operations, not arbitrary executable paths, environment variables, or restic arguments.
- Progress is currently returned in status snapshots. A future subscription channel will push rate-limited status/progress changes without continuous polling.
- Messages have explicit protocol versions, reject unknown fields within a version, and have bounded sizes and per-connection I/O time.

The pipe DACL, connecting-token impersonation, elevated-administrator checks, frame bounds, request IDs, and exact protocol-version checks are implemented and covered by Windows tests. Configuration reads and per-item backup-failure details are administrator-only because they expose machine configuration or source paths; ordinary users receive only the redacted canonical status and sanitized history summaries, including an aggregate failed-item count.

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

`%ProgramData%\ResticPal` is a mandatory, case-insensitive service-owned exclusion for every backup. It is added independently of local or managed user exclusions and cannot be removed by either policy source. Because restic intentionally exempts an explicitly named source leaf from exclusion matching, the wrapper also rejects any source that lexically or canonically resolves inside the protected data root. This keeps the repository metadata cache, DPAPI credential blobs, configuration, history, logs, update staging, and other internal state out of a backup even when an administrator selects a differently cased parent path, an alias, or an entire system drive.

### VSS and metadata

The backup invocation always requests restic's Windows filesystem snapshot support (`--use-fs-snapshot`) so locked local files can be read through VSS. Stock restic can continue against live files when Windows cannot create a requested VSS snapshot and can still exit successfully. resticpal recognizes restic's structured snapshot-creation diagnostics, recording the completed run as `succeeded_with_warnings` with `restic_vss_fallback`; the UI explains that one or more sources changing during the run may not represent one consistent point in time. A post-backup VSS deletion problem is recorded as `restic_vss_cleanup_failed`; combined codes preserve cleanup together with fallback or partial-source warnings. UNC paths and mapped remote drives are rejected as source roots: a network spelling can alias a local administrative share without matching the mandatory internal-data exclusion, and network roots remain unsupported until the wrapper can bind them to a stable filesystem identity. The advanced options `vss.exclude-volumes` and `vss.exclude-all-mount-points` are reserved and rejected for backup construction so configuration cannot silently opt out of coverage.

Restic 0.19.1 does not emit a machine-readable success event for every source and silently reads live data below an unsupported nested mounted volume. The current client therefore cannot prove complete VSS coverage for that edge case. A strict guarantee requires wrapper-owned VSS path mapping or an upstream/pinned restic `--require-fs-snapshot` behavior; this remains an explicit gap. The LocalSystem VSS path passes the installed Windows 11 Sandbox lifecycle, while the wider Windows 10/11 matrix and metadata restoration remain to be qualified.

Every restic operation explicitly uses the protected persistent repository-metadata cache at `%ProgramData%\ResticPal\Cache`. Restic keeps repository-specific indexes and metadata there, so scans do not have to redownload all reusable repository metadata after each service restart. Each backup also enables restic's local `--cleanup-cache` lifecycle so repository namespaces that have remained unused for more than restic's age threshold do not accumulate indefinitely; this is local-only and remains safe for append-only repositories. This cache is distinct from VSS: VSS supplies the point-in-time source-filesystem view, while the restic cache accelerates repository access. VSS improves consistency but does not turn the product into an application-aware or bare-metal backup system.

## Scheduling, wake, power, and network

The scheduler is deadline-based rather than a single fixed wall-clock task.

- Default cadence: once per day.
- If a deadline is missed because the machine is asleep or off, the backup becomes overdue.
- On resume, an overdue backup waits through a default five-minute grace period before starting.
- Manual, schedule, startup, resume, power, time-change, and periodic condition reevaluations coalesce into a single pending run. A dedicated network-change notification is not wired yet; blocked network conditions are reevaluated every minute.
- Only one repository-mutating restic operation may run at a time. Every run first executes a two-minute-bounded, cancellable `restic unlock` preflight, which removes only locks restic itself classifies as stale; cleanup failure blocks the backup and enters the normal sanitized retry path.
- Unattended battery operation is allowed by default and configurable per field; an explicit **Run backup now** bypasses that battery setting for one run without changing the saved policy.
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
- prune unreferenced data every 7 days

Retention is configurable in the file and UI. After each successful standard-mode backup, the service runs an independently constructed `forget` operation with the effective keep counts. It runs a separate `prune` operation only when the persisted seven-day cadence is due because pruning may be expensive.

Retention counts and prune cadence are bounded, resolved per field, and honor managed-policy locks. Maintenance uses the same cancellation and bounded wake-lock model as backup execution. A maintenance failure is recorded as a sanitized warning and does not misreport the already-created backup snapshot as failed.

### Append-only/server-maintained mode

In append-only mode, the backup client is intentionally not a repository administrator.

- resticpal may create backups, remove restic-classified stale lock records, and perform explicitly approved read-only inspection.
- The append-only storage credential must be able to list and delete objects in the repository's lock namespace. This narrow exception prevents abandoned client processes from permanently blocking protection; it must not grant deletion or rewriting of snapshots, indexes, packs, keys, or repository configuration.
- Lock cleanup invokes plain `restic unlock` before each backup and never `--remove-all`, so locks restic considers active are not force-removed.
- resticpal must never invoke local retention or destructive/rewriting maintenance, including `forget`, `prune`, `rewrite`, repository migration, destructive repair, key removal, or equivalent future commands.
- Repository initialization is disabled unless a separate full-access provisioning flow is explicitly configured.
- The retention UI displays **Managed by server** and does not offer a local prune action.
- Any retention values received by the client are informational unless the repository is later switched to standard mode with appropriate credentials.
- Remote status reports include the configured repository maintenance mode.
- The server or another isolated maintenance host uses separate full-access credentials to perform pruning and other administration. The initial `resticpal-server` runner queues one job per repository and constructs an allowlisted `forget --keep-within … --prune` invocation. Repository secrets are mapped from explicitly named server environment variables, never accepted from an API request, and never delivered to clients.

The client-side command restriction is defense in depth, not proof of append-only storage. Real protection must also be enforced by the repository service, proxy, S3 IAM/bucket policy, object immutability/versioning controls, or another storage-side mechanism. resticpal must label the mode as configured; it must not claim to have verified backend immutability unless a future backend-specific verification exists.

The append-only authorization matrix is implemented in the core command builder. It explicitly permits only backup, narrow stale-lock cleanup, connection probing, snapshots, and check, while tests continue to reject initialization, forget, prune, rewrite, migration, destructive repair, and key removal. The repository UI labels append-only retention as server-managed, and the service rejects initialization in this mode.

When a server maintains a repository that receives append-only client backups, its retention design should follow restic's append-only guidance, including careful use of time-window retention to avoid an untrusted client manipulating which snapshots appear newest.

## Configuration model

### Files and state

Machine state lives under `%ProgramData%\ResticPal` with service/admin-only ACLs where appropriate.

| Item | Location | Current status |
| --- | --- | --- |
| Local configuration | `config.toml` | Implemented; typed TOML with no secrets, loaded at startup and replaced atomically for accepted UI/service changes |
| Managed policy cache | `managed-policy.json` | Planned; signed, versioned, last-known-good |
| Credential store | implementation-private `Credentials\` | Implemented for repository secrets with service-identity DPAPI, opaque references, restricted ACLs, and atomic replacement |
| Restic repository cache | `Cache\` | Implemented; explicit persistent cache for every restic operation, protected for service/admin access and reused across service restarts |
| Scheduler checkpoint | `state.json` | Implemented; last success plus repository-validation identity/time for restart-safe scheduling |
| Run history | `state.db` | Implemented lazily; SQLite, newest 200 attempts, sanitized summaries, plus administrator-only bounded source-item detail for partial backups; non-blocking |
| Logs | `Logs\` | Planned; structured, rotated, sanitized |
| Bundled tools | under installation directory | Implemented in the development MSI with checksum-verified restic 0.19.1; signing and upgrade replacement remain |

The UI writes configuration through elevated typed service operations, not directly. Those writes validate a candidate effective configuration before atomically replacing `config.toml` and immediately reevaluating the scheduler. A directly edited file is validated at service startup, but live file watching and last-known-good recovery for invalid edits are not implemented yet.

### Effective policy and per-field locks

Effective configuration is calculated from:

1. product defaults;
2. valid local administrator configuration;
3. the last valid signed managed policy;
4. transient one-run choices such as an allowed deferral.

Managed policy marks individual fields as locked. A managed value overrides local configuration only for that field. The UI explains the source of each managed value and disables editing only where locked. Inner managed-policy schema v1 remains accepted for existing backup/repository/schedule/retention fields; schema v2 adds `updates.automatic_install`. A v1 policy containing that member is rejected, and a v2 policy must not be deployed to older clients until reported application versions show that they support it. The adjacent server refuses a v2 initial enrollment from clients through 1.0.6 with HTTP 426 before consuming their one-time token. Unknown fields are rejected according to schema-version rules; they never become raw restic arguments.

Loss of server connectivity does not stop backups. The service continues with its last valid policy and reports that management connectivity is stale. Only a signed explicit disable policy may stop managed backups.

The typed precedence/lock resolver and lock-aware service/UI mutation paths are implemented and tested. The running service resolves product defaults, local configuration, and an optional signed managed document; it verifies freshness and sequence, persists a last-known-good cache, and applies new revisions without restarting.

## Credentials and device keys

Repository and device-identity credential storage is implemented with DPAPI under the service identity. Credential values cross only protected administrator IPC or the verified enrollment exchange, are written under opaque collision-resistant references, are zeroized where practical, and never enter TOML or response views. Enrollment and rotation commit new protected references and configuration before retiring superseded credential files.

The target credential/device-key model is:

- Repository secrets are encrypted at rest using Windows DPAPI or a non-exportable Windows CNG key, scoped so only the service identity can use them.
- Protected files and registry entries also have service/admin-only ACLs; encryption does not replace access control.
- The service generates its device key locally during enrollment and sends only the public portion to the server.
- Bootstrapped secrets are encrypted for the enrolled device in addition to transport security.
- Secrets are redacted from logs, status, crash information, command lines, configuration files, and UI diagnostics.
- A local administrator may unenroll with an elevation prompt. Unenrollment removes management credentials and device enrollment state, records a local audit event, and does not delete repositories or existing backups.

## Enrollment and remote policy

The transport is now split into two explicit modes. `plain_manifest` fetches a direct outer-schema-v1 payload from a bounded HTTPS URL (loopback HTTP is allowed for local testing), supports offline last-known-good startup, and cannot configure signing or status reporting. Because a plain manifest is unsigned, HTTPS is required so its transport provides the only integrity guarantee against an on-path attacker rewriting policy. `signed_manifest` fetches an outer-schema-v1 Ed25519 envelope, pins the public key locally, requires HTTPS except on loopback, rejects expiry/tampering/rollback/schema mismatch, and may use a DPAPI-backed bearer token for manifest and status requests. Either envelope can carry a compatible inner managed-policy schema, and both modes apply only typed `ManagedPolicy` fields through the existing resolver.

The enrollment protocol below is implemented end to end by the service, first-run WinUI Settings page, MSI bootstrap staging, and companion server. The application opens first-run setup when a fresh service reports `Unconfigured`; unattended installs may instead set the hidden `RESTICPAL_BOOTSTRAP_URL` property.

The first-run application offers a bootstrap URL field and an explicit local-configuration path. Interactive entry is preferred because one-time URLs and tokens can leak through process listings, shell history, response logs, or MSI logs when passed on a command line.

### Trust model

- Bootstrap uses HTTPS and a time-limited, one-time signed URL.
- The bootstrap descriptor pins the server's Ed25519 policy-signing public key or fingerprint.
- Policy documents are independently signed and verified even though they are transported over TLS.
- The service rejects expired, replayed, incorrectly signed, downgraded, or schema-incompatible policy documents.
- The last valid policy is retained atomically.
- Client status requests are authenticated with the enrolled device identity.

### Enrollment sequence

1. The UI sends the bootstrap URL over administrator-only IPC, or the installer stores it in a service-readable protected registry value.
2. The service parses the URL fragment, removes it from the HTTP request target, and extracts the one-time bearer token plus pinned Ed25519 public key.
3. The service generates a device key pair.
4. The service enrolls with its public key, hostname, app/restic version, OS build, architecture, and a nonce.
5. The server returns a device ID, policy/status endpoints, a signed initial policy, and any secrets encrypted for the device.
6. The service verifies the response signature, request nonce, freshness, URLs, and initial signed manifest; decrypts the secret bundle; commits DPAPI references, cache, and configuration; then erases the one-time bootstrap material.
7. Future policy fetches use conditional requests such as ETag/version and retain the last-known-good policy when offline.

The adjacent `resticpal-server` repository implements the initial conventional API and carries v1 JSON Schemas and a plain-manifest fixture. A normal static HTTP server remains a supported metadata-only integration and never gains status reporting. OpenAPI generation and signature test-vector publication remain.

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

The implemented canonical service state includes all entries above except client-only `service_unavailable`. Waiting reasons currently cover wake grace, network, battery, metered network, policy backoff, and repository validation. Running status begins in preparation, moves to uploading when JSON progress arrives, and enters retention while standard-mode maintenance runs; finer scanning/finalizing/checking phase reporting is still planned.

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

The current local status protocol exposes redacted state, repository name/mode, last attempt/success, deadline/blocker, bounded progress, retention configuration/state, bounded structured diagnostics, and redacted management enrollment state. The overview and tray reduce backup state to a friendly health summary and current progress without assuming every warning means a file failed. The WinUI History page exposes timestamps, duration, outcome, aggregate file/byte statistics, sanitized error or warning code, snapshot ID, and failed-item count for the newest 50 records; known VSS warning codes receive a friendly consistency or cleanup explanation. Only runs with unreadable source-item details make a separate elevated request to display the bounded paths, and the Overview links any warning state to History. Diagnostics exposes the newest 100 fixed-message events to an administrator without paths or raw restic output; Settings provides enrollment, rotation, unenrollment, installed-version display, and signed update controls. Diagnostic export and notification surfaces remain planned.

## Remote status reporting

Authenticated status delivery is implemented for signed-manifest configurations that provide a device ID, status URL, and DPAPI token reference. Enrollment creates and protects those credentials. The worker reloads the local management source so a live enrollment activates without restarting, reports important state changes, every five minutes while running, every six hours while idle, and uses bounded exponential backoff after failure. Delivery is isolated from backup success. Per-item failure paths exist only in local history and are not part of `ServiceStatus` or any report payload. Jitter, richer version fields, and durable delivery state remain.

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

The native Rust tray checks `https://updates.resticpal.com/appcast-v2.xml` at login and every six hours, with the stable GitHub Releases v2 appcast URL as its fallback. Each origin's bounded appcast and detached-signature documents are fetched as a pair; the next origin is tried on a network, parse, or signature failure. An available update from the preferred origin wins, but a trusted preferred feed that is merely current does not suppress a newer signed fallback; resticpal reports itself current only after every usable origin has been checked. The tray verifies the exact appcast bytes with the compiled Ed25519 update public key and only then parses its strictly numeric version, exact MSI URL, package signature, and declared length. A failure at every origin is silent and never offers an unverified update. Only an explicitly disabled effective automatic setting may produce the daily-bounded notification and `Update available…` menu action; if the setting cannot be read, the tray retries silently rather than guessing that prompts are allowed.

The WinUI application integrates NetSparkleUpdater 3.1.0 against the same ordered primary and fallback appcasts. Settings checks quietly when opened and supports an explicit user check. When the effective automatic setting is enabled, discovery immediately hands the signed package metadata to the protected service and never shows Download/Install buttons or confirmation dialogs; enabling the toggle also dispatches an already-known update or forces a fresh check. Only the explicitly prompted path downloads in the user session and asks before download and installation. GitHub's redirected asset response can omit the original filename, so resticpal explicitly assigns every prompted NetSparkle download a unique `.msi` filename before transfer; without that suffix NetSparkle cannot construct its `msiexec` command. NetSparkle independently verifies the MSI enclosure's Ed25519 signature immediately after download and again before starting the user-selected Windows Installer upgrade.

An administrator may enable `[updates] automatic_install = true` in `config.toml` or Settings, or a managed-policy-v2 `updates.automatic_install` field may recommend or lock either value. The Settings toggle reports and enforces the per-field lock, and background policy revisions refresh it without restarting the UI. The tray and WinUI then send signed package metadata to the protected service instead of prompting. The service accepts `install_update` only while the effective setting is enabled, requires a strictly newer three-part version, restricts the URL to the exact versioned resticpal MSI path on GitHub Releases or `updates.resticpal.com`, bounds the download to 512 MiB and the signed declared length, verifies the compiled Ed25519 package key, and atomically changes a `.partial` file to `.msi` only after verification. It invokes `msiexec /qn /norestart` as LocalSystem. A running backup or repository/management operation is never interrupted; the tray retries a busy service after five minutes without exposing a manual fallback.

Release-tag and explicitly dispatched builds Authenticode-sign the MSI and its service, tray, UI, and bundled restic payload as StackFoundry LLC; ordinary CI builds remain unsigned. Release preparation refuses to create the Ed25519 package signature until that Authenticode identity and version have been verified.

The update key is distinct from all resticpal-server policy/enrollment keys. Its private and public backup files live under `~/Dropbox/resticpal/keys/updates`; only the public key is committed and compiled into the Authenticode-signed tray and WinUI executables. The private key is never stored in the repository or GitHub Secrets. A pinned NetSparkle appcast tool and local release script download an Authenticode-signed CI artifact, reject an unexpected publisher or version, generate and verify the appcast/MSI Ed25519 signatures, and optionally publish a reviewed GitHub release.

Release publication is bridge-first. Staging publishes only the exact signed MSI, checksum, license, and notices, with no appcast asset. The deployed legacy release hook can therefore mirror and validate the candidate at a direct `.msi` URL without changing the live feed. Each stage invocation adds a fresh hidden deployment marker to the release notes so an interrupted mirror can be retriggered even when the package assets are already exact. Only after that URL and both prompted and silent LocalSystem upgrades from the official v1.0.6 client pass does preparation's schema-5 `dual_named_feed` become publishable: one canonical signed `appcast-v2.xml` pair is copied byte-for-byte to the `appcast.xml` names. Finalization uploads the checksum and v2 pair before the legacy signature and XML; `appcast.xml` is last because its appearance on the already-latest GitHub release is the rollout point. The subsequent release edit makes the legacy hook advance `updates.resticpal.com/appcast.xml`. It does not yet mirror `appcast-v2.xml`, so v1.0.7 clients temporarily see a 404 from that primary and use their identical signed GitHub v2 fallback; no trust or package URL is weakened by this hosting limitation.

Current behavior is:

- update checks are automatic while a user is signed in; installation is either explicitly selected or performed silently after an administrator opts in;
- update metadata and packages require Ed25519 verification;
- NetSparkle downloads are assigned an explicit `.msi` filename before transfer; prompted installs remain user-selected, while automatic LocalSystem installs use quiet, no-restart MSI arguments;
- automatic application runs only through the protected LocalSystem service and its pinned release-asset allowlist;
- before launching the MSI, the service rejects an update while a backup, repository operation, or management operation is active;
- an accepted preparation holds new backup/repository work for a bounded period while the MSI starts, and automatically expires if installation is cancelled;
- WiX major-upgrade behavior stops the service, replaces the signed application and bundled restic payload, and restarts the service;
- rollback, interrupted-upgrade recovery, and repair behavior remain to be qualified before automatic installation is considered.

## Installer and deployment

The first development installer is implemented as a per-machine x64 WiX 6 MSI. Its build pipeline creates optimized Rust binaries, a self-contained WinUI payload, and a checksum-verified pinned restic 0.19.1 payload; embeds project/third-party notices and package metadata; and runs MSI ICE validation. A non-privileged administrative-image extraction has verified cabinet layout and packaged service/restic execution.

The MSI currently:

- installs the application under 64-bit Program Files;
- registers an automatically started LocalSystem `ResticPal` service with bounded restart recovery actions;
- authors service/admin/system access for `%ProgramData%\ResticPal` while leaving created configuration, credentials, state, history, and repositories untracked by MSI so uninstall preserves them;
- registers the lightweight tray under the machine Run key for every user's future interactive logon and launches it in the installing user's normal desktop token immediately after installation;
- installs an all-users Start Menu shortcut and the on-demand self-contained WinUI application beside the tray;
- opens first-run setup with a bootstrap URL field and local-configuration alternative when the service is unconfigured;
- gracefully closes tray and settings processes during uninstall and major upgrade before replacing their binaries;
- registers the candidate payload before removing the prior product, so a higher compatible self-contained .NET patch retained by Windows Installer cannot be costed out and then deleted during the upgrade;
- embeds the pinned restic binary and development license/notice inventory;
- supports standard MSI reinstall, repair, upgrade, and uninstall mechanics, though the upgrade/repair matrix is not yet qualified.

An elevated PowerShell E2E harness refuses to touch a pre-existing installation or data directory, installs the MSI, verifies identity/payload/registration, current-session tray launch, first-run setup, all-users logon startup and the Start Menu shortcut, configures a disposable local repository and DPAPI password through protected IPC, initializes then switches it to append-only mode, runs a VSS backup, checks restart-persistent history, uninstalls, proves process/shortcut/registration cleanup and data preservation, and removes only its synthetic state. The same lifecycle runs in a disposable Windows Sandbox through the Windows 11 Sandbox CLI, with a `.wsb` fallback on older hosts.

The following packaging work remains:

The development WinUI project currently declares `10.0.17763.0` (Windows 10 version 1809) as its target-platform minimum. That is a build setting, not yet a qualified support promise.

- Target Windows 10 and Windows 11; the exact minimum Windows 10 build will be validated with the first WinUI/installer prototype.
- Qualify LocalSystem source access, ProgramData ACL behavior, VSS, repair, same-version and additional major upgrades, and reboot/restart cases across supported Windows 10/11 builds. The published-client v1.0.4-to-v1.0.5 updater lifecycle and direct-MSI v1.0.6-to-v1.0.7 major upgrade pass on Sandbox build 26100; signed v1.0.7 prompted and automatic previous-client qualifications remain release blockers.
- Decide whether an interactive MSI bootstrap dialog adds enough value beyond first-run application setup and unattended bootstrap staging.
- Qualify signed NetSparkle download/install, cancelled UAC, interrupted MSI, rollback, and repair behavior on the supported OS matrix.
- Add an explicit uninstall choice for removing local configuration and credentials; default uninstall remains data-preserving.

## Logging and audit

- Bounded run history is implemented in SQLite, retaining the newest 200 attempts and returning at most 100 records per local IPC request.
- Sanitized run summaries persist timestamps, outcomes, aggregate counts, failed-item counts, and allowlisted error/snapshot identifiers. They contain no source paths, filenames, repository URLs, raw output, credentials, or exception text and remain readable by ordinary local clients.
- For a partial backup only, a separate child table retains at most 100 unique source-item paths, each at most 4 KiB. Control characters, bidirectional formatting controls, oversized entries, and excess entries are omitted and counted. The table is stored under protected ProgramData, never enters diagnostics or remote status, and is returned only through an elevated local detail operation; values are validated again on read.
- History I/O failure is logged to the service diagnostic stream and never blocks or fails a backup.
- Structured rotating logs, stable event IDs, correlation/run IDs, Windows Event Log integration, and audit events are planned.

## Security boundaries

The service-only execution boundary, shell-free command construction, typed option validation, bounded IPC/progress parsing, DPAPI credential references, append-only operation checks, initial MSI ACL authoring, Authenticode payload signing, and distinct policy/update Ed25519 trust roots are implemented. Installed ACL and update recovery behavior still need wider elevated qualification.

- Only the service launches restic.
- Installed executable paths will be fixed and ACL-protected; the current executor already fixes restic to the sibling application path.
- Remote and local configuration are mapped to typed allowlisted arguments/options.
- No shell command construction is used.
- All paths, URLs, options, environment names, sizes, and message lengths are validated.
- Managed repository configuration is powerful: an enrolled server can direct readable files to a repository it controls. Enrollment therefore represents an explicit administrator trust decision and must be clearly presented.
- Append-only mode is enforced both by resticpal command policy and, for meaningful ransomware resistance, by the storage system.
- Update signatures and policy signatures use distinct keys and trust purposes.

## Initial delivery milestones

1. **Windows feasibility spike — in progress**
   - Service installation and least-privilege identity feasibility.
   - Named-pipe authorization.
   - VSS backup of known folders on Windows 10 and Windows 11.
   - Sleep/resume event handling and two-hour power request.
   - Measure idle service/tray resource use.

   The service host, named-pipe authorization, resume event, timed power request, MSI service registration, and installed-service harness exist. A virtual service account was rejected for v1 after clean-machine testing proved it could not request VSS or reliably cover arbitrary user ACLs; the LocalSystem fallback now passes the disposable installed-service VSS lifecycle. OS-matrix qualification and resource measurement remain.

2. **Local vertical slice — functional development slice**
   - Rust service and tray.
   - On-demand WinUI overview/settings shell.
   - Bundled restic, one repository, source selection, daily/wake scheduling, progress, cancellation, and history.
   - DPAPI/CNG secret storage.

   All listed application behaviors and development release bundling are implemented. Production installation/qualification remains. Cancellation is currently immediate Job Object termination rather than graceful-first shutdown.

3. **Repository and policy breadth — partially implemented**
   - Create/connect flows and typed common backends.
   - S3-compatible configuration.
   - Standard retention and append-only/server-maintained mode.
   - Configuration file reload and per-field managed locks.

   Create/connect, S3/advanced configuration, append-only enforcement, typed policy precedence, per-field lock enforcement, plain/signed manifest ingestion, live remote policy application, and standard client-side retention UI/execution are implemented. Live local-file reload remains.

4. **Enrollment and reporting — functional alpha**
   Plain-file mode, signed-envelope verification, replay/freshness checks, atomic cache fallback, authenticated status delivery, one-time bootstrap, generated device identity, encrypted secret bootstrap, installer staging, UI enrollment/rotation/unenrollment, schemas, tests, and the companion server are implemented. Conditional requests, audit retention, rate limits, and production deployment hardening remain.

5. **Packaging and updates — functional signed alpha**
   - WiX MSI, startup registration, upgrade/repair/uninstall behavior, NetSparkle appcast verification, explicit MSI staging, and optional prompt-free service updater.

   The x64 MSI, service/tray registration and immediate launch, all-users Start Menu entry, first-run bootstrap/local setup, bundled restic, synchronized version resources, data-preserving uninstall authoring, validation, Sandbox E2E harness, Azure-signed GitHub CI artifact, strict NetSparkle client, separate backed-up update key, bridge-first dual-named signed-appcast tooling, extension-preserving download, service-side silent install, and backup-safe update preparation exist. Windows-version/VSS/update and upgrade/repair/rollback qualification plus optional interactive MSI UX remain.

## Required test themes

The automated Rust baseline covers scheduling/deadlines/resume/power/network decisions, retry/cancellation state, policy precedence and locks, standard retention/prune construction and state, append-only authorization, configuration bounds, mandatory internal-data exclusion, explicit restic cache use and stale-namespace cleanup, bounded/redacted diagnostics, signed-manifest tampering/expiry/replay, plain-HTTP fetch and offline cache recovery, named-pipe framing/ACL/token checks, DPAPI persistence/rotation/redaction, repository validation/restart behavior, executor JSON/progress/timeouts, bounded partial-source and VSS-warning parsing, SQLite history retention/migration/detail authorization/redaction/restart behavior, and tray first-run gating and launch reentrancy. Pure .NET tests cover ordered update-feed arbitration, the minimum manual-backup acknowledgement dwell, source-failure and consistency-warning copy, and managed-revision synchronization across initial observation, forced enrollment refresh, per-page completion, busy/failing reloads, baseline-gated editing, unsaved edits, explicit discard, field-level payload omission, retry, and overlapping revisions. The server baseline covers constant-time token authentication, API manifest/status round trips, bounded SQLite state, configuration allowlists, and maintenance environment isolation. Four ignored, opt-in tests cover two disposable real-restic repository paths, live signed update feeds, and live server enrollment; the non-VSS real-restic path also executes real `forget` and `prune`. The MSI is release-build, ICE, administrative-image, payload-hash, restic-version, and packaged-service console validated. The elevated installed-service lifecycle passed in a disposable Windows 11 Sandbox on build 26100 for both a clean v1.0.7 installation and a direct-MSI v1.0.6-to-v1.0.7 major upgrade, including compatible bundled-runtime preservation, single-instance onboarding, current-session tray launch, Start Menu launch, control-level Settings and `--updates` verification, real append-only and standard local backups, restart persistence, retention/prune, and uninstall cleanup. The WinUI project is build-validated, and its installed automation protects the setup/update navigation and single-instance contracts while asserting repository-waiting, manually requested, running, and completed overview-card states; broader page interaction and accessibility coverage remain.

The following themes remain required as their corresponding product areas land:

- Scheduler tests for shutdown races, daylight-saving transitions, dedicated network-change events, and graceful cancellation escalation.
- Policy tests for signed-cache offline expiry behavior, key rotation, schema evolution, and atomic-cache interruption recovery.
- End-to-end command tests proving enrolled configuration cannot introduce arbitrary arguments or executables.
- Append-only integration tests for storage-side delete/overwrite rejection.
- S3 integration tests for endpoint variants, regions, bucket addressing, temporary credentials, and rotation.
- Service security tests for installed binary/config ACLs, LocalSystem command-surface restrictions, and VSS access across the supported OS matrix.
- Update tests for signature failure, interrupted download/install, an active backup, rollback/repair, and restic version replacement.
- Resource tests for long idle periods, repeated UI open/close, disconnected server/network, and large progress streams.

## Open implementation decisions

- Remote schema shapes, signature envelopes, and API/metadata serialization.
- Local IPC subscription framing and compatibility policy beyond protocol v3.
- Concrete idle memory/CPU targets after the feasibility prototype.
- Default prune cadence in standard/client-maintained mode.
- Notification thresholds.
- Exact minimum supported Windows 10 build.

## License

resticpal uses the BSD 2-Clause license with:

```text
Copyright (c) 2026 Yann Ramin
```

The bundled restic binary and all other third-party components retain their own copyright notices and licenses and will be included in third-party notices.

## References

- [restic project](https://github.com/restic/restic)
- [restic Windows VSS backup documentation](https://restic.readthedocs.io/en/stable/040_backup.html)
- [restic repository backends and S3 configuration](https://restic.readthedocs.io/en/stable/030_preparing_a_new_repo.html)
- [restic append-only maintenance guidance](https://restic.readthedocs.io/en/latest/060_forget.html)
- [Microsoft VSS requester security considerations](https://learn.microsoft.com/en-us/windows/win32/vss/security-considerations-for-requestors)
- [Windows App SDK platform overview](https://learn.microsoft.com/en-us/windows/apps/develop/platform/)
- [NetSparkleUpdater](https://github.com/NetSparkleUpdater/NetSparkle)
