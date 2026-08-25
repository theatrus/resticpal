use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf, Prefix};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{
    ConfigValidationError, EffectiveConfig, MAX_PATH_CHARACTERS, RepositoryMode,
    SecretEnvironmentVariable, repository_option_disables_source_snapshots,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResticOperation {
    Backup,
    Unlock,
    Probe,
    Snapshots,
    List,
    Restore,
    Check,
    Initialize,
    Forget,
    Prune,
    Rewrite,
    Migrate,
    Repair,
    RemoveKey,
}

impl ResticOperation {
    #[must_use]
    pub const fn allowed_in(self, mode: RepositoryMode) -> bool {
        match mode {
            RepositoryMode::Standard => true,
            RepositoryMode::AppendOnly => {
                matches!(
                    self,
                    Self::Backup
                        | Self::Unlock
                        | Self::Probe
                        | Self::Snapshots
                        | Self::List
                        | Self::Restore
                        | Self::Check
                )
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResticInvocation {
    pub operation: ResticOperation,
    pub executable: PathBuf,
    pub arguments: Vec<OsString>,
    pub environment: BTreeMap<OsString, OsString>,
    pub secret_environment: BTreeMap<SecretEnvironmentVariable, String>,
}

#[derive(Debug, Clone)]
pub struct ResticCommandBuilder {
    executable: PathBuf,
}

impl ResticCommandBuilder {
    #[must_use]
    pub fn new(executable: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
        }
    }

    pub fn backup(&self, config: &EffectiveConfig) -> Result<ResticInvocation, InvocationError> {
        self.backup_with_required_exclusions(config, &[])
    }

    /// Builds a backup invocation with exclusions enforced by the caller.
    ///
    /// Required exclusions are appended after the configurable exclusions and
    /// before every backup source. Keeping them outside [`EffectiveConfig`]
    /// prevents local or managed configuration from removing service-owned
    /// safety exclusions such as the application's internal data directory.
    pub fn backup_with_required_exclusions(
        &self,
        config: &EffectiveConfig,
        exclusions: &[PathBuf],
    ) -> Result<ResticInvocation, InvocationError> {
        authorize_operation(config.repository.mode, ResticOperation::Backup)?;
        reject_snapshot_disabling_options(config)?;
        config.validate()?;

        let repository = config
            .repository
            .url
            .as_ref()
            .ok_or(InvocationError::MissingRepository)?;
        if config.backup.paths.is_empty() {
            return Err(InvocationError::NoBackupPaths);
        }
        if config.backup.paths.iter().any(|source| {
            exclusions
                .iter()
                .any(|protected| windows_path_is_same_or_descendant(source, protected))
        }) {
            // Restic deliberately does not apply exclude patterns to an
            // explicitly named source leaf. Rejecting protected sources here
            // keeps a direct Credentials\<blob> source from bypassing the
            // mandatory directory exclusion.
            return Err(InvocationError::ProtectedBackupSource);
        }

        let mut arguments = repository_options(config);
        // Restic caches data per repository. Clean only obsolete local cache
        // namespaces as part of a backup so repositories removed from policy do
        // not leave unbounded service-owned state behind.
        arguments.push("--cleanup-cache".into());
        arguments.push("backup".into());
        arguments.push("--json".into());
        arguments.push("--use-fs-snapshot".into());

        for exclusion in &config.backup.exclusions {
            arguments.push("--exclude".into());
            arguments.push(exclusion.into());
        }
        for exclusion in exclusions {
            // Windows paths are case-insensitive even when the source spelling
            // differs from the canonical ProgramData casing. A case-sensitive
            // rule can therefore be bypassed by selecting the same directory
            // through a differently cased parent path.
            arguments.push("--iexclude".into());
            arguments.push(exclusion.as_os_str().to_os_string());
        }
        arguments.extend(
            config
                .backup
                .paths
                .iter()
                .map(|path| path.as_os_str().to_os_string()),
        );

        Ok(ResticInvocation {
            operation: ResticOperation::Backup,
            executable: self.executable.clone(),
            arguments,
            environment: BTreeMap::from([(
                OsString::from("RESTIC_REPOSITORY"),
                OsString::from(repository),
            )]),
            secret_environment: config.repository.secret_refs.clone(),
        })
    }

    /// Builds the narrow lock cleanup operation used before every backup.
    ///
    /// Restic's plain `unlock` command removes only locks it classifies as
    /// stale. Deliberately omit `--remove-all`: an active lock owned by another
    /// client must continue to block the subsequent backup.
    pub fn unlock(&self, config: &EffectiveConfig) -> Result<ResticInvocation, InvocationError> {
        authorize_operation(config.repository.mode, ResticOperation::Unlock)?;
        config.validate()?;

        let repository = config
            .repository
            .url
            .as_ref()
            .ok_or(InvocationError::MissingRepository)?;
        let mut arguments = repository_options(config);
        arguments.push("unlock".into());

        Ok(ResticInvocation {
            operation: ResticOperation::Unlock,
            executable: self.executable.clone(),
            arguments,
            environment: BTreeMap::from([(
                OsString::from("RESTIC_REPOSITORY"),
                OsString::from(repository),
            )]),
            secret_environment: config.repository.secret_refs.clone(),
        })
    }

    pub fn inspection(
        &self,
        config: &EffectiveConfig,
        operation: ResticOperation,
    ) -> Result<ResticInvocation, InvocationError> {
        if !matches!(
            operation,
            ResticOperation::Snapshots | ResticOperation::Check
        ) {
            return Err(InvocationError::NotInspectionOperation(operation));
        }
        authorize_operation(config.repository.mode, operation)?;
        config.validate()?;
        let repository = config
            .repository
            .url
            .as_ref()
            .ok_or(InvocationError::MissingRepository)?;

        let mut arguments = repository_options(config);
        match operation {
            ResticOperation::Snapshots => {
                arguments.push("snapshots".into());
                arguments.push("--json".into());
            }
            ResticOperation::Check => {
                arguments.push("check".into());
                arguments.push("--json".into());
            }
            _ => unreachable!("operation was checked above"),
        }

        Ok(ResticInvocation {
            operation,
            executable: self.executable.clone(),
            arguments,
            environment: BTreeMap::from([(
                OsString::from("RESTIC_REPOSITORY"),
                OsString::from(repository),
            )]),
            secret_environment: config.repository.secret_refs.clone(),
        })
    }

    pub fn repository_setup(
        &self,
        config: &EffectiveConfig,
        operation: ResticOperation,
    ) -> Result<ResticInvocation, InvocationError> {
        if !matches!(
            operation,
            ResticOperation::Probe | ResticOperation::Initialize
        ) {
            return Err(InvocationError::NotRepositorySetupOperation(operation));
        }
        authorize_operation(config.repository.mode, operation)?;
        config.validate()?;
        let repository = config
            .repository
            .url
            .as_ref()
            .ok_or(InvocationError::MissingRepository)?;

        let mut arguments = repository_options(config);
        match operation {
            ResticOperation::Probe => {
                arguments.push("cat".into());
                arguments.push("config".into());
            }
            ResticOperation::Initialize => arguments.push("init".into()),
            _ => unreachable!("operation was checked above"),
        }

        Ok(ResticInvocation {
            operation,
            executable: self.executable.clone(),
            arguments,
            environment: BTreeMap::from([(
                OsString::from("RESTIC_REPOSITORY"),
                OsString::from(repository),
            )]),
            secret_environment: config.repository.secret_refs.clone(),
        })
    }

    /// Lists exactly one directory from one unambiguous repository snapshot.
    pub fn directory_listing(
        &self,
        config: &EffectiveConfig,
        snapshot_id: &str,
        path: &str,
    ) -> Result<ResticInvocation, InvocationError> {
        authorize_operation(config.repository.mode, ResticOperation::List)?;
        validate_restore_snapshot_id(snapshot_id)?;
        validate_restore_snapshot_path(path)?;
        config.validate()?;
        let repository = config
            .repository
            .url
            .as_ref()
            .ok_or(InvocationError::MissingRepository)?;
        let mut arguments = repository_options(config);
        arguments.extend([
            OsString::from("ls"),
            OsString::from("--json"),
            OsString::from("--sort"),
            OsString::from("name"),
            OsString::from(snapshot_id),
            OsString::from(path),
        ]);
        Ok(ResticInvocation {
            operation: ResticOperation::List,
            executable: self.executable.clone(),
            arguments,
            environment: BTreeMap::from([(
                OsString::from("RESTIC_REPOSITORY"),
                OsString::from(repository),
            )]),
            secret_environment: config.repository.secret_refs.clone(),
        })
    }

    /// Restores one exact node into a newly-created destination. The snapshot
    /// subtree syntax strips ancestors; an anchored, escaped include selects
    /// the leaf without interpreting glob characters in real filenames.
    pub fn restore(
        &self,
        config: &EffectiveConfig,
        snapshot_id: &str,
        path: &str,
        destination: &Path,
    ) -> Result<ResticInvocation, InvocationError> {
        authorize_operation(config.repository.mode, ResticOperation::Restore)?;
        validate_restore_snapshot_id(snapshot_id)?;
        validate_restore_snapshot_path(path)?;
        if path == "/" {
            return Err(InvocationError::InvalidRestoreSnapshotPath);
        }
        if !destination.is_absolute() {
            return Err(InvocationError::InvalidRestoreDestination);
        }
        config.validate()?;
        let repository = config
            .repository
            .url
            .as_ref()
            .ok_or(InvocationError::MissingRepository)?;
        let (parent, leaf) = path
            .rsplit_once('/')
            .ok_or(InvocationError::InvalidRestoreSnapshotPath)?;
        let parent = if parent.is_empty() { "/" } else { parent };
        let selection = format!("{snapshot_id}:{parent}");
        let include = format!("/{}", escape_restic_pattern_component(leaf));
        let mut arguments = repository_options(config);
        arguments.extend([
            OsString::from("restore"),
            OsString::from("--json"),
            OsString::from("--verify"),
            OsString::from("--overwrite"),
            OsString::from("never"),
            OsString::from("--target"),
            destination.as_os_str().to_os_string(),
            OsString::from("--include"),
            OsString::from(include),
            OsString::from(selection),
        ]);
        Ok(ResticInvocation {
            operation: ResticOperation::Restore,
            executable: self.executable.clone(),
            arguments,
            environment: BTreeMap::from([(
                OsString::from("RESTIC_REPOSITORY"),
                OsString::from(repository),
            )]),
            secret_environment: config.repository.secret_refs.clone(),
        })
    }

    pub fn retention(
        &self,
        config: &EffectiveConfig,
        operation: ResticOperation,
    ) -> Result<ResticInvocation, InvocationError> {
        if !matches!(operation, ResticOperation::Forget | ResticOperation::Prune) {
            return Err(InvocationError::NotRetentionOperation(operation));
        }
        authorize_operation(config.repository.mode, operation)?;
        config.validate()?;
        let repository = config
            .repository
            .url
            .as_ref()
            .ok_or(InvocationError::MissingRepository)?;

        let mut arguments = repository_options(config);
        match operation {
            ResticOperation::Forget => {
                arguments.extend([
                    "forget".into(),
                    "--keep-daily".into(),
                    config.retention.daily.to_string().into(),
                    "--keep-weekly".into(),
                    config.retention.weekly.to_string().into(),
                    "--keep-monthly".into(),
                    config.retention.monthly.to_string().into(),
                    "--keep-yearly".into(),
                    config.retention.yearly.to_string().into(),
                ]);
            }
            ResticOperation::Prune => arguments.push("prune".into()),
            _ => unreachable!("operation was checked above"),
        }

        Ok(ResticInvocation {
            operation,
            executable: self.executable.clone(),
            arguments,
            environment: BTreeMap::from([(
                OsString::from("RESTIC_REPOSITORY"),
                OsString::from(repository),
            )]),
            secret_environment: config.repository.secret_refs.clone(),
        })
    }
}

pub fn validate_restore_snapshot_id(snapshot_id: &str) -> Result<(), InvocationError> {
    if snapshot_id.len() != 64
        || !snapshot_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(InvocationError::InvalidRestoreSnapshotId);
    }
    Ok(())
}

pub fn validate_restore_snapshot_path(path: &str) -> Result<(), InvocationError> {
    if path.is_empty()
        || path.len() > MAX_PATH_CHARACTERS
        || !path.starts_with('/')
        || path.starts_with("//")
        || path.contains('\\')
        || path.chars().any(char::is_control)
        || path != "/"
            && path
                .split('/')
                .skip(1)
                .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        return Err(InvocationError::InvalidRestoreSnapshotPath);
    }
    Ok(())
}

fn escape_restic_pattern_component(component: &str) -> String {
    let mut escaped = String::with_capacity(component.len());
    for character in component.chars() {
        match character {
            '*' => escaped.push_str("[*]"),
            '?' => escaped.push_str("[?]"),
            '[' => escaped.push_str("[[]"),
            _ => escaped.push(character),
        }
    }
    escaped
}

pub fn authorize_operation(
    mode: RepositoryMode,
    operation: ResticOperation,
) -> Result<(), InvocationError> {
    if operation.allowed_in(mode) {
        Ok(())
    } else {
        Err(InvocationError::ForbiddenByRepositoryMode { mode, operation })
    }
}

fn repository_options(config: &EffectiveConfig) -> Vec<OsString> {
    let mut arguments = Vec::with_capacity(config.repository.options.len() * 2);
    for (key, value) in &config.repository.options {
        arguments.push("--option".into());
        arguments.push(format!("{key}={value}").into());
    }
    arguments
}

fn reject_snapshot_disabling_options(config: &EffectiveConfig) -> Result<(), InvocationError> {
    if let Some(option) = config
        .repository
        .options
        .keys()
        .find(|option| repository_option_disables_source_snapshots(option))
    {
        return Err(InvocationError::SnapshotDisablingRepositoryOption(
            option.clone(),
        ));
    }

    Ok(())
}

/// Compares absolute Windows paths lexically using Windows-style casing and
/// prefix equivalence, resolving `.` and `..` components without touching the
/// filesystem. The service adds a canonical filesystem check for aliases and
/// reparse points before constructing a backup.
#[must_use]
pub fn windows_path_is_same_or_descendant(path: &Path, directory: &Path) -> bool {
    let Some(path) = normalized_windows_path(path) else {
        return false;
    };
    let Some(directory) = normalized_windows_path(directory) else {
        return false;
    };
    path.len() >= directory.len()
        && path
            .iter()
            .zip(directory.iter())
            .all(|(path, directory)| path == directory)
}

fn normalized_windows_path(path: &Path) -> Option<Vec<String>> {
    if !path.is_absolute() {
        return None;
    }

    let mut normalized = Vec::new();
    let mut floor = 0;
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                normalized.push(normalized_windows_prefix(prefix.kind()));
                floor = normalized.len();
            }
            Component::RootDir => {
                normalized.push("root".to_owned());
                floor = normalized.len();
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if normalized.len() <= floor {
                    return None;
                }
                normalized.pop();
            }
            Component::Normal(value) => {
                normalized.push(format!("name:{}", value.to_string_lossy().to_lowercase()));
            }
        }
    }
    Some(normalized)
}

