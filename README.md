# resticpal

resticpal is an early-stage, Windows-focused companion for [restic](https://restic.net/). It will provide machine-wide file backups through a Windows service, a low-resource notification-area process, and an on-demand WinUI 3 application.

The agreed product and architecture direction is recorded in [DESIGN.md](DESIGN.md).

## Repository layout

- `crates/resticpal-core`: testable policy, scheduling, status, and restic invocation logic
- `crates/resticpal-protocol`: versioned and length-bounded local IPC messages
- `crates/resticpal-windows`: ACL-protected Windows named-pipe integration
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
- an on-demand WinUI application that reads service status and sends run-now requests.

Persistence, secret resolution, actual restic process execution, source discovery, installation, and enrollment are not wired up yet. IPC currently uses bounded one-request/response connections; a later status-subscription channel will replace UI polling.

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

## License

BSD 2-Clause. Copyright (c) 2026 Yann Ramin. See [LICENSE](LICENSE).
