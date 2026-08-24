use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{
    BackupConfig, CONFIG_SCHEMA_VERSION, ConfigValidationError, EffectiveConfig, LocalConfig,
    RepositoryConfig, RepositoryMode, RetentionConfig, ScheduleConfig, SecretEnvironmentVariable,
    UpdateConfig,
};

pub const MAX_POLICY_REVISION_CHARACTERS: usize = 256;
pub const MANAGED_POLICY_SCHEMA_VERSION: u32 = 2;
const LEGACY_MANAGED_POLICY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagedPolicy {
    pub schema_version: u32,
    pub revision: String,
    pub backup: ManagedBackupPolicy,
    pub repository: ManagedRepositoryPolicy,
    pub schedule: ManagedSchedulePolicy,
    pub retention: ManagedRetentionPolicy,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "deserialize_managed_update_policy"
    )]
    pub updates: Option<ManagedUpdatePolicy>,
}

impl Default for ManagedPolicy {
    fn default() -> Self {
        Self {
            schema_version: MANAGED_POLICY_SCHEMA_VERSION,
            revision: String::new(),
            backup: ManagedBackupPolicy::default(),
            repository: ManagedRepositoryPolicy::default(),
            schedule: ManagedSchedulePolicy::default(),
            retention: ManagedRetentionPolicy::default(),
            updates: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagedBackupPolicy {
    pub paths: Option<Managed<Vec<PathBuf>>>,
    pub exclusions: Option<Managed<Vec<String>>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagedRepositoryPolicy {
    pub display_name: Option<Managed<Option<String>>>,
    pub url: Option<Managed<Option<String>>>,
    pub mode: Option<Managed<RepositoryMode>>,
    pub options: Option<Managed<BTreeMap<String, String>>>,
    pub secret_refs: Option<Managed<BTreeMap<SecretEnvironmentVariable, String>>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagedSchedulePolicy {
    pub interval_hours: Option<Managed<u32>>,
    pub wake_grace_seconds: Option<Managed<u64>>,
    pub wake_lock_timeout_seconds: Option<Managed<u64>>,
    pub allow_on_battery: Option<Managed<bool>>,
    pub allow_metered_network: Option<Managed<bool>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagedRetentionPolicy {
    pub daily: Option<Managed<u32>>,
    pub weekly: Option<Managed<u32>>,
    pub monthly: Option<Managed<u32>>,
    pub yearly: Option<Managed<u32>>,
    pub prune_interval_days: Option<Managed<u32>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagedUpdatePolicy {
    pub automatic_install: Option<Managed<bool>>,
}

fn deserialize_managed_update_policy<'de, D>(
    deserializer: D,
) -> Result<Option<ManagedUpdatePolicy>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // Unlike Option's default deserializer, this deliberately rejects an
    // explicit null. That keeps every serialized `updates` member
    // presence-aware so schema v1 cannot disguise the v2 field as null.
    ManagedUpdatePolicy::deserialize(deserializer).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Managed<T> {
    pub value: T,
    #[serde(default)]
    pub locked: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyField {
    BackupPaths,
    BackupExclusions,
    RepositoryDisplayName,
    RepositoryUrl,
    RepositoryMode,
    RepositoryOptions,
    RepositorySecretRefs,
    ScheduleIntervalHours,
    ScheduleWakeGraceSeconds,
    ScheduleWakeLockTimeoutSeconds,
    ScheduleAllowOnBattery,
    ScheduleAllowMeteredNetwork,
    RetentionDaily,
    RetentionWeekly,
    RetentionMonthly,
    RetentionYearly,
    RetentionPruneIntervalDays,
    UpdateAutomaticInstall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSource {
    ProductDefault,
    LocalAdministrator,
    ManagedRecommendation,
    ManagedLocked,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldResolution {
    pub source: ValueSource,
    pub locked: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedConfig {
    pub effective: EffectiveConfig,
    pub managed_revision: Option<String>,
    pub fields: BTreeMap<PolicyField, FieldResolution>,
}

impl ResolvedConfig {
    #[must_use]
    pub fn locked_fields(&self) -> BTreeSet<PolicyField> {
        self.fields
            .iter()
            .filter_map(|(field, resolution)| resolution.locked.then_some(*field))
            .collect()
    }
}

pub fn resolve_config(
    defaults: &EffectiveConfig,
    local: &LocalConfig,
    managed: Option<&ManagedPolicy>,
) -> Result<ResolvedConfig, PolicyError> {
    if local.schema_version != CONFIG_SCHEMA_VERSION {
        return Err(PolicyError::UnsupportedLocalSchema(local.schema_version));
    }

    if let Some(policy) = managed
        && !matches!(
            policy.schema_version,
            LEGACY_MANAGED_POLICY_SCHEMA_VERSION | MANAGED_POLICY_SCHEMA_VERSION
        )
    {
        return Err(PolicyError::UnsupportedManagedSchema(policy.schema_version));
    }
    if let Some(policy) = managed
        && policy.schema_version == LEGACY_MANAGED_POLICY_SCHEMA_VERSION
        && policy.updates.is_some()
    {
        return Err(PolicyError::UpdatesRequireManagedSchemaV2);
    }
    if let Some(policy) = managed
        && (policy.revision.trim().is_empty()
            || policy.revision.chars().count() > MAX_POLICY_REVISION_CHARACTERS
            || policy.revision.contains(['\0', '\r', '\n']))
    {
        return Err(PolicyError::InvalidManagedRevision);
    }

    let mut fields = BTreeMap::new();
    let no_backup = ManagedBackupPolicy::default();
    let no_repository = ManagedRepositoryPolicy::default();
    let no_schedule = ManagedSchedulePolicy::default();
    let no_retention = ManagedRetentionPolicy::default();
    let no_updates = ManagedUpdatePolicy::default();
    let managed_backup = managed.map_or(&no_backup, |policy| &policy.backup);
    let managed_repository = managed.map_or(&no_repository, |policy| &policy.repository);
    let managed_schedule = managed.map_or(&no_schedule, |policy| &policy.schedule);
    let managed_retention = managed.map_or(&no_retention, |policy| &policy.retention);
    let managed_updates = managed
        .and_then(|policy| policy.updates.as_ref())
        .unwrap_or(&no_updates);
    let local_repository_display_name = local.repository.display_name.clone().map(Some);
    let local_repository_url = local.repository.url.clone().map(Some);

    let effective = EffectiveConfig {
        backup: BackupConfig {
            paths: choose(
                PolicyField::BackupPaths,
                &defaults.backup.paths,
                local.backup.paths.as_ref(),
                managed_backup.paths.as_ref(),
                &mut fields,
            ),
            exclusions: choose(
                PolicyField::BackupExclusions,
                &defaults.backup.exclusions,
                local.backup.exclusions.as_ref(),
                managed_backup.exclusions.as_ref(),
                &mut fields,
            ),
        },
        repository: RepositoryConfig {
            display_name: choose(
                PolicyField::RepositoryDisplayName,
                &defaults.repository.display_name,
                local_repository_display_name.as_ref(),
                managed_repository.display_name.as_ref(),
                &mut fields,
            ),
            url: choose(
                PolicyField::RepositoryUrl,
                &defaults.repository.url,
                local_repository_url.as_ref(),
                managed_repository.url.as_ref(),
                &mut fields,
            ),
            mode: choose(
                PolicyField::RepositoryMode,
                &defaults.repository.mode,
                local.repository.mode.as_ref(),
                managed_repository.mode.as_ref(),
                &mut fields,
            ),
            options: choose(
                PolicyField::RepositoryOptions,
                &defaults.repository.options,
                local.repository.options.as_ref(),
                managed_repository.options.as_ref(),
                &mut fields,
            ),
            secret_refs: choose(
                PolicyField::RepositorySecretRefs,
                &defaults.repository.secret_refs,
                local.repository.secret_refs.as_ref(),
                managed_repository.secret_refs.as_ref(),
                &mut fields,
            ),
        },
        schedule: ScheduleConfig {
            interval_hours: choose(
                PolicyField::ScheduleIntervalHours,
                &defaults.schedule.interval_hours,
                local.schedule.interval_hours.as_ref(),
                managed_schedule.interval_hours.as_ref(),
                &mut fields,
            ),
            wake_grace_seconds: choose(
                PolicyField::ScheduleWakeGraceSeconds,
                &defaults.schedule.wake_grace_seconds,
                local.schedule.wake_grace_seconds.as_ref(),
                managed_schedule.wake_grace_seconds.as_ref(),
                &mut fields,
            ),
            wake_lock_timeout_seconds: choose(
                PolicyField::ScheduleWakeLockTimeoutSeconds,
                &defaults.schedule.wake_lock_timeout_seconds,
                local.schedule.wake_lock_timeout_seconds.as_ref(),
                managed_schedule.wake_lock_timeout_seconds.as_ref(),
                &mut fields,
            ),
            allow_on_battery: choose(
                PolicyField::ScheduleAllowOnBattery,
                &defaults.schedule.allow_on_battery,
                local.schedule.allow_on_battery.as_ref(),
                managed_schedule.allow_on_battery.as_ref(),
                &mut fields,
            ),
            allow_metered_network: choose(
                PolicyField::ScheduleAllowMeteredNetwork,
                &defaults.schedule.allow_metered_network,
                local.schedule.allow_metered_network.as_ref(),
                managed_schedule.allow_metered_network.as_ref(),
                &mut fields,
            ),
        },
        retention: RetentionConfig {
            daily: choose(
                PolicyField::RetentionDaily,
                &defaults.retention.daily,
                local.retention.daily.as_ref(),
                managed_retention.daily.as_ref(),
                &mut fields,
            ),
            weekly: choose(
                PolicyField::RetentionWeekly,
                &defaults.retention.weekly,
                local.retention.weekly.as_ref(),
                managed_retention.weekly.as_ref(),
                &mut fields,
            ),
            monthly: choose(
                PolicyField::RetentionMonthly,
                &defaults.retention.monthly,
                local.retention.monthly.as_ref(),
                managed_retention.monthly.as_ref(),
                &mut fields,
            ),
            yearly: choose(
                PolicyField::RetentionYearly,
                &defaults.retention.yearly,
                local.retention.yearly.as_ref(),
                managed_retention.yearly.as_ref(),
                &mut fields,
            ),
            prune_interval_days: choose(
                PolicyField::RetentionPruneIntervalDays,
                &defaults.retention.prune_interval_days,
                local.retention.prune_interval_days.as_ref(),
                managed_retention.prune_interval_days.as_ref(),
                &mut fields,
            ),
        },
        updates: UpdateConfig {
            automatic_install: choose(
                PolicyField::UpdateAutomaticInstall,
                &defaults.updates.automatic_install,
                local.updates.automatic_install.as_ref(),
                managed_updates.automatic_install.as_ref(),
                &mut fields,
            ),
        },
    };

    effective.validate()?;

    Ok(ResolvedConfig {
        effective,
        managed_revision: managed.map(|policy| policy.revision.clone()),
        fields,
    })
}

fn choose<T: Clone>(
    field: PolicyField,
    default: &T,
    local: Option<&T>,
    managed: Option<&Managed<T>>,
    resolutions: &mut BTreeMap<PolicyField, FieldResolution>,
) -> T {
    let (value, source, locked) = match managed {
        Some(managed) if managed.locked => (&managed.value, ValueSource::ManagedLocked, true),
        Some(_) if local.is_some() => (
            local.expect("local value was checked"),
            ValueSource::LocalAdministrator,
            false,
        ),
        Some(managed) => (&managed.value, ValueSource::ManagedRecommendation, false),
        None => match local {
            Some(local) => (local, ValueSource::LocalAdministrator, false),
            None => (default, ValueSource::ProductDefault, false),
        },
    };

    resolutions.insert(field, FieldResolution { source, locked });
    value.clone()
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PolicyError {
    #[error("unsupported local configuration schema {0}")]
    UnsupportedLocalSchema(u32),
    #[error("unsupported managed policy schema {0}")]
    UnsupportedManagedSchema(u32),
    #[error("managed update policy requires schema {MANAGED_POLICY_SCHEMA_VERSION}")]
    UpdatesRequireManagedSchemaV2,
    #[error("managed policy revision must be a non-empty single-line value within the size limit")]
    InvalidManagedRevision,
    #[error(transparent)]
    InvalidEffectiveConfig(#[from] ConfigValidationError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LocalRepositoryConfig, LocalScheduleConfig, LocalUpdateConfig};

    fn managed<T>(value: T, locked: bool) -> Option<Managed<T>> {
        Some(Managed { value, locked })
    }

    #[test]
    fn locked_managed_value_wins_and_is_reported() {
        let local = LocalConfig {
            schedule: LocalScheduleConfig {
                interval_hours: Some(12),
                ..LocalScheduleConfig::default()
            },
            ..LocalConfig::default()
        };
        let policy = ManagedPolicy {
            revision: "policy-7".to_owned(),
            schedule: ManagedSchedulePolicy {
                interval_hours: managed(6, true),
                ..ManagedSchedulePolicy::default()
            },
            ..ManagedPolicy::default()
        };

        let result = resolve_config(&EffectiveConfig::default(), &local, Some(&policy))
            .expect("policy should resolve");

        assert_eq!(result.effective.schedule.interval_hours, 6);
        assert_eq!(result.managed_revision.as_deref(), Some("policy-7"));
        assert!(
            result
                .locked_fields()
                .contains(&PolicyField::ScheduleIntervalHours)
        );
    }

    #[test]
    fn managed_policy_requires_a_bounded_revision() {
        let local = LocalConfig::default();
        for revision in [String::new(), "bad\nrevision".to_owned(), "x".repeat(257)] {
            let policy = ManagedPolicy {
                revision,
                ..ManagedPolicy::default()
            };
            assert_eq!(
                resolve_config(&EffectiveConfig::default(), &local, Some(&policy)),
                Err(PolicyError::InvalidManagedRevision)
            );
        }
    }

    #[test]
    fn local_value_wins_over_unlocked_managed_recommendation() {
        let local = LocalConfig {
            schedule: LocalScheduleConfig {
                allow_on_battery: Some(false),
                ..LocalScheduleConfig::default()
            },
            ..LocalConfig::default()
        };
        let policy = ManagedPolicy {
            revision: "policy-1".to_owned(),
            schedule: ManagedSchedulePolicy {
                allow_on_battery: managed(true, false),
                ..ManagedSchedulePolicy::default()
            },
            ..ManagedPolicy::default()
        };

        let result = resolve_config(&EffectiveConfig::default(), &local, Some(&policy))
            .expect("policy should resolve");

        assert!(!result.effective.schedule.allow_on_battery);
        assert_eq!(
            result.fields[&PolicyField::ScheduleAllowOnBattery].source,
            ValueSource::LocalAdministrator
        );
    }

    #[test]
    fn unlocked_managed_value_fills_an_absent_local_value() {
        let policy = ManagedPolicy {
            revision: "policy-1".to_owned(),
            repository: ManagedRepositoryPolicy {
                mode: managed(RepositoryMode::AppendOnly, false),
                ..ManagedRepositoryPolicy::default()
            },
            ..ManagedPolicy::default()
        };

        let result = resolve_config(
            &EffectiveConfig::default(),
            &LocalConfig::default(),
            Some(&policy),
        )
        .expect("policy should resolve");

        assert_eq!(result.effective.repository.mode, RepositoryMode::AppendOnly);
        assert_eq!(
            result.fields[&PolicyField::RepositoryMode].source,
            ValueSource::ManagedRecommendation
        );
    }

    #[test]
    fn locked_repository_url_can_clear_a_local_url() {
        let local = LocalConfig {
            repository: LocalRepositoryConfig {
                url: Some("local:C:/backups".to_owned()),
                ..LocalRepositoryConfig::default()
            },
            ..LocalConfig::default()
        };
        let policy = ManagedPolicy {
            revision: "policy-1".to_owned(),
            repository: ManagedRepositoryPolicy {
                url: managed(None, true),
                ..ManagedRepositoryPolicy::default()
            },
            ..ManagedPolicy::default()
        };

        let result = resolve_config(&EffectiveConfig::default(), &local, Some(&policy))
            .expect("policy should resolve");

        assert_eq!(result.effective.repository.url, None);
    }

    #[test]
    fn managed_update_recommendations_and_locks_follow_field_precedence() {
        let recommendation = ManagedPolicy {
            revision: "policy-update-recommendation".to_owned(),
            updates: Some(ManagedUpdatePolicy {
                automatic_install: managed(true, false),
            }),
            ..ManagedPolicy::default()
        };
        let recommended = resolve_config(
            &EffectiveConfig::default(),
            &LocalConfig::default(),
            Some(&recommendation),
        )
        .expect("recommendation should resolve");
        assert!(recommended.effective.updates.automatic_install);
        assert_eq!(
            recommended.fields[&PolicyField::UpdateAutomaticInstall],
            FieldResolution {
                source: ValueSource::ManagedRecommendation,
                locked: false,
            }
        );

        let local = LocalConfig {
            updates: LocalUpdateConfig {
                automatic_install: Some(false),
            },
            ..LocalConfig::default()
        };
        let local_override =
            resolve_config(&EffectiveConfig::default(), &local, Some(&recommendation))
                .expect("local override should resolve");
        assert!(!local_override.effective.updates.automatic_install);
        assert_eq!(
            local_override.fields[&PolicyField::UpdateAutomaticInstall].source,
            ValueSource::LocalAdministrator
        );

        let locked = ManagedPolicy {
            revision: "policy-update-lock".to_owned(),
            updates: Some(ManagedUpdatePolicy {
                automatic_install: managed(true, true),
            }),
            ..ManagedPolicy::default()
        };
        let locked_result = resolve_config(&EffectiveConfig::default(), &local, Some(&locked))
            .expect("locked policy should resolve");
        assert!(locked_result.effective.updates.automatic_install);
        assert!(
            locked_result
                .locked_fields()
                .contains(&PolicyField::UpdateAutomaticInstall)
        );
    }

    #[test]
    fn legacy_managed_policy_is_accepted_only_without_update_fields() {
        let legacy_json = r#"{
            "schema_version": 1,
            "revision": "legacy-policy",
            "schedule": {
                "allow_on_battery": { "value": false }
            }
        }"#;
        let legacy: ManagedPolicy =
            serde_json::from_str(legacy_json).expect("legacy policy should deserialize");
        assert_eq!(legacy.schema_version, LEGACY_MANAGED_POLICY_SCHEMA_VERSION);
        assert_eq!(legacy.updates, None);
        let resolved = resolve_config(
            &EffectiveConfig::default(),
            &LocalConfig::default(),
            Some(&legacy),
        )
        .expect("legacy policy should resolve");
        assert!(!resolved.effective.schedule.allow_on_battery);
        assert!(!resolved.effective.updates.automatic_install);

        let invalid_legacy = ManagedPolicy {
            revision: "legacy-with-updates".to_owned(),
            schema_version: LEGACY_MANAGED_POLICY_SCHEMA_VERSION,
            updates: Some(ManagedUpdatePolicy {
                automatic_install: managed(true, false),
            }),
            ..ManagedPolicy::default()
        };
        assert_eq!(
            resolve_config(
                &EffectiveConfig::default(),
                &LocalConfig::default(),
                Some(&invalid_legacy),
            ),
            Err(PolicyError::UpdatesRequireManagedSchemaV2)
        );

        let empty_updates: ManagedPolicy = serde_json::from_str(
            r#"{
                "schema_version": 1,
                "revision": "legacy-with-empty-updates",
                "updates": {}
            }"#,
        )
        .expect("known update policy field should deserialize before schema validation");
        assert_eq!(
            resolve_config(
                &EffectiveConfig::default(),
                &LocalConfig::default(),
                Some(&empty_updates),
            ),
            Err(PolicyError::UpdatesRequireManagedSchemaV2)
        );

        assert!(
            serde_json::from_str::<ManagedPolicy>(
                r#"{
                    "schema_version": 1,
                    "revision": "legacy-with-null-updates",
                    "updates": null
                }"#,
            )
            .is_err(),
            "an explicit updates member cannot deserialize as absence"
        );
    }
}
