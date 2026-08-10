//! Restic child-process execution with bounded output parsing and cancellation.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read};
use std::mem::size_of;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use resticpal_core::config::{EffectiveConfig, SecretEnvironmentVariable};
use resticpal_core::restic::{ResticCommandBuilder, ResticInvocation, ResticOperation};
use resticpal_core::status::BackupProgress;
use resticpal_windows::credentials::DpapiSecretStore;
use serde::Deserialize;
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
    SetInformationJobObject, TerminateJobObject,
};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;
use windows::core::PCWSTR;
use zeroize::Zeroizing;

use crate::power_request::TimedSystemPowerRequest;

const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(100);
const REPOSITORY_OPERATION_TIMEOUT: Duration = Duration::from_secs(2 * 60);
const MAX_JSON_LINE_BYTES: usize = 1024 * 1024;
const MAX_STDERR_BYTES: usize = 64 * 1024;
const MAX_PENDING_PROGRESS_EVENTS: usize = 16;
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

    fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
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
    secrets: Arc<dyn SecretResolver>,
    wake_locks: Arc<dyn WakeLockProvider>,
}

impl ResticExecutor {
    pub fn new(
        executable: impl Into<OsString>,
        secrets: Arc<dyn SecretResolver>,
        wake_locks: Arc<dyn WakeLockProvider>,
    ) -> Self {
        Self {
            executable: Arc::new(executable.into()),
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
        let invocation = match ResticCommandBuilder::new(self.executable.as_ref()).backup(config) {
            Ok(invocation) => invocation,
            Err(_) => return BackupOutcome::failed("invalid_configuration"),
        };

        self.execute_invocation(
            &invocation,
            Duration::from_secs(config.schedule.wake_lock_timeout_seconds),
            cancellation,
            on_progress,
        )
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
        let mut command = command_for(invocation, &secret_environment);
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
        let started = Instant::now();
        let mut cancelled = false;
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    if cancellation.is_cancelled() && !cancelled {
                        cancelled = true;
                        let _ = job.terminate();
                    } else if started.elapsed() >= timeout && !timed_out {
                        timed_out = true;
                        let _ = job.terminate();
                    }
                    thread::sleep(PROCESS_POLL_INTERVAL);
                }
                Err(_) => {
                    let _ = job.terminate();
                    break Err(());
                }
            }
        };
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

    fn execute_invocation(
        &self,
        invocation: &ResticInvocation,
        wake_lock_timeout: Duration,
        cancellation: &CancellationToken,
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
        let mut command = command_for(invocation, &secret_environment);
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
        let stdout_thread = thread::spawn(move || read_json_output(stdout, &output_tx));
        let stderr_thread = thread::spawn(move || read_bounded(stderr, MAX_STDERR_BYTES));

        let mut cancelled = false;
        let status = loop {
            collect_progress_events(&output_rx, &mut on_progress);

            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    if cancellation.is_cancelled() && !cancelled {
                        cancelled = true;
                        let _ = job.terminate();
                    }
                    thread::sleep(PROCESS_POLL_INTERVAL);
                }
                Err(_) => {
                    let _ = job.terminate();
                    break Err(());
                }
            }
        };

        let output_result = stdout_thread.join().unwrap_or(Err(OutputReadError));
        let stderr = stderr_thread.join().unwrap_or_default();
        collect_progress_events(&output_rx, &mut on_progress);
        if cancelled {
            return BackupOutcome::cancelled();
        }
        let status = match status {
            Ok(status) => status,
            Err(()) => return BackupOutcome::failed("restic_wait_failed"),
        };
        finish_outcome(status, output_result, &stderr)
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

fn command_for(
    invocation: &ResticInvocation,
    secrets: &[(SecretEnvironmentVariable, Zeroizing<String>)],
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
}

impl BackupOutcome {
    pub(crate) fn succeeded(summary: BackupSummary) -> Self {
        Self {
            kind: BackupOutcomeKind::Succeeded,
            summary: Some(summary),
            warning_code: None,
        }
    }

    pub(crate) fn warnings(summary: BackupSummary, code: impl Into<String>) -> Self {
        Self {
            kind: BackupOutcomeKind::SucceededWithWarnings,
            summary: Some(summary),
            warning_code: Some(code.into()),
        }
    }

    pub(crate) fn failed(code: impl Into<String>) -> Self {
        Self {
            kind: BackupOutcomeKind::Failed { code: code.into() },
            summary: None,
            warning_code: None,
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self {
            kind: BackupOutcomeKind::Cancelled,
            summary: None,
            warning_code: None,
        }
    }

    pub(crate) fn with_warning(mut self, code: impl Into<String>) -> Self {
        if matches!(
            self.kind,
            BackupOutcomeKind::Succeeded | BackupOutcomeKind::SucceededWithWarnings
        ) {
            self.kind = BackupOutcomeKind::SucceededWithWarnings;
            self.warning_code = Some(code.into());
        }
        self
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
    stderr: &[u8],
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
            Some(0) if parsed.invalid_message => {
                BackupOutcome::failed("restic_output_invalid")
            }
            Some(0) => BackupOutcome::failed("restic_summary_missing"),
            Some(code) => BackupOutcome::failed(
                classify_stderr(stderr)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("restic_exit_{code}")),
            ),
            None => BackupOutcome::failed("restic_terminated"),
        };
    };
    match status.code() {
        Some(0) => BackupOutcome::succeeded(summary),
        Some(3) => BackupOutcome::warnings(summary, "restic_partial_source"),
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

fn classify_stderr(stderr: &[u8]) -> Option<&'static str> {
    let message = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    if message.contains("access denied")
        || message.contains("permission denied")
        || message.contains("insufficient privilege")
    {
        Some("restic_permission_denied")
    } else if message.contains("shadow copy")
        || message.contains("volume shadow")
        || message.contains("vss")
    {
        Some("restic_vss_unavailable")
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
}

#[derive(Debug, Default)]
struct ParsedOutput {
    summary: Option<BackupSummary>,
    invalid_message: bool,
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
            Ok(None) => {}
            Err(()) => parsed.invalid_message = true,
        }
    }
    Ok(parsed)
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
}

