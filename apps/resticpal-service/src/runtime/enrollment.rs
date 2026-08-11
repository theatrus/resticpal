use super::events::RuntimeEvent;
use super::helpers::*;
use super::state::ServiceRuntime;

use chrono::Utc;
use resticpal_core::config::{EffectiveConfig, LocalManagementConfig, ManagementMode};
use resticpal_core::policy::{ManagedPolicy, PolicyError, resolve_config};
use resticpal_core::status::BackupState;
use resticpal_protocol::{ManagementView, RepositoryOperationStatus, ResponsePayload};
use resticpal_windows::named_pipe::ClientIdentity;

use crate::management::{
    ManagementClient, PendingEnrollment, remove_management_cache, save_enrollment_cache,
};

impl ServiceRuntime {
    pub fn enroll_bootstrap(&self, bootstrap_url: &str) -> Result<(), String> {
        {
            let mut state = self.state_guard();
            if matches!(state.status.state, BackupState::Running { .. })
                || matches!(
                    state.repository_operation,
                    RepositoryOperationStatus::Running { .. }
                )
                || state.management_operation_active
            {
                return Err("another service operation is active".to_owned());
            }
            state.management_operation_active = true;
        }
        let result = self.perform_enrollment(bootstrap_url);
        self.state_guard().management_operation_active = false;
        result
    }

    pub(super) fn management_view(&self) -> ManagementView {
        let management = self.local_config_guard().management.clone();
        ManagementView {
            mode: management.mode,
            enrolled: management.mode == ManagementMode::SignedManifest
                && management.status_token_ref.is_some()
                && management.enrollment_key_ref.is_some(),
            device_id: management.device_id,
            manifest_url: management.manifest_url,
        }
    }

    pub(super) fn enroll(&self, bootstrap_url: &[u8], identity: ClientIdentity) -> ResponsePayload {
        if !identity.is_elevated_administrator {
            return administrator_required();
        }
        let Ok(bootstrap_url) = std::str::from_utf8(bootstrap_url) else {
            return rejected(
                "invalid_bootstrap_url",
                "The bootstrap URL is not valid UTF-8.",
            );
        };
        {
            let mut state = self.state_guard();
            if matches!(state.status.state, BackupState::Running { .. })
                || matches!(
                    state.repository_operation,
                    RepositoryOperationStatus::Running { .. }
                )
                || state.management_operation_active
            {
                return rejected(
                    "operation_running",
                    "Wait for the current backup, repository, or enrollment operation to finish.",
                );
            }
            state.management_operation_active = true;
        }
        let result = self.perform_enrollment(bootstrap_url);
        self.state_guard().management_operation_active = false;
        match result {
            Ok(()) => ResponsePayload::Accepted {
                message: "This device is now enrolled and its signed policy is active.".to_owned(),
            },
            Err(error) => {
                eprintln!("managed enrollment failed: {error}");
                rejected(
                    "enrollment_failed",
                    "Enrollment failed. Check that the one-time URL is current and try again.",
                )
            }
        }
    }

    fn perform_enrollment(&self, bootstrap_url: &str) -> Result<(), String> {
        let pending = ManagementClient::new()
            .enroll(bootstrap_url)
            .map_err(|error| error.to_string())?;
        self.commit_enrollment(pending)
    }

    pub(super) fn commit_enrollment(&self, pending: PendingEnrollment) -> Result<(), String> {
        let credential_store = self
            .credential_store
            .as_ref()
            .ok_or_else(|| "protected credential storage is unavailable".to_owned())?;
        let config_store = self
            .config_store
            .as_ref()
            .ok_or_else(|| "local configuration storage is unavailable".to_owned())?;
        let old_local = self.local_config_guard().clone();
        let mut candidate = old_local.clone();
        let mut created_references = Vec::new();
        let stored = (|| -> Result<(), String> {
            let status_ref = credential_store
                .put_new("management-token", pending.material.status_token.as_bytes())
                .map_err(|error| error.to_string())?;
            created_references.push(status_ref.clone());
            let key_ref = credential_store
                .put_new("management-key", &*pending.private_key)
                .map_err(|error| error.to_string())?;
            created_references.push(key_ref.clone());

            let secret_refs = candidate.repository.secret_refs.get_or_insert_default();
            for (variable, secret) in &pending.material.repository_secrets {
                let reference = credential_store
                    .put_new(variable.reference_prefix(), secret.as_bytes())
                    .map_err(|error| error.to_string())?;
                created_references.push(reference.clone());
                secret_refs.insert(*variable, reference);
            }
            candidate.management = LocalManagementConfig {
                mode: ManagementMode::SignedManifest,
                manifest_url: Some(pending.material.manifest_url.clone()),
                signing_public_key: Some(pending.public_key.clone()),
                refresh_interval_minutes: Some(pending.material.refresh_interval_minutes),
                status_url: Some(pending.material.status_url.clone()),
                device_id: Some(pending.material.device_id.clone()),
                status_token_ref: Some(status_ref),
                enrollment_key_ref: Some(key_ref),
            };
            candidate
                .management
                .validate()
                .map_err(|error| error.to_string())?;
            if pending
                .material
                .initial_manifest
                .payload
                .policy
                .repository
                .secret_refs
                .is_some()
            {
                return Err(
                    "enrolled policy contains client-local credential references".to_owned(),
                );
            }
            let resolved = resolve_config(
                &EffectiveConfig::default(),
                &candidate,
                Some(&pending.material.initial_manifest.payload.policy),
            )
            .map_err(|error| error.to_string())?;

            config_store
                .save(&candidate)
                .map_err(|error| error.to_string())?;
            if let Err(error) = save_enrollment_cache(
                config_store.path(),
                &candidate.management,
                &pending.material.initial_manifest_document,
            ) {
                eprintln!(
                    "enrollment committed but its initial policy cache could not be saved: {error}"
                );
            }

            let next_config = resolved.effective;
            let now = Utc::now();
            let mut state = self.state_guard();
            if state
                .service_state
                .repository_requires_validation(&next_config)
            {
                state.service_state.require_repository_validation();
                if let Some(store) = &self.state_store {
                    let _ = store.save(&state.service_state);
                }
                state.repository_operation = RepositoryOperationStatus::ValidationRequired;
            }
            *self.config_write() = next_config.clone();
            *self
                .field_resolutions
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = resolved.fields;
            state.status.managed_revision = resolved.managed_revision;
            Self::apply_configuration_status(&mut state, &next_config, now);
            drop(state);
            *self.local_config_guard() = candidate;
            Ok(())
        })();

        if let Err(error) = stored {
            for reference in &created_references {
                let _ = credential_store.remove(reference);
            }
            return Err(error);
        }

        let new_local = self.local_config_guard().clone();
        let mut retired = Vec::new();
        if let Some(reference) = old_local.management.status_token_ref {
            retired.push(reference);
        }
        if let Some(reference) = old_local.management.enrollment_key_ref {
            retired.push(reference);
        }
        for variable in pending.material.repository_secrets.keys() {
            if let Some(reference) = old_local
                .repository
                .secret_refs
                .as_ref()
                .and_then(|refs| refs.get(variable))
                && new_local
                    .repository
                    .secret_refs
                    .as_ref()
                    .and_then(|refs| refs.get(variable))
                    != Some(reference)
            {
                retired.push(reference.clone());
            }
        }
        for reference in retired {
            let _ = credential_store.remove(&reference);
        }
        let _ = self.events.send(RuntimeEvent::ConfigurationChanged);
        Ok(())
    }

