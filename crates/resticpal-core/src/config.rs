use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONFIG_SCHEMA_VERSION: u32 = 1;
pub const MAX_BACKUP_PATHS: usize = 128;
pub const MAX_EXCLUSIONS: usize = 512;
pub const MAX_PATH_CHARACTERS: usize = 32_767;
pub const MAX_EXCLUSION_CHARACTERS: usize = 1_024;

const DEFAULT_INTERVAL_HOURS: u32 = 24;
const DEFAULT_WAKE_GRACE_SECONDS: u64 = 5 * 60;
const DEFAULT_WAKE_LOCK_TIMEOUT_SECONDS: u64 = 2 * 60 * 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    pub schema_version: u32,
    pub backup: LocalBackupConfig,
    pub repository: LocalRepositoryConfig,
    pub schedule: LocalScheduleConfig,
    pub retention: LocalRetentionConfig,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            backup: LocalBackupConfig::default(),
            repository: LocalRepositoryConfig::default(),
            schedule: LocalScheduleConfig::default(),
            retention: LocalRetentionConfig::default(),
        }
    }
}

impl LocalConfig {
    /// Parses the human-editable local configuration layer.
    ///
    /// Fields omitted from this layer may be supplied by managed policy or by
    /// product defaults during policy resolution.
    pub fn from_toml(input: &str) -> Result<Self, LocalConfigError> {
        let config: Self = toml::from_str(input)?;
        if config.schema_version != CONFIG_SCHEMA_VERSION {
            return Err(LocalConfigError::UnsupportedSchema {
                expected: CONFIG_SCHEMA_VERSION,
                actual: config.schema_version,
            });
        }
        Ok(config)
    }

