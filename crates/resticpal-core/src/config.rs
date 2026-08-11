use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

pub const CONFIG_SCHEMA_VERSION: u32 = 1;
pub const MAX_BACKUP_PATHS: usize = 128;
pub const MAX_EXCLUSIONS: usize = 512;
pub const MAX_PATH_CHARACTERS: usize = 32_767;
pub const MAX_EXCLUSION_CHARACTERS: usize = 1_024;
pub const MAX_REPOSITORY_URL_CHARACTERS: usize = 8 * 1_024;
pub const MAX_REPOSITORY_DISPLAY_NAME_CHARACTERS: usize = 256;
pub const MAX_REPOSITORY_OPTIONS: usize = 64;
pub const MAX_REPOSITORY_OPTION_VALUE_CHARACTERS: usize = 4 * 1_024;
pub const MAX_SECRET_REFERENCE_BYTES: usize = 64;
pub const MAX_SCHEDULE_INTERVAL_HOURS: u32 = 24 * 365;
pub const MAX_RETENTION_COUNT: u32 = 10_000;
pub const MAX_PRUNE_INTERVAL_DAYS: u32 = 365;
pub const MAX_MANAGEMENT_URL_CHARACTERS: usize = 8 * 1_024;
pub const MAX_MANAGEMENT_ID_CHARACTERS: usize = 256;
pub const MIN_MANAGEMENT_REFRESH_MINUTES: u32 = 5;
pub const MAX_MANAGEMENT_REFRESH_MINUTES: u32 = 24 * 60;

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
    pub updates: LocalUpdateConfig,
    pub management: LocalManagementConfig,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            schema_version: CONFIG_SCHEMA_VERSION,
            backup: LocalBackupConfig::default(),
            repository: LocalRepositoryConfig::default(),
            schedule: LocalScheduleConfig::default(),
            retention: LocalRetentionConfig::default(),
            updates: LocalUpdateConfig::default(),
            management: LocalManagementConfig::default(),
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
        config.management.validate()?;
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
    pub prune_interval_days: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalUpdateConfig {
    /// Install strictly signed product updates in the background through the
    /// LocalSystem service. This remains opt-in for existing installations.
    pub automatic_install: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LocalManagementConfig {
    pub mode: ManagementMode,
    pub manifest_url: Option<String>,
    pub signing_public_key: Option<String>,
    pub refresh_interval_minutes: Option<u32>,
    pub status_url: Option<String>,
    pub device_id: Option<String>,
    pub status_token_ref: Option<String>,
    pub enrollment_key_ref: Option<String>,
}

impl LocalManagementConfig {
    pub fn validate(&self) -> Result<(), ManagementConfigError> {
        let refresh = self.refresh_interval_minutes.unwrap_or(15);
        if !(MIN_MANAGEMENT_REFRESH_MINUTES..=MAX_MANAGEMENT_REFRESH_MINUTES).contains(&refresh) {
            return Err(ManagementConfigError::InvalidRefreshInterval);
        }

        match self.mode {
            ManagementMode::Disabled => {
                if self.manifest_url.is_some()
                    || self.signing_public_key.is_some()
                    || self.status_url.is_some()
                    || self.device_id.is_some()
                    || self.status_token_ref.is_some()
                    || self.enrollment_key_ref.is_some()
                {
                    return Err(ManagementConfigError::UnexpectedDisabledFields);
                }
            }
            ManagementMode::PlainManifest => {
                // A plain manifest is unsigned, so its only integrity guarantee
                // is the transport. Require HTTPS (loopback HTTP stays allowed
                // for local testing) so an on-path attacker cannot substitute
                // policy that clears backup paths or redirects the repository.
                let manifest_url = validate_management_url(self.manifest_url.as_deref())?;
                validate_signed_transport(&manifest_url)?;
                if self.signing_public_key.is_some()
                    || self.status_url.is_some()
                    || self.device_id.is_some()
                    || self.status_token_ref.is_some()
                    || self.enrollment_key_ref.is_some()
                {
                    return Err(ManagementConfigError::PlainModeCannotReport);
                }
            }
            ManagementMode::SignedManifest => {
                let manifest_url = validate_management_url(self.manifest_url.as_deref())?;
                validate_signed_transport(&manifest_url)?;
                let key = self
                    .signing_public_key
                    .as_deref()
                    .ok_or(ManagementConfigError::MissingSigningKey)?;
                if key.is_empty() || key.len() > 256 || key.contains(['\0', '\r', '\n']) {
                    return Err(ManagementConfigError::InvalidSigningKey);
                }

                let reporting_fields = [
                    self.status_url.is_some(),
                    self.device_id.is_some(),
                    self.status_token_ref.is_some(),
                ];
                if reporting_fields.iter().any(|present| *present)
                    && !reporting_fields.iter().all(|present| *present)
                {
                    return Err(ManagementConfigError::IncompleteStatusConfiguration);
                }
                if let Some(status_url) = self.status_url.as_deref() {
                    let status_url = validate_management_url(Some(status_url))?;
                    validate_signed_transport(&status_url)?;
                }
                if self.device_id.as_ref().is_some_and(|value| {
                    value.is_empty()
                        || value.len() > MAX_MANAGEMENT_ID_CHARACTERS
                        || !value
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
                }) {
                    return Err(ManagementConfigError::InvalidDeviceId);
                }
                if self
                    .status_token_ref
                    .as_deref()
                    .is_some_and(|reference| !is_valid_secret_reference(reference))
                {
                    return Err(ManagementConfigError::InvalidStatusTokenReference);
                }
                if self
                    .enrollment_key_ref
                    .as_deref()
                    .is_some_and(|reference| !is_valid_secret_reference(reference))
                {
                    return Err(ManagementConfigError::InvalidEnrollmentKeyReference);
                }
            }
        }
        Ok(())
    }

    #[must_use]
    pub fn refresh_interval_minutes(&self) -> u32 {
        self.refresh_interval_minutes.unwrap_or(15)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementMode {
    #[default]
    Disabled,
    PlainManifest,
    SignedManifest,
}

fn validate_management_url(value: Option<&str>) -> Result<Url, ManagementConfigError> {
    let value = value.ok_or(ManagementConfigError::MissingManifestUrl)?;
    if value.chars().count() > MAX_MANAGEMENT_URL_CHARACTERS || value.contains(['\0', '\r', '\n']) {
        return Err(ManagementConfigError::InvalidUrl);
    }
    let url = Url::parse(value).map_err(|_| ManagementConfigError::InvalidUrl)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(ManagementConfigError::InvalidUrl);
    }
    Ok(url)
}

fn validate_signed_transport(value: &Url) -> Result<(), ManagementConfigError> {
    let loopback = value.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("localhost")
            || host
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback())
    });
    if value.scheme() == "https" || loopback {
        Ok(())
    } else {
        Err(ManagementConfigError::InsecureTransport)
    }
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

    #[must_use]
    pub const fn reference_prefix(self) -> &'static str {
        match self {
            Self::ResticPassword => "restic-password",
            Self::AwsAccessKeyId => "aws-access-key-id",
            Self::AwsSecretAccessKey => "aws-secret-access-key",
            Self::AwsSessionToken => "aws-session-token",
            Self::AzureAccountKey => "azure-account-key",
            Self::B2AccountKey => "b2-account-key",
            Self::GoogleApplicationCredentials => "google-credentials",
            Self::RcloneConfigPassword => "rclone-config-password",
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
        if let Some(url) = &self.repository.url {
            if url.trim().is_empty() {
                return Err(ConfigValidationError::EmptyRepositoryUrl);
            }
            if url.chars().count() > MAX_REPOSITORY_URL_CHARACTERS
                || url.contains(['\0', '\r', '\n'])
            {
                return Err(ConfigValidationError::InvalidRepositoryUrl);
            }
        }
        if self.repository.display_name.as_ref().is_some_and(|name| {
            name.trim().is_empty()
                || name.chars().count() > MAX_REPOSITORY_DISPLAY_NAME_CHARACTERS
                || name.contains(['\0', '\r', '\n'])
        }) {
            return Err(ConfigValidationError::InvalidRepositoryDisplayName);
        }

        if !(1..=MAX_SCHEDULE_INTERVAL_HOURS).contains(&self.schedule.interval_hours) {
            return Err(ConfigValidationError::InvalidScheduleInterval);
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
                || path.to_string_lossy().contains('\0')
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

        if self.repository.options.len() > MAX_REPOSITORY_OPTIONS {
            return Err(ConfigValidationError::TooManyRepositoryOptions);
        }
        for (key, value) in &self.repository.options {
            if !is_valid_option_name(key) {
                return Err(ConfigValidationError::InvalidRepositoryOption(key.clone()));
            }
            if value.chars().count() > MAX_REPOSITORY_OPTION_VALUE_CHARACTERS
                || value.contains(['\0', '\r', '\n'])
            {
                return Err(ConfigValidationError::InvalidRepositoryOptionValue(
                    key.clone(),
                ));
            }
        }

        for secret_id in self.repository.secret_refs.values() {
            if !is_valid_secret_reference(secret_id) {
                return Err(ConfigValidationError::InvalidSecretReference);
            }
        }

        let retention = &self.retention;
        if [
            retention.daily,
            retention.weekly,
            retention.monthly,
            retention.yearly,
        ]
        .iter()
        .any(|count| *count > MAX_RETENTION_COUNT)
        {
            return Err(ConfigValidationError::InvalidRetentionCount);
        }
        if retention.daily == 0
            && retention.weekly == 0
            && retention.monthly == 0
            && retention.yearly == 0
        {
            return Err(ConfigValidationError::EmptyRetentionPolicy);
        }
        if !(1..=MAX_PRUNE_INTERVAL_DAYS).contains(&retention.prune_interval_days) {
            return Err(ConfigValidationError::InvalidPruneInterval);
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
    pub prune_interval_days: u32,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            daily: 7,
            weekly: 5,
            monthly: 12,
            yearly: 3,
            prune_interval_days: 7,
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

#[must_use]
pub fn is_valid_secret_reference(reference: &str) -> bool {
    !reference.is_empty()
        && reference.len() <= MAX_SECRET_REFERENCE_BYTES
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[derive(Debug, Error)]
pub enum LocalConfigError {
    #[error("could not parse local TOML configuration: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("could not serialize local TOML configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("unsupported configuration schema {actual}; expected {expected}")]
    UnsupportedSchema { expected: u32, actual: u32 },
    #[error(transparent)]
    Management(#[from] ManagementConfigError),
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ManagementConfigError {
    #[error("management refresh interval must be between 5 minutes and 24 hours")]
    InvalidRefreshInterval,
    #[error("disabled management mode cannot contain manifest or reporting settings")]
    UnexpectedDisabledFields,
    #[error("management mode requires an HTTP or HTTPS manifest URL")]
    MissingManifestUrl,
    #[error("management URLs must be bounded, single-line HTTP or HTTPS URLs")]
    InvalidUrl,
    #[error("plain-manifest mode cannot configure signing or status reporting")]
    PlainModeCannotReport,
    #[error("signed-manifest mode requires a pinned Ed25519 public key")]
    MissingSigningKey,
    #[error("the pinned signing key is malformed")]
    InvalidSigningKey,
    #[error("management and status URLs require HTTPS (loopback HTTP is allowed for testing)")]
    InsecureTransport,
    #[error("status URL, device ID, and token reference must be configured together")]
    IncompleteStatusConfiguration,
    #[error("the managed device ID is malformed")]
    InvalidDeviceId,
    #[error("the status token reference is malformed")]
    InvalidStatusTokenReference,
    #[error("the enrollment key reference is malformed")]
    InvalidEnrollmentKeyReference,
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ConfigValidationError {
    #[error("repository URL cannot be empty")]
    EmptyRepositoryUrl,
    #[error("repository URL must be a single-line value within the size limit")]
    InvalidRepositoryUrl,
    #[error("repository display name must be a non-empty single-line value within the size limit")]
    InvalidRepositoryDisplayName,
    #[error("schedule interval must be between one hour and {MAX_SCHEDULE_INTERVAL_HOURS} hours")]
    InvalidScheduleInterval,
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
    #[error("repository option value must be a single-line value within the size limit: {0}")]
    InvalidRepositoryOptionValue(String),
    #[error("repository configuration exceeds the maximum of {MAX_REPOSITORY_OPTIONS} options")]
    TooManyRepositoryOptions,
    #[error("secret reference IDs must contain 1-64 lowercase letters, digits, or hyphens")]
    InvalidSecretReference,
    #[error("retention counts must be between zero and {MAX_RETENTION_COUNT}")]
    InvalidRetentionCount,
    #[error("retention must keep at least one daily, weekly, monthly, or yearly snapshot")]
    EmptyRetentionPolicy,
    #[error("prune interval must be between one and {MAX_PRUNE_INTERVAL_DAYS} days")]
    InvalidPruneInterval,
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
        assert_eq!(config.retention.prune_interval_days, 7);
    }

    #[test]
    fn retention_policy_is_bounded_and_never_empty() {
        let mut config = EffectiveConfig::default();
        config.retention.daily = MAX_RETENTION_COUNT + 1;
        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::InvalidRetentionCount)
        );

        config.retention.daily = 0;
        config.retention.weekly = 0;
        config.retention.monthly = 0;
        config.retention.yearly = 0;
        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::EmptyRetentionPolicy)
        );

        config.retention.daily = 1;
        config.retention.prune_interval_days = 0;
        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::InvalidPruneInterval)
        );
    }

    #[test]
    fn management_modes_keep_plain_files_separate_from_reporting() {
        let plain = LocalManagementConfig {
            mode: ManagementMode::PlainManifest,
            manifest_url: Some("https://files.example.test/resticpal.json".to_owned()),
            ..LocalManagementConfig::default()
        };
        assert!(plain.validate().is_ok());

        // Loopback HTTP stays allowed so a local file server can be used in tests.
        let plain_loopback = LocalManagementConfig {
            mode: ManagementMode::PlainManifest,
            manifest_url: Some("http://127.0.0.1:8080/resticpal.json".to_owned()),
            ..LocalManagementConfig::default()
        };
        assert!(plain_loopback.validate().is_ok());

        // An unsigned manifest over cleartext HTTP would let an on-path attacker
        // rewrite policy, so it is rejected like the signed mode.
        let plain_insecure = LocalManagementConfig {
            mode: ManagementMode::PlainManifest,
            manifest_url: Some("http://files.example.test/resticpal.json".to_owned()),
            ..LocalManagementConfig::default()
        };
        assert_eq!(
            plain_insecure.validate(),
            Err(ManagementConfigError::InsecureTransport)
        );

        let plain_with_reporting = LocalManagementConfig {
            status_url: Some("https://files.example.test/status".to_owned()),
            ..plain
        };
        assert_eq!(
            plain_with_reporting.validate(),
            Err(ManagementConfigError::PlainModeCannotReport)
        );

        let insecure_signed = LocalManagementConfig {
            mode: ManagementMode::SignedManifest,
            manifest_url: Some("http://management.example.test/policy".to_owned()),
            signing_public_key: Some("pinned-key".to_owned()),
            ..LocalManagementConfig::default()
        };
        assert_eq!(
            insecure_signed.validate(),
            Err(ManagementConfigError::InsecureTransport)
        );

        let lookalike_loopback = LocalManagementConfig {
            mode: ManagementMode::SignedManifest,
            manifest_url: Some("http://localhost.example.test/policy".to_owned()),
            signing_public_key: Some("pinned-key".to_owned()),
            ..LocalManagementConfig::default()
        };
        assert_eq!(
            lookalike_loopback.validate(),
            Err(ManagementConfigError::InsecureTransport)
        );

        let credentialed_url = LocalManagementConfig {
            mode: ManagementMode::PlainManifest,
            manifest_url: Some("https://user:password@example.test/policy".to_owned()),
            ..LocalManagementConfig::default()
        };
        assert_eq!(
            credentialed_url.validate(),
            Err(ManagementConfigError::InvalidUrl)
        );
    }

    #[test]
    fn schedule_safety_bounds_are_enforced() {
        let mut config = EffectiveConfig::default();
        config.schedule.interval_hours = MAX_SCHEDULE_INTERVAL_HOURS + 1;
        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::InvalidScheduleInterval)
        );

        config.schedule.interval_hours = 24;
        config.schedule.wake_grace_seconds = 24 * 60 * 60 + 1;
        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::WakeGraceTooLong)
        );

        config.schedule.wake_grace_seconds = 300;
        config.schedule.wake_lock_timeout_seconds = 0;
        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::InvalidWakeLockTimeout)
        );
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
    fn rejects_unbounded_or_multiline_repository_metadata() {
        let mut config = EffectiveConfig::default();
        config.repository.url =
            Some("s3:https://example.test/bucket\n--password-command".to_owned());
        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::InvalidRepositoryUrl)
        );

        config.repository.url = Some("local:C:/backup".to_owned());
        config.repository.display_name = Some("line one\nline two".to_owned());
        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::InvalidRepositoryDisplayName)
        );

        config.repository.display_name = Some("Backup".to_owned());
        config
            .repository
            .options
            .insert("s3.region".to_owned(), "us-west-2\nmalformed".to_owned());
        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::InvalidRepositoryOptionValue(
                "s3.region".to_owned()
            ))
        );
    }

    #[test]
    fn repository_collection_limits_are_enforced() {
        let mut config = EffectiveConfig::default();
        config.repository.url = Some("local:C:/backup".to_owned());
        for index in 0..=MAX_REPOSITORY_OPTIONS {
            config
                .repository
                .options
                .insert(format!("option{index}"), "value".to_owned());
        }

        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::TooManyRepositoryOptions)
        );
    }

    #[test]
    fn secret_references_match_the_credential_store_namespace() {
        let mut config = EffectiveConfig::default();
        config.repository.secret_refs.insert(
            SecretEnvironmentVariable::ResticPassword,
            "UPPERCASE".to_owned(),
        );
        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::InvalidSecretReference)
        );

        config.repository.secret_refs.insert(
            SecretEnvironmentVariable::ResticPassword,
            "a".repeat(MAX_SECRET_REFERENCE_BYTES + 1),
        );
        assert_eq!(
            config.validate(),
            Err(ConfigValidationError::InvalidSecretReference)
        );

        config.repository.secret_refs.insert(
            SecretEnvironmentVariable::ResticPassword,
            "restic-password-0123456789abcdef".to_owned(),
        );
        assert_eq!(config.validate(), Ok(()));
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
