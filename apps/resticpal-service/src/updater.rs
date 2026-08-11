use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use resticpal_protocol::UpdatePackage;
use thiserror::Error;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};

const UPDATE_PUBLIC_KEY: &str = include_str!("../../../config/update-public-key.txt");
const MAX_UPDATE_BYTES: u64 = 512 * 1024 * 1024;
const UPDATE_HTTP_TIMEOUT: Duration = Duration::from_secs(20 * 60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateInstallOutcome {
    Completed { version: String },
    Failed { code: &'static str },
}

pub fn install(package: &UpdatePackage, data_root: &Path) -> UpdateInstallOutcome {
    match download_verify_and_launch(package, data_root) {
        Ok(()) => UpdateInstallOutcome::Completed {
            version: package.version.clone(),
        },
        Err(error) => {
            eprintln!("automatic update {} failed: {error}", package.version);
            UpdateInstallOutcome::Failed { code: error.code() }
        }
    }
}

pub fn validate_package(package: &UpdatePackage) -> Result<(), UpdateError> {
    let version = parse_version(&package.version).ok_or(UpdateError::InvalidVersion)?;
    let current = parse_version(env!("CARGO_PKG_VERSION")).expect("package version is numeric");
    if version <= current {
        return Err(UpdateError::NotNewer);
    }
    if package.length == 0 || package.length > MAX_UPDATE_BYTES {
        return Err(UpdateError::InvalidLength);
    }
    let expected_github_url = format!(
        "https://github.com/theatrus/resticpal/releases/download/v{0}/resticpal-{0}-x64.msi",
        package.version
    );
    let expected_primary_url = format!(
        "https://updates.resticpal.com/releases/v{0}/resticpal-{0}-x64.msi",
        package.version
    );
    if package.url != expected_github_url && package.url != expected_primary_url {
        return Err(UpdateError::InvalidUrl);
    }
    decode_signature(&package.signature)?;
    Ok(())
}

fn download_verify_and_launch(
    package: &UpdatePackage,
    data_root: &Path,
) -> Result<(), UpdateError> {
    validate_package(package)?;
    let update_root = data_root.join("Updates");
    fs::create_dir_all(&update_root)?;
    let file_name = format!("resticpal-{}-x64.msi", package.version);
    let installer_path = update_root.join(&file_name);
    let partial_path = update_root.join(format!("{file_name}.partial"));
    remove_if_present(&partial_path)?;

    let download_result = download_to(package, &partial_path);
    if let Err(error) = download_result {
        let _ = fs::remove_file(&partial_path);
        return Err(error);
    }
    if let Err(error) = verify_file(&partial_path, &package.signature) {
        let _ = fs::remove_file(&partial_path);
        return Err(error);
    }

    remove_if_present(&installer_path)?;
    fs::rename(&partial_path, &installer_path)?;
    launch_msi(&installer_path, &update_root.join("install.log"))
}

fn download_to(package: &UpdatePackage, destination: &Path) -> Result<(), UpdateError> {
    let config = ureq::Agent::config_builder()
        .timeout_global(Some(UPDATE_HTTP_TIMEOUT))
        .tls_config(
            TlsConfig::builder()
                .provider(TlsProvider::NativeTls)
                .root_certs(RootCerts::PlatformVerifier)
                .build(),
        )
        .user_agent(concat!("resticpal/", env!("CARGO_PKG_VERSION")))
        .build();
    let agent: ureq::Agent = config.into();
    let mut response = agent.get(&package.url).call()?;
    let mut reader = response
        .body_mut()
        .with_config()
        .limit(package.length.saturating_add(1))
        .reader();
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)?;
    let copied = io::copy(&mut reader, &mut file)?;
    file.flush()?;
    file.sync_all()?;
    if copied != package.length {
        return Err(UpdateError::LengthMismatch {
            expected: package.length,
            actual: copied,
        });
    }
    Ok(())
}

fn verify_file(path: &Path, encoded_signature: &str) -> Result<(), UpdateError> {
    verify_file_with_public_key(path, encoded_signature, UPDATE_PUBLIC_KEY)
}

fn verify_file_with_public_key(
    path: &Path,
    encoded_signature: &str,
    encoded_public_key: &str,
) -> Result<(), UpdateError> {
    let key = STANDARD
        .decode(encoded_public_key.trim())
        .map_err(|_| UpdateError::InvalidPublicKey)?;
    let key: [u8; 32] = key.try_into().map_err(|_| UpdateError::InvalidPublicKey)?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| UpdateError::InvalidPublicKey)?;
    let signature = decode_signature(encoded_signature)?;
    let contents = fs::read(path)?;
    key.verify_strict(&contents, &signature)
        .map_err(|_| UpdateError::InvalidSignature)
}

