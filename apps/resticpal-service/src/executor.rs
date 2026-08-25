//! Restic child-process execution with bounded output parsing and cancellation.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{self, BufRead, BufReader, Read};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Component, Path, PathBuf, Prefix};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use resticpal_core::config::{EffectiveConfig, SecretEnvironmentVariable};
use resticpal_core::restic::{
    InvocationError, ResticCommandBuilder, ResticInvocation, ResticOperation,
    validate_restore_snapshot_id, validate_restore_snapshot_path,
    windows_path_is_same_or_descendant,
};
use resticpal_core::status::{BackupProgress, MAX_BACKUP_FAILED_ITEMS, is_safe_backup_failed_item};
use resticpal_protocol::{RestoreEntryView, RestoreNodeType, RestoreSnapshotView};
use resticpal_windows::credentials::DpapiSecretStore;
use serde::Deserialize;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_ALREADY_EXISTS, ERROR_CANCELLED, ERROR_MORE_DATA, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetSecurityInfo,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, EqualSid, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, GetSecurityDescriptorOwner, OWNER_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, PSID, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
    UNPROTECTED_DACL_SECURITY_INFORMATION,
};
use windows::Win32::Storage::FileSystem::{
    CreateDirectoryW, FILE_GENERIC_READ, GetDriveTypeW, WRITE_DAC,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::SystemInformation::{
    ComputerNamePhysicalDnsHostname, GetComputerNameExW,
};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;
use windows::Win32::System::WindowsProgramming::DRIVE_REMOTE;
use windows::core::{BOOL, HRESULT, PCWSTR, PWSTR, w};
use zeroize::Zeroizing;

use crate::power_request::TimedSystemPowerRequest;

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const PROCESS_TERMINATION_GRACE: Duration = Duration::from_secs(5);
const REPOSITORY_OPERATION_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const STALE_LOCK_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const MAX_JSON_LINE_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_PENDING_PROGRESS_EVENTS: usize = 16;
const MAX_RESTORE_SNAPSHOTS: usize = 2_048;
const MAX_RESTORE_DIRECTORY_ENTRIES: usize = 4_096;
const MAX_RESTORE_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_RESTORE_PREFLIGHT_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESTORE_PREFLIGHT_NODES: usize = 250_000;
const MAX_RESTORE_ITEM_PATH_BYTES: usize = 4 * 1024;
const MAX_RESTORE_ITEM_NAME_BYTES: usize = 1_024;
const MAX_RESTORE_SNAPSHOT_PATHS: usize = 128;
const MAX_RESTORE_HOSTNAME_BYTES: usize = 255;
#[cfg(not(test))]
const RESTORE_DIRECTORY_SDDL: PCWSTR = w!("O:SYG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
// Unit and disposable-repository tests execute under the developer account,
// not LocalSystem. Owner Rights keeps those non-service harnesses usable while
// production builds always require an explicitly SYSTEM-owned directory.
#[cfg(test)]
const RESTORE_DIRECTORY_SDDL: PCWSTR = w!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)");
const INHERITED_ENVIRONMENT: &[&str] = &[
    "SystemRoot",
    "TEMP",
    "TMP",
    "LOCALAPPDATA",
    "APPDATA",
    "USERPROFILE",
];

#[derive(Debug, Clone, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RestoreError {
    pub(crate) code: &'static str,
}

