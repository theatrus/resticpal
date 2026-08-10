use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use resticpal_core::config::{EffectiveConfig, LocalConfig, LocalManagementConfig, ManagementMode};
use resticpal_core::management::{
    BootstrapDescriptor, DeviceStatusReport, EnrollmentError, EnrollmentMaterial,
    MANAGEMENT_SCHEMA_VERSION, MAX_ENROLLMENT_BYTES, MAX_MANIFEST_BYTES, ManifestError,
    new_enrollment_request, parse_plain_manifest, verify_and_decrypt_enrollment,
    verify_signed_manifest,
};
use resticpal_core::policy::{ManagedPolicy, resolve_config};
use resticpal_core::status::ServiceStatus;
use resticpal_windows::credentials::{CredentialStoreError, DpapiSecretStore};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use zeroize::Zeroizing;

use crate::atomic_file::{self, AtomicFileError};

const CACHE_SCHEMA_VERSION: u32 = 1;
const MAX_CACHE_BYTES: u64 = 2 * 1024 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct LoadedPolicy {
    pub policy: ManagedPolicy,
    pub sequence: u64,
}

pub struct ManagementClient {
    agent: ureq::Agent,
}

pub struct PendingEnrollment {
    pub material: EnrollmentMaterial,
    pub private_key: Zeroizing<[u8; 32]>,
    pub public_key: String,
}

impl ManagementClient {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(HTTP_TIMEOUT))
            .tls_config(
                TlsConfig::builder()
                    .provider(TlsProvider::NativeTls)
                    .root_certs(RootCerts::PlatformVerifier)
                    .build(),
            )
            // Never forward a device bearer token to a redirect target. Static
            // manifest operators should publish the final URL directly.
            .max_redirects(0)
            .user_agent(concat!("resticpal/", env!("CARGO_PKG_VERSION")))
            .build();
        Self {
            agent: config.into(),
        }
    }

    pub fn fetch_policy(
        &self,
        config: &LocalManagementConfig,
        minimum_sequence: Option<u64>,
        credential_store: Option<&DpapiSecretStore>,
    ) -> Result<(LoadedPolicy, String), ManagementError> {
        let url = config
            .manifest_url
            .as_deref()
            .ok_or(ManagementError::MissingManifestUrl)?;
        let mut request = self.agent.get(url);
        let authorization = if config.mode == ManagementMode::SignedManifest {
            config
                .status_token_ref
                .as_deref()
                .map(|reference| bearer_token(credential_store, reference))
                .transpose()?
        } else {
            None
        };
        if let Some(authorization) = &authorization {
            request = request.header("Authorization", authorization.as_str());
        }
        let mut response = request.call()?;
        let document = response
            .body_mut()
            .with_config()
            .limit(u64::try_from(MAX_MANIFEST_BYTES + 1).expect("manifest limit fits in u64"))
            .read_to_vec()?;
        if document.len() > MAX_MANIFEST_BYTES {
            return Err(ManagementError::ManifestTooLarge);
        }
        let now = Utc::now();
        let verified = match config.mode {
            ManagementMode::PlainManifest => {
                parse_plain_manifest(&document, now, minimum_sequence)?
            }
            ManagementMode::SignedManifest => verify_signed_manifest(
                &document,
                config
                    .signing_public_key
                    .as_deref()
                    .ok_or(ManagementError::MissingSigningKey)?,
                now,
                minimum_sequence,
            )?,
            ManagementMode::Disabled => return Err(ManagementError::ManagementDisabled),
        };
        let document = String::from_utf8(document).map_err(|_| ManagementError::InvalidUtf8)?;
        Ok((
            LoadedPolicy {
                policy: verified.payload.policy,
                sequence: verified.payload.sequence,
            },
            document,
        ))
    }

    pub fn enroll(&self, bootstrap_url: &str) -> Result<PendingEnrollment, ManagementError> {
        let descriptor = BootstrapDescriptor::parse(bootstrap_url)?;
        let request_context = new_enrollment_request(
            bounded_hostname(),
            env!("CARGO_PKG_VERSION").to_owned(),
            std::env::consts::ARCH.to_owned(),
        )?;
        let authorization = Zeroizing::new(format!("Bearer {}", descriptor.token()));
        let mut response = self
            .agent
            .post(descriptor.endpoint_url())
            .header("Authorization", authorization.as_str())
            .send_json(&request_context.request)?;
        let document = response
            .body_mut()
            .with_config()
            .limit(u64::try_from(MAX_ENROLLMENT_BYTES + 1).expect("enrollment limit fits in u64"))
            .read_to_vec()?;
        if document.len() > MAX_ENROLLMENT_BYTES {
            return Err(ManagementError::EnrollmentTooLarge);
        }
        let material = verify_and_decrypt_enrollment(
            &document,
            &descriptor,
            &request_context.private_key,
            &request_context.request_nonce,
            Utc::now(),
        )?;
        Ok(PendingEnrollment {
            material,
            private_key: request_context.private_key,
            public_key: descriptor.public_key().to_owned(),
        })
    }

    pub fn report_status(
        &self,
        config: &LocalManagementConfig,
        credential_store: &DpapiSecretStore,
        status: ServiceStatus,
    ) -> Result<(), ManagementError> {
        let status_url = config
            .status_url
            .as_deref()
            .ok_or(ManagementError::StatusReportingNotConfigured)?;
        let device_id = config
            .device_id
            .as_deref()
            .ok_or(ManagementError::StatusReportingNotConfigured)?;
        let token_ref = config
            .status_token_ref
            .as_deref()
            .ok_or(ManagementError::StatusReportingNotConfigured)?;
        let token = credential_store.get(token_ref)?;
        let token = std::str::from_utf8(&token).map_err(|_| ManagementError::InvalidStatusToken)?;
        if token.contains(['\0', '\r', '\n']) {
            return Err(ManagementError::InvalidStatusToken);
        }
        let authorization = Zeroizing::new(format!("Bearer {token}"));
        let report = DeviceStatusReport {
            schema_version: MANAGEMENT_SCHEMA_VERSION,
            device_id: device_id.to_owned(),
            hostname: bounded_hostname(),
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
            reported_at: Utc::now(),
            status,
        };
        self.agent
            .post(status_url)
            .header("Authorization", authorization.as_str())
            .send_json(&report)?;
        Ok(())
    }
}

