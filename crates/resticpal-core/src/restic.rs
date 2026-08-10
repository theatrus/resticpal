use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{
    ConfigValidationError, EffectiveConfig, RepositoryMode, SecretEnvironmentVariable,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResticOperation {
    Backup,
    Probe,
    Snapshots,
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
                    Self::Backup | Self::Probe | Self::Snapshots | Self::Check
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
        authorize_operation(config.repository.mode, ResticOperation::Backup)?;
        config.validate()?;

        let repository = config
            .repository
            .url
            .as_ref()
            .ok_or(InvocationError::MissingRepository)?;
        if config.backup.paths.is_empty() {
            return Err(InvocationError::NoBackupPaths);
        }

        let mut arguments = repository_options(config);
        arguments.push("backup".into());
        arguments.push("--json".into());
        arguments.push("--use-fs-snapshot".into());

        for exclusion in &config.backup.exclusions {
            arguments.push("--exclude".into());
            arguments.push(exclusion.into());
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

#[derive(Debug, Error, PartialEq, Eq)]
pub enum InvocationError {
    #[error(transparent)]
    InvalidConfiguration(#[from] ConfigValidationError),
    #[error("repository is not configured")]
    MissingRepository,
    #[error("at least one backup path is required")]
    NoBackupPaths,
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
    fn append_only_allows_backup_and_read_only_inspection() {
        for operation in [
            ResticOperation::Backup,
            ResticOperation::Probe,
            ResticOperation::Snapshots,
            ResticOperation::Check,
        ] {
            assert_eq!(
                authorize_operation(RepositoryMode::AppendOnly, operation),
                Ok(())
            );
        }
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