impl RestoreError {
    pub(crate) const fn new(code: &'static str) -> Self {
        Self { code }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RestoreProgress {
    pub(crate) destination: Option<PathBuf>,
    pub(crate) files_restored: Option<u64>,
    pub(crate) bytes_restored: Option<u64>,
    pub(crate) total_files: Option<u64>,
    pub(crate) total_bytes: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RestoreOutcome {
    pub(crate) kind: RestoreOutcomeKind,
    pub(crate) destination: Option<PathBuf>,
    pub(crate) files_restored: u64,
    pub(crate) bytes_restored: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RestoreOutcomeKind {
    Succeeded,
    Failed { code: String },
    Cancelled,
}

impl RestoreOutcome {
    pub(crate) fn failed(code: &str) -> Self {
        Self {
            kind: RestoreOutcomeKind::Failed {
                code: code.to_owned(),
            },
            destination: None,
            files_restored: 0,
            bytes_restored: 0,
        }
    }

    fn failed_at(code: &str, destination: Option<PathBuf>) -> Self {
        let mut outcome = Self::failed(code);
        outcome.destination = destination;
        outcome
    }

    fn cancelled(destination: Option<PathBuf>) -> Self {
        Self {
            kind: RestoreOutcomeKind::Cancelled,
            destination,
            files_restored: 0,
            bytes_restored: 0,
        }
    }
}

pub trait SecretResolver: Send + Sync {
    fn resolve(&self, reference: &str) -> Result<Zeroizing<String>, SecretResolveError>;
}

#[derive(Debug, Clone, Copy)]
pub struct UnavailableSecretResolver;

impl SecretResolver for UnavailableSecretResolver {
    fn resolve(&self, _reference: &str) -> Result<Zeroizing<String>, SecretResolveError> {
        Err(SecretResolveError)
    }
}

#[derive(Debug, Clone)]
pub struct DpapiSecretResolver {
    store: DpapiSecretStore,
}

impl DpapiSecretResolver {
    pub fn new(store: DpapiSecretStore) -> Self {
        Self { store }
    }
}

impl SecretResolver for DpapiSecretResolver {
    fn resolve(&self, reference: &str) -> Result<Zeroizing<String>, SecretResolveError> {
        let bytes = self.store.get(reference).map_err(|_| SecretResolveError)?;
        let value = std::str::from_utf8(&bytes)
            .map_err(|_| SecretResolveError)?
            .to_owned();
        if value.contains('\0') {
            return Err(SecretResolveError);
        }
        Ok(Zeroizing::new(value))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SecretResolveError;

pub trait WakeLockLease: Send {}

impl<T: Send> WakeLockLease for T {}

pub trait WakeLockProvider: Send + Sync {
    fn acquire(&self, timeout: Duration) -> Result<Box<dyn WakeLockLease>, WakeLockAcquireError>;
}

#[derive(Debug, Clone, Copy)]
pub struct SystemWakeLockProvider;

impl WakeLockProvider for SystemWakeLockProvider {
    fn acquire(&self, timeout: Duration) -> Result<Box<dyn WakeLockLease>, WakeLockAcquireError> {
        TimedSystemPowerRequest::acquire("resticpal backup in progress", timeout)
            .map(|request| Box::new(request) as Box<dyn WakeLockLease>)
            .map_err(|_| WakeLockAcquireError)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct WakeLockAcquireError;

#[derive(Clone)]
pub struct ResticExecutor {
    executable: Arc<OsString>,
    data_directory: Arc<PathBuf>,
    cache_directory: Arc<PathBuf>,
    secrets: Arc<dyn SecretResolver>,
    wake_locks: Arc<dyn WakeLockProvider>,
}

impl ResticExecutor {
    pub fn new(
        executable: impl Into<OsString>,
        data_directory: impl Into<PathBuf>,
        secrets: Arc<dyn SecretResolver>,
        wake_locks: Arc<dyn WakeLockProvider>,
    ) -> Self {
        let data_directory = data_directory.into();
        let cache_directory = data_directory.join("Cache");
        Self {
            executable: Arc::new(executable.into()),
            data_directory: Arc::new(data_directory),
            cache_directory: Arc::new(cache_directory),
            secrets,
            wake_locks,
        }
    }

    pub fn backup(
        &self,
        config: &EffectiveConfig,
        cancellation: &CancellationToken,
        on_progress: impl FnMut(BackupProgress),
    ) -> BackupOutcome {
        let (unlock, _) = match build_backup_invocations(
            self.executable.as_ref().as_os_str(),
            self.data_directory.as_ref(),
            config,
        ) {
            Ok(invocations) => invocations,
            Err(error) => return BackupOutcome::failed(backup_invocation_error_code(&error)),
        };

        let cleanup =
            self.execute_repository_invocation(&unlock, STALE_LOCK_CLEANUP_TIMEOUT, cancellation);
        if let Some(outcome) = backup_outcome_for_lock_cleanup(&cleanup) {
            return outcome;
        }

        // Source paths live outside the protected service directory and can
        // change while restic performs stale-lock cleanup. Resolve and validate
        // them again after that operation so a junction or namespace alias
        // introduced during cleanup cannot reuse a plan built against the old
        // filesystem layout.
        let (_, backup) = match build_backup_invocations(
            self.executable.as_ref().as_os_str(),
            self.data_directory.as_ref(),
            config,
        ) {
            Ok(invocations) => invocations,
            Err(error) => return BackupOutcome::failed(backup_invocation_error_code(&error)),
        };

        self.execute_invocation(
            &backup,
            Duration::from_secs(config.schedule.wake_lock_timeout_seconds),
            cancellation,
            false,
            on_progress,
        )
    }

    /// Reads a bounded list of snapshots created by this Windows hostname.
    pub(crate) fn list_snapshots(
        &self,
        config: &EffectiveConfig,
        cancellation: &CancellationToken,
    ) -> Result<Vec<RestoreSnapshotView>, RestoreError> {
        let hostname = current_restic_hostname()?;
        let mut invocation = ResticCommandBuilder::new(self.executable.as_ref())
            .inspection(config, ResticOperation::Snapshots)
            .map_err(|_| RestoreError::new("invalid_repository_configuration"))?;
        invocation.arguments.push("--host".into());
        invocation.arguments.push(hostname.clone().into());
        let output = self.execute_capture_invocation(
            &invocation,
            REPOSITORY_OPERATION_TIMEOUT,
            cancellation,
        )?;
        let wire: Vec<WireRestoreSnapshot> = serde_json::from_slice(&output)
            .map_err(|_| RestoreError::new("restore_snapshot_output_invalid"))?;
        if wire.len() > MAX_RESTORE_SNAPSHOTS {
            return Err(RestoreError::new("restore_snapshot_limit_exceeded"));
        }
        let mut snapshots = Vec::with_capacity(wire.len());
        for snapshot in wire {
            validate_restore_snapshot_id(&snapshot.id)
                .map_err(|_| RestoreError::new("restore_snapshot_output_invalid"))?;
            if snapshot.hostname.len() > MAX_RESTORE_HOSTNAME_BYTES
                || snapshot.hostname.chars().any(char::is_control)
                || snapshot.paths.len() > MAX_RESTORE_SNAPSHOT_PATHS
                || snapshot.paths.iter().any(|path| {
                    path.is_empty()
                        || path.len() > MAX_RESTORE_ITEM_PATH_BYTES
                        || path.chars().any(char::is_control)
                })
            {
                return Err(RestoreError::new("restore_snapshot_output_invalid"));
            }
            // The CLI host selector and this independent check must both agree
            // before another machine's private snapshot metadata is exposed.
            if !snapshot.hostname.eq_ignore_ascii_case(&hostname) {
                continue;
            }
            snapshots.push(RestoreSnapshotView {
                id: snapshot.id,
                time: snapshot.time,
                hostname: snapshot.hostname,
                paths: snapshot.paths,
            });
        }
        snapshots.sort_by_key(|snapshot| std::cmp::Reverse(snapshot.time));
        Ok(snapshots)
    }

    /// Lists a single directory without recursive enumeration or raw output.
    pub(crate) fn list_directory(
        &self,
        config: &EffectiveConfig,
        snapshot_id: &str,
        path: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<RestoreEntryView>, RestoreError> {
        let invocation = ResticCommandBuilder::new(self.executable.as_ref())
            .directory_listing(config, snapshot_id, path)
            .map_err(|error| RestoreError::new(restore_invocation_error_code(&error)))?;
        let output = self.execute_capture_invocation(
            &invocation,
            REPOSITORY_OPERATION_TIMEOUT,
            cancellation,
        )?;
        parse_restore_directory(&output, path)
    }

    /// Restores one validated node into a fresh local destination. Restic
    /// creates its ordinary shared repository lock; no unlock or destructive
    /// maintenance command is ever executed as part of this operation.
    pub(crate) fn restore(
        &self,
        config: &EffectiveConfig,
        snapshot_id: &str,
        path: &str,
        destination: &Path,
        cancellation: &CancellationToken,
        mut on_progress: impl FnMut(RestoreProgress),
    ) -> RestoreOutcome {
        if let Err(error) = validate_restore_snapshot_id(snapshot_id)
            .and_then(|()| validate_restore_snapshot_path(path))
        {
            return RestoreOutcome::failed(restore_invocation_error_code(&error));
        }
        if path == "/" {
            return RestoreOutcome::failed("restore_path_invalid");
        }
        let parent = match validate_restore_destination(
            destination,
            self.data_directory.as_ref(),
            self.executable.as_ref(),
        ) {
            Ok(parent) => parent,
            Err(error) => return RestoreOutcome::failed(error.code),
        };
        let parent_lock = match lock_restore_directory(&parent) {
            Ok(lock) => lock,
            Err(error) => return RestoreOutcome::failed(error.code),
        };
        if let Err(error) = ensure_locked_restore_directory(&parent) {
            return RestoreOutcome::failed(error.code);
        }
        if cancellation.is_cancelled() {
            return RestoreOutcome::cancelled(None);
        }
        let operation_timeout = Duration::from_secs(config.schedule.wake_lock_timeout_seconds);
        let operation_started = Instant::now();
        let mut preflight = match ResticCommandBuilder::new(self.executable.as_ref())
            .directory_listing(config, snapshot_id, path)
        {
            Ok(invocation) => invocation,
            Err(error) => return RestoreOutcome::failed(restore_invocation_error_code(&error)),
        };
        let Some(command_index) = preflight
            .arguments
            .iter()
            .position(|argument| argument == OsStr::new("ls"))
        else {
            return RestoreOutcome::failed("restore_subtree_preflight_failed");
        };
        preflight
            .arguments
            .insert(command_index + 1, OsString::from("--recursive"));
        if let Err(error) = self.execute_restore_subtree_preflight(
            &preflight,
            operation_timeout,
            cancellation,
            path,
        ) {
            if error.code == "restore_cancelled" {
                return RestoreOutcome::cancelled(None);
            }
            if error.code == "restore_query_timed_out" {
                return RestoreOutcome::failed("restore_timed_out");
            }
            return RestoreOutcome::failed(error.code);
        }
        if cancellation.is_cancelled() {
            return RestoreOutcome::cancelled(None);
        }
        if operation_started.elapsed() >= operation_timeout {
            return RestoreOutcome::failed("restore_timed_out");
        }
        let unique_destination = match create_restore_destination(&parent) {
            Ok(destination) => destination,
            Err(error) => return RestoreOutcome::failed(error.code),
        };
        let destination = Some(unique_destination.clone());
        let destination_lock = match lock_restore_directory_for_handoff(&unique_destination) {
            Ok(lock) => lock,
            Err(error) => return RestoreOutcome::failed_at(error.code, destination),
        };
        let outcome = (|| {
            if let Err(error) = ensure_locked_restore_directory(&unique_destination) {
                return RestoreOutcome::failed_at(error.code, destination.clone());
            }
            if let Err(error) = verify_protected_restore_directory(&parent_lock, &destination_lock)
            {
                return RestoreOutcome::failed_at(error.code, destination.clone());
            }
            on_progress(RestoreProgress {
                destination: destination.clone(),
                ..RestoreProgress::default()
            });
            let invocation = match ResticCommandBuilder::new(self.executable.as_ref()).restore(
                config,
                snapshot_id,
                path,
                &unique_destination,
            ) {
                Ok(invocation) => invocation,
                Err(error) => {
                    return RestoreOutcome::failed_at(
                        restore_invocation_error_code(&error),
                        destination.clone(),
                    );
                }
            };
            let Some(remaining_timeout) = operation_timeout
                .checked_sub(operation_started.elapsed())
                .filter(|remaining| !remaining.is_zero())
            else {
                return RestoreOutcome::failed_at("restore_timed_out", destination.clone());
            };
            self.execute_restore_invocation(
                &invocation,
                remaining_timeout,
                destination.clone(),
                cancellation,
                &mut on_progress,
            )
        })();
        finish_restore_destination(&parent_lock, &destination_lock, outcome, destination)
    }

    pub fn repository_operation(
        &self,
        config: &EffectiveConfig,
        operation: ResticOperation,
        cancellation: &CancellationToken,
    ) -> RepositoryOutcome {
        let invocation = match ResticCommandBuilder::new(self.executable.as_ref())
            .repository_setup(config, operation)
        {
            Ok(invocation) => invocation,
            Err(_) => return RepositoryOutcome::failed("invalid_repository_configuration"),
        };

        self.execute_repository_invocation(&invocation, REPOSITORY_OPERATION_TIMEOUT, cancellation)
    }

    pub fn retention(
        &self,
        config: &EffectiveConfig,
        prune_due: bool,
        cancellation: &CancellationToken,
    ) -> RetentionOutcome {
        let timeout = Duration::from_secs(config.schedule.wake_lock_timeout_seconds);
        let builder = ResticCommandBuilder::new(self.executable.as_ref());
        let forget = match builder.retention(config, ResticOperation::Forget) {
            Ok(invocation) => invocation,
            Err(_) => return RetentionOutcome::failed("retention_forbidden"),
        };
        match self
            .execute_repository_invocation(&forget, timeout, cancellation)
            .kind
        {
            RepositoryOutcomeKind::Succeeded => {}
            RepositoryOutcomeKind::Cancelled => return RetentionOutcome::cancelled(),
            RepositoryOutcomeKind::Failed { code } => {
                return RetentionOutcome::failed(if code == "repository_operation_failed" {
                    "retention_forget_failed"
                } else {
                    &code
                });
            }
        }
        if !prune_due {
            return RetentionOutcome::succeeded(false);
        }

        let prune = match builder.retention(config, ResticOperation::Prune) {
            Ok(invocation) => invocation,
            Err(_) => return RetentionOutcome::failed("retention_forbidden"),
        };
        match self
            .execute_repository_invocation(&prune, timeout, cancellation)
            .kind
        {
            RepositoryOutcomeKind::Succeeded => RetentionOutcome::succeeded(true),
            RepositoryOutcomeKind::Cancelled => RetentionOutcome::cancelled(),
            RepositoryOutcomeKind::Failed { code } => {
                RetentionOutcome::failed(if code == "repository_operation_failed" {
                    "retention_prune_failed"
                } else {
                    &code
                })
            }
        }
    }

    fn execute_repository_invocation(
        &self,
        invocation: &ResticInvocation,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> RepositoryOutcome {
        let secret_environment = match self.resolve_secrets(&invocation.secret_environment) {
            Ok(environment) => environment,
            Err(_) => return RepositoryOutcome::failed("credential_unavailable"),
        };
        let _wake_lock = match self.wake_locks.acquire(timeout) {
            Ok(lock) => lock,
            Err(_) => return RepositoryOutcome::failed("wake_lock_unavailable"),
        };
        if cancellation.is_cancelled() {
            return RepositoryOutcome::cancelled();
        }

        let job = match KillOnDropJob::new() {
            Ok(job) => job,
            Err(_) => return RepositoryOutcome::failed("process_isolation_failed"),
        };
        let mut command = command_for(
            invocation,
            &secret_environment,
            self.cache_directory.as_ref(),
        );
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => return RepositoryOutcome::failed("restic_start_failed"),
        };
        drop(command);
        drop(secret_environment);
        if job.assign(&child).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return RepositoryOutcome::failed("process_isolation_failed");
        }

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => return RepositoryOutcome::failed("restic_output_unavailable"),
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => return RepositoryOutcome::failed("restic_output_unavailable"),
        };
        let stdout_thread = thread::spawn(move || drain(stdout));
        let stderr_thread = thread::spawn(move || read_bounded(stderr, MAX_STDERR_BYTES));
        let mut job = Some(job);
        let started = Instant::now();
        let mut cancelled = false;
        let mut timed_out = false;
        let mut termination_failed = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    if cancellation.is_cancelled() && !cancelled {
                        cancelled = true;
                        termination_failed = !terminate_process_tree(
                            job.take().expect("a running child retains its job"),
                            &mut child,
                        );
                        break Err(());
                    } else if started.elapsed() >= timeout && !timed_out {
                        timed_out = true;
                        termination_failed = !terminate_process_tree(
                            job.take().expect("a running child retains its job"),
                            &mut child,
                        );
                        break Err(());
                    }
                    thread::sleep(PROCESS_POLL_INTERVAL);
                }
                Err(_) => {
                    termination_failed = !terminate_process_tree(
                        job.take().expect("a running child retains its job"),
                        &mut child,
                    );
                    break Err(());
                }
            }
        };
        if termination_failed {
            drop(stdout_thread);
            drop(stderr_thread);
            return RepositoryOutcome::failed("restic_termination_failed");
        }
        let _ = stdout_thread.join();
        let stderr = stderr_thread.join().unwrap_or_default();

        if cancelled {
            return RepositoryOutcome::cancelled();
        }
        if timed_out {
            return RepositoryOutcome::failed("repository_operation_timed_out");
        }
        let status = match status {
            Ok(status) => status,
            Err(()) => return RepositoryOutcome::failed("restic_wait_failed"),
        };
        if status.success() {
            return RepositoryOutcome::succeeded();
        }

        let classified = classify_stderr(&stderr);
        let code = match (invocation.operation, status.code()) {
            (ResticOperation::Probe, Some(10)) => "repository_not_found",
            (_, _) if classified.is_some() => classified.expect("classification was checked"),
            (ResticOperation::Probe, _) => "repository_validation_failed",
            (ResticOperation::Initialize, _) => "repository_initialization_failed",
            _ => "repository_operation_failed",
        };
        RepositoryOutcome::failed(code)
    }

    fn execute_capture_invocation(
        &self,
        invocation: &ResticInvocation,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<Vec<u8>, RestoreError> {
        let secrets = self
            .resolve_secrets(&invocation.secret_environment)
            .map_err(|_| RestoreError::new("credential_unavailable"))?;
        let _wake_lock = self
            .wake_locks
            .acquire(timeout)
            .map_err(|_| RestoreError::new("wake_lock_unavailable"))?;
        if cancellation.is_cancelled() {
            return Err(RestoreError::new("restore_cancelled"));
        }
        let job =
            KillOnDropJob::new().map_err(|_| RestoreError::new("process_isolation_failed"))?;
        let mut command = command_for(invocation, &secrets, self.cache_directory.as_ref());
        let mut child = command
            .spawn()
            .map_err(|_| RestoreError::new("restic_start_failed"))?;
        drop(command);
        drop(secrets);
        if job.assign(&child).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RestoreError::new("process_isolation_failed"));
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RestoreError::new("restic_output_unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| RestoreError::new("restic_output_unavailable"))?;
        let stdout_thread =
            thread::spawn(move || read_bounded_with_overflow(stdout, MAX_RESTORE_OUTPUT_BYTES));
        let stderr_thread = thread::spawn(move || read_bounded(stderr, MAX_STDERR_BYTES));
        let mut job = Some(job);
        let started = Instant::now();
        let mut cancelled = false;
        let mut timed_out = false;
        let mut termination_failed = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    if cancellation.is_cancelled() || started.elapsed() >= timeout {
                        cancelled = cancellation.is_cancelled();
                        timed_out = !cancelled;
                        termination_failed = !terminate_process_tree(
                            job.take().expect("a running child retains its job"),
                            &mut child,
                        );
                        break Err(());
                    }
                    thread::sleep(PROCESS_POLL_INTERVAL);
                }
                Err(_) => {
                    termination_failed = !terminate_process_tree(
                        job.take().expect("a running child retains its job"),
                        &mut child,
                    );
                    break Err(());
                }
            }
        };
        if termination_failed {
            drop(stdout_thread);
            drop(stderr_thread);
            return Err(RestoreError::new("restic_termination_failed"));
        }
        let (stdout, overflow) = stdout_thread.join().unwrap_or_else(|_| (Vec::new(), true));
        let stderr = stderr_thread.join().unwrap_or_default();
        if cancelled {
            return Err(RestoreError::new("restore_cancelled"));
        }
        if timed_out {
            return Err(RestoreError::new("restore_query_timed_out"));
        }
        let status = status.map_err(|()| RestoreError::new("restic_wait_failed"))?;
        if !status.success() {
            return Err(RestoreError::new(
                classify_stderr(&stderr).unwrap_or("restore_repository_query_failed"),
            ));
        }
        if overflow {
            return Err(RestoreError::new("restore_output_limit_exceeded"));
        }
        Ok(stdout)
    }

    /// Parses recursive preflight output as it arrives rather than retaining a
    /// directory tree in memory. This keeps ordinary large Documents restores
    /// practical while still bounding work and rejecting every unsafe node.
    fn execute_restore_subtree_preflight(
        &self,
        invocation: &ResticInvocation,
        timeout: Duration,
        cancellation: &CancellationToken,
        selected: &str,
    ) -> Result<(), RestoreError> {
        let secrets = self
            .resolve_secrets(&invocation.secret_environment)
            .map_err(|_| RestoreError::new("credential_unavailable"))?;
        let _wake_lock = self
            .wake_locks
            .acquire(timeout)
            .map_err(|_| RestoreError::new("wake_lock_unavailable"))?;
        if cancellation.is_cancelled() {
            return Err(RestoreError::new("restore_cancelled"));
        }
        let job =
            KillOnDropJob::new().map_err(|_| RestoreError::new("process_isolation_failed"))?;
        let mut command = command_for(invocation, &secrets, self.cache_directory.as_ref());
        let mut child = command
            .spawn()
            .map_err(|_| RestoreError::new("restic_start_failed"))?;
        drop(command);
        drop(secrets);
        if job.assign(&child).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(RestoreError::new("process_isolation_failed"));
        }
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| RestoreError::new("restic_output_unavailable"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| RestoreError::new("restic_output_unavailable"))?;
        let selected = selected.to_owned();
        let stdout_thread = thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let result = validate_restore_subtree_reader(&mut reader, &selected);
            if result.is_err() {
                // Keep draining until restic exits (or cancellation kills it)
                // so rejecting one bad node cannot deadlock a full stdout pipe.
                drain(reader);
            }
            result
        });
        let stderr_thread = thread::spawn(move || read_bounded(stderr, MAX_STDERR_BYTES));
        let mut job = Some(job);
        let started = Instant::now();
        let mut cancelled = false;
        let mut timed_out = false;
        let mut termination_failed = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    if cancellation.is_cancelled() || started.elapsed() >= timeout {
                        cancelled = cancellation.is_cancelled();
                        timed_out = !cancelled;
                        termination_failed = !terminate_process_tree(
                            job.take().expect("a running child retains its job"),
                            &mut child,
                        );
                        break Err(());
                    }
                    thread::sleep(PROCESS_POLL_INTERVAL);
                }
                Err(_) => {
                    termination_failed = !terminate_process_tree(
                        job.take().expect("a running child retains its job"),
                        &mut child,
                    );
                    break Err(());
                }
            }
        };
        if termination_failed {
            drop(stdout_thread);
            drop(stderr_thread);
            return Err(RestoreError::new("restic_termination_failed"));
        }
        let parsed = stdout_thread
            .join()
            .unwrap_or(Err(RestoreError::new("restore_subtree_preflight_failed")));
        let stderr = stderr_thread.join().unwrap_or_default();
        if cancelled {
            return Err(RestoreError::new("restore_cancelled"));
        }
        if timed_out {
            return Err(RestoreError::new("restore_query_timed_out"));
        }
        let status = status.map_err(|()| RestoreError::new("restic_wait_failed"))?;
        if !status.success() {
            return Err(RestoreError::new(
                classify_stderr(&stderr).unwrap_or("restore_subtree_preflight_failed"),
            ));
        }
        parsed
    }

    fn execute_restore_invocation(
        &self,
        invocation: &ResticInvocation,
        timeout: Duration,
        destination: Option<PathBuf>,
        cancellation: &CancellationToken,
        mut on_progress: impl FnMut(RestoreProgress),
    ) -> RestoreOutcome {
        let secrets = match self.resolve_secrets(&invocation.secret_environment) {
            Ok(secrets) => secrets,
            Err(_) => return RestoreOutcome::failed_at("credential_unavailable", destination),
        };
        let _wake_lock = match self.wake_locks.acquire(timeout) {
            Ok(lock) => lock,
            Err(_) => return RestoreOutcome::failed_at("wake_lock_unavailable", destination),
        };
        if cancellation.is_cancelled() {
            return RestoreOutcome::cancelled(destination);
        }
        let job = match KillOnDropJob::new() {
            Ok(job) => job,
            Err(_) => return RestoreOutcome::failed_at("process_isolation_failed", destination),
        };
        let mut command = command_for(invocation, &secrets, self.cache_directory.as_ref());
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => return RestoreOutcome::failed_at("restic_start_failed", destination),
        };
        drop(command);
        drop(secrets);
        if job.assign(&child).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return RestoreOutcome::failed_at("process_isolation_failed", destination);
        }
        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => return RestoreOutcome::failed_at("restic_output_unavailable", destination),
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => return RestoreOutcome::failed_at("restic_output_unavailable", destination),
        };
        let (progress_tx, progress_rx) = mpsc::sync_channel(MAX_PENDING_PROGRESS_EVENTS);
        let stdout_thread = thread::spawn(move || read_restore_output(stdout, &progress_tx));
        let stderr_thread = thread::spawn(move || read_bounded(stderr, MAX_STDERR_BYTES));
        let mut job = Some(job);
        let started = Instant::now();
        let mut cancelled = false;
        let mut timed_out = false;
        let mut termination_failed = false;
        let status = loop {
            for mut progress in progress_rx.try_iter() {
                progress.destination = destination.clone();
                on_progress(progress);
            }
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    if cancellation.is_cancelled() || started.elapsed() >= timeout {
                        cancelled = cancellation.is_cancelled();
                        timed_out = !cancelled;
                        termination_failed = !terminate_process_tree(
                            job.take().expect("a running child retains its job"),
                            &mut child,
                        );
                        break Err(());
                    }
                    thread::sleep(PROCESS_POLL_INTERVAL);
                }
                Err(_) => {
                    termination_failed = !terminate_process_tree(
                        job.take().expect("a running child retains its job"),
                        &mut child,
                    );
                    break Err(());
                }
            }
        };
        if termination_failed {
            drop(stdout_thread);
            drop(stderr_thread);
            // The outer restore stack owns deny-DELETE locks for the selected
            // parent and its SYSTEM-only destination. Never return while the
            // privileged child may still reopen paths there: doing so would
            // release those handles, permit FILE_DELETE_CHILD replacement, and
            // let a local attacker redirect subsequent LocalSystem writes.
            // Catastrophic OS termination failures deliberately quarantine this
            // worker (and all repository/update operations) until exit is
            // positively confirmed.
            quarantine_restore_child_until_exit(&mut child);
            return RestoreOutcome::failed_at("restic_termination_failed", destination);
        }
        let parsed = stdout_thread
            .join()
            .unwrap_or(Err(RestoreError::new("restore_output_invalid")));
        let stderr = stderr_thread.join().unwrap_or_default();
        for mut progress in progress_rx.try_iter() {
            progress.destination = destination.clone();
            on_progress(progress);
        }
        if cancelled {
            return RestoreOutcome::cancelled(destination);
        }
        if timed_out {
            return RestoreOutcome::failed_at("restore_timed_out", destination);
        }
        let status = match status {
            Ok(status) => status,
            Err(()) => return RestoreOutcome::failed_at("restic_wait_failed", destination),
        };
        if !status.success() {
            return RestoreOutcome::failed_at(
                classify_stderr(&stderr).unwrap_or("restore_failed"),
                destination,
            );
        }
        let summary = match parsed {
            Ok(summary) => summary,
            Err(error) => return RestoreOutcome::failed_at(error.code, destination),
        };
        RestoreOutcome {
            kind: RestoreOutcomeKind::Succeeded,
            destination,
            files_restored: summary.files_restored.unwrap_or_default(),
            bytes_restored: summary.bytes_restored.unwrap_or_default(),
        }
    }

    fn execute_invocation(
        &self,
        invocation: &ResticInvocation,
        wake_lock_timeout: Duration,
        cancellation: &CancellationToken,
        known_vss_fallback: bool,
        mut on_progress: impl FnMut(BackupProgress),
    ) -> BackupOutcome {
        let secret_environment = match self.resolve_secrets(&invocation.secret_environment) {
            Ok(environment) => environment,
            Err(_) => return BackupOutcome::failed("credential_unavailable"),
        };
        let _wake_lock = match self.wake_locks.acquire(wake_lock_timeout) {
            Ok(lock) => lock,
            Err(_) => return BackupOutcome::failed("wake_lock_unavailable"),
        };
        if cancellation.is_cancelled() {
            return BackupOutcome::cancelled();
        }

        let job = match KillOnDropJob::new() {
            Ok(job) => job,
            Err(_) => return BackupOutcome::failed("process_isolation_failed"),
        };
        let mut command = command_for(
            invocation,
            &secret_environment,
            self.cache_directory.as_ref(),
        );
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(_) => return BackupOutcome::failed("restic_start_failed"),
        };
        drop(command);
        drop(secret_environment);
        if job.assign(&child).is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return BackupOutcome::failed("process_isolation_failed");
        }

        let stdout = match child.stdout.take() {
            Some(stdout) => stdout,
            None => return BackupOutcome::failed("restic_output_unavailable"),
        };
        let stderr = match child.stderr.take() {
            Some(stderr) => stderr,
            None => return BackupOutcome::failed("restic_output_unavailable"),
        };
        let (output_tx, output_rx) = mpsc::sync_channel(MAX_PENDING_PROGRESS_EVENTS);
        let stdout_progress = output_tx.clone();
        let stdout_thread = thread::spawn(move || read_json_output(stdout, &stdout_progress));
        let stderr_thread = thread::spawn(move || read_stderr_output(stderr, &output_tx));

        let mut job = Some(job);
        let mut cancelled = false;
        let mut termination_failed = false;
        let status = loop {
            collect_progress_events(&output_rx, &mut on_progress);

            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    if cancellation.is_cancelled() && !cancelled {
                        cancelled = true;
                        termination_failed = !terminate_process_tree(
                            job.take().expect("a running child retains its job"),
                            &mut child,
                        );
                        break Err(());
                    }
                    thread::sleep(PROCESS_POLL_INTERVAL);
                }
                Err(_) => {
                    termination_failed = !terminate_process_tree(
                        job.take().expect("a running child retains its job"),
                        &mut child,
                    );
                    break Err(());
                }
            }
        };

        if termination_failed {
            drop(stdout_thread);
            drop(stderr_thread);
            return BackupOutcome::failed("restic_termination_failed");
        }
        let output_result = stdout_thread.join().unwrap_or(Err(OutputReadError));
        let (stderr_output, stderr) = stderr_thread
            .join()
            .unwrap_or_else(|_| (Err(OutputReadError), Vec::new()));
        collect_progress_events(&output_rx, &mut on_progress);
        if cancelled {
            return BackupOutcome::cancelled();
        }
        let status = match status {
            Ok(status) => status,
            Err(()) => return BackupOutcome::failed("restic_wait_failed"),
        };
        finish_outcome(
            status,
            output_result,
            stderr_output,
            &stderr,
            known_vss_fallback,
        )
    }

    fn resolve_secrets(
        &self,
        references: &BTreeMap<SecretEnvironmentVariable, String>,
    ) -> Result<Vec<(SecretEnvironmentVariable, Zeroizing<String>)>, SecretResolveError> {
        references
            .iter()
            .map(|(variable, reference)| {
                self.secrets
                    .resolve(reference)
                    .map(|secret| (*variable, secret))
            })
            .collect()
    }
}

/// Constructs the complete preflight-and-backup plan before starting either
/// process. In particular, an invalid backup source configuration must not
/// perform even the narrow lock cleanup mutation.
fn build_backup_invocations(
    executable: &std::ffi::OsStr,
    data_directory: &Path,
    config: &EffectiveConfig,
) -> Result<(ResticInvocation, ResticInvocation), InvocationError> {
    let builder = ResticCommandBuilder::new(executable);
    validate_backup_source_paths(&config.backup.paths, Some(data_directory))?;

    let mut required_exclusions = vec![data_directory.to_path_buf()];
    if let Ok(canonical_data_directory) = std::fs::canonicalize(data_directory) {
        let canonical_data_directory = ordinary_windows_path(&canonical_data_directory);
        // Match the namespace used for resolved local sources. Keeping the
        // original spelling as well protects broad non-existent DOS sources
        // when they later become available under their configured path.
        if !required_exclusions
            .iter()
            .any(|exclusion| exclusion == &canonical_data_directory)
        {
            required_exclusions.push(canonical_data_directory);
        }
    }
    let backup = builder.backup_with_required_exclusions(config, &required_exclusions)?;
    let unlock = builder.unlock(config)?;
    Ok((unlock, backup))
}

fn restore_invocation_error_code(error: &InvocationError) -> &'static str {
    match error {
        InvocationError::InvalidRestoreSnapshotId => "restore_snapshot_invalid",
        InvocationError::InvalidRestoreSnapshotPath => "restore_path_invalid",
        InvocationError::InvalidRestoreDestination => "restore_destination_invalid",
        InvocationError::ForbiddenByRepositoryMode { .. } => "restore_forbidden",
        _ => "invalid_repository_configuration",
    }
}