fn decode_signature(value: &str) -> Result<Signature, UpdateError> {
    if value.len() > 256 || value.contains(['\0', '\r', '\n']) {
        return Err(UpdateError::InvalidSignature);
    }
    let signature = STANDARD
        .decode(value)
        .map_err(|_| UpdateError::InvalidSignature)?;
    let signature: [u8; 64] = signature
        .try_into()
        .map_err(|_| UpdateError::InvalidSignature)?;
    Ok(Signature::from_bytes(&signature))
}

fn launch_msi(installer_path: &Path, log_path: &Path) -> Result<(), UpdateError> {
    let system_root = env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let msiexec = PathBuf::from(system_root)
        .join("System32")
        .join("msiexec.exe");
    let status = Command::new(msiexec)
        .arg("/i")
        .arg(installer_path)
        .arg("/qn")
        .arg("/norestart")
        .arg("/l*v")
        .arg(log_path)
        .status()
        .map_err(UpdateError::InstallerStart)?;
    if status.success() || status.code() == Some(3010) {
        Ok(())
    } else {
        Err(UpdateError::InstallerExit(status.code()))
    }
}

fn remove_if_present(path: &Path) -> Result<(), io::Error> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn parse_version(value: &str) -> Option<[u64; 3]> {
    let mut components = value.split('.');
    let parsed = [
        parse_component(components.next()?)?,
        parse_component(components.next()?)?,
        parse_component(components.next()?)?,
    ];
    components.next().is_none().then_some(parsed)
}

fn parse_component(value: &str) -> Option<u64> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
}

#[derive(Debug, Error)]
pub enum UpdateError {
    #[error("the update version is invalid")]
    InvalidVersion,
    #[error("the update is not newer than this service")]
    NotNewer,
    #[error("the update URL is not an approved resticpal release asset")]
    InvalidUrl,
    #[error("the update size is invalid")]
    InvalidLength,
    #[error("the embedded update public key is invalid")]
    InvalidPublicKey,
    #[error("the update signature is invalid")]
    InvalidSignature,
    #[error("the update download length was {actual} bytes; expected {expected}")]
    LengthMismatch { expected: u64, actual: u64 },
    #[error("update file I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("update download failed: {0}")]
    Http(#[from] ureq::Error),
    #[error("the Windows Installer could not be started: {0}")]
    InstallerStart(io::Error),
    #[error("the Windows Installer exited with code {0:?}")]
    InstallerExit(Option<i32>),
}

impl UpdateError {
    const fn code(&self) -> &'static str {
        match self {
            Self::InvalidVersion | Self::NotNewer | Self::InvalidUrl | Self::InvalidLength => {
                "update_metadata_invalid"
            }
            Self::InvalidPublicKey | Self::InvalidSignature => "update_signature_invalid",
            Self::LengthMismatch { .. } | Self::Io(_) | Self::Http(_) => "update_download_failed",
            Self::InstallerStart(_) | Self::InstallerExit(_) => "update_installer_failed",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use tempfile::tempdir;

    fn future_version() -> String {
        let [major, _, _] = parse_version(env!("CARGO_PKG_VERSION")).unwrap();
        format!("{}.0.0", major + 1)
    }

    fn package(version: &str) -> UpdatePackage {
        UpdatePackage {
            version: version.to_owned(),
            url: format!(
                "https://github.com/theatrus/resticpal/releases/download/v{version}/resticpal-{version}-x64.msi"
            ),
            signature: STANDARD.encode([0_u8; 64]),
            length: 1024,
        }
    }

    #[test]
    fn package_metadata_is_bounded_and_pinned_to_the_release_asset() {
        let version = future_version();
        assert!(validate_package(&package(&version)).is_ok());

        let mut redirected = package(&version);
        redirected.url = "https://example.test/resticpal.msi".to_owned();
        assert!(matches!(
            validate_package(&redirected),
            Err(UpdateError::InvalidUrl)
        ));

        let mut primary = package(&version);
        primary.url = format!(
            "https://updates.resticpal.com/releases/v{version}/resticpal-{version}-x64.msi"
        );
        assert!(validate_package(&primary).is_ok());

        let mut oversized = package(&version);
        oversized.length = MAX_UPDATE_BYTES + 1;
        assert!(matches!(
            validate_package(&oversized),
            Err(UpdateError::InvalidLength)
        ));
    }

    #[test]
    fn downloaded_installer_requires_the_pinned_signature() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("update.msi");
        let contents = b"signed Windows Installer fixture";
        fs::write(&path, contents).unwrap();

        let private = SigningKey::from_bytes(&[7_u8; 32]);
        let signature = STANDARD.encode(private.sign(contents).to_bytes());
        let public = STANDARD.encode(private.verifying_key().to_bytes());
        assert!(verify_file_with_public_key(&path, &signature, &public).is_ok());

        fs::write(&path, b"tampered Windows Installer fixture").unwrap();
        assert!(matches!(
            verify_file_with_public_key(&path, &signature, &public),
            Err(UpdateError::InvalidSignature)
        ));
    }
}