fn normalized_windows_prefix(prefix: Prefix<'_>) -> String {
    match prefix {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
            format!("disk:{}", char::from(drive).to_ascii_lowercase())
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => format!(
            "unc:{}/{}",
            server.to_string_lossy().to_lowercase(),
            share.to_string_lossy().to_lowercase()
        ),
        Prefix::DeviceNS(device) => {
            format!("device:{}", device.to_string_lossy().to_lowercase())
        }
        Prefix::Verbatim(value) => {
            format!("verbatim:{}", value.to_string_lossy().to_lowercase())
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvocationError {
    #[error(transparent)]
    InvalidConfiguration(#[from] ConfigValidationError),
    #[error("repository is not configured")]
    MissingRepository,
    #[error("at least one backup path is required")]
    NoBackupPaths,
    #[error(
        "repository option {0:?} is forbidden for backups because it can disable filesystem snapshots"
    )]
    SnapshotDisablingRepositoryOption(String),
    #[error("a backup source is inside the protected resticpal data directory")]
    ProtectedBackupSource,
    #[error("a backup source uses an unsupported local device namespace")]
    UnsupportedBackupSourceNamespace,
    #[error("network backup sources are not supported")]
    UnsupportedNetworkBackupSource,
    #[error("restore requires one exact 64-character lowercase snapshot ID")]
    InvalidRestoreSnapshotId,
    #[error("restore requires a normalized absolute snapshot path")]
    InvalidRestoreSnapshotPath,
    #[error("restore requires an absolute local destination directory")]
    InvalidRestoreDestination,
    #[error("{operation:?} is forbidden in repository mode {mode:?}")]
    ForbiddenByRepositoryMode {
        mode: RepositoryMode,
        operation: ResticOperation,
    },
    #[error("{0:?} is not an inspection operation")]
    NotInspectionOperation(ResticOperation),
    #[error("{0:?} is not a repository setup operation")]
    NotRepositorySetupOperation(ResticOperation),
    #[error("{0:?} is not a retention operation")]
    NotRetentionOperation(ResticOperation),
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn configured(mode: RepositoryMode) -> EffectiveConfig {
        let mut config = EffectiveConfig::default();
        config.repository.url = Some("s3:https://s3.example.test/backups/device-1".to_owned());
        config.repository.mode = mode;
        config
            .repository
            .options
            .insert("s3.region".to_owned(), "us-west-2".to_owned());
        config.repository.secret_refs.insert(
            SecretEnvironmentVariable::ResticPassword,
            "repository-password".to_owned(),
        );
        config.repository.secret_refs.insert(
            SecretEnvironmentVariable::AwsSecretAccessKey,
            "s3-secret-key".to_owned(),
        );
        config.backup.paths = vec![PathBuf::from(r"C:\Users\Yann\Documents")];
        config.backup.exclusions = vec!["*.tmp".to_owned()];
        config
    }

    #[test]
    fn backup_invocation_uses_no_shell_and_contains_no_secret_values() {
        let config = configured(RepositoryMode::AppendOnly);
        let invocation = ResticCommandBuilder::new(r"C:\Program Files\ResticPal\restic.exe")
            .backup(&config)
            .expect("backup should be allowed");
        let args: Vec<_> = invocation
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();

        assert_eq!(
            args,
            vec![
                "--option",
                "s3.region=us-west-2",
                "--cleanup-cache",
                "backup",
                "--json",
                "--use-fs-snapshot",
                "--exclude",
                "*.tmp",
                r"C:\Users\Yann\Documents",
            ]
        );
        assert_eq!(
            invocation.environment[&OsString::from("RESTIC_REPOSITORY")],
            OsString::from("s3:https://s3.example.test/backups/device-1")
        );
        assert_eq!(
            invocation.secret_environment[&SecretEnvironmentVariable::AwsSecretAccessKey],
            "s3-secret-key"
        );
        assert!(!args.iter().any(|argument| argument == "s3-secret-key"));
    }

    #[test]
    fn backup_places_required_path_exclusions_after_config_and_before_sources() {
        let config = configured(RepositoryMode::AppendOnly);
        let internal_data = PathBuf::from(r"C:\ProgramData\Restic Pal\Internal Data");

        let invocation = ResticCommandBuilder::new("restic.exe")
            .backup_with_required_exclusions(&config, std::slice::from_ref(&internal_data))
            .expect("backup should include the required exclusion");

        assert_eq!(
            invocation.arguments,
            [
                OsString::from("--option"),
                OsString::from("s3.region=us-west-2"),
                OsString::from("--cleanup-cache"),
                OsString::from("backup"),
                OsString::from("--json"),
                OsString::from("--use-fs-snapshot"),
                OsString::from("--exclude"),
                OsString::from("*.tmp"),
                OsString::from("--iexclude"),
                internal_data.into_os_string(),
                OsString::from(r"C:\Users\Yann\Documents"),
            ]
        );
    }

    #[test]
    fn configured_exclusions_cannot_remove_a_required_exclusion() {
        let mut config = configured(RepositoryMode::AppendOnly);
        config.backup.exclusions.clear();
        let internal_data = PathBuf::from(r"C:\ProgramData\ResticPal");

        let invocation = ResticCommandBuilder::new("restic.exe")
            .backup_with_required_exclusions(&config, std::slice::from_ref(&internal_data))
            .expect("backup should include the required exclusion");

        let required_pair = [OsString::from("--iexclude"), internal_data.into_os_string()];
        assert!(
            invocation
                .arguments
                .windows(required_pair.len())
                .any(|arguments| arguments == required_pair),
            "the service-owned exclusion must be present even when config has none"
        );
    }

    #[test]
    fn explicit_protected_sources_are_rejected_before_restic_can_bypass_excludes() {
        let protected = PathBuf::from(r"C:\ProgramData\ResticPal");
        for source in [
            PathBuf::from(r"C:\ProgramData\ResticPal"),
            PathBuf::from(r"c:\PROGRAMDATA\RESTICPAL\Credentials\secret.bin"),
            PathBuf::from(r"C:\ProgramData\ResticPal\Cache\..\config.toml"),
            PathBuf::from(r"\\?\C:\ProgramData\ResticPal\state.db"),
        ] {
            let mut config = configured(RepositoryMode::AppendOnly);
            config.backup.paths = vec![source];
            assert_eq!(
                ResticCommandBuilder::new("restic.exe")
                    .backup_with_required_exclusions(&config, std::slice::from_ref(&protected)),
                Err(InvocationError::ProtectedBackupSource)
            );
        }

        let mut config = configured(RepositoryMode::AppendOnly);
        config.backup.paths = vec![PathBuf::from(r"C:\")];
        assert!(
            ResticCommandBuilder::new("restic.exe")
                .backup_with_required_exclusions(&config, &[protected])
                .is_ok(),
            "a protected descendant must not reject its legitimate parent source"
        );
    }

    #[test]
    fn backup_rejects_repository_options_that_can_disable_vss() {
        for option in [
            "vss.exclude-volumes",
            "vss.exclude-all-mount-points",
            "VsS.ExClUdE-VoLuMeS",
            "VSS.EXCLUDE-ALL-MOUNT-POINTS",
        ] {
            let mut config = configured(RepositoryMode::Standard);
            config
                .repository
                .options
                .insert(option.to_owned(), "C:".to_owned());

            assert_eq!(
                ResticCommandBuilder::new("restic.exe")
                    .backup_with_required_exclusions(&config, &[]),
                Err(InvocationError::SnapshotDisablingRepositoryOption(
                    option.to_owned()
                )),
                "{option} must not be accepted for a backup"
            );
        }
    }

    #[test]
    fn append_only_allows_backup_stale_lock_cleanup_and_read_only_inspection() {
        for operation in [
            ResticOperation::Backup,
            ResticOperation::Unlock,
            ResticOperation::Probe,
            ResticOperation::Snapshots,
            ResticOperation::List,
            ResticOperation::Restore,
            ResticOperation::Check,
        ] {
            assert_eq!(
                authorize_operation(RepositoryMode::AppendOnly, operation),
                Ok(())
            );
        }
    }

    #[test]
    fn unlock_is_narrow_and_allowed_in_both_repository_modes() {
        for mode in [RepositoryMode::Standard, RepositoryMode::AppendOnly] {
            let config = configured(mode);
            let invocation = ResticCommandBuilder::new("restic.exe")
                .unlock(&config)
                .expect("stale lock cleanup is allowed");

            assert_eq!(invocation.operation, ResticOperation::Unlock);
            assert_eq!(
                invocation.arguments,
                ["--option", "s3.region=us-west-2", "unlock"].map(OsString::from)
            );
            assert!(
                !invocation
                    .arguments
                    .iter()
                    .any(|argument| argument == "--remove-all")
            );
            assert_eq!(invocation.secret_environment, config.repository.secret_refs);
        }
    }

    #[test]
    fn snapshot_and_directory_restore_are_allowlisted_without_repository_mutations() {
        let config = configured(RepositoryMode::AppendOnly);
        let snapshot_id = "a".repeat(64);
        let builder = ResticCommandBuilder::new("restic.exe");
        let listing = builder
            .directory_listing(&config, &snapshot_id, "/C/Users/Yann")
            .expect("append-only snapshot browsing");
        assert_eq!(listing.operation, ResticOperation::List);
        assert_eq!(
            listing.arguments,
            [
                "--option",
                "s3.region=us-west-2",
                "ls",
                "--json",
                "--sort",
                "name",
                snapshot_id.as_str(),
                "/C/Users/Yann",
            ]
            .map(OsString::from)
        );

        let restore = builder
            .restore(
                &config,
                &snapshot_id,
                "/C/Users/Yann/report [2025]?.txt",
                Path::new(r"C:\Recovery\Fresh"),
            )
            .expect("append-only safe restore");
        assert_eq!(restore.operation, ResticOperation::Restore);
        let arguments: Vec<_> = restore
            .arguments
            .iter()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect();
        assert!(arguments.contains(&"--verify".to_owned()));
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--overwrite", "never"])
        );
        assert!(
            arguments
                .windows(2)
                .any(|pair| pair == ["--include", "/report [[]2025][?].txt"])
        );
        assert!(arguments.contains(&format!("{snapshot_id}:/C/Users/Yann")));
        assert!(!arguments.iter().any(|argument| argument == "--delete"));
    }

    #[test]
    fn restore_rejects_ambiguous_snapshot_ids_traversal_and_relative_destinations() {
        let config = configured(RepositoryMode::AppendOnly);
        let builder = ResticCommandBuilder::new("restic.exe");
        for invalid in ["latest", "abc", &"A".repeat(64), &"g".repeat(64)] {
            assert_eq!(
                builder.directory_listing(&config, invalid, "/"),
                Err(InvocationError::InvalidRestoreSnapshotId)
            );
        }
        let snapshot_id = "a".repeat(64);
        for invalid in [
            "",
            "relative",
            "//server/share",
            "/a/../b",
            "/a/./b",
            "/a//b",
            "/a\\b",
            "/a\n",
        ] {
            assert_eq!(
                builder.directory_listing(&config, &snapshot_id, invalid),
                Err(InvocationError::InvalidRestoreSnapshotPath),
                "path {invalid:?}"
            );
        }
        assert_eq!(
            builder.restore(&config, &snapshot_id, "/file", Path::new("relative")),
            Err(InvocationError::InvalidRestoreDestination)
        );
        assert_eq!(
            builder.restore(&config, &snapshot_id, "/", Path::new(r"C:\Recovery")),
            Err(InvocationError::InvalidRestoreSnapshotPath)
        );
    }

    #[test]
    fn setup_invocations_are_allowlisted_and_inherit_repository_secrets() {
        let config = configured(RepositoryMode::Standard);
        let builder = ResticCommandBuilder::new("restic.exe");

        let probe = builder
            .repository_setup(&config, ResticOperation::Probe)
            .expect("probe");
        assert_eq!(
            probe.arguments,
            ["--option", "s3.region=us-west-2", "cat", "config"].map(OsString::from)
        );
        assert_eq!(probe.secret_environment, config.repository.secret_refs);

        let initialize = builder
            .repository_setup(&config, ResticOperation::Initialize)
            .expect("initialize");
        assert_eq!(
            initialize.arguments,
            ["--option", "s3.region=us-west-2", "init"].map(OsString::from)
        );
    }

    #[test]
    fn append_only_rejects_every_destructive_or_administrative_operation() {
        for operation in [
            ResticOperation::Initialize,
            ResticOperation::Forget,
            ResticOperation::Prune,
            ResticOperation::Rewrite,
            ResticOperation::Migrate,
            ResticOperation::Repair,
            ResticOperation::RemoveKey,
        ] {
            assert_eq!(
                authorize_operation(RepositoryMode::AppendOnly, operation),
                Err(InvocationError::ForbiddenByRepositoryMode {
                    mode: RepositoryMode::AppendOnly,
                    operation,
                })
            );
        }
    }

    #[test]
    fn standard_retention_builds_separate_forget_and_prune_invocations() {
        let config = configured(RepositoryMode::Standard);
        let builder = ResticCommandBuilder::new("restic.exe");
        let forget = builder
            .retention(&config, ResticOperation::Forget)
            .expect("forget");
        let arguments: Vec<_> = forget
            .arguments
            .iter()
            .map(|value| value.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            arguments,
            [
                "--option",
                "s3.region=us-west-2",
                "forget",
                "--keep-daily",
                "7",
                "--keep-weekly",
                "5",
                "--keep-monthly",
                "12",
                "--keep-yearly",
                "3",
            ]
        );

        let prune = builder
            .retention(&config, ResticOperation::Prune)
            .expect("prune");
        assert_eq!(
            prune.arguments,
            ["--option", "s3.region=us-west-2", "prune"].map(OsString::from)
        );
    }

    #[test]
    fn append_only_builder_rejects_retention() {
        let config = configured(RepositoryMode::AppendOnly);
        let result =
            ResticCommandBuilder::new("restic.exe").retention(&config, ResticOperation::Forget);
        assert!(matches!(
            result,
            Err(InvocationError::ForbiddenByRepositoryMode {
                mode: RepositoryMode::AppendOnly,
                operation: ResticOperation::Forget
            })
        ));
    }
}