fn validate_restore_destination(
    destination: &Path,
    protected_data: &Path,
    executable: &OsStr,
) -> Result<PathBuf, RestoreError> {
    if !destination.is_absolute()
        || destination.as_os_str().encode_wide().count()
            > resticpal_core::config::MAX_PATH_CHARACTERS
        || destination
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
        || !matches!(
            destination.components().next(),
            Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
        )
    {
        return Err(RestoreError::new("restore_destination_invalid"));
    }
    if source_is_network_path(destination) {
        return Err(RestoreError::new("restore_destination_network_unsupported"));
    }
    let metadata = fs::symlink_metadata(destination)
        .map_err(|_| RestoreError::new("restore_destination_unavailable"))?;
    if !metadata.is_dir() {
        return Err(RestoreError::new("restore_destination_not_directory"));
    }
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(RestoreError::new("restore_destination_alias_unsupported"));
    }
    let canonical = ordinary_windows_path(
        &fs::canonicalize(destination)
            .map_err(|_| RestoreError::new("restore_destination_unavailable"))?,
    );
    let equivalent = windows_path_is_same_or_descendant(destination, &canonical)
        && windows_path_is_same_or_descendant(&canonical, destination);
    if !equivalent {
        return Err(RestoreError::new("restore_destination_alias_unsupported"));
    }

    let mut protected = vec![protected_data.to_path_buf()];
    if let Ok(canonical_data) = fs::canonicalize(protected_data) {
        protected.push(ordinary_windows_path(&canonical_data));
    }
    let executable = Path::new(executable);
    if let Ok(executable) = fs::canonicalize(executable)
        && let Some(parent) = ordinary_windows_path(&executable).parent()
    {
        protected.push(parent.to_path_buf());
    }
    if let Some(windows) = std::env::var_os("SystemRoot") {
        protected.push(PathBuf::from(windows));
    }
    if protected.iter().any(|protected| {
        windows_path_is_same_or_descendant(destination, protected)
            || windows_path_is_same_or_descendant(&canonical, protected)
    }) {
        return Err(RestoreError::new("restore_destination_protected"));
    }
    Ok(canonical)
}

struct RestoreSecurityDescriptor(PSECURITY_DESCRIPTOR);

impl RestoreSecurityDescriptor {
    fn protected_directory() -> Result<Self, RestoreError> {
        Self::from_sddl(RESTORE_DIRECTORY_SDDL)
    }

    fn from_sddl(sddl: PCWSTR) -> Result<Self, RestoreError> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the static SDDL is null-terminated and conversion returns a
        // uniquely owned LocalAlloc descriptor.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl,
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
        }
        .map_err(|_| RestoreError::new("restore_destination_security_failed"))?;
        Ok(Self(descriptor))
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
                .expect("SECURITY_ATTRIBUTES size fits in u32"),
            lpSecurityDescriptor: self.0.0,
            bInheritHandle: false.into(),
        }
    }
}

impl Drop for RestoreSecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: both SDDL conversion and GetSecurityInfo return one
            // descriptor allocated by LocalAlloc.
            let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }
}

fn create_restore_destination(parent: &Path) -> Result<PathBuf, RestoreError> {
    let security = RestoreSecurityDescriptor::protected_directory()?;
    let attributes = security.attributes();
    let label = format!(
        "ResticPal Restore - {}",
        Utc::now().format("%Y-%m-%d %H%M%S")
    );
    for attempt in 0..1_000_u16 {
        let name = if attempt == 0 {
            label.clone()
        } else {
            format!("{label} ({attempt})")
        };
        let candidate = parent.join(name);
        let candidate_wide: Vec<u16> = candidate
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SECURITY_ATTRIBUTES installs the protected SYSTEM/Administrators
        // DACL atomically; an unprivileged parent owner never gets a window to
        // plant a junction inside the directory before LocalSystem extraction.
        match unsafe {
            CreateDirectoryW(PCWSTR(candidate_wide.as_ptr()), Some(&raw const attributes))
        } {
            Ok(()) => {
                let canonical = ordinary_windows_path(
                    &fs::canonicalize(&candidate)
                        .map_err(|_| RestoreError::new("restore_destination_unavailable"))?,
                );
                if !windows_path_is_same_or_descendant(&canonical, parent)
                    || !windows_path_is_same_or_descendant(&candidate, &canonical)
                    || !windows_path_is_same_or_descendant(&canonical, &candidate)
                {
                    return Err(RestoreError::new("restore_destination_alias_unsupported"));
                }
                return Ok(canonical);
            }
            Err(error) if error.code() == HRESULT::from_win32(ERROR_ALREADY_EXISTS.0) => {}
            Err(_) => return Err(RestoreError::new("restore_destination_creation_failed")),
        }
    }
    Err(RestoreError::new("restore_destination_creation_failed"))
}

/// Deny DELETE sharing while restic runs so a user-writable destination cannot
/// be exchanged for a junction between validation and LocalSystem extraction.
fn lock_restore_directory(path: &Path) -> Result<fs::File, RestoreError> {
    open_locked_restore_directory(path, false)
}

fn lock_restore_directory_for_handoff(path: &Path) -> Result<fs::File, RestoreError> {
    open_locked_restore_directory(path, true)
}

fn open_locked_restore_directory(
    path: &Path,
    permit_security_handoff: bool,
) -> Result<fs::File, RestoreError> {
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    const FILE_SHARE_READ_WRITE: u32 = 0x0000_0003;
    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .share_mode(FILE_SHARE_READ_WRITE);
    if permit_security_handoff {
        options.access_mode(FILE_GENERIC_READ.0 | WRITE_DAC.0);
    }
    let handle = options
        .open(path)
        .map_err(|_| RestoreError::new("restore_destination_unavailable"))?;
    let metadata = handle
        .metadata()
        .map_err(|_| RestoreError::new("restore_destination_unavailable"))?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(RestoreError::new("restore_destination_alias_unsupported"));
    }
    Ok(handle)
}

fn directory_security(
    directory: &fs::File,
) -> Result<(RestoreSecurityDescriptor, *mut ACL), RestoreError> {
    let mut owner = PSID::default();
    let mut dacl = std::ptr::null_mut();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the handle owns READ_CONTROL, output pointers are writable, and
    // Windows allocates one descriptor containing the returned owner and DACL.
    unsafe {
        GetSecurityInfo(
            HANDLE(directory.as_raw_handle()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | OWNER_SECURITY_INFORMATION,
            Some(&raw mut owner),
            None,
            Some(&raw mut dacl),
            None,
            Some(&raw mut descriptor),
        )
    }
    .ok()
    .map_err(|_| RestoreError::new("restore_destination_security_failed"))?;
    let descriptor = RestoreSecurityDescriptor(descriptor);
    if dacl.is_null() || owner.is_invalid() {
        return Err(RestoreError::new("restore_destination_security_failed"));
    }
    Ok((descriptor, dacl))
}

fn restore_descriptor_owner(descriptor: &RestoreSecurityDescriptor) -> Result<PSID, RestoreError> {
    let mut owner = PSID::default();
    let mut defaulted = BOOL::default();
    // SAFETY: the descriptor remains live and both owner outputs are writable.
    unsafe { GetSecurityDescriptorOwner(descriptor.0, &raw mut owner, &raw mut defaulted) }
        .map_err(|_| RestoreError::new("restore_destination_security_failed"))?;
    if owner.is_invalid() {
        return Err(RestoreError::new("restore_destination_security_failed"));
    }
    Ok(owner)
}

fn restore_descriptor_dacl(
    descriptor: &RestoreSecurityDescriptor,
) -> Result<*mut ACL, RestoreError> {
    let mut present = BOOL::default();
    let mut defaulted = BOOL::default();
    let mut dacl = std::ptr::null_mut();
    // SAFETY: the descriptor remains live and every DACL output is writable.
    unsafe {
        GetSecurityDescriptorDacl(
            descriptor.0,
            &raw mut present,
            &raw mut dacl,
            &raw mut defaulted,
        )
    }
    .map_err(|_| RestoreError::new("restore_destination_security_failed"))?;
    if !present.as_bool() || dacl.is_null() {
        return Err(RestoreError::new("restore_destination_security_failed"));
    }
    Ok(dacl)
}

fn same_restore_acl(expected: *const ACL, actual: *const ACL) -> bool {
    if expected.is_null() || actual.is_null() {
        return false;
    }
    // SAFETY: each ACL points inside a live descriptor validated by Windows;
    // AclSize is the readable extent of that descriptor-owned allocation.
    let expected_size = usize::from(unsafe { (*expected).AclSize });
    let actual_size = usize::from(unsafe { (*actual).AclSize });
    expected_size == actual_size
        && unsafe {
            std::slice::from_raw_parts(expected.cast::<u8>(), expected_size)
                == std::slice::from_raw_parts(actual.cast::<u8>(), actual_size)
        }
}

fn directory_security_is_protected(
    descriptor: &RestoreSecurityDescriptor,
) -> Result<bool, RestoreError> {
    let mut control = 0_u16;
    let mut revision = 0_u32;
    // SAFETY: the descriptor is live and output pointers are writable.
    unsafe { GetSecurityDescriptorControl(descriptor.0, &raw mut control, &raw mut revision) }
        .map_err(|_| RestoreError::new("restore_destination_security_failed"))?;
    Ok(control & SE_DACL_PROTECTED.0 != 0)
}

fn verify_restore_security_descriptor(
    actual: &RestoreSecurityDescriptor,
    expected: &RestoreSecurityDescriptor,
    expected_owner: PSID,
) -> Result<(), RestoreError> {
    if !directory_security_is_protected(actual)? {
        return Err(RestoreError::new("restore_destination_security_failed"));
    }
    let actual_owner = restore_descriptor_owner(actual)?;
    // SAFETY: both SIDs live inside their still-owned security descriptors.
    if expected_owner.is_invalid() || unsafe { EqualSid(actual_owner, expected_owner) }.is_err() {
        return Err(RestoreError::new("restore_destination_security_failed"));
    }
    if !same_restore_acl(
        restore_descriptor_dacl(expected)?.cast_const(),
        restore_descriptor_dacl(actual)?.cast_const(),
    ) {
        return Err(RestoreError::new("restore_destination_security_failed"));
    }
    Ok(())
}

fn verify_protected_restore_directory(
    parent: &fs::File,
    destination: &fs::File,
) -> Result<(), RestoreError> {
    let expected = RestoreSecurityDescriptor::protected_directory()?;
    let (_parent_descriptor, _) = directory_security(parent)?;
    let (actual, _) = directory_security(destination)?;
    #[cfg(not(test))]
    let expected_owner = restore_descriptor_owner(&expected)?;
    #[cfg(test)]
    let expected_owner = restore_descriptor_owner(&_parent_descriptor)?;
    verify_restore_security_descriptor(&actual, &expected, expected_owner)
}

/// Expose completed or partial recovery output only after restic and all
/// LocalSystem writes have stopped. Windows propagates the parent DACL's
/// inheritable ACEs to already-created files and nested directories.
fn handoff_restore_directory(
    parent: &fs::File,
    destination: &fs::File,
) -> Result<(), RestoreError> {
    let (_parent_descriptor, _) = directory_security(parent)?;
    let (_trusted_descriptor, trusted_dacl) = directory_security(destination)?;
    // Preserve the destination's authenticated SYSTEM/Administrators ACEs and
    // let Windows add only legitimately inheritable parent ACEs. Copying the
    // whole parent DACL would incorrectly expose parent-only permissions; a
    // null DACL would grant full access to everyone.
    // SAFETY: the destination handle owns WRITE_DAC, its non-null trusted DACL
    // remains live through the descriptor, and both directories cannot move.
    unsafe {
        SetSecurityInfo(
            HANDLE(destination.as_raw_handle()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(trusted_dacl.cast_const()),
            None,
        )
    }
    .ok()
    .map_err(|_| RestoreError::new("restore_destination_security_failed"))?;
    let (descriptor, _) = directory_security(destination)?;
    if directory_security_is_protected(&descriptor)? {
        return Err(RestoreError::new("restore_destination_security_failed"));
    }
    Ok(())
}

fn finish_restore_destination(
    parent: &fs::File,
    destination_lock: &fs::File,
    outcome: RestoreOutcome,
    destination: Option<PathBuf>,
) -> RestoreOutcome {
    if matches!(
        &outcome.kind,
        RestoreOutcomeKind::Failed { code } if code == "restic_termination_failed"
    ) {
        // A termination fault has already kept the worker quarantined until
        // exit was proven. Leave its partial output protected and undisclosed
        // anyway; administrators can inspect or remove it deliberately.
        return RestoreOutcome::failed("restic_termination_failed");
    }
    match handoff_restore_directory(parent, destination_lock) {
        Ok(()) => outcome,
        Err(error) => RestoreOutcome::failed_at(error.code, destination),
    }
}

fn quarantine_restore_child_until_exit(child: &mut Child) {
    loop {
        let _ = child.kill();
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) | Err(_) => thread::sleep(PROCESS_POLL_INTERVAL),
        }
    }
}

fn ensure_locked_restore_directory(path: &Path) -> Result<(), RestoreError> {
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0400;
    let metadata = fs::symlink_metadata(path)
        .map_err(|_| RestoreError::new("restore_destination_unavailable"))?;
    if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(RestoreError::new("restore_destination_alias_unsupported"));
    }
    let canonical = ordinary_windows_path(
        &fs::canonicalize(path)
            .map_err(|_| RestoreError::new("restore_destination_unavailable"))?,
    );
    if !windows_path_is_same_or_descendant(path, &canonical)
        || !windows_path_is_same_or_descendant(&canonical, path)
    {
        return Err(RestoreError::new("restore_destination_alias_unsupported"));
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct WireRestoreSnapshot {
    id: String,
    time: DateTime<Utc>,
    hostname: String,
    #[serde(default)]
    paths: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct WireRestoreEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default, rename = "type")]
    node_type: Option<String>,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    mtime: Option<DateTime<Utc>>,
}

fn parse_restore_directory(
    output: &[u8],
    requested: &str,
) -> Result<Vec<RestoreEntryView>, RestoreError> {
    let mut reader = BufReader::new(output);
    let mut line = Vec::new();
    let mut entries = Vec::new();
    while let Some(valid) = read_bounded_line(&mut reader, &mut line)
        .map_err(|_| RestoreError::new("restore_directory_output_invalid"))?
    {
        if !valid {
            return Err(RestoreError::new("restore_directory_output_invalid"));
        }
        let wire: WireRestoreEntry = serde_json::from_slice(&line)
            .map_err(|_| RestoreError::new("restore_directory_output_invalid"))?;
        let (name, path, kind) = match (wire.name, wire.path, wire.node_type) {
            (None, None, None) => {
                // `restic ls --json` begins with a snapshot metadata object.
                continue;
            }
            (Some(name), Some(path), Some(kind)) => (name, path, kind),
            _ => return Err(RestoreError::new("restore_directory_output_invalid")),
        };
        if path == requested {
            continue;
        }
        let Some((parent, leaf)) = path.rsplit_once('/') else {
            return Err(RestoreError::new("restore_directory_output_invalid"));
        };
        let parent = if parent.is_empty() { "/" } else { parent };
        if parent != requested {
            // Never trust an accidental recursive result or an unrelated path.
            continue;
        }
        if path.len() > MAX_RESTORE_ITEM_PATH_BYTES
            || name.len() > MAX_RESTORE_ITEM_NAME_BYTES
            || name.is_empty()
            || name != leaf
            || name.contains('/')
            || name.contains('\\')
            || name.chars().any(char::is_control)
            || validate_restore_snapshot_path(&path).is_err()
        {
            return Err(RestoreError::new("restore_directory_output_invalid"));
        }
        let node_type = match kind.as_str() {
            "file" => RestoreNodeType::File,
            "dir" | "directory" => RestoreNodeType::Directory,
            // Symlinks, junction-like entries, sockets, and devices must never
            // be restored by the initial local-only file recovery surface.
            _ => continue,
        };
        if entries.len() >= MAX_RESTORE_DIRECTORY_ENTRIES {
            return Err(RestoreError::new("restore_directory_limit_exceeded"));
        }
        entries.push(RestoreEntryView {
            name,
            path,
            node_type,
            size: wire.size,
            modified_at: wire.mtime,
        });
    }
    Ok(entries)
}

#[cfg(test)]
fn validate_restore_subtree(output: &[u8], selected: &str) -> Result<(), RestoreError> {
    validate_restore_subtree_reader(&mut BufReader::new(output), selected)
}

fn validate_restore_subtree_reader(
    reader: &mut impl BufRead,
    selected: &str,
) -> Result<(), RestoreError> {
    let mut line = Vec::new();
    let mut found = false;
    let mut count = 0_usize;
    let mut bytes = 0_usize;
    let prefix = format!("{selected}/");
    while let Some(valid) = read_bounded_line(reader, &mut line)
        .map_err(|_| RestoreError::new("restore_subtree_preflight_failed"))?
    {
        bytes = bytes.saturating_add(line.len());
        if !valid || bytes > MAX_RESTORE_PREFLIGHT_BYTES {
            return Err(RestoreError::new("restore_output_limit_exceeded"));
        }
        let wire: WireRestoreEntry = serde_json::from_slice(&line)
            .map_err(|_| RestoreError::new("restore_subtree_preflight_failed"))?;
        let (name, path, kind) = match (wire.name, wire.path, wire.node_type) {
            (None, None, None) => continue,
            (Some(name), Some(path), Some(kind)) => (name, path, kind),
            _ => return Err(RestoreError::new("restore_subtree_preflight_failed")),
        };
        if path != selected && !path.starts_with(&prefix) {
            continue;
        }
        if path.len() > MAX_RESTORE_ITEM_PATH_BYTES
            || name.len() > MAX_RESTORE_ITEM_NAME_BYTES
            || name.is_empty()
            || name.contains('/')
            || name.contains('\\')
            || name.chars().any(char::is_control)
            || path.rsplit('/').next() != Some(name.as_str())
            || validate_restore_snapshot_path(&path).is_err()
        {
            return Err(RestoreError::new("restore_subtree_preflight_failed"));
        }
        if !matches!(kind.as_str(), "file" | "dir" | "directory") {
            return Err(RestoreError::new("restore_unsupported_node"));
        }
        count = count.saturating_add(1);
        if count > MAX_RESTORE_PREFLIGHT_NODES {
            return Err(RestoreError::new("restore_directory_limit_exceeded"));
        }
        if path == selected {
            found = true;
        }
    }
    if !found {
        return Err(RestoreError::new("restore_selected_node_unavailable"));
    }
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
struct WireRestoreProgress {
    #[serde(default)]
    message_type: String,
    #[serde(default)]
    files_restored: Option<u64>,
    #[serde(default)]
    files_done: Option<u64>,
    #[serde(default)]
    bytes_restored: Option<u64>,
    #[serde(default)]
    bytes_done: Option<u64>,
    #[serde(default)]
    total_files: Option<u64>,
    #[serde(default)]
    total_bytes: Option<u64>,
}

fn read_restore_output(
    stdout: impl Read,
    progress: &mpsc::SyncSender<RestoreProgress>,
) -> Result<RestoreProgress, RestoreError> {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut bytes = 0_usize;
    let mut last = RestoreProgress::default();
    while let Some(valid) = read_bounded_line(&mut reader, &mut line)
        .map_err(|_| RestoreError::new("restore_output_invalid"))?
    {
        bytes = bytes.saturating_add(line.len());
        if !valid || bytes > MAX_RESTORE_OUTPUT_BYTES {
            drain(reader);
            return Err(RestoreError::new("restore_output_limit_exceeded"));
        }
        let wire: WireRestoreProgress = serde_json::from_slice(&line)
            .map_err(|_| RestoreError::new("restore_output_invalid"))?;
        if !matches!(wire.message_type.as_str(), "status" | "summary") {
            continue;
        }
        let update = RestoreProgress {
            destination: None,
            files_restored: wire.files_restored.or(wire.files_done),
            bytes_restored: wire.bytes_restored.or(wire.bytes_done),
            total_files: wire.total_files,
            total_bytes: wire.total_bytes,
        };
        if update.files_restored.is_some() {
            last.files_restored = update.files_restored;
        }
        if update.bytes_restored.is_some() {
            last.bytes_restored = update.bytes_restored;
        }
        if update.total_files.is_some() {
            last.total_files = update.total_files;
        }
        if update.total_bytes.is_some() {
            last.total_bytes = update.total_bytes;
        }
        let _ = progress.try_send(last.clone());
    }
    Ok(last)
}

pub(crate) fn validate_backup_source_paths(
    sources: &[PathBuf],
    data_directory: Option<&Path>,
) -> Result<(), InvocationError> {
    let mut protected_paths = data_directory
        .into_iter()
        .map(Path::to_path_buf)
        .collect::<Vec<_>>();
    if let Some(data_directory) = data_directory
        && let Ok(canonical_data_directory) = std::fs::canonicalize(data_directory)
    {
        protected_paths.push(ordinary_windows_path(&canonical_data_directory));
    }

    for source in sources {
        if source_is_network_path(source) {
            // A UNC path can refer back to this machine through an administrative
            // share while retaining a namespace that does not match the
            // mandatory local-data exclusion. Mapped remote drives have the
            // same problem. Reject network source roots until the wrapper can
            // bind them to a stable filesystem identity without weakening its
            // internal-data boundary.
            return Err(InvocationError::UnsupportedNetworkBackupSource);
        }
        if source_uses_unsupported_local_namespace(source) {
            return Err(InvocationError::UnsupportedBackupSourceNamespace);
        }
        let canonical_source = std::fs::canonicalize(source)
            .ok()
            .map(|source| ordinary_windows_path(&source));
        if protected_paths.iter().any(|protected| {
            windows_path_is_same_or_descendant(source, protected)
                || canonical_source.as_ref().is_some_and(|canonical_source| {
                    windows_path_is_same_or_descendant(canonical_source, protected)
                })
        }) {
            // Report the protected-data violation even when the source reached
            // it through an alias. This is both the more specific diagnosis and
            // the security boundary the namespace check exists to defend.
            return Err(InvocationError::ProtectedBackupSource);
        }
        if let Some(canonical_source) = canonical_source {
            let equivalent = windows_path_is_same_or_descendant(source, &canonical_source)
                && windows_path_is_same_or_descendant(&canonical_source, source);
            if !equivalent {
                // Rewriting a junction, SUBST, short-name, or volume-GUID
                // source would also invalidate configured absolute exclude
                // patterns. Reject the alias instead of creating a path
                // namespace in which either user or mandatory exclusions
                // can be bypassed.
                return Err(InvocationError::UnsupportedBackupSourceNamespace);
            }
        }
    }
    Ok(())
}

fn backup_invocation_error_code(error: &InvocationError) -> &'static str {
    match error {
        InvocationError::UnsupportedNetworkBackupSource => "network_backup_source_unsupported",
        InvocationError::UnsupportedBackupSourceNamespace => "backup_source_namespace_unsupported",
        InvocationError::ProtectedBackupSource => "protected_backup_source",
        _ => "invalid_configuration",
    }
}