pub fn load_best_policy(
    config_path: &Path,
    local: &LocalConfig,
    credential_store: Option<&DpapiSecretStore>,
) -> Result<Option<LoadedPolicy>, ManagementError> {
    if local.management.mode == ManagementMode::Disabled {
        return Ok(None);
    }
    local.management.validate()?;
    let cache = ManagedPolicyCache::next_to_config(config_path);
    let client = ManagementClient::new();
    let cached = cache.load(&local.management);
    let minimum_sequence = cached.as_ref().ok().map(|loaded| loaded.sequence);
    match client.fetch_policy(&local.management, minimum_sequence, credential_store) {
        Ok((loaded, document)) => {
            if minimum_sequence == Some(loaded.sequence) {
                let cached = cached.expect("a minimum sequence comes from a valid cache");
                validate_effective_policy(local, &cached.policy)?;
                return Ok(Some(cached));
            }
            validate_effective_policy(local, &loaded.policy)?;
            cache.save(&local.management, &document, Utc::now())?;
            Ok(Some(loaded))
        }
        Err(fetch_error) => match cached {
            Ok(loaded) => {
                validate_effective_policy(local, &loaded.policy)?;
                Ok(Some(loaded))
            }
            Err(cache_error) => Err(ManagementError::FetchAndCacheUnavailable {
                fetch: fetch_error.to_string(),
                cache: cache_error.to_string(),
            }),
        },
    }
}

pub fn refresh_policy(
    client: &ManagementClient,
    config_path: &Path,
    local: &LocalConfig,
    minimum_sequence: u64,
    credential_store: Option<&DpapiSecretStore>,
) -> Result<LoadedPolicy, ManagementError> {
    let (loaded, document) =
        client.fetch_policy(&local.management, Some(minimum_sequence), credential_store)?;
    if loaded.sequence == minimum_sequence {
        return Ok(loaded);
    }
    validate_effective_policy(local, &loaded.policy)?;
    ManagedPolicyCache::next_to_config(config_path).save(
        &local.management,
        &document,
        Utc::now(),
    )?;
    Ok(loaded)
}

fn validate_effective_policy(
    local: &LocalConfig,
    policy: &ManagedPolicy,
) -> Result<(), ManagementError> {
    if local.management.enrollment_key_ref.is_some() && policy.repository.secret_refs.is_some() {
        return Err(ManagementError::EnrolledPolicyContainsSecretReferences);
    }
    resolve_config(&EffectiveConfig::default(), local, Some(policy))?;
    Ok(())
}

#[derive(Debug, Clone)]
struct ManagedPolicyCache {
    path: PathBuf,
}

impl ManagedPolicyCache {
    fn next_to_config(config_path: &Path) -> Self {
        Self {
            path: config_path.with_file_name("managed-policy-cache.json"),
        }
    }

