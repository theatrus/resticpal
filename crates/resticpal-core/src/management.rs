use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, VerifyingKey};
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use std::collections::BTreeMap;
use thiserror::Error;
use url::Url;
use x25519_dalek::{PublicKey, StaticSecret};
use zeroize::Zeroizing;

use crate::config::SecretEnvironmentVariable;
use crate::policy::ManagedPolicy;
use crate::status::ServiceStatus;

pub const MANAGEMENT_SCHEMA_VERSION: u32 = 1;
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
pub const MAX_ENROLLMENT_BYTES: usize = 1024 * 1024;
const MAX_SIGNED_MANIFEST_LIFETIME_DAYS: i64 = 30;
const MAX_CLOCK_SKEW_MINUTES: i64 = 5;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestPayload {
    pub schema_version: u32,
    pub sequence: u64,
    pub issued_at: DateTime<Utc>,
    pub expires_at: Option<DateTime<Utc>>,
    pub policy: ManagedPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedManifestEnvelope {
    pub schema_version: u32,
    pub algorithm: String,
    pub key_id: String,
    pub payload: String,
    pub signature: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedManifest {
    pub payload: ManifestPayload,
    pub signed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceStatusReport {
    pub schema_version: u32,
    pub device_id: String,
    pub hostname: String,
    pub app_version: String,
    pub architecture: String,
    pub reported_at: DateTime<Utc>,
    pub status: ServiceStatus,
}

pub struct BootstrapDescriptor {
    endpoint_url: String,
    token: Zeroizing<String>,
    public_key: String,
    key_id: String,
}

impl BootstrapDescriptor {
    pub fn parse(value: &str) -> Result<Self, EnrollmentError> {
        if value.is_empty() || value.len() > 16 * 1024 || value.contains(['\0', '\r', '\n']) {
            return Err(EnrollmentError::InvalidBootstrapUrl);
        }
        let mut url = Url::parse(value).map_err(|_| EnrollmentError::InvalidBootstrapUrl)?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || !signed_transport_allowed(&url)
        {
            return Err(EnrollmentError::InvalidBootstrapUrl);
        }
        let fragment = url
            .fragment()
            .ok_or(EnrollmentError::MissingBootstrapFields)?;
        let mut token = None;
        let mut public_key = None;
        let mut key_id = None;
        for (name, value) in url::form_urlencoded::parse(fragment.as_bytes()) {
            let target = match name.as_ref() {
                "token" => &mut token,
                "key" => &mut public_key,
                "key_id" => &mut key_id,
                _ => return Err(EnrollmentError::InvalidBootstrapUrl),
            };
            if target.replace(value.into_owned()).is_some() {
                return Err(EnrollmentError::InvalidBootstrapUrl);
            }
        }
        let token = token.filter(|value| !value.is_empty() && value.len() <= 4096);
        let public_key = public_key.filter(|value| decode_raw_array::<32>(value).is_ok());
        let key_id = key_id.filter(|value| valid_text(value, 256));
        let (token, public_key, key_id) = match (token, public_key, key_id) {
            (Some(token), Some(public_key), Some(key_id)) => (token, public_key, key_id),
            _ => return Err(EnrollmentError::MissingBootstrapFields),
        };
        url.set_fragment(None);
        Ok(Self {
            endpoint_url: url.to_string(),
            token: Zeroizing::new(token),
            public_key,
            key_id,
        })
    }

    #[must_use]
    pub fn endpoint_url(&self) -> &str {
        &self.endpoint_url
    }

    #[must_use]
    pub fn token(&self) -> &str {
        &self.token
    }

    #[must_use]
    pub fn public_key(&self) -> &str {
        &self.public_key
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollmentRequest {
    pub schema_version: u32,
    pub device_public_key: String,
    pub request_nonce: String,
    pub hostname: String,
    pub app_version: String,
    pub architecture: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentSecretBundle {
    schema_version: u32,
    status_token: String,
    repository_secrets: BTreeMap<SecretEnvironmentVariable, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SealedSecretBundle {
    algorithm: String,
    ephemeral_public_key: String,
    salt: String,
    nonce: String,
    ciphertext: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnrollmentPayload {
    schema_version: u32,
    device_id: String,
    request_nonce: String,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    manifest_url: String,
    status_url: String,
    refresh_interval_minutes: u32,
    initial_manifest: SignedManifestEnvelope,
    secrets: SealedSecretBundle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignedEnrollmentEnvelope {
    schema_version: u32,
    algorithm: String,
    key_id: String,
    payload: String,
    signature: String,
}

pub struct EnrollmentMaterial {
    pub device_id: String,
    pub manifest_url: String,
    pub status_url: String,
    pub refresh_interval_minutes: u32,
    pub initial_manifest_document: String,
    pub initial_manifest: VerifiedManifest,
    pub status_token: Zeroizing<String>,
    pub repository_secrets: BTreeMap<SecretEnvironmentVariable, Zeroizing<String>>,
}

pub struct EnrollmentRequestContext {
    pub request: EnrollmentRequest,
    pub private_key: Zeroizing<[u8; 32]>,
    pub request_nonce: [u8; 32],
}

pub fn new_enrollment_request(
    hostname: String,
    app_version: String,
    architecture: String,
) -> Result<EnrollmentRequestContext, EnrollmentError> {
    if !valid_text(&hostname, 256)
        || !valid_text(&app_version, 64)
        || !valid_text(&architecture, 32)
    {
        return Err(EnrollmentError::InvalidRequestMetadata);
    }
    let private_key = StaticSecret::random();
    let public_key = PublicKey::from(&private_key);
    let mut request_nonce = [0_u8; 32];
    getrandom::fill(&mut request_nonce).map_err(|_| EnrollmentError::Random)?;
    Ok(EnrollmentRequestContext {
        request: EnrollmentRequest {
            schema_version: MANAGEMENT_SCHEMA_VERSION,
            device_public_key: URL_SAFE_NO_PAD.encode(public_key.as_bytes()),
            request_nonce: URL_SAFE_NO_PAD.encode(request_nonce),
            hostname,
            app_version,
            architecture,
        },
        private_key: Zeroizing::new(private_key.to_bytes()),
        request_nonce,
    })
}

pub fn verify_and_decrypt_enrollment(
    document: &[u8],
    descriptor: &BootstrapDescriptor,
    private_key: &[u8; 32],
    expected_nonce: &[u8; 32],
    now: DateTime<Utc>,
) -> Result<EnrollmentMaterial, EnrollmentError> {
    if document.is_empty() || document.len() > MAX_ENROLLMENT_BYTES {
        return Err(EnrollmentError::InvalidDocumentSize);
    }
    let envelope: SignedEnrollmentEnvelope = serde_json::from_slice(document)?;
    if envelope.schema_version != MANAGEMENT_SCHEMA_VERSION
        || envelope.algorithm != "ed25519"
        || envelope.key_id != descriptor.key_id
    {
        return Err(EnrollmentError::InvalidEnvelope);
    }
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(&envelope.payload)
        .map_err(|_| EnrollmentError::InvalidEnvelope)?;
    let public_key = decode_raw_array::<32>(&descriptor.public_key)
        .map_err(|_| EnrollmentError::InvalidEnvelope)?;
    let signature = decode_raw_array::<64>(&envelope.signature)
        .map_err(|_| EnrollmentError::InvalidEnvelope)?;
    let verifying_key =
        VerifyingKey::from_bytes(&public_key).map_err(|_| EnrollmentError::InvalidEnvelope)?;
    verifying_key
        .verify_strict(&payload_bytes, &Signature::from_bytes(&signature))
        .map_err(|_| EnrollmentError::SignatureVerificationFailed)?;
    let payload: EnrollmentPayload = serde_json::from_slice(&payload_bytes)?;
    if payload.schema_version != MANAGEMENT_SCHEMA_VERSION
        || payload.request_nonce != URL_SAFE_NO_PAD.encode(expected_nonce)
        || !valid_id(&payload.device_id)
        || payload.issued_at > now + Duration::minutes(MAX_CLOCK_SKEW_MINUTES)
        || payload.expires_at < now
        || payload.expires_at <= payload.issued_at
        || payload.expires_at - payload.issued_at > Duration::minutes(15)
        || !(5..=24 * 60).contains(&payload.refresh_interval_minutes)
        || payload.initial_manifest.key_id != descriptor.key_id
    {
        return Err(EnrollmentError::InvalidPayload);
    }
    for value in [&payload.manifest_url, &payload.status_url] {
        let url = Url::parse(value).map_err(|_| EnrollmentError::InvalidPayload)?;
        if url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || !signed_transport_allowed(&url)
        {
            return Err(EnrollmentError::InvalidPayload);
        }
    }
    let initial_manifest_document = serde_json::to_string(&payload.initial_manifest)?;
    let initial_manifest = verify_signed_manifest(
        initial_manifest_document.as_bytes(),
        descriptor.public_key(),
        now,
        None,
    )?;
    if payload.secrets.algorithm != "x25519-hkdf-sha256-chacha20poly1305" {
        return Err(EnrollmentError::InvalidEncryption);
    }
    let ephemeral = PublicKey::from(
        decode_raw_array::<32>(&payload.secrets.ephemeral_public_key)
            .map_err(|_| EnrollmentError::InvalidEncryption)?,
    );
    let salt = decode_raw_array::<32>(&payload.secrets.salt)
        .map_err(|_| EnrollmentError::InvalidEncryption)?;
    let nonce_bytes = decode_raw_array::<12>(&payload.secrets.nonce)
        .map_err(|_| EnrollmentError::InvalidEncryption)?;
    let ciphertext = URL_SAFE_NO_PAD
        .decode(&payload.secrets.ciphertext)
        .map_err(|_| EnrollmentError::InvalidEncryption)?;
    let private_key = StaticSecret::from(*private_key);
    let shared_secret = private_key.diffie_hellman(&ephemeral);
    if !shared_secret.was_contributory() {
        return Err(EnrollmentError::InvalidEncryption);
    }
    let mut encryption_key = Zeroizing::new([0_u8; 32]);
    Hkdf::<Sha256>::new(Some(&salt), shared_secret.as_bytes())
        .expand(
            b"resticpal enrollment secret bundle v1",
            encryption_key.as_mut(),
        )
        .map_err(|_| EnrollmentError::InvalidEncryption)?;
    let cipher = ChaCha20Poly1305::new((&*encryption_key).into());
    let plaintext = Zeroizing::new(
        cipher
            .decrypt(
                &Nonce::from(nonce_bytes),
                Payload {
                    msg: &ciphertext,
                    aad: payload.device_id.as_bytes(),
                },
            )
            .map_err(|_| EnrollmentError::DecryptionFailed)?,
    );
    let secrets: EnrollmentSecretBundle = serde_json::from_slice(&plaintext)?;
    if secrets.schema_version != MANAGEMENT_SCHEMA_VERSION
        || secrets.status_token.is_empty()
        || secrets.status_token.len() > 4096
        || secrets.status_token.contains(['\0', '\r', '\n'])
        || secrets
            .repository_secrets
            .values()
            .any(|secret| secret.is_empty() || secret.len() > 32 * 1024 || secret.contains('\0'))
    {
        return Err(EnrollmentError::InvalidSecrets);
    }
    Ok(EnrollmentMaterial {
        device_id: payload.device_id,
        manifest_url: payload.manifest_url,
        status_url: payload.status_url,
        refresh_interval_minutes: payload.refresh_interval_minutes,
        initial_manifest_document,
        initial_manifest,
        status_token: Zeroizing::new(secrets.status_token),
        repository_secrets: secrets
            .repository_secrets
            .into_iter()
            .map(|(variable, value)| (variable, Zeroizing::new(value)))
            .collect(),
    })
}

pub fn parse_plain_manifest(
    document: &[u8],
    now: DateTime<Utc>,
    minimum_sequence: Option<u64>,
) -> Result<VerifiedManifest, ManifestError> {
    validate_document_size(document)?;
    let payload: ManifestPayload = serde_json::from_slice(document)?;
    validate_payload(&payload, now, minimum_sequence, false)?;
    Ok(VerifiedManifest {
        payload,
        signed: false,
    })
}

pub fn verify_signed_manifest(
    document: &[u8],
    public_key: &str,
    validation_time: DateTime<Utc>,
    minimum_sequence: Option<u64>,
) -> Result<VerifiedManifest, ManifestError> {
    validate_document_size(document)?;
    let envelope: SignedManifestEnvelope = serde_json::from_slice(document)?;
    if envelope.schema_version != MANAGEMENT_SCHEMA_VERSION {
        return Err(ManifestError::UnsupportedEnvelopeSchema(
            envelope.schema_version,
        ));
    }
    if envelope.algorithm != "ed25519" {
        return Err(ManifestError::UnsupportedAlgorithm);
    }
    if envelope.key_id.is_empty()
        || envelope.key_id.len() > 256
        || envelope.key_id.contains(['\0', '\r', '\n'])
    {
        return Err(ManifestError::InvalidKeyId);
    }

    let payload_bytes = URL_SAFE_NO_PAD
        .decode(&envelope.payload)
        .map_err(|_| ManifestError::InvalidPayloadEncoding)?;
    validate_document_size(&payload_bytes)?;
    let public_key = decode_array::<32>(public_key, ManifestError::InvalidPublicKey)?;
    let signature = decode_array::<64>(&envelope.signature, ManifestError::InvalidSignature)?;
    let verifying_key =
        VerifyingKey::from_bytes(&public_key).map_err(|_| ManifestError::InvalidPublicKey)?;
    let signature = Signature::from_bytes(&signature);
    verifying_key
        .verify_strict(&payload_bytes, &signature)
        .map_err(|_| ManifestError::SignatureVerificationFailed)?;

    let payload: ManifestPayload = serde_json::from_slice(&payload_bytes)?;
    validate_payload(&payload, validation_time, minimum_sequence, true)?;
    Ok(VerifiedManifest {
        payload,
        signed: true,
    })
}

fn validate_document_size(document: &[u8]) -> Result<(), ManifestError> {
    if document.is_empty() || document.len() > MAX_MANIFEST_BYTES {
        Err(ManifestError::InvalidDocumentSize)
    } else {
        Ok(())
    }
}

fn validate_payload(
    payload: &ManifestPayload,
    now: DateTime<Utc>,
    minimum_sequence: Option<u64>,
    require_expiry: bool,
) -> Result<(), ManifestError> {
    if payload.schema_version != MANAGEMENT_SCHEMA_VERSION {
        return Err(ManifestError::UnsupportedPayloadSchema(
            payload.schema_version,
        ));
    }
    if payload.sequence == 0 {
        return Err(ManifestError::InvalidSequence);
    }
    if minimum_sequence.is_some_and(|minimum| payload.sequence < minimum) {
        return Err(ManifestError::RollbackAttempt);
    }
    if payload.issued_at > now + Duration::minutes(MAX_CLOCK_SKEW_MINUTES) {
        return Err(ManifestError::IssuedInFuture);
    }
    let Some(expires_at) = payload.expires_at else {
        if require_expiry {
            return Err(ManifestError::MissingExpiry);
        }
        return Ok(());
    };
    if expires_at <= payload.issued_at
        || expires_at - payload.issued_at > Duration::days(MAX_SIGNED_MANIFEST_LIFETIME_DAYS)
    {
        return Err(ManifestError::InvalidLifetime);
    }
    if expires_at < now {
        return Err(ManifestError::Expired);
    }
    Ok(())
}

fn decode_array<const N: usize>(
    value: &str,
    error: ManifestError,
) -> Result<[u8; N], ManifestError> {
    let decoded = URL_SAFE_NO_PAD.decode(value).map_err(|_| error.clone())?;
    decoded.try_into().map_err(|_| error)
}

fn decode_raw_array<const N: usize>(value: &str) -> Result<[u8; N], ()> {
    URL_SAFE_NO_PAD
        .decode(value)
        .map_err(|_| ())?
        .try_into()
        .map_err(|_| ())
}

fn valid_text(value: &str, limit: usize) -> bool {
    !value.is_empty() && value.len() <= limit && !value.contains(['\0', '\r', '\n'])
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn signed_transport_allowed(url: &Url) -> bool {
    url.scheme() == "https"
        || url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| address.is_loopback())
        })
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum EnrollmentError {
    #[error("bootstrap URL is malformed or does not use a secure transport")]
    InvalidBootstrapUrl,
    #[error("bootstrap URL is missing its token, pinned key, or key ID")]
    MissingBootstrapFields,
    #[error("enrollment request metadata is malformed")]
    InvalidRequestMetadata,
    #[error("enrollment document is empty or exceeds the size limit")]
    InvalidDocumentSize,
    #[error("signed enrollment envelope is malformed")]
    InvalidEnvelope,
    #[error("enrollment signature verification failed")]
    SignatureVerificationFailed,
    #[error("enrollment payload is malformed, stale, or does not match this request")]
    InvalidPayload,
    #[error("enrollment secret encryption metadata is malformed")]
    InvalidEncryption,
    #[error("enrollment secrets could not be decrypted")]
    DecryptionFailed,
    #[error("decrypted enrollment secrets are malformed")]
    InvalidSecrets,
    #[error("enrollment JSON is invalid: {0}")]
    InvalidJson(String),
    #[error("operating-system random number generation failed")]
    Random,
    #[error(transparent)]
    Manifest(#[from] ManifestError),
}

impl From<serde_json::Error> for EnrollmentError {
    fn from(error: serde_json::Error) -> Self {
        Self::InvalidJson(error.to_string())
    }
}

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ManifestError {
    #[error("manifest document is empty or exceeds the size limit")]
    InvalidDocumentSize,
    #[error("manifest JSON is invalid: {0}")]
    InvalidJson(String),
    #[error("unsupported signed-envelope schema {0}")]
    UnsupportedEnvelopeSchema(u32),
    #[error("unsupported manifest payload schema {0}")]
    UnsupportedPayloadSchema(u32),
    #[error("manifest signature algorithm must be ed25519")]
    UnsupportedAlgorithm,
    #[error("manifest key ID is malformed")]
    InvalidKeyId,
    #[error("manifest payload is not valid base64url")]
    InvalidPayloadEncoding,
    #[error("pinned Ed25519 public key is malformed")]
    InvalidPublicKey,
    #[error("manifest signature is malformed")]
    InvalidSignature,
    #[error("manifest signature verification failed")]
    SignatureVerificationFailed,
    #[error("manifest sequence is older than the last accepted sequence")]
    RollbackAttempt,
    #[error("manifest sequence must be greater than zero")]
    InvalidSequence,
    #[error("manifest issue time is too far in the future")]
    IssuedInFuture,
    #[error("signed manifests require an expiry time")]
    MissingExpiry,
    #[error("manifest validity lifetime is invalid or exceeds 30 days")]
    InvalidLifetime,
    #[error("manifest has expired")]
    Expired,
}

impl From<serde_json::Error> for ManifestError {
    fn from(error: serde_json::Error) -> Self {
        Self::InvalidJson(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chacha20poly1305::aead::Aead;
    use ed25519_dalek::{Signer, SigningKey};
    use x25519_dalek::EphemeralSecret;

    fn policy(sequence: u64, now: DateTime<Utc>) -> ManifestPayload {
        ManifestPayload {
            schema_version: MANAGEMENT_SCHEMA_VERSION,
            sequence,
            issued_at: now,
            expires_at: Some(now + Duration::days(7)),
            policy: ManagedPolicy {
                revision: format!("policy-{sequence}"),
                ..ManagedPolicy::default()
            },
        }
    }

    fn signed_document(payload: &ManifestPayload, signing_key: &SigningKey) -> Vec<u8> {
        let payload = serde_json::to_vec(payload).expect("payload serializes");
        let envelope = SignedManifestEnvelope {
            schema_version: MANAGEMENT_SCHEMA_VERSION,
            algorithm: "ed25519".to_owned(),
            key_id: "test-key".to_owned(),
            payload: URL_SAFE_NO_PAD.encode(&payload),
            signature: URL_SAFE_NO_PAD.encode(signing_key.sign(&payload).to_bytes()),
        };
        serde_json::to_vec(&envelope).expect("envelope serializes")
    }

    #[test]
    fn signed_manifest_round_trips_with_strict_verification() {
        let now = Utc::now();
        let signing_key = SigningKey::from_bytes(&[7_u8; 32]);
        let public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());
        let document = signed_document(&policy(9, now), &signing_key);

        let verified = verify_signed_manifest(&document, &public_key, now, Some(8))
            .expect("signature should verify");

        assert!(verified.signed);
        assert_eq!(verified.payload.sequence, 9);
        assert_eq!(verified.payload.policy.revision, "policy-9");
    }

    #[test]
    fn signed_manifest_rejects_tampering_expiry_and_rollback() {
        let now = Utc::now();
        let signing_key = SigningKey::from_bytes(&[8_u8; 32]);
        let public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());
        let mut document = signed_document(&policy(4, now), &signing_key);
        let last = document.len() - 2;
        document[last] ^= 1;
        assert!(verify_signed_manifest(&document, &public_key, now, None).is_err());

        let expired = signed_document(&policy(4, now - Duration::days(8)), &signing_key);
        assert_eq!(
            verify_signed_manifest(&expired, &public_key, now, None),
            Err(ManifestError::Expired)
        );

        let old = signed_document(&policy(4, now), &signing_key);
        assert_eq!(
            verify_signed_manifest(&old, &public_key, now, Some(5)),
            Err(ManifestError::RollbackAttempt)
        );
    }

    #[test]
    fn plain_manifest_can_be_static_but_still_rejects_rollback() {
        let now = Utc::now();
        let mut payload = policy(3, now);
        payload.expires_at = None;
        let document = serde_json::to_vec(&payload).expect("payload serializes");

        assert!(parse_plain_manifest(&document, now, Some(3)).is_ok());
        assert_eq!(
            parse_plain_manifest(&document, now, Some(4)),
            Err(ManifestError::RollbackAttempt)
        );

        payload.sequence = 0;
        let document = serde_json::to_vec(&payload).expect("payload serializes");
        assert_eq!(
            parse_plain_manifest(&document, now, None),
            Err(ManifestError::InvalidSequence)
        );
    }

    #[test]
    fn signed_enrollment_verifies_nonce_and_decrypts_device_secrets() {
        let now = Utc::now();
        let signing_key = SigningKey::from_bytes(&[31_u8; 32]);
        let public_key = URL_SAFE_NO_PAD.encode(signing_key.verifying_key().to_bytes());
        let descriptor = BootstrapDescriptor::parse(&format!(
            "http://127.0.0.1:8080/v1/enroll/test#token=one-time&key={public_key}&key_id=server-primary"
        ))
        .expect("bootstrap descriptor");
        let request_context = new_enrollment_request(
            "TEST-PC".to_owned(),
            "0.1.0".to_owned(),
            "x86_64".to_owned(),
        )
        .expect("enrollment request");
        let request = request_context.request;
        let private_key = request_context.private_key;
        let request_nonce = request_context.request_nonce;
        let device_public_bytes: [u8; 32] = URL_SAFE_NO_PAD
            .decode(&request.device_public_key)
            .expect("public key encoding")
            .try_into()
            .expect("public key length");
        let device_public = PublicKey::from(device_public_bytes);
        let ephemeral_secret = EphemeralSecret::random();
        let ephemeral_public = PublicKey::from(&ephemeral_secret);
        let shared = ephemeral_secret.diffie_hellman(&device_public);
        let salt = [41_u8; 32];
        let nonce = [43_u8; 12];
        let mut key = [0_u8; 32];
        Hkdf::<Sha256>::new(Some(&salt), shared.as_bytes())
            .expand(b"resticpal enrollment secret bundle v1", &mut key)
            .expect("derive key");
        let secret_bundle = serde_json::to_vec(&EnrollmentSecretBundle {
            schema_version: MANAGEMENT_SCHEMA_VERSION,
            status_token: "device-token".to_owned(),
            repository_secrets: BTreeMap::from([(
                SecretEnvironmentVariable::ResticPassword,
                "repository-password".to_owned(),
            )]),
        })
        .expect("secret bundle");
        let ciphertext = ChaCha20Poly1305::new((&key).into())
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: &secret_bundle,
                    aad: b"test-device",
                },
            )
            .expect("encrypt bundle");
        let mut initial_manifest: SignedManifestEnvelope =
            serde_json::from_slice(&signed_document(&policy(5, now), &signing_key))
                .expect("manifest envelope");
        initial_manifest.key_id = "server-primary".to_owned();
        let enrollment_payload = EnrollmentPayload {
            schema_version: MANAGEMENT_SCHEMA_VERSION,
            device_id: "test-device".to_owned(),
            request_nonce: request.request_nonce,
            issued_at: now,
            expires_at: now + Duration::minutes(10),
            manifest_url: "http://127.0.0.1:8080/v1/manifest/test-device".to_owned(),
            status_url: "http://127.0.0.1:8080/v1/status".to_owned(),
            refresh_interval_minutes: 15,
            initial_manifest,
            secrets: SealedSecretBundle {
                algorithm: "x25519-hkdf-sha256-chacha20poly1305".to_owned(),
                ephemeral_public_key: URL_SAFE_NO_PAD.encode(ephemeral_public.as_bytes()),
                salt: URL_SAFE_NO_PAD.encode(salt),
                nonce: URL_SAFE_NO_PAD.encode(nonce),
                ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
            },
        };
        let payload = serde_json::to_vec(&enrollment_payload).expect("enrollment payload");
        let document = serde_json::to_vec(&SignedEnrollmentEnvelope {
            schema_version: MANAGEMENT_SCHEMA_VERSION,
            algorithm: "ed25519".to_owned(),
            key_id: "server-primary".to_owned(),
            payload: URL_SAFE_NO_PAD.encode(&payload),
            signature: URL_SAFE_NO_PAD.encode(signing_key.sign(&payload).to_bytes()),
        })
        .expect("enrollment response");

        let material = verify_and_decrypt_enrollment(
            &document,
            &descriptor,
            &private_key,
            &request_nonce,
            now,
        )
        .expect("verified enrollment");
        assert_eq!(material.device_id, "test-device");
        assert_eq!(material.status_token.as_str(), "device-token");
        assert_eq!(
            material.repository_secrets[&SecretEnvironmentVariable::ResticPassword].as_str(),
            "repository-password"
        );
        assert_eq!(material.initial_manifest.payload.sequence, 5);
    }
}
