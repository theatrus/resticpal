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
use std::time::Duration;

use resticpal_core::config::{EffectiveConfig, SecretEnvironmentVariable};
use resticpal_core::restic::{ResticCommandBuilder, ResticInvocation};
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
const MAX_JSON_LINE_BYTES: usize = 1024 * 1024;
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
        let value = String::from_utf8(bytes.to_vec()).map_err(|_| SecretResolveError)?;
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
        let (output_tx, output_rx) = mpsc::channel();
        let stdout_thread = thread::spawn(move || read_json_output(stdout, &output_tx));
        let stderr_thread = thread::spawn(move || drain(stderr));

        let mut cancelled = false;
        let mut parsed = ParsedOutput::default();
        let status = loop {
            collect_output_events(&output_rx, &mut on_progress, &mut parsed);
            if cancellation.is_cancelled() && !cancelled {
                cancelled = true;
                let _ = job.terminate();
            }

            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => thread::sleep(PROCESS_POLL_INTERVAL),
                Err(_) => {
                    let _ = job.terminate();
                    break Err(());
                }
            }
        };

        let output_result = stdout_thread.join().unwrap_or(Err(OutputReadError));
        let _ = stderr_thread.join();
        collect_output_events(&output_rx, &mut on_progress, &mut parsed);
        if cancelled {
            return BackupOutcome::cancelled();
        }
        let status = match status {
            Ok(status) => status,
            Err(()) => return BackupOutcome::failed("restic_wait_failed"),
        };
        finish_outcome(status, output_result, parsed)
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
}

impl BackupOutcome {
    pub(crate) fn succeeded(summary: BackupSummary) -> Self {
        Self {
            kind: BackupOutcomeKind::Succeeded,
            summary: Some(summary),
        }
    }

    pub(crate) fn warnings(summary: BackupSummary) -> Self {
        Self {
            kind: BackupOutcomeKind::SucceededWithWarnings,
            summary: Some(summary),
        }
    }

    pub(crate) fn failed(code: &str) -> Self {
        Self {
            kind: BackupOutcomeKind::Failed {
                code: code.to_owned(),
            },
            summary: None,
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self {
            kind: BackupOutcomeKind::Cancelled,
            summary: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackupOutcomeKind {
    Succeeded,
    SucceededWithWarnings,
    Failed { code: String },
    Cancelled,
}

fn finish_outcome(
    status: ExitStatus,
    output_result: Result<(), OutputReadError>,
    parsed: ParsedOutput,
) -> BackupOutcome {
    if status.code() == Some(130) {
        return BackupOutcome::cancelled();
    }
    if output_result.is_err() || parsed.invalid_message {
        return BackupOutcome::failed("restic_output_invalid");
    }

    let Some(summary) = parsed.summary else {
        return BackupOutcome::failed(if status.success() {
            "restic_summary_missing"
        } else {
            "restic_failed"
        });
    };
    match status.code() {
        Some(0) => BackupOutcome::succeeded(summary),
        Some(3) => BackupOutcome::warnings(summary),
        Some(code) => BackupOutcome::failed(&format!("restic_exit_{code}")),
        None => BackupOutcome::failed("restic_terminated"),
    }
}

#[derive(Debug)]
enum OutputEvent {
    Progress(BackupProgress),
    Summary(BackupSummary),
    Invalid,
}

#[derive(Debug, Default)]
struct ParsedOutput {
    summary: Option<BackupSummary>,
    invalid_message: bool,
}

fn collect_output_events(
    events: &mpsc::Receiver<OutputEvent>,
    on_progress: &mut impl FnMut(BackupProgress),
    parsed: &mut ParsedOutput,
) {
    for event in events.try_iter() {
        match event {
            OutputEvent::Progress(progress) => on_progress(progress),
            OutputEvent::Summary(summary) => parsed.summary = Some(summary),
            OutputEvent::Invalid => parsed.invalid_message = true,
        }
    }
}

fn read_json_output(
    stdout: impl Read,
    events: &mpsc::Sender<OutputEvent>,
) -> Result<(), OutputReadError> {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    while let Some(valid) =
        read_bounded_line(&mut reader, &mut line).map_err(|_| OutputReadError)?
    {
        if !valid {
            let _ = events.send(OutputEvent::Invalid);
            continue;
        }
        match parse_output_event(&line) {
            Ok(Some(event)) => {
                let _ = events.send(event);
            }
            Ok(None) => {}
            Err(()) => {
                let _ = events.send(OutputEvent::Invalid);
            }
        }
    }
    Ok(())
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
        unsafe {
            SetInformationJobObject(
                handle,
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                u32::try_from(size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>())
                    .expect("job information size fits in u32"),
            )
        }?;
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
    use std::path::PathBuf;
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
}