    pub(super) fn unenroll(&self, identity: ClientIdentity) -> ResponsePayload {
        if !identity.is_elevated_administrator {
            return administrator_required();
        }
        {
            let state = self.state_guard();
            if matches!(state.status.state, BackupState::Running { .. })
                || matches!(
                    state.repository_operation,
                    RepositoryOperationStatus::Running { .. }
                )
                || state.management_operation_active
            {
                return rejected(
                    "operation_running",
                    "Wait for the current backup, repository, or enrollment operation to finish.",
                );
            }
        }
        let Some(config_store) = &self.config_store else {
            return rejected(
                "configuration_unavailable",
                "Local configuration storage is unavailable.",
            );
        };
        let old_local = self.local_config_guard().clone();
        if old_local.management.mode == ManagementMode::Disabled {
            return ResponsePayload::Accepted {
                message: "This device is already unmanaged.".to_owned(),
            };
        }
        let mut candidate = old_local.clone();
        candidate.management = LocalManagementConfig::default();
        let resolved = match resolve_config(&EffectiveConfig::default(), &candidate, None) {
            Ok(resolved) => resolved,
            Err(error) => {
                eprintln!("could not resolve unmanaged configuration: {error}");
                return rejected(
                    "configuration_invalid",
                    "The remaining local configuration is invalid.",
                );
            }
        };
        if let Err(error) = config_store.save(&candidate) {
            eprintln!("could not save unmanaged configuration: {error}");
            return rejected(
                "configuration_save_failed",
                "The unmanaged configuration could not be saved.",
            );
        }
        if let Err(error) = remove_management_cache(config_store.path()) {
            eprintln!("could not remove the managed policy cache: {error}");
        }
        *self.local_config_guard() = candidate;
        *self.config_write() = resolved.effective.clone();
        *self
            .field_resolutions
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = resolved.fields;
        let mut state = self.state_guard();
        state.status.managed_revision = None;
        Self::apply_configuration_status(&mut state, &resolved.effective, Utc::now());
        drop(state);
        if let Some(credentials) = &self.credential_store {
            for reference in [
                old_local.management.status_token_ref,
                old_local.management.enrollment_key_ref,
            ]
            .into_iter()
            .flatten()
            {
                let _ = credentials.remove(&reference);
            }
        }
        let _ = self.events.send(RuntimeEvent::ConfigurationChanged);
        ResponsePayload::Accepted {
            message: "Managed policy and reporting were removed; repository credentials remain protected locally."
                .to_owned(),
        }
    }

    pub fn apply_managed_policy(&self, policy: &ManagedPolicy) -> Result<bool, PolicyError> {
        let local = self.local_config_guard().clone();
        let resolved = resolve_config(&EffectiveConfig::default(), &local, Some(policy))?;
        let next_config = resolved.effective;
        let now = Utc::now();
        let mut state = self.state_guard();
        if matches!(state.status.state, BackupState::Running { .. })
            || matches!(
                state.repository_operation,
                RepositoryOperationStatus::Running { .. }
            )
        {
            return Ok(false);
        }

        if state
            .service_state
            .repository_requires_validation(&next_config)
        {
            state.service_state.require_repository_validation();
            if let Some(store) = &self.state_store
                && let Err(error) = store.save(&state.service_state)
            {
                eprintln!("could not persist repository validation requirement: {error}");
            }
            state.repository_operation = RepositoryOperationStatus::ValidationRequired;
        }

        *self.config_write() = next_config.clone();
        *self
            .field_resolutions
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = resolved.fields;
        state.status.managed_revision = resolved.managed_revision;
        Self::apply_configuration_status(&mut state, &next_config, now);
        drop(state);
        let _ = self.events.send(RuntimeEvent::ConfigurationChanged);
        Ok(true)
    }
}