fn source_uses_unsupported_local_namespace(path: &Path) -> bool {
    matches!(
        path.components().next(),
        Some(Component::Prefix(prefix))
            if matches!(prefix.kind(), Prefix::DeviceNS(_))
                || matches!(prefix.kind(), Prefix::Verbatim(_))
    )
}

fn source_is_network_path(path: &Path) -> bool {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return false;
    };
    match prefix.kind() {
        Prefix::UNC(_, _) | Prefix::VerbatimUNC(_, _) => true,
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
            let root = OsString::from(format!("{}:\\", char::from(drive)));
            let wide = root
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            // SAFETY: wide is an immutable, terminated drive-root string for
            // the duration of this call.
            unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) == DRIVE_REMOTE }
        }
        Prefix::DeviceNS(_) | Prefix::Verbatim(_) => false,
    }
}

fn ordinary_windows_path(path: &Path) -> PathBuf {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return path.to_path_buf();
    };
    let mut ordinary = match prefix.kind() {
        Prefix::VerbatimDisk(drive) => PathBuf::from(format!("{}:\\", char::from(drive))),
        Prefix::VerbatimUNC(server, share) => {
            let mut unc = PathBuf::from(r"\\");
            unc.push(server);
            unc.push(share);
            unc
        }
        _ => return path.to_path_buf(),
    };
    for component in path.components().skip(1) {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::CurDir => {}
            Component::ParentDir => ordinary.push(".."),
            Component::Normal(value) => ordinary.push(value),
        }
    }
    ordinary
}

fn backup_outcome_for_lock_cleanup(cleanup: &RepositoryOutcome) -> Option<BackupOutcome> {
    match &cleanup.kind {
        RepositoryOutcomeKind::Succeeded => None,
        RepositoryOutcomeKind::Cancelled => Some(BackupOutcome::cancelled()),
        RepositoryOutcomeKind::Failed { code } => {
            let code = match code.as_str() {
                "repository_operation_timed_out" => "stale_lock_cleanup_timed_out",
                "repository_operation_failed" => "stale_lock_cleanup_failed",
                classified => classified,
            };
            Some(BackupOutcome::failed(code))
        }
    }
}

/// Matches Go's Windows `os.Hostname`, which restic records in each snapshot.
///
/// `COMPUTERNAME` contains the legacy NetBIOS name and is capped at 15
/// characters. On Windows Sandbox, domain hosts, and ordinary machines with a
/// longer DNS name, selecting that value would silently hide every real backup.
/// Never fall back to a different host identity: doing so could expose another
/// computer's snapshots from a shared repository.
fn current_restic_hostname() -> Result<String, RestoreError> {
    let mut buffer = vec![0_u16; 64];
    loop {
        let mut required = u32::try_from(buffer.len())
            .map_err(|_| RestoreError::new("restore_hostname_unavailable"))?;
        // SAFETY: `buffer` contains at least `required` UTF-16 elements and the
        // Win32 API initializes at most that many before reporting its length.
        let result = unsafe {
            GetComputerNameExW(
                ComputerNamePhysicalDnsHostname,
                Some(PWSTR(buffer.as_mut_ptr())),
                &mut required,
            )
        };
        match result {
            Ok(()) => {
                let length = usize::try_from(required)
                    .map_err(|_| RestoreError::new("restore_hostname_unavailable"))?;
                let value = buffer
                    .get(..length)
                    .ok_or_else(|| RestoreError::new("restore_hostname_unavailable"))?;
                return decode_restic_hostname(value);
            }
            Err(error) if error.code() == HRESULT::from_win32(ERROR_MORE_DATA.0) => {
                let required = usize::try_from(required)
                    .map_err(|_| RestoreError::new("restore_hostname_unavailable"))?;
                // Windows includes the trailing NUL in its required capacity.
                // Any UTF-16 value larger than this cannot meet the strict
                // UTF-8-byte bound imposed on exposed snapshot hostnames.
                if required <= buffer.len() || required > MAX_RESTORE_HOSTNAME_BYTES + 1 {
                    return Err(RestoreError::new("restore_hostname_unavailable"));
                }
                buffer.resize(required, 0);
            }
            Err(_) => return Err(RestoreError::new("restore_hostname_unavailable")),
        }
    }
}

fn decode_restic_hostname(value: &[u16]) -> Result<String, RestoreError> {
    let hostname =
        String::from_utf16(value).map_err(|_| RestoreError::new("restore_hostname_unavailable"))?;
    if hostname.is_empty()
        || hostname.len() > MAX_RESTORE_HOSTNAME_BYTES
        || hostname.chars().any(char::is_control)
    {
        return Err(RestoreError::new("restore_hostname_unavailable"));
    }
    Ok(hostname)
}

fn command_for(
    invocation: &ResticInvocation,
    secrets: &[(SecretEnvironmentVariable, Zeroizing<String>)],
    cache_directory: &Path,
) -> Command {
    let mut command = Command::new(&invocation.executable);
    command
        .args(&invocation.arguments)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW.0);

    for name in INHERITED_ENVIRONMENT {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command.envs(&invocation.environment);
    command.env("RESTIC_PROGRESS_FPS", "1");
    for (variable, value) in secrets {
        command.env(variable.as_str(), value.as_str());
    }
    // Pin restic's repository metadata cache to the protected machine-wide
    // service directory. This assignment deliberately follows every other
    // environment injection so no local or managed value can redirect
    // LocalSystem writes elsewhere.
    command.env("RESTIC_CACHE_DIR", cache_directory);
    command
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupSummary {
    pub files_processed: u64,
    pub bytes_processed: u64,
    pub data_added: u64,
    pub snapshot_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupOutcome {
    pub kind: BackupOutcomeKind,
    pub summary: Option<BackupSummary>,
    pub warning_code: Option<String>,
    pub failure_details: BackupFailureDetails,
}

impl BackupOutcome {
    pub(crate) fn succeeded(summary: BackupSummary) -> Self {
        Self {
            kind: BackupOutcomeKind::Succeeded,
            summary: Some(summary),
            warning_code: None,
            failure_details: BackupFailureDetails::default(),
        }
    }

    pub(crate) fn warnings(
        summary: BackupSummary,
        code: impl Into<String>,
        failure_details: BackupFailureDetails,
    ) -> Self {
        Self {
            kind: BackupOutcomeKind::SucceededWithWarnings,
            summary: Some(summary),
            warning_code: Some(code.into()),
            failure_details,
        }
    }

    pub(crate) fn failed(code: impl Into<String>) -> Self {
        Self {
            kind: BackupOutcomeKind::Failed { code: code.into() },
            summary: None,
            warning_code: None,
            failure_details: BackupFailureDetails::default(),
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self {
            kind: BackupOutcomeKind::Cancelled,
            summary: None,
            warning_code: None,
            failure_details: BackupFailureDetails::default(),
        }
    }

    pub(crate) fn with_warning(mut self, code: impl Into<String>) -> Self {
        if matches!(
            self.kind,
            BackupOutcomeKind::Succeeded | BackupOutcomeKind::SucceededWithWarnings
        ) {
            self.kind = BackupOutcomeKind::SucceededWithWarnings;
            // Backup-source and consistency warnings are the only warning
            // details stored with the run. Retention has its own durable state
            // and diagnostic event, so a later retention warning must not hide
            // a warning that explains incomplete or live-source backup data.
            if self.warning_code.is_none() {
                self.warning_code = Some(code.into());
            }
        }
        self
    }
}

/// Sensitive source names collected from restic's structured per-item errors.
/// The custom debug representation prevents an incidental outcome log from
/// copying paths into diagnostics or console output.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct BackupFailureDetails {
    items: Vec<String>,
    omitted: u64,
}

impl BackupFailureDetails {
    #[cfg(test)]
    pub(crate) fn from_items(items: Vec<String>, omitted: u64) -> Self {
        let mut details = Self {
            items: Vec::new(),
            omitted,
        };
        for item in items {
            details.push(item);
        }
        details
    }

    pub(crate) fn items(&self) -> &[String] {
        &self.items
    }

    pub(crate) const fn omitted(&self) -> u64 {
        self.omitted
    }

    fn push(&mut self, item: String) {
        if self.items.iter().any(|existing| existing == &item) {
            return;
        }
        if !is_safe_backup_failed_item(&item) || self.items.len() >= MAX_BACKUP_FAILED_ITEMS {
            self.omitted = self.omitted.saturating_add(1);
            return;
        }
        self.items.push(item);
    }

    fn merge(&mut self, other: Self) {
        for item in other.items {
            self.push(item);
        }
        self.omitted = self.omitted.saturating_add(other.omitted);
    }
}

impl std::fmt::Debug for BackupFailureDetails {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("BackupFailureDetails")
            .field("retained", &self.items.len())
            .field("omitted", &self.omitted)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupOutcomeKind {
    Succeeded,
    SucceededWithWarnings,
    Failed { code: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryOutcome {
    pub kind: RepositoryOutcomeKind,
}

impl RepositoryOutcome {
    fn succeeded() -> Self {
        Self {
            kind: RepositoryOutcomeKind::Succeeded,
        }
    }

    pub(crate) fn failed(code: &str) -> Self {
        Self {
            kind: RepositoryOutcomeKind::Failed {
                code: code.to_owned(),
            },
        }
    }

    fn cancelled() -> Self {
        Self {
            kind: RepositoryOutcomeKind::Cancelled,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryOutcomeKind {
    Succeeded,
    Failed { code: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetentionOutcome {
    pub kind: RetentionOutcomeKind,
}

impl RetentionOutcome {
    fn succeeded(pruned: bool) -> Self {
        Self {
            kind: RetentionOutcomeKind::Succeeded { pruned },
        }
    }

    fn failed(code: &str) -> Self {
        Self {
            kind: RetentionOutcomeKind::Failed {
                code: code.to_owned(),
            },
        }
    }

    fn cancelled() -> Self {
        Self {
            kind: RetentionOutcomeKind::Cancelled,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetentionOutcomeKind {
    Succeeded { pruned: bool },
    Failed { code: String },
    Cancelled,
}

fn finish_outcome(
    status: ExitStatus,
    output_result: Result<ParsedOutput, OutputReadError>,
    stderr_output_result: Result<ParsedOutput, OutputReadError>,
    stderr: &[u8],
    known_vss_fallback: bool,
) -> BackupOutcome {
    if status.code() == Some(130) {
        return BackupOutcome::cancelled();
    }
    let Ok(parsed) = output_result else {
        return BackupOutcome::failed("restic_output_invalid");
    };

    // A single oversized or non-JSON stdout line (e.g. a huge status update)
    // must not poison an otherwise-successful run. When restic exits cleanly and
    // produced a valid summary, honor it regardless of intermediate parse
    // failures; the invalid-message flag only refines the diagnostic when the
    // summary is genuinely missing.
    let Some(summary) = parsed.summary else {
        return match status.code() {
            Some(0) if parsed.invalid_message => BackupOutcome::failed("restic_output_invalid"),
            Some(0) => BackupOutcome::failed("restic_summary_missing"),
            Some(code) => BackupOutcome::failed(
                classify_stderr(stderr)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("restic_exit_{code}")),
            ),
            None => BackupOutcome::failed("restic_terminated"),
        };
    };
    let mut failure_details = parsed.failure_details;
    let mut vss_fallback = parsed.vss_fallback || known_vss_fallback;
    let mut vss_cleanup_failed = parsed.vss_cleanup_failed;
    if let Ok(stderr_output) = stderr_output_result {
        failure_details.merge(stderr_output.failure_details);
        vss_fallback |= stderr_output.vss_fallback;
        vss_cleanup_failed |= stderr_output.vss_cleanup_failed;
    }
    match status.code() {
        Some(0) if vss_fallback && vss_cleanup_failed => BackupOutcome::warnings(
            summary,
            "restic_vss_fallback_and_cleanup_failed",
            failure_details,
        ),
        Some(0) if vss_fallback => {
            BackupOutcome::warnings(summary, "restic_vss_fallback", failure_details)
        }
        Some(0) if vss_cleanup_failed => {
            BackupOutcome::warnings(summary, "restic_vss_cleanup_failed", failure_details)
        }
        Some(0) => BackupOutcome::succeeded(summary),
        Some(3) if vss_fallback && vss_cleanup_failed => BackupOutcome::warnings(
            summary,
            "restic_vss_fallback_partial_source_and_cleanup_failed",
            failure_details,
        ),
        Some(3) if vss_fallback => BackupOutcome::warnings(
            summary,
            "restic_vss_fallback_and_partial_source",
            failure_details,
        ),
        Some(3) if vss_cleanup_failed => BackupOutcome::warnings(
            summary,
            "restic_partial_source_and_vss_cleanup_failed",
            failure_details,
        ),
        Some(3) => BackupOutcome::warnings(summary, "restic_partial_source", failure_details),
        Some(code) => BackupOutcome::failed(
            classify_stderr(stderr)
                .map(str::to_owned)
                .unwrap_or_else(|| format!("restic_exit_{code}")),
        ),
        None => BackupOutcome::failed("restic_terminated"),
    }
}

fn read_bounded(mut reader: impl Read, limit: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(limit.min(8 * 1024));
    let _ = reader
        .by_ref()
        .take(u64::try_from(limit).unwrap_or(u64::MAX))
        .read_to_end(&mut output);
    drain(reader);
    output
}

fn read_bounded_with_overflow(mut reader: impl Read, limit: usize) -> (Vec<u8>, bool) {
    let mut output = Vec::with_capacity(limit.min(8 * 1024));
    let maximum = u64::try_from(limit.saturating_add(1)).unwrap_or(u64::MAX);
    let read_failed = reader
        .by_ref()
        .take(maximum)
        .read_to_end(&mut output)
        .is_err();
    let overflow = read_failed || output.len() > limit;
    if output.len() > limit {
        output.truncate(limit);
    }
    drain(reader);
    (output, overflow)
}

fn classify_stderr(stderr: &[u8]) -> Option<&'static str> {
    let message = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if message.contains("vss error:")
        || message.contains("failed to create snapshot for [")
        || message.contains("failed to delete vss snapshot")
        || message.contains("volume shadow copy service")
        || message.contains("shadow copy provider")
    {
        Some("restic_vss_unavailable")
    } else if message.contains("access denied")
        || message.contains("permission denied")
        || message.contains("insufficient privilege")
    {
        Some("restic_permission_denied")
    } else if message.contains("already locked")
        || message.contains("repository is locked")
        || message.contains("unable to create lock")
    {
        Some("restic_repository_locked")
    } else if message.contains("unauthorized")
        || message.contains("authentication failed")
        || message.contains("invalid access key")
        || message.contains("wrong password")
    {
        Some("restic_authentication_failed")
    } else if message.contains("connection refused")
        || message.contains("connection reset")
        || message.contains("timed out")
        || message.contains("timeout")
        || message.contains("no such host")
        || message.contains("name resolution")
    {
        Some("restic_repository_unreachable")
    } else if message.contains("no such file or directory")
        || message.contains("cannot find the path")
        || message.contains("path does not exist")
    {
        Some("restic_source_unavailable")
    } else {
        None
    }
}

#[derive(Debug)]
enum OutputEvent {
    Progress(BackupProgress),
    Summary(BackupSummary),
    FailureItem(String),
    FailureItemOmitted,
    VssFallback,
    VssCleanupFailed,
}

#[derive(Debug, Default)]
struct ParsedOutput {
    summary: Option<BackupSummary>,
    invalid_message: bool,
    failure_details: BackupFailureDetails,
    vss_fallback: bool,
    vss_cleanup_failed: bool,
}

fn collect_progress_events(
    events: &mpsc::Receiver<BackupProgress>,
    on_progress: &mut impl FnMut(BackupProgress),
) {
    for progress in events.try_iter() {
        on_progress(progress);
    }
}

fn read_json_output(
    stdout: impl Read,
    progress: &mpsc::SyncSender<BackupProgress>,
) -> Result<ParsedOutput, OutputReadError> {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let mut parsed = ParsedOutput::default();
    while let Some(valid) =
        read_bounded_line(&mut reader, &mut line).map_err(|_| OutputReadError)?
    {
        if !valid {
            parsed.invalid_message = true;
            continue;
        }
        match parse_output_event(&line) {
            Ok(Some(OutputEvent::Progress(update))) => {
                let _ = progress.try_send(update);
            }
            Ok(Some(OutputEvent::Summary(summary))) => parsed.summary = Some(summary),
            Ok(Some(OutputEvent::FailureItem(item))) => parsed.failure_details.push(item),
            Ok(Some(OutputEvent::FailureItemOmitted)) => {
                parsed.failure_details.omitted = parsed.failure_details.omitted.saturating_add(1);
            }
            Ok(Some(OutputEvent::VssFallback)) => parsed.vss_fallback = true,
            Ok(Some(OutputEvent::VssCleanupFailed)) => parsed.vss_cleanup_failed = true,
            Ok(None) => {}
            Err(()) => parsed.invalid_message = true,
        }
    }
    Ok(parsed)
}

fn read_stderr_output(
    stderr: impl Read,
    progress: &mpsc::SyncSender<BackupProgress>,
) -> (Result<ParsedOutput, OutputReadError>, Vec<u8>) {
    let mut captured = PrefixCapturingReader::new(stderr, MAX_STDERR_BYTES);
    let parsed = read_json_output(&mut captured, progress);
    (parsed, captured.into_prefix())
}

struct PrefixCapturingReader<R> {
    inner: R,
    prefix: Vec<u8>,
    limit: usize,
}

impl<R> PrefixCapturingReader<R> {
    fn new(inner: R, limit: usize) -> Self {
        Self {
            inner,
            prefix: Vec::with_capacity(limit.min(8 * 1024)),
            limit,
        }
    }

    fn into_prefix(self) -> Vec<u8> {
        self.prefix
    }
}

impl<R: Read> Read for PrefixCapturingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        let remaining = self.limit.saturating_sub(self.prefix.len());
        self.prefix
            .extend_from_slice(&buffer[..read.min(remaining)]);
        Ok(read)
    }
}

fn read_bounded_line(reader: &mut impl BufRead, line: &mut Vec<u8>) -> io::Result<Option<bool>> {
    line.clear();
    let mut oversized = false;
    let mut read_any = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(read_any.then_some(!oversized));
        }
        read_any = true;
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if !oversized && line.len() + consumed <= MAX_JSON_LINE_BYTES {
            line.extend_from_slice(&available[..consumed]);
        } else {
            oversized = true;
        }
        let ended = available[consumed - 1] == b'\n';
        reader.consume(consumed);
        if ended {
            return Ok(Some(!oversized));
        }
    }
}

#[derive(Debug, Deserialize)]
struct WireMessage {
    message_type: String,
    #[serde(default)]
    percent_done: Option<f64>,
    #[serde(default)]
    total_files: Option<u64>,
    #[serde(default)]
    files_done: u64,
    #[serde(default)]
    total_bytes: Option<u64>,
    #[serde(default)]
    bytes_done: u64,
    #[serde(default)]
    error_count: u64,
    #[serde(default)]
    total_files_processed: u64,
    #[serde(default)]
    total_bytes_processed: u64,
    #[serde(default)]
    data_added: u64,
    #[serde(default)]
    snapshot_id: Option<String>,
    #[serde(default)]
    item: Option<String>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Debug, Deserialize)]
struct WireError {
    #[serde(default)]
    message: String,
}

fn parse_output_event(line: &[u8]) -> Result<Option<OutputEvent>, ()> {
    if let Some(item) = parse_plain_missing_item(line) {
        return Ok(Some(OutputEvent::FailureItem(item)));
    }
    let message: WireMessage = serde_json::from_slice(line).map_err(|_| ())?;
    match message.message_type.as_str() {
        "status" => Ok(Some(OutputEvent::Progress(BackupProgress {
            percent_done: message
                .percent_done
                .filter(|percent| percent.is_finite())
                .map(|percent| (percent.clamp(0.0, 1.0) * 100.0).round() as u8),
            files_done: message.files_done,
            total_files: message.total_files,
            bytes_done: message.bytes_done,
            total_bytes: message.total_bytes,
            error_count: message.error_count,
        }))),
        "summary" => Ok(Some(OutputEvent::Summary(BackupSummary {
            files_processed: message.total_files_processed,
            bytes_processed: message.total_bytes_processed,
            data_added: message.data_added,
            snapshot_id: message.snapshot_id,
        }))),
        "error" => {
            let error_message = message
                .error
                .as_ref()
                .map_or("", |error| error.message.as_str());
            if is_vss_fallback_error(error_message) {
                Ok(Some(OutputEvent::VssFallback))
            } else if is_vss_cleanup_error(error_message) {
                Ok(Some(OutputEvent::VssCleanupFailed))
            } else {
                Ok(Some(match message.item {
                    Some(item) => OutputEvent::FailureItem(item),
                    None => OutputEvent::FailureItemOmitted,
                }))
            }
        }
        _ => Ok(None),
    }
}

fn is_vss_fallback_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("failed to create snapshot for [")
        || (message.starts_with("vss error: getsnapshotproperties()")
            && message.contains("mount point"))
}

fn is_vss_cleanup_error(message: &str) -> bool {
    message
        .to_ascii_lowercase()
        .contains("failed to delete vss snapshot")
}

fn parse_plain_missing_item(line: &[u8]) -> Option<String> {
    let line = std::str::from_utf8(line)
        .ok()?
        .trim_end_matches(['\r', '\n']);
    line.strip_suffix(" does not exist, skipping")
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
}

fn drain(mut input: impl Read) {
    let _ = io::copy(&mut input, &mut io::sink());
}

#[derive(Debug, Clone, Copy)]
struct OutputReadError;

struct KillOnDropJob {
    handle: HANDLE,
}

impl KillOnDropJob {
    fn new() -> windows::core::Result<Self> {
        // SAFETY: no custom security attributes or global name are supplied.
        let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }?;
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: limits points to the correctly sized structure for this
        // information class, and handle is owned by this object.
        if let Err(error) = unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .expect("job information size fits in u32"),
            )
        } {
            // SAFETY: handle was created above and has not been transferred.
            let _ = unsafe { CloseHandle(handle) };
            return Err(error);
        }
        Ok(Self { handle })
    }

    fn assign(&self, child: &std::process::Child) -> windows::core::Result<()> {
        // SAFETY: both handles remain live for the duration of the call.
        unsafe { AssignProcessToJobObject(self.handle, HANDLE(child.as_raw_handle())) }
    }

    fn terminate(&self) -> windows::core::Result<()> {
        // SAFETY: the handle remains owned and live.
        unsafe { TerminateJobObject(self.handle, ERROR_CANCELLED.0) }
    }
}

/// Requests termination through the job, then closes the kill-on-close job and
/// directly terminates the root child as a fallback. The caller may join pipe
/// readers only after this function confirms that the child was reaped.
fn terminate_process_tree(job: KillOnDropJob, child: &mut Child) -> bool {
    let _ = job.terminate();
    drop(job);
    let _ = child.kill();

    let deadline = Instant::now() + PROCESS_TERMINATION_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(PROCESS_POLL_INTERVAL);
            }
            Ok(None) | Err(_) => return false,
        }
    }
}

