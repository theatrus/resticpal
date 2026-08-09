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
- authenticated local IPC using client token impersonation and a protected DACL;
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
- a durable repository-validation gate tied to connection fields and credential references, preventing backups after unverified changes;
- elevated, per-field policy-aware schedule configuration with atomic persistence and immediate scheduler reevaluation;
- a WinUI Schedule page for interval, wake grace, wake-lock timeout, battery, and metered-network behavior;
- bounded SQLite run history containing only timestamps, outcomes, aggregate counts, sanitized codes, and snapshot identifiers;
- read-only bounded history IPC for interactive users and a native WinUI History page.

Credential provisioning through installer bootstrap/enrollment, automatic discovery for newly created profiles, direct-file configuration watching, installation, and enrollment are not wired up yet. The executor fails closed when a referenced credential is absent, corrupt, or not a valid environment value, and packaging still needs to supply the pinned sibling `restic.exe`. Cancellation currently terminates the contained process job; graceful restic shutdown before escalation remains to be added. IPC currently uses bounded one-request/response connections; a later status-subscription channel will provide push updates.

## Build and test

The repository pins Rust 1.97 and .NET SDK 10.0.302.

```powershell
cargo fmt --all -- --check
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
dotnet build ResticPal.slnx --configuration Debug
```

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