    fn save(
        &self,
        config: &LocalManagementConfig,
        document: &str,
        verified_at: DateTime<Utc>,
    ) -> Result<(), ManagementError> {
        let cache = CachedManifest {
            schema_version: CACHE_SCHEMA_VERSION,
            mode: config.mode,
            source_url: config.manifest_url.clone().unwrap_or_default(),
            verified_at,
            document: document.to_owned(),
        };
        let contents = serde_json::to_vec_pretty(&cache)?;
        if contents.len() as u64 > MAX_CACHE_BYTES {
            return Err(ManagementError::CacheTooLarge);
        }
        atomic_file::replace(&self.path, &contents, "managed-policy")?;
        Ok(())
    }

    fn load(&self, config: &LocalManagementConfig) -> Result<LoadedPolicy, ManagementError> {
        let mut file = fs::File::open(&self.path)?;
        let mut contents = Vec::new();
        Read::by_ref(&mut file)
            .take(MAX_CACHE_BYTES + 1)
            .read_to_end(&mut contents)?;
        if contents.len() as u64 > MAX_CACHE_BYTES {
            return Err(ManagementError::CacheTooLarge);
        }
        let cache: CachedManifest = serde_json::from_slice(&contents)?;
        if cache.schema_version != CACHE_SCHEMA_VERSION
            || cache.mode != config.mode
            || Some(cache.source_url.as_str()) != config.manifest_url.as_deref()
        {
            return Err(ManagementError::CacheDoesNotMatchSource);
        }
        let verified = match cache.mode {
            ManagementMode::PlainManifest => {
                parse_plain_manifest(cache.document.as_bytes(), cache.verified_at, None)?
            }
            ManagementMode::SignedManifest => verify_signed_manifest(
                cache.document.as_bytes(),
                config
                    .signing_public_key
                    .as_deref()
                    .ok_or(ManagementError::MissingSigningKey)?,
                cache.verified_at,
                None,
            )?,
            ManagementMode::Disabled => return Err(ManagementError::ManagementDisabled),
        };
        Ok(LoadedPolicy {
            policy: verified.payload.policy,
            sequence: verified.payload.sequence,
        })
    }
}

pub fn save_enrollment_cache(
    config_path: &Path,
    management: &LocalManagementConfig,
    document: &str,
) -> Result<(), ManagementError> {
    ManagedPolicyCache::next_to_config(config_path).save(management, document, Utc::now())
}

pub fn remove_management_cache(config_path: &Path) -> Result<(), ManagementError> {
    let path = ManagedPolicyCache::next_to_config(config_path).path;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CachedManifest {
    schema_version: u32,
    mode: ManagementMode,
    source_url: String,
    verified_at: DateTime<Utc>,
    document: String,
}

fn bounded_hostname() -> String {
    std::env::var("COMPUTERNAME")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(256).collect())
        .unwrap_or_else(|| "unknown-windows-device".to_owned())
}

fn bearer_token(
    credential_store: Option<&DpapiSecretStore>,
    reference: &str,
) -> Result<Zeroizing<String>, ManagementError> {
    let credential_store = credential_store.ok_or(ManagementError::CredentialStoreUnavailable)?;
    let token = credential_store.get(reference)?;
    let token = std::str::from_utf8(&token).map_err(|_| ManagementError::InvalidStatusToken)?;
    if token.contains(['\0', '\r', '\n']) {
        return Err(ManagementError::InvalidStatusToken);
    }
    Ok(Zeroizing::new(format!("Bearer {token}")))
}