fn parse_output_event(line: &[u8]) -> Result<Option<OutputEvent>, ()> {
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
        _ => Ok(None),
    }
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

    use super::*;

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

    fn real_restic_executor(executable: &Path, password: &str) -> ResticExecutor {
        ResticExecutor::new(
            executable.as_os_str().to_os_string(),
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
        let mut output =
            br#"{"message_type":"status","files_done":1,"note":""#.to_vec();
        output.resize(output.len() + MAX_JSON_LINE_BYTES + 16, b'x');
        output.extend_from_slice(br#""}"#);
        output.push(b'\n');
        output.extend_from_slice(
            br#"{"message_type":"summary","total_files_processed":3,"total_bytes_processed":4,"data_added":5,"snapshot_id":"ok"}"#,
        );
        output.push(b'\n');
        let (progress_tx, _progress_rx) = mpsc::sync_channel(1);

        let parsed = read_json_output(output.as_slice(), &progress_tx).expect("read output");
        assert!(parsed.invalid_message, "oversized line marks invalid_message");
        assert!(parsed.summary.is_some(), "summary still parses");

        let outcome = finish_outcome(ExitStatus::from_raw(0), Ok(parsed), b"");
        assert_eq!(outcome.kind, BackupOutcomeKind::Succeeded);
        assert_eq!(
            outcome.summary.expect("summary").snapshot_id.as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn fake_process_reports_progress_and_success_without_leaking_secret_arguments() {
        let mut invocation = powershell_invocation(
            r#"if ($env:RESTIC_PASSWORD -ne 'test-secret') { exit 12 }; [Console]::Out.WriteLine('{"message_type":"status","percent_done":0.5,"total_files":2,"files_done":1,"total_bytes":20,"bytes_done":10}'); [Console]::Out.WriteLine('{"message_type":"summary","total_files_processed":2,"total_bytes_processed":20,"data_added":7,"snapshot_id":"snapshot-1"}')"#,
        );
        invocation.secret_environment.insert(
            SecretEnvironmentVariable::ResticPassword,
            "password-ref".to_owned(),
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

        let outcome =
            runner.execute_invocation(&invocation, Duration::from_secs(10), &cancellation, |_| {});
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
        invocation.operation = ResticOperation::Initialize;
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
    fn restic_partial_source_exit_is_a_success_with_warnings() {
        let invocation = powershell_invocation(
            r#"[Console]::Out.WriteLine('{"message_type":"summary","total_files_processed":2,"total_bytes_processed":20,"data_added":7,"snapshot_id":"partial"}'); exit 3"#,
        );
        let runner = executor(BTreeMap::new());

        let outcome = runner.execute_invocation(
            &invocation,
            Duration::from_secs(10),
            &CancellationToken::default(),
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
    fn append_only_configuration_still_builds_only_a_backup_operation() {
        let mut config = EffectiveConfig::default();
        config.repository.url = Some("local:C:/backup".to_owned());
        config.repository.mode = RepositoryMode::AppendOnly;
        config.backup.paths = vec![PathBuf::from(r"C:\data")];

        let invocation = ResticCommandBuilder::new("restic.exe")
            .backup(&config)
            .expect("append-only permits backup");

        assert_eq!(invocation.operation, ResticOperation::Backup);
        assert!(
            invocation
                .arguments
                .iter()
                .any(|argument| argument == OsStr::new("backup"))
        );
        assert!(!invocation.arguments.iter().any(|argument| {
            matches!(
                argument.to_string_lossy().as_ref(),
                "forget" | "prune" | "rewrite"
            )
        }));
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

        let mut invocation = ResticCommandBuilder::new(restic)
            .backup(config)
            .expect("real backup invocation");
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
            |_| {},
        )
    }

    fn exercise_real_restic_local_repository(use_vss: bool) {
        let restic = real_restic_executable();
        let temporary = tempfile::tempdir().expect("temporary directory");
        let repository = temporary.path().join("repository");
        let source = temporary.path().join("source");
        fs::create_dir(&source).expect("source directory");
        fs::write(source.join("document.txt"), b"first version\n").expect("initial source file");

        let mut config = local_repository_config(&repository, &source);
        let runner = real_restic_executor(&restic, "correct horse battery staple");
        let cancellation = CancellationToken::default();

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

        let wrong_password = real_restic_executor(&restic, "definitely wrong");
        assert_eq!(
            wrong_password
                .repository_operation(&config, ResticOperation::Probe, &cancellation)
                .kind,
            RepositoryOutcomeKind::Failed {
                code: "restic_authentication_failed".to_owned()
            }
        );

        config.repository.mode = RepositoryMode::AppendOnly;
        let first = execute_real_backup(&runner, &restic, &config, &cancellation, use_vss);
        assert_eq!(first.kind, BackupOutcomeKind::Succeeded);
        let first_summary = first.summary.expect("first backup summary");
        assert!(first_summary.files_processed >= 1);
        assert!(first_summary.bytes_processed >= 14);
        assert!(first_summary.data_added > 0);
        assert!(first_summary.snapshot_id.is_some());

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