impl Drop for KillOnDropJob {
    fn drop(&mut self) {
        // KILL_ON_JOB_CLOSE ensures subprocesses cannot outlive the service.
        let _ = unsafe { CloseHandle(self.handle) };
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::Instant;

    use resticpal_core::config::RepositoryMode;
    use resticpal_core::restic::ResticOperation;
    use resticpal_core::status::MAX_BACKUP_FAILED_ITEM_BYTES;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_QUERY_INFORMATION, PROCESS_READ_CONTROL, PROCESS_SYNCHRONIZE,
    };

    use super::*;

    #[test]
    fn restic_hostname_preserves_physical_dns_names_beyond_the_netbios_limit() {
        let dns_hostname = "1358B227-E6DF-42B8-B03A-F5EB8D629D10";
        let legacy_computer_name = "1358B227-E6DF-4";
        let encoded: Vec<_> = dns_hostname.encode_utf16().collect();

        let decoded = decode_restic_hostname(&encoded).expect("valid physical DNS hostname");

        assert_eq!(decoded, dns_hostname);
        assert_ne!(decoded, legacy_computer_name);
    }

    #[test]
    fn restic_hostname_rejects_invalid_utf16_controls_and_oversized_utf8() {
        let oversized: Vec<_> = "é".repeat(128).encode_utf16().collect();
        let control: Vec<_> = "backup\nhost".encode_utf16().collect();

        for value in [
            &[][..],
            &[0xD800][..],
            control.as_slice(),
            oversized.as_slice(),
        ] {
            assert_eq!(
                decode_restic_hostname(value)
                    .expect_err("invalid hostname must fail closed")
                    .code,
                "restore_hostname_unavailable"
            );
        }
    }

    #[test]
    fn restic_hostname_matches_windows_physical_dns_hostname_exactly() {
        let mut buffer = [0_u16; MAX_RESTORE_HOSTNAME_BYTES + 1];
        let mut length = u32::try_from(buffer.len()).expect("bounded hostname buffer");
        // SAFETY: the fixed buffer has the advertised UTF-16 capacity.
        unsafe {
            GetComputerNameExW(
                ComputerNamePhysicalDnsHostname,
                Some(PWSTR(buffer.as_mut_ptr())),
                &mut length,
            )
        }
        .expect("physical Windows DNS hostname");
        let length = usize::try_from(length).expect("bounded Windows hostname length");
        let expected = String::from_utf16(&buffer[..length]).expect("valid Windows DNS hostname");

        assert_eq!(
            current_restic_hostname().expect("restic physical DNS hostname"),
            expected,
            "restic snapshots must use Go's physical DNS identity, including its casing"
        );
    }

    #[derive(Debug)]
    struct MapSecretResolver(BTreeMap<String, String>);

    impl SecretResolver for MapSecretResolver {
        fn resolve(&self, reference: &str) -> Result<Zeroizing<String>, SecretResolveError> {
            self.0
                .get(reference)
                .cloned()
                .map(Zeroizing::new)
                .ok_or(SecretResolveError)
        }
    }

    #[derive(Debug)]
    struct NoopWakeLockProvider;

    impl WakeLockProvider for NoopWakeLockProvider {
        fn acquire(
            &self,
            _timeout: Duration,
        ) -> Result<Box<dyn WakeLockLease>, WakeLockAcquireError> {
            Ok(Box::new(()))
        }
    }

    fn executor(secrets: BTreeMap<String, String>) -> ResticExecutor {
        ResticExecutor::new(
            "unused.exe",
            PathBuf::from(r"C:\ProgramData\ResticPal-Test"),
            Arc::new(MapSecretResolver(secrets)),
            Arc::new(NoopWakeLockProvider),
        )
    }

    fn real_restic_executable() -> PathBuf {
        let path = std::env::var_os("RESTICPAL_TEST_RESTIC")
            .map(PathBuf::from)
            .expect("set RESTICPAL_TEST_RESTIC to an absolute path to restic.exe");
        assert!(path.is_absolute(), "RESTICPAL_TEST_RESTIC must be absolute");
        assert!(path.is_file(), "RESTICPAL_TEST_RESTIC does not name a file");
        path
    }

    fn real_restic_executor(
        executable: &Path,
        data_directory: &Path,
        password: &str,
    ) -> ResticExecutor {
        ResticExecutor::new(
            executable.as_os_str().to_os_string(),
            data_directory.to_path_buf(),
            Arc::new(MapSecretResolver(BTreeMap::from([(
                "integration-password".to_owned(),
                password.to_owned(),
            )]))),
            Arc::new(NoopWakeLockProvider),
        )
    }

    fn local_repository_config(repository: &Path, source: &Path) -> EffectiveConfig {
        let mut config = EffectiveConfig::default();
        config.repository.display_name = Some("Disposable integration repository".to_owned());
        config.repository.url = Some(repository.to_string_lossy().into_owned());
        config.repository.secret_refs.insert(
            SecretEnvironmentVariable::ResticPassword,
            "integration-password".to_owned(),
        );
        config.backup.paths = vec![source.to_path_buf()];
        config.schedule.wake_lock_timeout_seconds = 60;
        config
    }

    fn repository_lock_count(repository: &Path) -> usize {
        fs::read_dir(repository.join("locks"))
            .map(|entries| entries.filter_map(Result::ok).count())
            .unwrap_or_default()
    }