#[derive(Debug, Error)]
pub enum ManagementError {
    #[error("management is disabled")]
    ManagementDisabled,
    #[error("manifest URL is missing")]
    MissingManifestUrl,
    #[error("signed manifest public key is missing")]
    MissingSigningKey,
    #[error("management manifest exceeds the size limit")]
    ManifestTooLarge,
    #[error("enrollment response exceeds the size limit")]
    EnrollmentTooLarge,
    #[error("management manifest is not UTF-8 JSON")]
    InvalidUtf8,
    #[error("status reporting is not configured")]
    StatusReportingNotConfigured,
    #[error("protected credential store is unavailable")]
    CredentialStoreUnavailable,
    #[error("status token is not valid single-line UTF-8")]
    InvalidStatusToken,
    #[error("enrolled policies must deliver secret values during enrollment, not DPAPI references")]
    EnrolledPolicyContainsSecretReferences,
    #[error("managed-policy cache exceeds the size limit")]
    CacheTooLarge,
    #[error("managed-policy cache does not match the configured source")]
    CacheDoesNotMatchSource,
    #[error("manifest fetch failed and no usable cache exists (fetch: {fetch}; cache: {cache})")]
    FetchAndCacheUnavailable { fetch: String, cache: String },
    #[error(transparent)]
    Config(#[from] resticpal_core::config::ManagementConfigError),
    #[error(transparent)]
    Manifest(#[from] ManifestError),
    #[error(transparent)]
    Enrollment(#[from] EnrollmentError),
    #[error(transparent)]
    Policy(#[from] resticpal_core::policy::PolicyError),
    #[error("management HTTP request failed: {0}")]
    Http(String),
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Atomic(#[from] AtomicFileError),
    #[error(transparent)]
    Credential(#[from] CredentialStoreError),
}

impl From<ureq::Error> for ManagementError {
    fn from(error: ureq::Error) -> Self {
        Self::Http(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use resticpal_core::management::{MANAGEMENT_SCHEMA_VERSION, ManifestPayload};

    #[test]
    fn management_client_uses_windows_native_tls_and_platform_roots() {
        let client = ManagementClient::new();
        let tls = client.agent.config().tls_config();

        assert_eq!(tls.provider(), TlsProvider::NativeTls);
        assert!(matches!(tls.root_certs(), RootCerts::PlatformVerifier));
    }

    #[test]
    fn plain_http_manifest_is_cached_for_offline_startup() {
        let now = Utc::now();
        let document = serde_json::to_vec(&ManifestPayload {
            schema_version: MANAGEMENT_SCHEMA_VERSION,
            sequence: 12,
            issued_at: now,
            expires_at: None,
            policy: ManagedPolicy {
                revision: "plain-12".to_owned(),
                ..ManagedPolicy::default()
            },
        })
        .expect("manifest serializes");
        let (url, server) = serve_once(document);
        let temporary = tempfile::tempdir().expect("tempdir");
        let config_path = temporary.path().join("resticpal.toml");
        let local = LocalConfig {
            management: LocalManagementConfig {
                mode: ManagementMode::PlainManifest,
                manifest_url: Some(url),
                ..LocalManagementConfig::default()
            },
            ..LocalConfig::default()
        };

        let fetched = load_best_policy(&config_path, &local, None)
            .expect("online manifest loads")
            .expect("managed policy");
        server.join().expect("server exits");
        assert_eq!(fetched.sequence, 12);

        let cached = load_best_policy(&config_path, &local, None)
            .expect("offline cache loads")
            .expect("cached policy");
        assert_eq!(cached.policy.revision, "plain-12");
    }

    #[test]
    fn cached_sequence_blocks_a_restart_rollback() {
        let now = Utc::now();
        let documents = [12_u64, 11_u64].map(|sequence| {
            serde_json::to_vec(&ManifestPayload {
                schema_version: MANAGEMENT_SCHEMA_VERSION,
                sequence,
                issued_at: now,
                expires_at: None,
                policy: ManagedPolicy {
                    revision: format!("plain-{sequence}"),
                    ..ManagedPolicy::default()
                },
            })
            .expect("manifest serializes")
        });
        let (url, server) = serve_documents(documents);
        let temporary = tempfile::tempdir().expect("tempdir");
        let config_path = temporary.path().join("resticpal.toml");
        let local = LocalConfig {
            management: LocalManagementConfig {
                mode: ManagementMode::PlainManifest,
                manifest_url: Some(url),
                ..LocalManagementConfig::default()
            },
            ..LocalConfig::default()
        };

        let current = load_best_policy(&config_path, &local, None)
            .expect("initial policy")
            .expect("managed policy");
        assert_eq!(current.sequence, 12);
        let after_replay = load_best_policy(&config_path, &local, None)
            .expect("cached policy survives rollback")
            .expect("managed policy");
        server.join().expect("server exits");
        assert_eq!(after_replay.sequence, 12);
        assert_eq!(after_replay.policy.revision, "plain-12");
    }

    fn serve_once(body: Vec<u8>) -> (String, thread::JoinHandle<()>) {
        serve_documents([body])
    }

    fn serve_documents<const N: usize>(bodies: [Vec<u8>; N]) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind listener");
        let address = listener.local_addr().expect("listener address");
        let server = thread::spawn(move || {
            for body in bodies {
                let (mut stream, _) = listener.accept().expect("accept request");
                let mut request = [0_u8; 4096];
                let count = stream.read(&mut request).expect("read request");
                assert!(request[..count].starts_with(b"GET /manifest HTTP/1.1"));
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .expect("write headers");
                stream.write_all(&body).expect("write body");
            }
        });
        (format!("http://{address}/manifest"), server)
    }
}