    pub fn to_toml_pretty(&self) -> Result<String, LocalConfigError> {
        Ok(toml::to_string_pretty(self)?)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalBackupConfig {
    pub paths: Option<Vec<PathBuf>>,
    pub exclusions: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalRepositoryConfig {
    pub display_name: Option<String>,
    pub url: Option<String>,
    pub mode: Option<RepositoryMode>,
    pub options: Option<BTreeMap<String, String>>,
    pub secret_refs: Option<BTreeMap<SecretEnvironmentVariable, String>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalScheduleConfig {
    pub interval_hours: Option<u32>,
    pub wake_grace_seconds: Option<u64>,
    pub wake_lock_timeout_seconds: Option<u64>,
    pub allow_on_battery: Option<bool>,
    pub allow_metered_network: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalRetentionConfig {
    pub daily: Option<u32>,
    pub weekly: Option<u32>,
    pub monthly: Option<u32>,
    pub yearly: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RepositoryMode {
    #[default]
    Standard,
    AppendOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum SecretEnvironmentVariable {
    #[serde(rename = "RESTIC_PASSWORD")]
    ResticPassword,
    #[serde(rename = "AWS_ACCESS_KEY_ID")]
    AwsAccessKeyId,
    #[serde(rename = "AWS_SECRET_ACCESS_KEY")]
    AwsSecretAccessKey,
    #[serde(rename = "AWS_SESSION_TOKEN")]
    AwsSessionToken,
    #[serde(rename = "AZURE_ACCOUNT_KEY")]
    AzureAccountKey,
    #[serde(rename = "B2_ACCOUNT_KEY")]
    B2AccountKey,
    #[serde(rename = "GOOGLE_APPLICATION_CREDENTIALS")]
    GoogleApplicationCredentials,
    #[serde(rename = "RCLONE_CONFIG_PASS")]
    RcloneConfigPassword,
}

impl SecretEnvironmentVariable {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ResticPassword => "RESTIC_PASSWORD",
            Self::AwsAccessKeyId => "AWS_ACCESS_KEY_ID",
            Self::AwsSecretAccessKey => "AWS_SECRET_ACCESS_KEY",
            Self::AwsSessionToken => "AWS_SESSION_TOKEN",
            Self::AzureAccountKey => "AZURE_ACCOUNT_KEY",
            Self::B2AccountKey => "B2_ACCOUNT_KEY",
            Self::GoogleApplicationCredentials => "GOOGLE_APPLICATION_CREDENTIALS",
            Self::RcloneConfigPassword => "RCLONE_CONFIG_PASS",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveConfig {
    pub backup: BackupConfig,
    pub repository: RepositoryConfig,
    pub schedule: ScheduleConfig,
    pub retention: RetentionConfig,
}

impl EffectiveConfig {
    pub fn validate(&self) -> Result<(), ConfigValidationError> {
        if let Some(url) = &self.repository.url
            && url.trim().is_empty()
        {
            return Err(ConfigValidationError::EmptyRepositoryUrl);
        }

        if self.schedule.interval_hours == 0 {
            return Err(ConfigValidationError::ZeroScheduleInterval);
        }

        if self.schedule.wake_grace_seconds > 24 * 60 * 60 {
            return Err(ConfigValidationError::WakeGraceTooLong);
        }

        if self.schedule.wake_lock_timeout_seconds == 0
            || self.schedule.wake_lock_timeout_seconds > 24 * 60 * 60
        {
            return Err(ConfigValidationError::InvalidWakeLockTimeout);
        }

        if self.backup.paths.len() > MAX_BACKUP_PATHS {
            return Err(ConfigValidationError::TooManyBackupPaths);
        }
        for path in &self.backup.paths {
            if path.as_os_str().is_empty()
                || !path.is_absolute()
                || path.to_string_lossy().encode_utf16().count() > MAX_PATH_CHARACTERS
            {
                return Err(ConfigValidationError::InvalidBackupPath(path.clone()));
            }
        }
        if self.backup.exclusions.len() > MAX_EXCLUSIONS {
            return Err(ConfigValidationError::TooManyExclusions);
        }
        for exclusion in &self.backup.exclusions {
            if exclusion.is_empty()
                || exclusion.chars().count() > MAX_EXCLUSION_CHARACTERS
                || exclusion.contains(['\0', '\r', '\n'])
            {
                return Err(ConfigValidationError::InvalidExclusion);
            }
        }

        for key in self.repository.options.keys() {
            if !is_valid_option_name(key) {
                return Err(ConfigValidationError::InvalidRepositoryOption(key.clone()));
            }
        }

        for secret_id in self.repository.secret_refs.values() {
            if secret_id.trim().is_empty() {
                return Err(ConfigValidationError::EmptySecretReference);
            }
        }

        Ok(())
    }

    #[must_use]
    pub fn is_configured(&self) -> bool {
        self.repository.url.is_some() && !self.backup.paths.is_empty()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupConfig {
    pub paths: Vec<PathBuf>,
    pub exclusions: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositoryConfig {
    pub display_name: Option<String>,
    pub url: Option<String>,
    pub mode: RepositoryMode,
    pub options: BTreeMap<String, String>,
    pub secret_refs: BTreeMap<SecretEnvironmentVariable, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduleConfig {
    pub interval_hours: u32,
    pub wake_grace_seconds: u64,
    pub wake_lock_timeout_seconds: u64,
    pub allow_on_battery: bool,
    pub allow_metered_network: bool,
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self {
            interval_hours: DEFAULT_INTERVAL_HOURS,
            wake_grace_seconds: DEFAULT_WAKE_GRACE_SECONDS,
            wake_lock_timeout_seconds: DEFAULT_WAKE_LOCK_TIMEOUT_SECONDS,
            allow_on_battery: true,
            allow_metered_network: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetentionConfig {
    pub daily: u32,
    pub weekly: u32,
    pub monthly: u32,
    pub yearly: u32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            daily: 7,
            weekly: 5,
            monthly: 12,
            yearly: 3,
        }
    }
}

fn is_valid_option_name(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };

    (first.is_ascii_alphanumeric())
        && chars.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-')
        })
}

#[derive(Debug, Error)]
pub enum LocalConfigError {
    #[error("could not parse local TOML configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("could not serialize local TOML configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("unsupported configuration schema {actual}; expected {expected}")]
    UnsupportedSchema { expected: u32, actual: u32 },
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ConfigValidationError {
    #[error("repository URL cannot be empty")]
    EmptyRepositoryUrl,
    #[error("schedule interval must be greater than zero")]
    ZeroScheduleInterval,
    #[error("wake grace period cannot exceed 24 hours")]
    WakeGraceTooLong,
    #[error("wake-lock timeout must be between one second and 24 hours")]
    InvalidWakeLockTimeout,
    #[error("backup configuration exceeds the maximum of {MAX_BACKUP_PATHS} paths")]
    TooManyBackupPaths,
    #[error("backup path must be an absolute path within the Windows path-length limit: {0}")]
    InvalidBackupPath(PathBuf),
    #[error("backup configuration exceeds the maximum of {MAX_EXCLUSIONS} exclusions")]
    TooManyExclusions,
    #[error("backup exclusions must be non-empty, single-line patterns within the size limit")]
    InvalidExclusion,
    #[error("invalid repository option name: {0}")]
    InvalidRepositoryOption(String),
    #[error("secret reference IDs cannot be empty")]
    EmptySecretReference,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_product_decisions() {
        let config = EffectiveConfig::default();

        assert_eq!(config.schedule.interval_hours, 24);
        assert_eq!(config.schedule.wake_grace_seconds, 300);
        assert_eq!(config.schedule.wake_lock_timeout_seconds, 7_200);
        assert!(config.schedule.allow_on_battery);
        assert!(config.schedule.allow_metered_network);
        assert_eq!(config.retention.daily, 7);
        assert_eq!(config.retention.weekly, 5);
        assert_eq!(config.retention.monthly, 12);
        assert_eq!(config.retention.yearly, 3);
    }

    #[test]
    fn parses_partial_local_toml_without_inventing_local_overrides() {
        let config = LocalConfig::from_toml(
            r#"
                schema_version = 1

                [schedule]
                allow_on_battery = false

                [repository]
                mode = "append_only"
            "#,
        )
        .expect("configuration should parse");

        assert_eq!(config.schedule.allow_on_battery, Some(false));
        assert_eq!(config.schedule.interval_hours, None);
        assert_eq!(config.repository.mode, Some(RepositoryMode::AppendOnly));
    }

    #[test]
    fn rejects_option_names_that_could_become_flags() {
        let mut config = EffectiveConfig::default();
        config
            .repository
            .options
            .insert("--password-file".to_owned(), "bad".to_owned());

        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::InvalidRepositoryOption(
                "--password-file".to_owned()
            ))
        );
    }

    #[test]
    fn parses_the_checked_in_append_only_s3_example() {
        let config = LocalConfig::from_toml(include_str!("../../../config/resticpal.example.toml"))
            .expect("checked-in example should stay valid");

        assert_eq!(config.repository.mode, Some(RepositoryMode::AppendOnly));
        assert_eq!(
            config
                .repository
                .secret_refs
                .expect("example has secret references")
                [&SecretEnvironmentVariable::AwsSecretAccessKey],
            "s3-secret-access-key"
        );
    }

    #[test]
    fn local_configuration_round_trips_through_pretty_toml() {
        let original =
            LocalConfig::from_toml(include_str!("../../../config/resticpal.example.toml"))
                .expect("example should parse");
        let serialized = original.to_toml_pretty().expect("config should serialize");

        assert_eq!(
            LocalConfig::from_toml(&serialized).expect("serialized config should parse"),
            original
        );
    }

    #[test]
    fn effective_configuration_rejects_relative_paths_and_multiline_exclusions() {
        let mut config = EffectiveConfig::default();
        config.backup.paths = vec![PathBuf::from("relative")];
        assert!(matches!(
            config.validate(),
            Err(ConfigValidationError::InvalidBackupPath(_))
        ));

        config.backup.paths = vec![PathBuf::from(r"C:\Users\Example\Documents")];
        config.backup.exclusions = vec!["valid".to_owned(), "bad\npattern".to_owned()];
        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::InvalidExclusion)
        );
    }
}
