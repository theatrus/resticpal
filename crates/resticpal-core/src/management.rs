use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::policy::ManagedPolicy;
use crate::status::ServiceStatus;

pub const MANAGEMENT_SCHEMA_VERSION: u32 = 1;
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;
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
    use ed25519_dalek::{Signer, SigningKey};

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
}