    fn is_finalized_repository_lock_name(name: &OsStr) -> bool {
        let Some(name) = name.to_str() else {
            return false;
        };
        name.len() == 64
            && name
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    fn finalized_repository_lock_count(repository: &Path) -> usize {
        fs::read_dir(repository.join("locks"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        let name = entry.file_name();
                        is_finalized_repository_lock_name(name.as_os_str())
                    })
                    .count()
            })
            .unwrap_or_default()
    }

    fn repository_cache_directories(cache: &Path) -> Vec<OsString> {
        let mut directories = fs::read_dir(cache)
            .expect("read explicit restic cache")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>();
        directories.sort();
        directories
    }

    fn age_cache_directory_for_cleanup(directory: &Path) {
        let powershell = PathBuf::from(
            std::env::var_os("SystemRoot").expect("Windows integration test requires SystemRoot"),
        )
        .join("System32")
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe");
        let status = Command::new(powershell)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "(Get-Item -LiteralPath $env:RESTICPAL_TEST_CACHE_DIRECTORY).LastWriteTimeUtc = [DateTime]::UtcNow.AddDays(-31)",
            ])
            .env("RESTICPAL_TEST_CACHE_DIRECTORY", directory)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW.0)
            .status()
            .expect("age stale cache directory");
        assert!(status.success(), "aging stale cache directory failed");
    }

    fn restic_snapshot_listing(
        restic: &Path,
        repository: &Path,
        cache: &Path,
        password: &str,
        snapshot_id: &str,
    ) -> Vec<u8> {
        let mut command = Command::new(restic);
        command
            .args(["ls", "--json", snapshot_id])
            .env_clear()
            .env("RESTIC_REPOSITORY", repository)
            .env("RESTIC_PASSWORD", password)
            .env("RESTIC_CACHE_DIR", cache)
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW.0);
        for name in INHERITED_ENVIRONMENT {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let output = command.output().expect("list real restic snapshot");
        assert!(
            output.status.success(),
            "restic snapshot listing failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn volume_guid_alias(path: &Path) -> PathBuf {
        let drive = match path.components().next() {
            Some(Component::Prefix(prefix)) => match prefix.kind() {
                Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => drive,
                _ => panic!("volume GUID fixture requires a drive-letter path"),
            },
            _ => panic!("volume GUID fixture requires an absolute Windows path"),
        };
        let drive_root = PathBuf::from(format!("{}:\\", char::from(drive)));
        let relative = path
            .strip_prefix(&drive_root)
            .expect("source path is below its drive root");
        let mountvol = PathBuf::from(
            std::env::var_os("SystemRoot").expect("Windows integration test requires SystemRoot"),
        )
        .join("System32")
        .join("mountvol.exe");
        let output = Command::new(mountvol)
            .arg(&drive_root)
            .arg("/L")
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW.0)
            .output()
            .expect("query source volume GUID");
        assert!(
            output.status.success(),
            "mountvol volume GUID query failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let volume_root = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        assert!(
            volume_root.starts_with(r"\\?\Volume{") && volume_root.ends_with('\\'),
            "mountvol returned an unexpected volume root"
        );
        let mut alias = PathBuf::from(volume_root);
        alias.push(relative);
        alias
    }

    #[test]
    fn recognizes_only_finalized_repository_lock_names() {
        let valid = "0123456789abcdef".repeat(4);
        assert!(is_finalized_repository_lock_name(OsStr::new(&valid)));
        assert!(!is_finalized_repository_lock_name(OsStr::new(&format!(
            "{valid}-tmp-1234"
        ))));
        assert!(!is_finalized_repository_lock_name(OsStr::new(
            &"0123456789ABCDEF".repeat(4)
        )));
        assert!(!is_finalized_repository_lock_name(OsStr::new(
            "not-a-restic-lock"
        )));
    }

    #[test]
    fn directory_listing_accepts_only_immediate_regular_files_and_directories() {
        let output = concat!(
            "{\"id\":\"snapshot metadata\"}\n",
            "{\"name\":\"docs\",\"path\":\"/docs\",\"type\":\"dir\",\"mtime\":\"2026-08-24T12:00:00Z\"}\n",
            "{\"name\":\"report [2025].txt\",\"path\":\"/report [2025].txt\",\"type\":\"file\",\"size\":42}\n",
            "{\"name\":\"link\",\"path\":\"/link\",\"type\":\"symlink\"}\n",
            "{\"name\":\"nested\",\"path\":\"/docs/nested\",\"type\":\"file\"}\n"
        );
        let entries = parse_restore_directory(output.as_bytes(), "/")
            .expect("bounded immediate directory listing");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].node_type, RestoreNodeType::Directory);
        assert_eq!(entries[1].node_type, RestoreNodeType::File);
        assert_eq!(entries[1].size, Some(42));
    }

    #[test]
    fn directory_listing_rejects_forged_or_unsafe_entry_names() {
        for output in [
            "{\"name\":\"wrong\",\"path\":\"/actual\",\"type\":\"file\"}\n",
            "{\"name\":\"..\",\"path\":\"/..\",\"type\":\"dir\"}\n",
            "{\"name\":\"hidden\",\"path\":\"/hidden\"}\n",
            "not-json\n",
        ] {
            assert_eq!(
                parse_restore_directory(output.as_bytes(), "/"),
                Err(RestoreError::new("restore_directory_output_invalid"))
            );
        }
    }

    #[test]
    fn recursive_restore_preflight_rejects_hidden_links_devices_and_missing_nodes() {
        let safe = concat!(
            "{\"id\":\"snapshot metadata\"}\n",
            "{\"name\":\"docs\",\"path\":\"/docs\",\"type\":\"dir\"}\n",
            "{\"name\":\"report.txt\",\"path\":\"/docs/report.txt\",\"type\":\"file\"}\n"
        );
        assert_eq!(validate_restore_subtree(safe.as_bytes(), "/docs"), Ok(()));
        for kind in ["symlink", "dev", "chardev", "fifo", "socket"] {
            let unsafe_tree = format!(
                "{safe}{{\"name\":\"hidden\",\"path\":\"/docs/hidden\",\"type\":\"{kind}\"}}\n"
            );
            assert_eq!(
                validate_restore_subtree(unsafe_tree.as_bytes(), "/docs"),
                Err(RestoreError::new("restore_unsupported_node")),
                "hidden {kind}"
            );
        }
        assert_eq!(
            validate_restore_subtree(safe.as_bytes(), "/missing"),
            Err(RestoreError::new("restore_selected_node_unavailable"))
        );
        let malformed = format!("{safe}{{\"name\":\"hidden\",\"path\":\"/docs/hidden\"}}\n");
        assert_eq!(
            validate_restore_subtree(malformed.as_bytes(), "/docs"),
            Err(RestoreError::new("restore_subtree_preflight_failed"))
        );
    }

    #[test]
    fn recursive_restore_preflight_streams_large_normal_directories() {
        let stem = "a".repeat(900);
        let mut listing = String::from("{\"name\":\"docs\",\"path\":\"/docs\",\"type\":\"dir\"}\n");
        for index in 0..5_000_u16 {
            let name = format!("{index:04}-{stem}.txt");
            listing.push_str(&format!(
                "{{\"name\":\"{name}\",\"path\":\"/docs/{name}\",\"type\":\"file\"}}\n"
            ));
        }
        assert!(listing.len() > MAX_RESTORE_OUTPUT_BYTES);
        assert!(listing.lines().count() > MAX_RESTORE_DIRECTORY_ENTRIES);
        assert_eq!(
            validate_restore_subtree_reader(&mut BufReader::new(listing.as_bytes()), "/docs"),
            Ok(())
        );
    }

    #[test]
    fn restore_destination_must_be_local_existing_and_outside_protected_state() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = ordinary_windows_path(
            &fs::canonicalize(temporary.path()).expect("canonical temporary root"),
        );
        let safe = root.join("recovered");
        let protected = root.join("service-data");
        fs::create_dir(&safe).expect("safe destination");
        fs::create_dir(&protected).expect("protected service data");
        assert_eq!(
            validate_restore_destination(&safe, &protected, OsStr::new("unused.exe")),
            Ok(safe.clone())
        );
        assert_eq!(
            validate_restore_destination(&protected, &protected, OsStr::new("unused.exe")),
            Err(RestoreError::new("restore_destination_protected"))
        );
        assert_eq!(
            validate_restore_destination(
                Path::new("relative"),
                &protected,
                OsStr::new("unused.exe")
            ),
            Err(RestoreError::new("restore_destination_invalid"))
        );
        assert_eq!(
            validate_restore_destination(
                &root.join("missing"),
                &protected,
                OsStr::new("unused.exe")
            ),
            Err(RestoreError::new("restore_destination_unavailable"))
        );
        let first = create_restore_destination(&safe).expect("first unique restore destination");
        let second = create_restore_destination(&safe).expect("second unique restore destination");
        assert_ne!(first, second);
        assert!(first.is_dir() && second.is_dir());
    }

    #[test]
    fn restore_directory_locks_prevent_rename_and_reparse_replacement() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = ordinary_windows_path(
            &fs::canonicalize(temporary.path()).expect("canonical temporary root"),
        );
        let parent = root.join("parent");
        fs::create_dir(&parent).expect("destination parent");
        let destination = create_restore_destination(&parent).expect("unique child");
        let parent_lock = lock_restore_directory(&parent).expect("lock parent against deletion");
        let child_lock =
            lock_restore_directory(&destination).expect("lock child against reparse replacement");

        assert!(fs::rename(&parent, root.join("moved-parent")).is_err());
        assert!(fs::rename(&destination, parent.join("moved-child")).is_err());
        assert!(fs::remove_dir(&destination).is_err());

        drop(child_lock);
        drop(parent_lock);
        fs::rename(&destination, parent.join("moved-child"))
            .expect("closed handles release the destination lock");
    }

    #[test]
    fn restored_children_are_protected_until_completed_parent_acl_handoff() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = ordinary_windows_path(
            &fs::canonicalize(temporary.path()).expect("canonical temporary root"),
        );
        let parent = root.join("parent");
        fs::create_dir(&parent).expect("restore destination parent");
        let parent_lock = lock_restore_directory(&parent).expect("locked destination parent");
        let destination = create_restore_destination(&parent).expect("protected restore child");
        let destination_lock = lock_restore_directory_for_handoff(&destination)
            .expect("destination handle with WRITE_DAC");
        verify_protected_restore_directory(&parent_lock, &destination_lock)
            .expect("restic must start with a protected LocalSystem directory");

        let nested = destination.join("nested");
        fs::create_dir(&nested).expect("nested restored directory");
        let restored_file = nested.join("document.txt");
        fs::write(&restored_file, b"restored contents\n").expect("service-owned restored contents");

        handoff_restore_directory(&parent_lock, &destination_lock)
            .expect("completed restore must inherit the chosen parent ACL");
        let (destination_descriptor, _) = directory_security(&destination_lock)
            .expect("handed-off destination security descriptor");
        assert!(
            !directory_security_is_protected(&destination_descriptor)
                .expect("handed-off inheritance control"),
            "the completed restore must resume inheriting its chosen parent ACL"
        );
        assert_eq!(
            fs::read(&restored_file).expect("handed-off nested file is readable"),
            b"restored contents\n"
        );
    }

    #[test]
    fn failed_restore_termination_never_hands_off_or_exposes_partial_output() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = ordinary_windows_path(
            &fs::canonicalize(temporary.path()).expect("canonical temporary root"),
        );
        let parent = root.join("parent");
        fs::create_dir(&parent).expect("restore destination parent");
        let parent_lock = lock_restore_directory(&parent).expect("locked destination parent");
        let destination = create_restore_destination(&parent).expect("protected recovery child");
        let destination_lock = lock_restore_directory_for_handoff(&destination)
            .expect("locked protected recovery child");
        verify_protected_restore_directory(&parent_lock, &destination_lock)
            .expect("authenticated LocalSystem recovery destination");

        let failure = finish_restore_destination(
            &parent_lock,
            &destination_lock,
            RestoreOutcome::failed_at("restic_termination_failed", Some(destination.clone())),
            Some(destination.clone()),
        );
        assert_eq!(
            failure.kind,
            RestoreOutcomeKind::Failed {
                code: "restic_termination_failed".to_owned()
            }
        );
        assert!(
            failure.destination.is_none(),
            "untrusted partial recovery output must not be advertised to the requester"
        );
        verify_protected_restore_directory(&parent_lock, &destination_lock)
            .expect("termination failure must preserve the original protected owner and ACL");
        assert!(
            fs::rename(&destination, parent.join("attacker-replacement")).is_err(),
            "the quarantined worker must keep its no-delete child handle alive"
        );
        assert!(
            fs::rename(&parent, root.join("attacker-parent")).is_err(),
            "the quarantined worker must keep its no-delete parent handle alive"
        );
    }

    fn test_descriptor_sddl(descriptor: &RestoreSecurityDescriptor) -> String {
        let mut value = windows::core::PWSTR::null();
        // SAFETY: the descriptor is live and Windows returns a LocalAlloc
        // string owned by this helper until it is converted and freed.
        unsafe {
            windows::Win32::Security::Authorization::ConvertSecurityDescriptorToStringSecurityDescriptorW(
                descriptor.0,
                SDDL_REVISION_1,
                DACL_SECURITY_INFORMATION,
                &raw mut value,
                None,
            )
        }
        .expect("security descriptor SDDL");
        let result = unsafe { value.to_string() }.expect("valid security descriptor SDDL");
        let _ = unsafe { LocalFree(Some(HLOCAL(value.0.cast()))) };
        result
    }

    #[test]
    fn restore_handoff_inherits_only_inheritable_parent_permissions() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = ordinary_windows_path(
            &fs::canonicalize(temporary.path()).expect("canonical temporary root"),
        );
        let parent = root.join("parent");
        fs::create_dir(&parent).expect("restore destination parent");
        let parent_lock =
            lock_restore_directory_for_handoff(&parent).expect("parent handle with WRITE_DAC");
        let parent_security = RestoreSecurityDescriptor::from_sddl(w!(
            "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)(A;OICI;FR;;;IU)(A;;FR;;;WD)"
        ))
        .expect("parent descriptor with inheritable and parent-only principals");
        let parent_dacl =
            restore_descriptor_dacl(&parent_security).expect("parent fixture descriptor DACL");
        // Everyone can read the parent itself but is deliberately forbidden
        // from inheriting access to recovered child data.
        unsafe {
            SetSecurityInfo(
                HANDLE(parent_lock.as_raw_handle()),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION
                    | windows::Win32::Security::PROTECTED_DACL_SECURITY_INFORMATION,
                None,
                None,
                Some(parent_dacl.cast_const()),
                None,
            )
        }
        .ok()
        .expect("custom parent-only Everyone ACE");

        let destination = create_restore_destination(&parent).expect("trusted child");
        let destination_lock = lock_restore_directory_for_handoff(&destination)
            .expect("trusted child security handle");
        verify_protected_restore_directory(&parent_lock, &destination_lock)
            .expect("authenticated protected restore destination");
        let nested = destination.join("nested");
        fs::create_dir(&nested).expect("nested recovered directory");
        let recovered = nested.join("recovered.txt");
        fs::write(&recovered, b"private restored content\n").expect("private recovery fixture");

        handoff_restore_directory(&parent_lock, &destination_lock)
            .expect("ordinary parent inheritance handoff");
        let (destination_descriptor, _) =
            directory_security(&destination_lock).expect("handed-off child descriptor");
        let destination_sddl = test_descriptor_sddl(&destination_descriptor);
        assert!(
            destination_sddl.contains(";;;IU)"),
            "inheritable interactive-user access must reach the restored root: {destination_sddl}"
        );
        assert!(
            !destination_sddl.contains(";;;WD)"),
            "parent-only Everyone access must not reach private recovered data: {destination_sddl}"
        );

        let nested_lock = lock_restore_directory(&nested).expect("nested recovered directory");
        let (nested_descriptor, _) =
            directory_security(&nested_lock).expect("handed-off nested directory descriptor");
        let nested_sddl = test_descriptor_sddl(&nested_descriptor);
        assert!(
            nested_sddl.contains(";;;IU)") && !nested_sddl.contains(";;;WD)"),
            "inheritable interactive access must propagate without parent-only access: {nested_sddl}"
        );
        assert_eq!(
            fs::read(&recovered).expect("readable recovered contents"),
            b"private restored content\n"
        );
    }

    #[test]
    fn protected_restore_destination_rejects_a_substituted_untrusted_directory() {
        let temporary = tempfile::tempdir().expect("temporary directory");
        let root = ordinary_windows_path(
            &fs::canonicalize(temporary.path()).expect("canonical temporary root"),
        );
        let parent = root.join("parent");
        fs::create_dir(&parent).expect("restore destination parent");
        let parent_lock = lock_restore_directory(&parent).expect("locked destination parent");
        let destination = create_restore_destination(&parent).expect("trusted restore child");

        // FILE_DELETE_CHILD on a writable parent can replace even a protected
        // child before its no-delete handle is opened. A protected bit alone
        // must never authenticate that replacement.
        fs::remove_dir(&destination).expect("simulate parent-owner child replacement");
        let untrusted = RestoreSecurityDescriptor::from_sddl(w!(
            "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;OW)(A;OICI;FA;;;WD)"
        ))
        .expect("attacker-controlled protected directory descriptor");
        let attributes = untrusted.attributes();
        let path_wide: Vec<u16> = destination
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe { CreateDirectoryW(PCWSTR(path_wide.as_ptr()), Some(&raw const attributes)) }
            .expect("attacker-controlled protected replacement");
        let destination_lock = lock_restore_directory_for_handoff(&destination)
            .expect("lock the substituted child before inspecting its actual identity");
        assert_eq!(
            verify_protected_restore_directory(&parent_lock, &destination_lock),
            Err(RestoreError::new("restore_destination_security_failed")),
            "a protected replacement with an extra writable principal must be rejected"
        );

        let expected =
            RestoreSecurityDescriptor::from_sddl(w!("O:SYG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"))
                .expect("production SYSTEM-owned security descriptor");
        let forged_owner =
            RestoreSecurityDescriptor::from_sddl(w!("O:BAG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)"))
                .expect("substituted administrator-owned security descriptor");
        let system_owner = restore_descriptor_owner(&expected).expect("SYSTEM owner SID");
        assert_eq!(
            verify_restore_security_descriptor(&forged_owner, &expected, system_owner),
            Err(RestoreError::new("restore_destination_security_failed")),
            "an exact privileged ACL must still be rejected when the owner is not SYSTEM"
        );
    }

    #[test]
    fn restore_progress_is_parsed_without_retaining_sensitive_output() {
        let output = concat!(
            "{\"message_type\":\"status\",\"files_restored\":1,\"bytes_restored\":20,\"total_files\":2,\"total_bytes\":42}\n",
            "{\"message_type\":\"summary\",\"files_restored\":2,\"bytes_restored\":42}\n"
        );
        let (sender, receiver) = mpsc::sync_channel(4);
        let summary = read_restore_output(output.as_bytes(), &sender).expect("restore output");
        assert_eq!(summary.files_restored, Some(2));
        assert_eq!(summary.bytes_restored, Some(42));
        assert_eq!(summary.total_files, Some(2));
        assert_eq!(receiver.try_iter().count(), 2);
    }

    fn wait_until_process_is_unobservable(pid: u32) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let access = PROCESS_READ_CONTROL | PROCESS_QUERY_INFORMATION | PROCESS_SYNCHRONIZE;
            let process = unsafe { OpenProcess(access, false, pid) };
            let Ok(process) = process else {
                return;
            };
            let _ = unsafe { CloseHandle(process) };

            assert!(
                Instant::now() < deadline,
                "terminated stale-lock process {pid} remained observable"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn leave_stale_restic_lock(restic: &Path, repository: &Path, password: &str) {
        assert_eq!(
            repository_lock_count(repository),
            0,
            "stale-lock fixture requires an initially unlocked repository"
        );
        let mut command = Command::new(restic);
        command
            .args(["backup", "--stdin", "--stdin-filename", "stale-lock-probe"])
            .env_clear()
            .env("RESTIC_REPOSITORY", repository)
            .env("RESTIC_PASSWORD", password)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW.0);
        for name in INHERITED_ENVIRONMENT {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command.spawn().expect("start stale-lock restic process");
        let stdin = child.stdin.take().expect("hold restic stdin open");
        let deadline = Instant::now() + Duration::from_secs(10);
        // The local backend writes a `<lock-id>-tmp-*` file before atomically
        // renaming it. Killing on the first directory entry would leave an
        // incomplete temporary file, not the stale lock this fixture needs.
        while finalized_repository_lock_count(repository) == 0 {
            assert!(
                child
                    .try_wait()
                    .expect("query stale-lock process")
                    .is_none(),
                "restic exited before acquiring the test lock"
            );
            assert!(
                Instant::now() < deadline,
                "restic did not create the test lock"
            );
            thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            repository_lock_count(repository),
            1,
            "fixture must observe one finalized lock and no temporary files"
        );

        let stale_pid = child.id();
        child.kill().expect("terminate restic without lock cleanup");
        child.wait().expect("reap terminated restic process");
        drop(stdin);
        drop(child);
        wait_until_process_is_unobservable(stale_pid);
        assert_eq!(
            repository_lock_count(repository),
            1,
            "forced termination must leave exactly one stale lock"
        );
    }

    fn powershell_invocation(script: &str) -> ResticInvocation {
        let system_root = std::env::var_os("SystemRoot").expect("Windows has SystemRoot");
        ResticInvocation {
            operation: ResticOperation::Backup,
            executable: PathBuf::from(system_root)
                .join(r"System32\WindowsPowerShell\v1.0\powershell.exe"),
            arguments: vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.into(),
            ],
            environment: BTreeMap::new(),
            secret_environment: BTreeMap::new(),
        }
    }

    fn cmd_invocation(script: &str) -> ResticInvocation {
        let system_root = std::env::var_os("SystemRoot").expect("Windows has SystemRoot");
        ResticInvocation {
            operation: ResticOperation::Backup,
            executable: PathBuf::from(system_root).join(r"System32\cmd.exe"),
            arguments: vec!["/d".into(), "/s".into(), "/c".into(), script.into()],
            environment: BTreeMap::new(),
            secret_environment: BTreeMap::new(),
        }
    }

    #[test]
    fn parses_documented_status_and_summary_messages() {
        let status = br#"{"message_type":"status","percent_done":0.426,"total_files":12,"files_done":5,"total_bytes":1000,"bytes_done":426,"error_count":1}"#;
        let summary = br#"{"message_type":"summary","total_files_processed":12,"total_bytes_processed":1000,"data_added":300,"snapshot_id":"abc123"}"#;

        assert!(matches!(
            parse_output_event(status),
            Ok(Some(OutputEvent::Progress(BackupProgress {
                percent_done: Some(43),
                files_done: 5,
                bytes_done: 426,
                error_count: 1,
                ..
            })))
        ));
        assert!(matches!(
            parse_output_event(summary),
            Ok(Some(OutputEvent::Summary(BackupSummary {
                files_processed: 12,
                bytes_processed: 1000,
                data_added: 300,
                snapshot_id: Some(id),
            }))) if id == "abc123"
        ));
    }

    #[test]
    fn parses_vss_control_errors_without_recording_volumes_as_failed_files() {
        let create_failure = br#"{"message_type":"error","error":{"message":"failed to create snapshot for [c:\\]: VSS error: unexpected provider error"},"during":"archival","item":"c:\\"}"#;
        let mount_failure = br#"{"message_type":"error","error":{"message":"VSS error: GetSnapshotProperties() for mount point d:\\ failed"},"during":"archival","item":"d:\\"}"#;
        let cleanup_failure = br#"{"message_type":"error","error":{"message":"failed to delete VSS snapshot: cleanup error"},"during":"archival","item":"c:\\"}"#;
        let ordinary_failure = br#"{"message_type":"error","error":{"message":"failed to create snapshot metadata"},"during":"archival","item":"C:\\Data\\file.txt"}"#;

        assert!(matches!(
            parse_output_event(create_failure),
            Ok(Some(OutputEvent::VssFallback))
        ));
        assert!(matches!(
            parse_output_event(mount_failure),
            Ok(Some(OutputEvent::VssFallback))
        ));
        assert!(matches!(
            parse_output_event(cleanup_failure),
            Ok(Some(OutputEvent::VssCleanupFailed))
        ));
        assert!(matches!(
            parse_output_event(ordinary_failure),
            Ok(Some(OutputEvent::FailureItem(item))) if item == r"C:\Data\file.txt"
        ));
    }

    #[test]
    fn network_sources_are_rejected_before_restic_runs() {
        for source in [
            PathBuf::from(r"\\server\share\Data"),
            PathBuf::from(r"\\?\UNC\server\share\Data"),
        ] {
            let mut config = EffectiveConfig::default();
            config.repository.url = Some("local:C:/backup".to_owned());
            config.backup.paths = vec![source];

            assert_eq!(
                build_backup_invocations(
                    OsStr::new("restic.exe"),
                    Path::new(r"C:\ProgramData\ResticPal"),
                    &config,
                ),
                Err(InvocationError::UnsupportedNetworkBackupSource)
            );
        }
    }

    #[test]
    fn local_source_resolution_preserves_ordinary_restic_path_namespaces() {
        assert_eq!(
            ordinary_windows_path(Path::new(r"\\?\C:\ProgramData\ResticPal")),
            PathBuf::from(r"C:\ProgramData\ResticPal")
        );
        assert_eq!(
            ordinary_windows_path(Path::new(r"\\?\UNC\server\share\Data")),
            PathBuf::from(r"\\server\share\Data")
        );
        assert!(source_is_network_path(Path::new(r"\\server\share\Data")));
        assert!(source_uses_unsupported_local_namespace(Path::new(
            r"\\.\PhysicalDrive0"
        )));
        assert!(source_uses_unsupported_local_namespace(Path::new(
            r"\\?\GLOBALROOT\Device\HarddiskVolumeShadowCopy1"
        )));
        assert!(source_uses_unsupported_local_namespace(Path::new(
            r"\\?\Volume{01234567-89ab-cdef-0123-456789abcdef}\Data"
        )));
    }

    #[test]
    fn stderr_is_reduced_to_allowlisted_diagnostic_codes() {
        assert_eq!(
            classify_stderr(br"open C:\Users\Yann\private.txt: Access denied"),
            Some("restic_permission_denied")
        );
        assert_eq!(
            classify_stderr(b"repository is already locked exclusively"),
            Some("restic_repository_locked")
        );
        assert_eq!(
            classify_stderr(b"dial tcp: connection refused"),
            Some("restic_repository_unreachable")
        );
        assert_eq!(
            classify_stderr(b"VSS error: insufficient privilege to create a shadow copy"),
            Some("restic_vss_unavailable")
        );
        assert_eq!(
            classify_stderr(br"C:\VssData does not exist, skipping"),
            None,
            "a path containing VSS must not be mistaken for a shadow-copy failure"
        );
        assert_eq!(classify_stderr(b"arbitrary secret output"), None);
    }

    #[test]
    fn progress_floods_are_dropped_without_losing_the_summary() {
        let mut output = Vec::new();
        for _ in 0..1_000 {
            output.extend_from_slice(br#"{"message_type":"status","files_done":1,"bytes_done":2}"#);
            output.push(b'\n');
        }
        output.extend_from_slice(
            br#"{"message_type":"summary","total_files_processed":3,"total_bytes_processed":4,"data_added":5,"snapshot_id":"bounded"}"#,
        );
        output.push(b'\n');
        let (progress_tx, progress_rx) = mpsc::sync_channel(1);

        let parsed = read_json_output(output.as_slice(), &progress_tx).expect("valid output");

        assert_eq!(progress_rx.try_iter().count(), 1);
        assert_eq!(
            parsed.summary.expect("summary").snapshot_id.as_deref(),
            Some("bounded")
        );
    }

    #[test]
    fn an_oversized_progress_line_does_not_fail_a_successful_backup() {
        use std::os::windows::process::ExitStatusExt;

        // A status line larger than MAX_JSON_LINE_BYTES is dropped as oversized,
        // followed by a valid summary from a clean restic exit.
        let mut output = br#"{"message_type":"status","files_done":1,"note":""#.to_vec();
        output.resize(output.len() + MAX_JSON_LINE_BYTES + 16, b'x');
        output.extend_from_slice(br#""}"#);
        output.push(b'\n');
        output.extend_from_slice(
            br#"{"message_type":"summary","total_files_processed":3,"total_bytes_processed":4,"data_added":5,"snapshot_id":"ok"}"#,
        );
        output.push(b'\n');
        let (progress_tx, _progress_rx) = mpsc::sync_channel(1);

        let parsed = read_json_output(output.as_slice(), &progress_tx).expect("read output");
        assert!(
            parsed.invalid_message,
            "oversized line marks invalid_message"
        );
        assert!(parsed.summary.is_some(), "summary still parses");

        let outcome = finish_outcome(
            ExitStatus::from_raw(0),
            Ok(parsed),
            Ok(ParsedOutput::default()),
            b"",
            false,
        );
        assert_eq!(outcome.kind, BackupOutcomeKind::Succeeded);
        assert_eq!(
            outcome.summary.expect("summary").snapshot_id.as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn fake_process_reports_progress_and_success_without_leaking_secret_arguments() {
        let mut invocation = powershell_invocation(
            r#"if ($env:RESTIC_PASSWORD -ne 'test-secret') { exit 12 }; if ($env:RESTIC_CACHE_DIR -ne 'C:\ProgramData\ResticPal-Test\Cache') { exit 13 }; [Console]::Out.WriteLine('{"message_type":"status","percent_done":0.5,"total_files":2,"files_done":1,"total_bytes":20,"bytes_done":10}'); [Console]::Out.WriteLine('{"message_type":"summary","total_files_processed":2,"total_bytes_processed":20,"data_added":7,"snapshot_id":"snapshot-1"}')"#,
        );
        invocation.secret_environment.insert(
            SecretEnvironmentVariable::ResticPassword,
            "password-ref".to_owned(),
        );
        invocation.environment.insert(
            OsString::from("RESTIC_CACHE_DIR"),
            OsString::from(r"C:\Untrusted\Cache"),
        );
        let runner = executor(BTreeMap::from([(
            "password-ref".to_owned(),
            "test-secret".to_owned(),
        )]));
        let mut progress = Vec::new();

        let outcome = runner.execute_invocation(
            &invocation,
            Duration::from_secs(10),
            &CancellationToken::default(),
            false,
            |update| progress.push(update),
        );

        assert_eq!(outcome.kind, BackupOutcomeKind::Succeeded);
        assert_eq!(
            outcome.summary.expect("summary").snapshot_id.as_deref(),
            Some("snapshot-1")
        );
        assert_eq!(
            progress.last().and_then(|value| value.percent_done),
            Some(50)
        );
        assert!(
            !invocation
                .arguments
                .iter()
                .any(|argument| argument == OsStr::new("test-secret"))
        );
    }

    #[test]
    fn cancellation_terminates_the_process_job() {
        let invocation = powershell_invocation("Start-Sleep -Seconds 30");
        let runner = executor(BTreeMap::new());
        let cancellation = CancellationToken::default();
        let cancel_from_thread = cancellation.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            cancel_from_thread.cancel();
        });
        let started = Instant::now();

        let outcome = runner.execute_invocation(
            &invocation,
            Duration::from_secs(10),
            &cancellation,
            false,
            |_| {},
        );
        canceller.join().expect("canceller should finish");

        assert_eq!(outcome.kind, BackupOutcomeKind::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn repository_probe_reports_success_and_not_found_without_parsing_console_text() {
        let mut successful = cmd_invocation("echo untrusted repository metadata & exit /b 0");
        successful.operation = ResticOperation::Probe;
        let runner = executor(BTreeMap::new());
        assert_eq!(
            runner
                .execute_repository_invocation(
                    &successful,
                    Duration::from_secs(10),
                    &CancellationToken::default(),
                )
                .kind,
            RepositoryOutcomeKind::Succeeded
        );

        let mut absent = cmd_invocation("exit /b 10");
        absent.operation = ResticOperation::Probe;
        assert_eq!(
            runner
                .execute_repository_invocation(
                    &absent,
                    Duration::from_secs(10),
                    &CancellationToken::default(),
                )
                .kind,
            RepositoryOutcomeKind::Failed {
                code: "repository_not_found".to_owned()
            }
        );
    }

    #[test]
    fn repository_operation_has_a_hard_timeout() {
        let mut invocation = powershell_invocation("Start-Sleep -Seconds 30");
        invocation.operation = ResticOperation::Unlock;
        let runner = executor(BTreeMap::new());
        let started = Instant::now();

        let outcome = runner.execute_repository_invocation(
            &invocation,
            Duration::from_millis(200),
            &CancellationToken::default(),
        );

        assert_eq!(
            outcome.kind,
            RepositoryOutcomeKind::Failed {
                code: "repository_operation_timed_out".to_owned()
            }
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn restore_operation_has_a_hard_timeout_and_releases_its_process_tree() {
        let mut invocation = powershell_invocation("Start-Sleep -Seconds 30");
        invocation.operation = ResticOperation::Restore;
        let runner = executor(BTreeMap::new());
        let destination = PathBuf::from(r"C:\Recovery\Fresh");
        let started = Instant::now();

        let outcome = runner.execute_restore_invocation(
            &invocation,
            Duration::from_millis(200),
            Some(destination.clone()),
            &CancellationToken::default(),
            |_| {},
        );

        assert_eq!(
            outcome.kind,
            RestoreOutcomeKind::Failed {
                code: "restore_timed_out".to_owned()
            }
        );
        assert_eq!(outcome.destination, Some(destination));
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn process_termination_falls_back_to_the_child_when_the_job_handle_is_invalid() {
        let invocation = powershell_invocation("Start-Sleep -Seconds 30");
        let cache_directory = PathBuf::from(r"C:\ProgramData\ResticPal-Test\Cache");
        let mut command = command_for(&invocation, &[], &cache_directory);
        let mut child = command.spawn().expect("start fallback process");
        let started = Instant::now();

        assert!(terminate_process_tree(
            KillOnDropJob {
                handle: HANDLE::default(),
            },
            &mut child,
        ));
        assert!(started.elapsed() < PROCESS_TERMINATION_GRACE);
        assert!(child.try_wait().expect("query reaped child").is_some());
    }

    #[test]
    fn stale_lock_cleanup_honors_backup_cancellation() {
        let mut invocation = powershell_invocation("Start-Sleep -Seconds 30");
        invocation.operation = ResticOperation::Unlock;
        let runner = executor(BTreeMap::new());
        let cancellation = CancellationToken::default();
        let cancel_from_thread = cancellation.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            cancel_from_thread.cancel();
        });
        let started = Instant::now();

        let outcome = runner.execute_repository_invocation(
            &invocation,
            Duration::from_secs(10),
            &cancellation,
        );
        canceller.join().expect("canceller should finish");

        assert_eq!(outcome.kind, RepositoryOutcomeKind::Cancelled);
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn stale_lock_cleanup_failure_blocks_backup_with_bounded_codes() {
        assert!(
            backup_outcome_for_lock_cleanup(&RepositoryOutcome {
                kind: RepositoryOutcomeKind::Succeeded,
            })
            .is_none()
        );

        for (repository_code, expected_backup_code) in [
            (
                "repository_operation_timed_out",
                "stale_lock_cleanup_timed_out",
            ),
            ("repository_operation_failed", "stale_lock_cleanup_failed"),
            (
                "restic_repository_unreachable",
                "restic_repository_unreachable",
            ),
            ("credential_unavailable", "credential_unavailable"),
        ] {
            let outcome = backup_outcome_for_lock_cleanup(&RepositoryOutcome {
                kind: RepositoryOutcomeKind::Failed {
                    code: repository_code.to_owned(),
                },
            })
            .expect("cleanup failure must block the backup");
            assert_eq!(
                outcome.kind,
                BackupOutcomeKind::Failed {
                    code: expected_backup_code.to_owned()
                }
            );
        }

        assert_eq!(
            backup_outcome_for_lock_cleanup(&RepositoryOutcome {
                kind: RepositoryOutcomeKind::Cancelled,
            })
            .expect("cleanup cancellation must cancel the backup")
            .kind,
            BackupOutcomeKind::Cancelled
        );
    }

    #[test]
    fn restic_partial_source_exit_is_a_success_with_warnings() {
        let invocation = powershell_invocation(
            r#"[Console]::Error.WriteLine('{"message_type":"error","error":{"message":"Access denied"},"during":"archival","item":"C:\\Users\\Example\\locked.txt"}'); [Console]::Out.WriteLine('{"message_type":"summary","total_files_processed":2,"total_bytes_processed":20,"data_added":7,"snapshot_id":"partial"}'); exit 3"#,
        );
        let runner = executor(BTreeMap::new());

        let outcome = runner.execute_invocation(
            &invocation,
            Duration::from_secs(10),
            &CancellationToken::default(),
            false,
            |_| {},
        );

        assert_eq!(outcome.kind, BackupOutcomeKind::SucceededWithWarnings);
        assert_eq!(
            outcome.warning_code.as_deref(),
            Some("restic_partial_source")
        );
        assert_eq!(
            outcome.summary.expect("summary").snapshot_id.as_deref(),
            Some("partial")
        );
        assert_eq!(
            outcome.failure_details.items(),
            [r"C:\Users\Example\locked.txt"]
        );
        assert_eq!(outcome.failure_details.omitted(), 0);
        assert!(
            !format!("{:?}", outcome.failure_details).contains(r"C:\Users\Example\locked.txt"),
            "debug output must redact sensitive source paths"
        );
    }

    #[test]
    fn successful_restic_exit_reports_vss_live_fallback_as_a_warning() {
        let invocation = powershell_invocation(
            r#"[Console]::Error.WriteLine('{"message_type":"error","error":{"message":"failed to create snapshot for [c:\\]: VSS error: unexpected provider error"},"during":"archival","item":"c:\\"}'); [Console]::Out.WriteLine('{"message_type":"summary","total_files_processed":2,"total_bytes_processed":20,"data_added":7,"snapshot_id":"live-fallback"}')"#,
        );
        let runner = executor(BTreeMap::new());

        let outcome = runner.execute_invocation(
            &invocation,
            Duration::from_secs(10),
            &CancellationToken::default(),
            false,
            |_| {},
        );

        assert_eq!(outcome.kind, BackupOutcomeKind::SucceededWithWarnings);
        assert_eq!(outcome.warning_code.as_deref(), Some("restic_vss_fallback"));
        assert_eq!(
            outcome.summary.expect("summary").snapshot_id.as_deref(),
            Some("live-fallback")
        );
        assert!(outcome.failure_details.items().is_empty());
        assert_eq!(outcome.failure_details.omitted(), 0);
    }

    #[test]
    fn vss_fallback_remains_primary_while_partial_file_details_are_retained() {
        let invocation = powershell_invocation(
            r#"[Console]::Error.WriteLine('{"message_type":"error","error":{"message":"failed to create snapshot for [c:\\]: VSS error: unexpected provider error"},"during":"archival","item":"c:\\"}'); [Console]::Error.WriteLine('{"message_type":"error","error":{"message":"Access denied"},"during":"archival","item":"C:\\Users\\Example\\locked.txt"}'); [Console]::Out.WriteLine('{"message_type":"summary","total_files_processed":2,"total_bytes_processed":20,"data_added":7,"snapshot_id":"partial-live"}'); exit 3"#,
        );
        let runner = executor(BTreeMap::new());

        let outcome = runner.execute_invocation(
            &invocation,
            Duration::from_secs(10),
            &CancellationToken::default(),
            false,
            |_| {},
        );

        assert_eq!(outcome.kind, BackupOutcomeKind::SucceededWithWarnings);
        assert_eq!(
            outcome.warning_code.as_deref(),
            Some("restic_vss_fallback_and_partial_source")
        );
        assert_eq!(
            outcome.failure_details.items(),
            [r"C:\Users\Example\locked.txt"]
        );
    }

    #[test]
    fn known_unc_source_and_vss_cleanup_failure_are_never_plain_successes() {
        use std::os::windows::process::ExitStatusExt;

        let summary = BackupSummary {
            files_processed: 1,
            bytes_processed: 2,
            data_added: 3,
            snapshot_id: Some("snapshot".to_owned()),
        };
        let unc_outcome = finish_outcome(
            ExitStatus::from_raw(0),
            Ok(ParsedOutput {
                summary: Some(summary.clone()),
                ..ParsedOutput::default()
            }),
            Ok(ParsedOutput::default()),
            b"",
            true,
        );
        assert_eq!(unc_outcome.kind, BackupOutcomeKind::SucceededWithWarnings);
        assert_eq!(
            unc_outcome.warning_code.as_deref(),
            Some("restic_vss_fallback")
        );

        let cleanup_outcome = finish_outcome(
            ExitStatus::from_raw(0),
            Ok(ParsedOutput {
                summary: Some(summary),
                vss_cleanup_failed: true,
                ..ParsedOutput::default()
            }),
            Ok(ParsedOutput::default()),
            b"",
            false,
        );
        assert_eq!(
            cleanup_outcome.kind,
            BackupOutcomeKind::SucceededWithWarnings
        );
        assert_eq!(
            cleanup_outcome.warning_code.as_deref(),
            Some("restic_vss_cleanup_failed")
        );

        let combined_outcome = finish_outcome(
            ExitStatus::from_raw(0),
            Ok(ParsedOutput {
                summary: Some(BackupSummary {
                    files_processed: 1,
                    bytes_processed: 2,
                    data_added: 3,
                    snapshot_id: Some("combined".to_owned()),
                }),
                vss_fallback: true,
                vss_cleanup_failed: true,
                ..ParsedOutput::default()
            }),
            Ok(ParsedOutput::default()),
            b"",
            false,
        );
        assert_eq!(
            combined_outcome.warning_code.as_deref(),
            Some("restic_vss_fallback_and_cleanup_failed")
        );

        let partial_cleanup_outcome = finish_outcome(
            ExitStatus::from_raw(3),
            Ok(ParsedOutput {
                summary: Some(BackupSummary {
                    files_processed: 1,
                    bytes_processed: 2,
                    data_added: 3,
                    snapshot_id: Some("partial-cleanup".to_owned()),
                }),
                vss_cleanup_failed: true,
                ..ParsedOutput::default()
            }),
            Ok(ParsedOutput::default()),
            b"",
            false,
        );
        assert_eq!(
            partial_cleanup_outcome.warning_code.as_deref(),
            Some("restic_partial_source_and_vss_cleanup_failed")
        );

        let fallback_partial_cleanup_outcome = finish_outcome(
            ExitStatus::from_raw(3),
            Ok(ParsedOutput {
                summary: Some(BackupSummary {
                    files_processed: 1,
                    bytes_processed: 2,
                    data_added: 3,
                    snapshot_id: Some("fallback-partial-cleanup".to_owned()),
                }),
                vss_fallback: true,
                vss_cleanup_failed: true,
                ..ParsedOutput::default()
            }),
            Ok(ParsedOutput::default()),
            b"",
            false,
        );
        assert_eq!(
            fallback_partial_cleanup_outcome.warning_code.as_deref(),
            Some("restic_vss_fallback_partial_source_and_cleanup_failed")
        );
    }

    #[test]
    fn documented_error_and_plain_missing_source_messages_capture_only_bounded_items() {
        let (progress_tx, _progress_rx) = mpsc::sync_channel(1);
        let mut stderr = br#"{"message_type":"error","error":{"message":"failed to save a private file"},"during":"archival","item":"C:\\Users\\Example\\private.txt"}
C:\Missing Source does not exist, skipping
{"message_type":"error","error":{"message":"source name unavailable"},"during":"archival"}
"#
        .to_vec();
        for index in 0..=MAX_BACKUP_FAILED_ITEMS {
            stderr.extend_from_slice(
                format!(
                    "{{\"message_type\":\"error\",\"during\":\"archival\",\"item\":\"C:\\\\Data\\\\{index}.txt\"}}\n"
                )
                .as_bytes(),
            );
        }
        stderr.extend_from_slice(
            format!(
                "{{\"message_type\":\"error\",\"item\":\"{}\"}}\n",
                "x".repeat(MAX_BACKUP_FAILED_ITEM_BYTES + 1)
            )
            .as_bytes(),
        );

        let (parsed, captured) = read_stderr_output(stderr.as_slice(), &progress_tx);
        let parsed = parsed.expect("stderr is readable");

        assert_eq!(
            parsed.failure_details.items().len(),
            MAX_BACKUP_FAILED_ITEMS
        );
        assert_eq!(
            parsed.failure_details.items()[0],
            r"C:\Users\Example\private.txt"
        );
        assert_eq!(parsed.failure_details.items()[1], r"C:\Missing Source");
        assert_eq!(parsed.failure_details.omitted(), 5);
        assert_eq!(captured.len(), MAX_STDERR_BYTES.min(stderr.len()));
    }

    #[test]
    fn unsafe_failure_item_text_is_omitted_from_local_details() {
        let mut details = BackupFailureDetails::default();
        details.push("C:\\Data\\safe.txt".to_owned());
        details.push("C:\\Data\\safe.txt".to_owned());
        details.push("C:\\Data\\spoof\u{202e}txt.exe".to_owned());
        details.push("C:\\Data\\line\nbreak.txt".to_owned());

        assert_eq!(details.items(), [r"C:\Data\safe.txt"]);
        assert_eq!(details.omitted(), 2);
    }

    #[test]
    fn unresolved_secret_fails_before_starting_a_process() {
        let mut invocation = powershell_invocation("exit 0");
        invocation.secret_environment.insert(
            SecretEnvironmentVariable::ResticPassword,
            "missing".to_owned(),
        );
        let runner = executor(BTreeMap::new());

        let outcome = runner.execute_invocation(
            &invocation,
            Duration::from_secs(10),
            &CancellationToken::default(),
            false,
            |_| {},
        );

        assert_eq!(
            outcome.kind,
            BackupOutcomeKind::Failed {
                code: "credential_unavailable".to_owned()
            }
        );
    }

    #[test]
    fn dpapi_resolver_returns_a_stored_utf8_secret() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = DpapiSecretStore::open(directory.path().join("credentials"))
            .expect("store should open");
        store
            .put("repository-password", b"protected-value")
            .expect("secret should store");
        let resolver = DpapiSecretResolver::new(store);

        let secret = resolver
            .resolve("repository-password")
            .expect("secret should resolve");

        assert_eq!(secret.as_str(), "protected-value");
    }

    #[test]
    fn dpapi_resolver_rejects_non_environment_values() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = DpapiSecretStore::open(directory.path().join("credentials"))
            .expect("store should open");
        store
            .put("binary-value", &[0xff, 0xfe])
            .expect("binary value should store");
        store
            .put("nul-value", b"before\0after")
            .expect("nul value should store");
        let resolver = DpapiSecretResolver::new(store);

        assert!(resolver.resolve("binary-value").is_err());
        assert!(resolver.resolve("nul-value").is_err());
    }

    #[test]
    fn append_only_backup_plan_contains_only_stale_unlock_then_backup() {
        let mut config = EffectiveConfig::default();
        config.repository.url = Some("local:C:/backup".to_owned());
        config.repository.mode = RepositoryMode::AppendOnly;
        config.backup.paths = vec![PathBuf::from(r"C:\data")];

        let executable = OsString::from("restic.exe");
        let data_directory = PathBuf::from(r"C:\ProgramData\ResticPal");
        let (unlock, backup) =
            build_backup_invocations(executable.as_os_str(), &data_directory, &config)
                .expect("append-only backup plan");

        assert_eq!(unlock.operation, ResticOperation::Unlock);
        assert_eq!(unlock.arguments, [OsString::from("unlock")]);
        assert!(
            !unlock
                .arguments
                .iter()
                .any(|argument| argument == OsStr::new("--remove-all"))
        );
        assert_eq!(backup.operation, ResticOperation::Backup);
        let required_exclusion = [
            OsString::from("--iexclude"),
            data_directory.into_os_string(),
        ];
        assert!(
            backup
                .arguments
                .windows(required_exclusion.len())
                .any(|arguments| arguments == required_exclusion),
            "every service backup must exclude resticpal's internal data directory"
        );
        assert!(
            backup
                .arguments
                .iter()
                .any(|argument| argument == OsStr::new("backup"))
        );
        for invocation in [&unlock, &backup] {
            assert!(!invocation.arguments.iter().any(|argument| {
                matches!(
                    argument.to_string_lossy().as_ref(),
                    "forget" | "prune" | "rewrite" | "migrate" | "repair" | "key"
                )
            }));
        }
    }

    fn execute_real_backup(
        runner: &ResticExecutor,
        restic: &Path,
        config: &EffectiveConfig,
        cancellation: &CancellationToken,
        use_vss: bool,
    ) -> BackupOutcome {
        if use_vss {
            return runner.backup(config, cancellation, |_| {});
        }

        let (unlock, mut invocation) =
            build_backup_invocations(restic.as_os_str(), runner.data_directory.as_ref(), config)
                .expect("real backup invocations");
        assert_eq!(
            runner
                .execute_repository_invocation(&unlock, STALE_LOCK_CLEANUP_TIMEOUT, cancellation,)
                .kind,
            RepositoryOutcomeKind::Succeeded
        );
        let vss_flag = OsStr::new("--use-fs-snapshot");
        let position = invocation
            .arguments
            .iter()
            .position(|argument| argument == vss_flag)
            .expect("production invocation must request VSS");
        invocation.arguments.remove(position);
        assert!(
            !invocation
                .arguments
                .iter()
                .any(|argument| argument == vss_flag)
        );
        runner.execute_invocation(
            &invocation,
            Duration::from_secs(config.schedule.wake_lock_timeout_seconds),
            cancellation,
            false,
            |_| {},
        )
    }

    fn exercise_real_restic_local_repository(use_vss: bool) {
        let restic = real_restic_executable();
        let temporary = tempfile::tempdir().expect("temporary directory");
        // Hosted Windows runners can expose TEMP through an 8.3 spelling such
        // as RUNNER~1. The successful lifecycle must use the ordinary resolved
        // spelling; alias rejection is exercised separately below.
        let temporary_root = ordinary_windows_path(
            &fs::canonicalize(temporary.path()).expect("canonical temporary directory"),
        );
        let repository = temporary_root.join("repository");
        let source = temporary_root.join("source");
        let data_directory = source.join("ProgramData").join("ResticPal");
        let cache_directory = data_directory.join("Cache");
        fs::create_dir(&source).expect("source directory");
        fs::create_dir_all(&cache_directory).expect("protected cache fixture");
        let stale_cache_directory = cache_directory.join("0".repeat(64));
        fs::create_dir(&stale_cache_directory).expect("stale cache namespace fixture");
        age_cache_directory_for_cleanup(&stale_cache_directory);
        fs::write(source.join("document.txt"), b"first version\n").expect("initial source file");
        fs::write(source.join("report [2025].txt"), b"glob-like literal\n")
            .expect("glob-like source filename");
        let nested_source = source.join("folder [one]");
        fs::create_dir(&nested_source).expect("nested source directory");
        fs::write(
            nested_source.join("r\u{00e9}sum\u{00e9}.txt"),
            b"nested unicode document\n",
        )
        .expect("nested Unicode source filename");
        let unsafe_source = source.join("unsafe links");
        fs::create_dir(&unsafe_source).expect("unsafe-node source directory");
        let unsafe_link_created = match std::os::windows::fs::symlink_file(
            source.join("document.txt"),
            unsafe_source.join("hidden-link.txt"),
        ) {
            Ok(()) => true,
            Err(error) => {
                eprintln!("skipping real-restic symbolic-link fixture: {error}");
                false
            }
        };
        let internal_sentinel = "resticpal-internal-sentinel-must-never-be-backed-up.txt";
        fs::write(data_directory.join(internal_sentinel), b"internal state\n")
            .expect("internal data exclusion fixture");

        let aliased_source = volume_guid_alias(&source);
        let mut config = local_repository_config(&repository, &aliased_source);
        let runner = real_restic_executor(&restic, &data_directory, "correct horse battery staple");
        let cancellation = CancellationToken::default();

        assert_eq!(
            runner.backup(&config, &cancellation, |_| {}).kind,
            BackupOutcomeKind::Failed {
                code: "backup_source_namespace_unsupported".to_owned()
            },
            "a volume-GUID source alias must fail closed instead of changing the exclusion namespace"
        );
        config.backup.paths = vec![PathBuf::from(source.to_string_lossy().to_ascii_uppercase())];

        let configured_sources = config.backup.paths.clone();
        config.backup.paths = vec![
            fs::canonicalize(data_directory.join(internal_sentinel))
                .expect("canonical protected-file source fixture"),
        ];
        assert_eq!(
            runner.backup(&config, &cancellation, |_| {}).kind,
            BackupOutcomeKind::Failed {
                code: "protected_backup_source".to_owned()
            },
            "an explicitly named internal file must be rejected before restic can bypass excludes"
        );
        config.backup.paths = configured_sources;

        assert_eq!(
            runner
                .repository_operation(&config, ResticOperation::Probe, &cancellation)
                .kind,
            RepositoryOutcomeKind::Failed {
                code: "repository_not_found".to_owned()
            }
        );
        assert_eq!(
            runner
                .repository_operation(&config, ResticOperation::Initialize, &cancellation)
                .kind,
            RepositoryOutcomeKind::Succeeded
        );
        assert_eq!(
            runner
                .repository_operation(&config, ResticOperation::Probe, &cancellation)
                .kind,
            RepositoryOutcomeKind::Succeeded
        );

        let wrong_password = real_restic_executor(&restic, &data_directory, "definitely wrong");
        assert_eq!(
            wrong_password
                .repository_operation(&config, ResticOperation::Probe, &cancellation)
                .kind,
            RepositoryOutcomeKind::Failed {
                code: "restic_authentication_failed".to_owned()
            }
        );

        config.repository.mode = RepositoryMode::AppendOnly;
        leave_stale_restic_lock(&restic, &repository, "correct horse battery staple");
        let first = execute_real_backup(&runner, &restic, &config, &cancellation, use_vss);
        assert_eq!(first.kind, BackupOutcomeKind::Succeeded);
        assert_eq!(
            repository_lock_count(&repository),
            0,
            "the backup preflight must remove the stale lock and its own lock"
        );
        let first_summary = first.summary.expect("first backup summary");
        assert!(first_summary.files_processed >= 1);
        assert!(first_summary.bytes_processed >= 14);
        assert!(first_summary.data_added > 0);
        assert!(first_summary.snapshot_id.is_some());
        let first_snapshot_id = first_summary
            .snapshot_id
            .as_deref()
            .expect("first snapshot id");
        let listing = restic_snapshot_listing(
            &restic,
            &repository,
            &cache_directory,
            "correct horse battery staple",
            first_snapshot_id,
        );
        assert!(
            !String::from_utf8_lossy(&listing).contains(internal_sentinel),
            "the service-owned data directory must be excluded even when it is under a source"
        );
        assert!(
            cache_directory.join("CACHEDIR.TAG").is_file(),
            "restic must mark its explicit cache directory"
        );
        assert!(
            !stale_cache_directory.exists(),
            "backup-time cache cleanup must remove an obsolete repository namespace"
        );
        let first_cache_directories = repository_cache_directories(&cache_directory);
        assert_eq!(
            first_cache_directories.len(),
            1,
            "one repository must produce exactly one reusable cache namespace"
        );

        let missing_source = temporary.path().join("missing-source");
        config.backup.paths.push(missing_source.clone());
        let partial = execute_real_backup(&runner, &restic, &config, &cancellation, use_vss);
        assert_eq!(partial.kind, BackupOutcomeKind::SucceededWithWarnings);
        assert_eq!(
            partial.warning_code.as_deref(),
            Some("restic_partial_source")
        );
        assert_eq!(
            partial.failure_details.items(),
            [missing_source.to_string_lossy().into_owned()]
        );
        config.backup.paths.pop();

        let builder = ResticCommandBuilder::new(&restic);
        for operation in [ResticOperation::Snapshots, ResticOperation::Check] {
            let invocation = builder
                .inspection(&config, operation)
                .expect("append-only inspection invocation");
            assert_eq!(
                runner
                    .execute_repository_invocation(
                        &invocation,
                        Duration::from_secs(60),
                        &cancellation,
                    )
                    .kind,
                RepositoryOutcomeKind::Succeeded
            );
        }

        fs::write(
            source.join("document.txt"),
            b"second version with changed content\n",
        )
        .expect("changed source file");
        fs::write(source.join("another.txt"), b"another file\n").expect("second source file");
        let second = execute_real_backup(&runner, &restic, &config, &cancellation, use_vss);
        assert_eq!(second.kind, BackupOutcomeKind::Succeeded);
        let second_summary = second.summary.expect("second backup summary");
        assert!(second_summary.files_processed >= 2);
        assert!(second_summary.data_added > 0);
        assert!(second_summary.snapshot_id.is_some());
        assert_ne!(first_summary.snapshot_id, second_summary.snapshot_id);
        assert_eq!(
            repository_cache_directories(&cache_directory),
            first_cache_directories,
            "successive backups must reuse the repository's persistent cache namespace"
        );

        let mut lowercase_source = source.to_string_lossy().into_owned();
        let lowercase_drive = lowercase_source
            .chars()
            .next()
            .expect("absolute Windows source drive")
            .to_ascii_lowercase();
        lowercase_source.replace_range(0..1, &lowercase_drive.to_string());
        config.backup.paths = vec![PathBuf::from(&lowercase_source)];
        let lowercase_backup =
            execute_real_backup(&runner, &restic, &config, &cancellation, use_vss);
        assert_eq!(lowercase_backup.kind, BackupOutcomeKind::Succeeded);
        let lowercase_snapshot_id = lowercase_backup
            .summary
            .and_then(|summary| summary.snapshot_id)
            .expect("lowercase-drive exact snapshot id");

        let snapshots = runner
            .list_snapshots(&config, &cancellation)
            .expect("bounded current-host append-only snapshot query");
        assert!(snapshots.len() >= 3);
        assert!(
            snapshots
                .windows(2)
                .all(|pair| pair[0].time >= pair[1].time)
        );
        assert!(snapshots.iter().all(|snapshot| {
            snapshot.hostname.eq_ignore_ascii_case(
                &current_restic_hostname().expect("restic physical DNS hostname"),
            )
        }));
        let lowercase_snapshot = snapshots
            .iter()
            .find(|snapshot| snapshot.id == lowercase_snapshot_id)
            .expect("current-host lowercase-drive snapshot");
        assert!(
            lowercase_snapshot
                .paths
                .iter()
                .any(|path| path.starts_with(&format!("{lowercase_drive}:"))),
            "snapshot source metadata must preserve the configured drive-letter case"
        );
        let snapshot_id = lowercase_snapshot_id.as_str();
        let mut directory = "/".to_owned();
        let source_entries = loop {
            let entries = runner
                .list_directory(&config, snapshot_id, &directory, &cancellation)
                .expect("lazy one-directory snapshot listing");
            if directory == "/" {
                assert!(
                    entries
                        .iter()
                        .any(|entry| entry.path == format!("/{lowercase_drive}")),
                    "snapshot directory browsing must preserve its actual lowercase drive path"
                );
            }
            if entries.iter().any(|entry| entry.name == "document.txt") {
                break entries;
            }
            let children: Vec<_> = entries
                .iter()
                .filter(|entry| entry.node_type == RestoreNodeType::Directory)
                .collect();
            assert_eq!(
                children.len(),
                1,
                "snapshot source ancestors should be unambiguous"
            );
            directory = children[0].path.clone();
        };
        let recovered = temporary_root.join("recovered");
        fs::create_dir(&recovered).expect("restore destination parent");
        let sentinel = recovered.join("never-overwrite.txt");
        fs::write(&sentinel, b"keep me\n").expect("unrelated restore parent sentinel");
        if unsafe_link_created {
            let unsafe_entry = source_entries
                .iter()
                .find(|entry| entry.name == "unsafe links")
                .expect("unsafe directory must remain visible without exposing its link");
            let original_destination_entries = fs::read_dir(&recovered)
                .expect("original restore parent contents")
                .count();
            let rejected = runner.restore(
                &config,
                snapshot_id,
                &unsafe_entry.path,
                &recovered,
                &cancellation,
                |_| {},
            );
            assert_eq!(
                rejected.kind,
                RestoreOutcomeKind::Failed {
                    code: "restore_unsupported_node".to_owned()
                },
                "a hidden symbolic-link descendant must fail the real-restic subtree preflight"
            );
            assert!(
                rejected.destination.is_none(),
                "unsafe-subtree rejection must occur before allocating a restore destination"
            );
            assert_eq!(
                fs::read_dir(&recovered)
                    .expect("restore parent contents after unsafe rejection")
                    .count(),
                original_destination_entries,
                "unsafe-subtree rejection must not create any output directory"
            );
            assert_eq!(
                fs::read(&sentinel).expect("sentinel after unsafe rejection"),
                b"keep me\n"
            );
        }
        for (name, expected_relative, expected_content) in [
            (
                "report [2025].txt",
                PathBuf::from("report [2025].txt"),
                b"glob-like literal\n".as_slice(),
            ),
            (
                "folder [one]",
                PathBuf::from("folder [one]").join("r\u{00e9}sum\u{00e9}.txt"),
                b"nested unicode document\n".as_slice(),
            ),
        ] {
            let entry = source_entries
                .iter()
                .find(|entry| entry.name == name)
                .expect("selected snapshot file or directory");
            let mut initial_destination = None;
            let restored = runner.restore(
                &config,
                snapshot_id,
                &entry.path,
                &recovered,
                &cancellation,
                |progress| {
                    if initial_destination.is_none() {
                        initial_destination = progress.destination;
                    }
                },
            );
            assert_eq!(
                restored.kind,
                RestoreOutcomeKind::Succeeded,
                "restoring {name}"
            );
            assert!(
                restored.files_restored >= 1,
                "restoring {name} must report at least one recovered file"
            );
            assert!(
                restored.bytes_restored >= u64::try_from(expected_content.len()).unwrap(),
                "restoring {name} must report its recovered byte count"
            );
            let destination = restored.destination.expect("unique restored destination");
            assert_eq!(initial_destination, Some(destination.clone()));
            assert_eq!(
                fs::read(destination.join(expected_relative)).expect("restored contents"),
                expected_content,
                "restoring {name} must select only its literal subtree"
            );
            assert_eq!(repository_lock_count(&repository), 0);
        }
        assert_eq!(
            fs::read(sentinel).expect("preserved sentinel"),
            b"keep me\n"
        );
        assert_eq!(
            runner
                .restore(
                    &config,
                    snapshot_id,
                    &source_entries[0].path,
                    &data_directory,
                    &cancellation,
                    |_| {},
                )
                .kind,
            RestoreOutcomeKind::Failed {
                code: "restore_destination_protected".to_owned()
            }
        );

        config.repository.mode = RepositoryMode::Standard;
        assert_eq!(
            runner.retention(&config, true, &cancellation).kind,
            RetentionOutcomeKind::Succeeded { pruned: true }
        );
    }

    #[test]
    #[ignore = "requires RESTICPAL_TEST_RESTIC and creates a disposable real repository"]
    fn real_restic_local_repository_lifecycle_without_vss() {
        exercise_real_restic_local_repository(false);
    }

    #[test]
    #[ignore = "requires an elevated token, RESTICPAL_TEST_RESTIC, and VSS"]
    fn real_restic_vss_local_repository_lifecycle() {
        exercise_real_restic_local_repository(true);
    }
}
