use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus};
use std::ptr;
use std::slice;
use std::thread;
use std::time::{Duration, Instant};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::{Signature, VerifyingKey};
use resticpal_protocol::UpdatePackage;
use thiserror::Error;
use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, GetSecurityInfo, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetSecurityInfo,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GROUP_SECURITY_INFORMATION, GetSecurityDescriptorControl,
    GetSecurityDescriptorDacl, GetSecurityDescriptorGroup, GetSecurityDescriptorOwner,
    OWNER_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID,
    SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, CreateDirectoryW, CreateFileW, FILE_ATTRIBUTE_DIRECTORY,
    FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_READ_ATTRIBUTES, FILE_SHARE_READ, GetFileInformationByHandle, OPEN_EXISTING, READ_CONTROL,
    WRITE_DAC, WRITE_OWNER,
};
use windows::core::{BOOL, HRESULT, PCWSTR, w};

const UPDATE_PUBLIC_KEY: &str = include_str!("../../../config/update-public-key.txt");
const MAX_UPDATE_BYTES: u64 = 512 * 1024 * 1024;
const UPDATE_HTTP_TIMEOUT: Duration = Duration::from_secs(20 * 60);
pub const UPDATE_INSTALL_TIMEOUT_SECONDS: u32 = 30 * 60;
const UPDATE_INSTALL_TIMEOUT: Duration = Duration::from_secs(UPDATE_INSTALL_TIMEOUT_SECONDS as u64);
const INSTALLER_POLL_INTERVAL: Duration = Duration::from_millis(250);
const SECURE_DIRECTORY_SDDL: PCWSTR = w!("O:SYG:SYD:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)");
const SECURE_FILE_SDDL: PCWSTR = w!("O:SYG:SYD:P(A;;FA;;;SY)(A;;FA;;;BA)");

pub(crate) struct ServiceDataRootGuard {
    _directories: Vec<SecureDirectory>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateInstallOutcome {
    Completed { version: String },
    Failed { code: &'static str },
    Indeterminate { code: &'static str },
}

/// Secures the service's machine-wide data root before configuration, state,
/// credentials, or IPC are initialized. Keeping the returned handle alive
/// prevents the directory itself from being renamed or replaced while the
/// service is running.
pub(crate) fn prepare_service_data_root(
    data_root: &Path,
) -> Result<ServiceDataRootGuard, UpdateError> {
    let directory_security = SecureDirectorySecurity::system_and_administrators()?;
    let file_security = SecureDirectorySecurity::system_and_administrators_file()?;
    let mut directories = Vec::with_capacity(5);
    directories.push(prepare_secure_directory(data_root, &directory_security)?);
    for name in ["Cache", "Credentials", "Logs", "Updates"] {
        directories.push(prepare_secure_directory(
            &data_root.join(name),
            &directory_security,
        )?);
    }
    for path in [
        data_root.join("config.toml"),
        data_root.join("state.json"),
        data_root.join("state.db"),
        data_root.join("state.db-wal"),
        data_root.join("state.db-shm"),
        data_root.join("state.db-journal"),
        data_root.join("managed-policy-cache.json"),
        data_root.join("service-startup-errors.log"),
        data_root.join("Logs").join("service.jsonl"),
    ] {
        prepare_existing_secure_file(&path, &file_security)?;
    }
    Ok(ServiceDataRootGuard {
        _directories: directories,
    })
}

pub fn install(package: &UpdatePackage, data_root: &Path) -> UpdateInstallOutcome {
    match download_verify_and_launch(package, data_root, Instant::now() + UPDATE_INSTALL_TIMEOUT) {
        Ok(()) => UpdateInstallOutcome::Completed {
            version: package.version.clone(),
        },
        Err(error) if error.installer_state_is_indeterminate() => {
            eprintln!(
                "automatic update {} has an indeterminate installer state: {error}",
                package.version
            );
            UpdateInstallOutcome::Indeterminate { code: error.code() }
        }
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
    deadline: Instant,
) -> Result<(), UpdateError> {
    validate_package(package)?;
    let security = SecureDirectorySecurity::system_and_administrators()?;
    let data_root_guard = prepare_secure_directory(data_root, &security)?;
    let update_root = data_root.join("Updates");
    let update_root_guard = prepare_secure_directory(&update_root, &security)?;
    let file_name = format!("resticpal-{}-x64.msi", package.version);
    let installer_path = update_root.join(&file_name);
    let partial_path = update_root.join(format!("{file_name}.partial"));
    let log_path = update_root.join("install.log");
    remove_staging_entry_if_present(&partial_path)?;
    remove_staging_entry_if_present(&installer_path)?;

    let download_result = download_to(package, &partial_path);
    if let Err(error) = download_result {
        let _ = remove_staging_entry_if_present(&partial_path);
        return Err(error);
    }
    fs::rename(&partial_path, &installer_path)?;
    let verified = match verify_installer_before_rotating_log(
        &installer_path,
        package.length,
        &package.signature,
        UPDATE_PUBLIC_KEY,
        &log_path,
    ) {
        Ok(verified) => verified,
        Err(error) => {
            let _ = remove_staging_entry_if_present(&installer_path);
            return Err(error);
        }
    };

    // Keep both directory handles and the read-only, non-delete-shared MSI
    // handle alive through Windows Installer. The final bytes were verified
    // only after the rename into the protected Updates directory.
    let _directory_guards = (data_root_guard, update_root_guard);
    launch_msi(&verified, &log_path, deadline)
}

fn verify_installer_before_rotating_log(
    installer_path: &Path,
    expected_length: u64,
    encoded_signature: &str,
    encoded_public_key: &str,
    log_path: &Path,
) -> Result<VerifiedInstaller, UpdateError> {
    let verified = VerifiedInstaller::open(
        installer_path,
        expected_length,
        encoded_signature,
        encoded_public_key,
    )?;
    remove_staging_entry_if_present(log_path)?;
    Ok(verified)
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

struct SecureDirectory {
    _handle: OwnedHandle,
}

struct SecureDirectorySecurity {
    descriptor: PSECURITY_DESCRIPTOR,
    dacl: *mut ACL,
    owner: PSID,
    group: PSID,
    enforce_trusted_owner: bool,
}

impl SecureDirectorySecurity {
    fn system_and_administrators() -> Result<Self, UpdateError> {
        Self::from_sddl(SECURE_DIRECTORY_SDDL, true)
    }

    fn system_and_administrators_file() -> Result<Self, UpdateError> {
        Self::from_sddl(SECURE_FILE_SDDL, true)
    }

    fn from_sddl(sddl: PCWSTR, enforce_trusted_owner: bool) -> Result<Self, UpdateError> {
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: SDDL is a live, null-terminated string and Windows allocates
        // the returned self-relative descriptor with LocalAlloc.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl,
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
        }
        .map_err(staging_windows_error)?;
        let mut security = Self {
            descriptor,
            dacl: ptr::null_mut(),
            owner: PSID::default(),
            group: PSID::default(),
            enforce_trusted_owner,
        };

        let mut dacl_present = BOOL::default();
        let mut dacl_defaulted = BOOL::default();
        // SAFETY: the descriptor is live and all output pointers are writable.
        unsafe {
            GetSecurityDescriptorDacl(
                security.descriptor,
                &raw mut dacl_present,
                &raw mut security.dacl,
                &raw mut dacl_defaulted,
            )
        }
        .map_err(staging_windows_error)?;
        if !dacl_present.as_bool() || security.dacl.is_null() {
            return Err(UpdateError::UnsafeStagingDirectory);
        }

        let mut owner_defaulted = BOOL::default();
        // SAFETY: the descriptor is live and both outputs are writable.
        unsafe {
            GetSecurityDescriptorOwner(
                security.descriptor,
                &raw mut security.owner,
                &raw mut owner_defaulted,
            )
        }
        .map_err(staging_windows_error)?;
        let mut group_defaulted = BOOL::default();
        // SAFETY: the descriptor is live and both outputs are writable.
        unsafe {
            GetSecurityDescriptorGroup(
                security.descriptor,
                &raw mut security.group,
                &raw mut group_defaulted,
            )
        }
        .map_err(staging_windows_error)?;
        if enforce_trusted_owner && (security.owner.is_invalid() || security.group.is_invalid()) {
            return Err(UpdateError::UnsafeStagingDirectory);
        }
        Ok(security)
    }

    fn attributes(&self) -> SECURITY_ATTRIBUTES {
        SECURITY_ATTRIBUTES {
            nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())
                .expect("SECURITY_ATTRIBUTES size fits in u32"),
            lpSecurityDescriptor: self.descriptor.0,
            bInheritHandle: false.into(),
        }
    }

    fn apply(&self, handle: HANDLE) -> Result<(), UpdateError> {
        let mut information = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
        let (owner, group) = if self.enforce_trusted_owner {
            information |= OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION;
            (Some(self.owner), Some(self.group))
        } else {
            (None, None)
        };
        // SAFETY: the directory handle has WRITE_DAC/WRITE_OWNER access and all
        // pointers remain owned by this descriptor through the call.
        unsafe {
            SetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                information,
                owner,
                group,
                Some(self.dacl.cast_const()),
                None,
            )
        }
        .ok()
        .map_err(staging_windows_error)
    }

    fn verify(&self, handle: HANDLE) -> Result<(), UpdateError> {
        let mut owner = PSID::default();
        let mut group = PSID::default();
        let mut dacl = ptr::null_mut();
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        let mut information = DACL_SECURITY_INFORMATION;
        if self.enforce_trusted_owner {
            information |= OWNER_SECURITY_INFORMATION | GROUP_SECURITY_INFORMATION;
        }
        // SAFETY: the handle has READ_CONTROL access, outputs are writable, and
        // Windows returns one LocalAlloc-owned descriptor containing the SIDs
        // and ACL pointers.
        unsafe {
            GetSecurityInfo(
                handle,
                SE_FILE_OBJECT,
                information,
                Some(&raw mut owner),
                Some(&raw mut group),
                Some(&raw mut dacl),
                None,
                Some(&raw mut descriptor),
            )
        }
        .ok()
        .map_err(staging_windows_error)?;
        let _descriptor = LocalSecurityDescriptor(descriptor);

        let mut control = 0_u16;
        let mut revision = 0_u32;
        // SAFETY: descriptor is live and both outputs are writable.
        unsafe { GetSecurityDescriptorControl(descriptor, &raw mut control, &raw mut revision) }
            .map_err(staging_windows_error)?;
        if control & SE_DACL_PROTECTED.0 == 0 {
            return Err(UpdateError::UnsafeStagingDirectory);
        }
        if dacl.is_null() || !same_acl(dacl, self.dacl) {
            return Err(UpdateError::UnsafeStagingDirectory);
        }
        if self.enforce_trusted_owner
            && (!same_sid(owner, self.owner) || !same_sid(group, self.group))
        {
            return Err(UpdateError::UnsafeStagingDirectory);
        }
        Ok(())
    }
}

impl Drop for SecureDirectorySecurity {
    fn drop(&mut self) {
        if !self.descriptor.0.is_null() {
            // SAFETY: conversion allocated the descriptor with LocalAlloc.
            let _ = unsafe { LocalFree(Some(HLOCAL(self.descriptor.0))) };
        }
    }
}

fn prepare_secure_directory(
    path: &Path,
    security: &SecureDirectorySecurity,
) -> Result<SecureDirectory, UpdateError> {
    let path_wide = wide_null(path.as_os_str());
    let attributes = security.attributes();
    // New directories receive the protected owner and DACL atomically. Existing
    // paths are opened without following reparse points and hardened below.
    let created = match unsafe {
        CreateDirectoryW(PCWSTR(path_wide.as_ptr()), Some(&raw const attributes))
    } {
        Ok(()) => true,
        Err(error) if error.code() == HRESULT::from_win32(ERROR_ALREADY_EXISTS.0) => false,
        Err(error) => return Err(staging_windows_error(error)),
    };

    let handle = open_directory_for_protection(&path_wide)?;
    validate_directory_handle(handle.0)?;

    let owner_was_trusted = created
        || !security.enforce_trusted_owner
        || directory_owner_is_system_or_administrator(handle.0)?;
    if !owner_was_trusted {
        // Changing an attacker-owned directory's owner and DACL cannot revoke
        // handles the attacker opened beforehand. Never adopt it in place,
        // even when it appears empty.
        return Err(UpdateError::UntrustedStagingDirectory);
    }

    security.apply(handle.0)?;
    validate_directory_handle(handle.0)?;
    security.verify(handle.0)?;

    Ok(SecureDirectory { _handle: handle })
}

fn prepare_existing_secure_file(
    path: &Path,
    security: &SecureDirectorySecurity,
) -> Result<(), UpdateError> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(staging_io_error(error)),
    };
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        return Err(UpdateError::UnsafeServiceDataEntry);
    }

    let path_wide = wide_null(path.as_os_str());
    let handle = open_file_for_protection(&path_wide)?;
    validate_file_handle(handle.0)?;
    if security.enforce_trusted_owner && !directory_owner_is_system_or_administrator(handle.0)? {
        return Err(UpdateError::UntrustedServiceDataEntry);
    }
    security.apply(handle.0)?;
    validate_file_handle(handle.0)?;
    security.verify(handle.0)
}

fn open_directory_for_protection(path_wide: &[u16]) -> Result<OwnedHandle, UpdateError> {
    let desired_access = FILE_READ_ATTRIBUTES.0 | READ_CONTROL.0 | WRITE_DAC.0 | WRITE_OWNER.0;
    // Read-only sharing rejects preexisting write/delete directory handles and
    // prevents new ones while the guard is live. This matters even for a
    // trusted-owned legacy directory whose old inherited ACL allowed Users to
    // create children. OPEN_REPARSE_POINT prevents traversal.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path_wide.as_ptr()),
            desired_access,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(staging_windows_error)?;
    Ok(OwnedHandle(handle))
}

fn open_file_for_protection(path_wide: &[u16]) -> Result<OwnedHandle, UpdateError> {
    let desired_access = FILE_READ_ATTRIBUTES.0 | READ_CONTROL.0 | WRITE_DAC.0 | WRITE_OWNER.0;
    // Read sharing is sufficient for startup inspection and prevents mutation,
    // unlinking, or replacement until owner/DACL verification completes.
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path_wide.as_ptr()),
            desired_access,
            FILE_SHARE_READ,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OPEN_REPARSE_POINT,
            None,
        )
    }
    .map_err(staging_windows_error)?;
    Ok(OwnedHandle(handle))
}

fn validate_directory_handle(handle: HANDLE) -> Result<(), UpdateError> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is live and information is writable.
    unsafe { GetFileInformationByHandle(handle, &raw mut information) }
        .map_err(staging_windows_error)?;
    let attributes = information.dwFileAttributes;
    if attributes & FILE_ATTRIBUTE_DIRECTORY.0 == 0
        || attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        return Err(UpdateError::UnsafeStagingDirectory);
    }
    Ok(())
}

fn validate_file_handle(handle: HANDLE) -> Result<(), UpdateError> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: the handle is live and information is writable.
    unsafe { GetFileInformationByHandle(handle, &raw mut information) }
        .map_err(staging_windows_error)?;
    let attributes = information.dwFileAttributes;
    if attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0
        || attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    {
        return Err(UpdateError::UnsafeServiceDataEntry);
    }
    Ok(())
}

fn directory_owner_is_system_or_administrator(handle: HANDLE) -> Result<bool, UpdateError> {
    let mut owner = PSID::default();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: the handle has READ_CONTROL access and Windows owns the returned
    // descriptor until the LocalFree guard below.
    unsafe {
        GetSecurityInfo(
            handle,
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION,
            Some(&raw mut owner),
            None,
            None,
            None,
            Some(&raw mut descriptor),
        )
    }
    .ok()
    .map_err(staging_windows_error)?;
    let _descriptor = LocalSecurityDescriptor(descriptor);
    Ok(sid_matches_sddl_owner(owner, w!("O:SY"))? || sid_matches_sddl_owner(owner, w!("O:BA"))?)
}

fn sid_matches_sddl_owner(owner: PSID, sddl: PCWSTR) -> Result<bool, UpdateError> {
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: SDDL is live and Windows allocates the descriptor.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl,
            SDDL_REVISION_1,
            &raw mut descriptor,
            None,
        )
    }
    .map_err(staging_windows_error)?;
    let _descriptor = LocalSecurityDescriptor(descriptor);
    let mut expected = PSID::default();
    let mut defaulted = BOOL::default();
    // SAFETY: descriptor and outputs are live.
    unsafe { GetSecurityDescriptorOwner(descriptor, &raw mut expected, &raw mut defaulted) }
        .map_err(staging_windows_error)?;
    Ok(same_sid(owner, expected))
}

fn same_sid(left: PSID, right: PSID) -> bool {
    if left.is_invalid() || right.is_invalid() {
        return false;
    }
    // EqualSid reports false as an error in the generated BOOL wrapper.
    unsafe { windows::Win32::Security::EqualSid(left, right) }.is_ok()
}

fn same_acl(left: *const ACL, right: *const ACL) -> bool {
    if left.is_null() || right.is_null() {
        return false;
    }
    // SAFETY: both ACLs are part of live descriptors and AclSize is the byte
    // extent validated by Windows when those descriptors were created/read.
    let left_size = unsafe { (*left).AclSize as usize };
    let right_size = unsafe { (*right).AclSize as usize };
    left_size == right_size
        && unsafe {
            slice::from_raw_parts(left.cast::<u8>(), left_size)
                == slice::from_raw_parts(right.cast::<u8>(), right_size)
        }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        // SAFETY: this type exclusively owns the handle.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct LocalSecurityDescriptor(PSECURITY_DESCRIPTOR);

impl Drop for LocalSecurityDescriptor {
    fn drop(&mut self) {
        if !self.0.0.is_null() {
            // SAFETY: GetSecurityInfo/conversion allocated this with LocalAlloc.
            let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0))) };
        }
    }
}

fn staging_windows_error(error: windows::core::Error) -> UpdateError {
    UpdateError::StagingProtection(io::Error::other(error))
}

fn staging_io_error(error: io::Error) -> UpdateError {
    UpdateError::StagingProtection(error)
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

struct VerifiedInstaller {
    path: PathBuf,
    _lock: File,
}

impl VerifiedInstaller {
    fn open(
        path: &Path,
        expected_length: u64,
        encoded_signature: &str,
        encoded_public_key: &str,
    ) -> Result<Self, UpdateError> {
        let mut file = OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
            .open(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
            return Err(UpdateError::UnsafeStagingEntry);
        }
        verify_open_file_with_public_key(
            &mut file,
            expected_length,
            encoded_signature,
            encoded_public_key,
        )?;
        Ok(Self {
            path: path.to_owned(),
            _lock: file,
        })
    }
}

fn remove_staging_entry_if_present(path: &Path) -> Result<(), UpdateError> {
    let metadata = match path.symlink_metadata() {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let attributes = metadata.file_attributes();
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0 {
        if attributes & FILE_ATTRIBUTE_DIRECTORY.0 != 0 {
            fs::remove_dir(path)?;
        } else {
            fs::remove_file(path)?;
        }
        return Ok(());
    }
    if metadata.is_file() {
        fs::remove_file(path)?;
        Ok(())
    } else {
        Err(UpdateError::UnsafeStagingEntry)
    }
}

#[cfg(test)]
fn verify_file_with_public_key(
    path: &Path,
    encoded_signature: &str,
    encoded_public_key: &str,
) -> Result<(), UpdateError> {
    let contents = fs::read(path)?;
    verify_contents_with_public_key(&contents, encoded_signature, encoded_public_key)
}

fn verify_open_file_with_public_key(
    file: &mut File,
    expected_length: u64,
    encoded_signature: &str,
    encoded_public_key: &str,
) -> Result<(), UpdateError> {
    let actual_length = file.metadata()?.len();
    if actual_length != expected_length {
        return Err(UpdateError::LengthMismatch {
            expected: expected_length,
            actual: actual_length,
        });
    }
    file.seek(SeekFrom::Start(0))?;
    let mut contents = Vec::with_capacity(usize::try_from(expected_length).unwrap_or(0));
    Read::by_ref(file)
        .take(expected_length.saturating_add(1))
        .read_to_end(&mut contents)?;
    if contents.len() as u64 != expected_length {
        return Err(UpdateError::LengthMismatch {
            expected: expected_length,
            actual: contents.len() as u64,
        });
    }
    let result = verify_contents_with_public_key(&contents, encoded_signature, encoded_public_key);
    file.seek(SeekFrom::Start(0))?;
    result
}

fn verify_contents_with_public_key(
    contents: &[u8],
    encoded_signature: &str,
    encoded_public_key: &str,
) -> Result<(), UpdateError> {
    let key = STANDARD
        .decode(encoded_public_key.trim())
        .map_err(|_| UpdateError::InvalidPublicKey)?;
    let key: [u8; 32] = key.try_into().map_err(|_| UpdateError::InvalidPublicKey)?;
    let key = VerifyingKey::from_bytes(&key).map_err(|_| UpdateError::InvalidPublicKey)?;
    let signature = decode_signature(encoded_signature)?;
    key.verify_strict(contents, &signature)
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

fn launch_msi(
    installer: &VerifiedInstaller,
    log_path: &Path,
    deadline: Instant,
) -> Result<(), UpdateError> {
    if Instant::now() >= deadline {
        return Err(UpdateError::UpdateDeadlineExpired);
    }
    let system_root = env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into());
    let msiexec = PathBuf::from(system_root)
        .join("System32")
        .join("msiexec.exe");
    let mut child = Command::new(msiexec)
        .arg("/i")
        .arg(&installer.path)
        .arg("/qn")
        .arg("/norestart")
        .arg("/l*v")
        .arg(log_path)
        .spawn()
        .map_err(UpdateError::InstallerStart)?;
    let status = wait_for_installer(&mut child, deadline)?;
    if status.success() || status.code() == Some(3010) {
        Ok(())
    } else {
        Err(UpdateError::InstallerExit(status.code()))
    }
}

fn wait_for_installer(child: &mut Child, deadline: Instant) -> Result<ExitStatus, UpdateError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    INSTALLER_POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) => {
                if let Err(error) = child.kill() {
                    return match child.try_wait() {
                        Ok(Some(status)) => Ok(status),
                        _ => Err(UpdateError::InstallerTermination(error)),
                    };
                }
                child.wait().map_err(UpdateError::InstallerTermination)?;
                return Err(UpdateError::InstallerTimeout);
            }
            Err(error) => {
                if child.kill().is_err() || child.wait().is_err() {
                    return Err(UpdateError::InstallerTermination(error));
                }
                return Err(UpdateError::InstallerTermination(error));
            }
        }
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
    #[error("the update staging directory is unsafe")]
    UnsafeStagingDirectory,
    #[error("an update staging entry is unsafe")]
    UnsafeStagingEntry,
    #[error("an update directory has an untrusted owner")]
    UntrustedStagingDirectory,
    #[error("a protected service data file is unsafe")]
    UnsafeServiceDataEntry,
    #[error("a protected service data file has an untrusted owner")]
    UntrustedServiceDataEntry,
    #[error("update staging protection failed: {0}")]
    StagingProtection(io::Error),
    #[error("update file I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("update download failed: {0}")]
    Http(#[from] ureq::Error),
    #[error("the Windows Installer could not be started: {0}")]
    InstallerStart(io::Error),
    #[error("the Windows Installer exited with code {0:?}")]
    InstallerExit(Option<i32>),
    #[error("the Windows Installer exceeded the automatic update deadline")]
    InstallerTimeout,
    #[error("the automatic update deadline expired before Windows Installer started")]
    UpdateDeadlineExpired,
    #[error("the Windows Installer could not be confirmed stopped: {0}")]
    InstallerTermination(io::Error),
}

impl UpdateError {
    const fn code(&self) -> &'static str {
        match self {
            Self::InvalidVersion | Self::NotNewer | Self::InvalidUrl | Self::InvalidLength => {
                "update_metadata_invalid"
            }
            Self::InvalidPublicKey | Self::InvalidSignature => "update_signature_invalid",
            Self::LengthMismatch { .. } | Self::Io(_) | Self::Http(_) => "update_download_failed",
            Self::UnsafeStagingDirectory
            | Self::UnsafeStagingEntry
            | Self::UntrustedStagingDirectory
            | Self::UnsafeServiceDataEntry
            | Self::UntrustedServiceDataEntry
            | Self::StagingProtection(_) => "update_staging_unsafe",
            Self::UpdateDeadlineExpired => "update_deadline_exceeded",
            Self::InstallerStart(_) | Self::InstallerExit(_) => "update_installer_failed",
            Self::InstallerTimeout | Self::InstallerTermination(_) => {
                "update_installer_indeterminate"
            }
        }
    }

    const fn installer_state_is_indeterminate(&self) -> bool {
        matches!(self, Self::InstallerTimeout | Self::InstallerTermination(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};
    use std::ffi::c_void;
    use std::os::windows::fs::{symlink_dir, symlink_file};
    use tempfile::tempdir;
    use windows::Win32::Security::{
        ACCESS_ALLOWED_ACE, CONTAINER_INHERIT_ACE, GetAce, OBJECT_INHERIT_ACE,
    };
    use windows::Win32::Storage::FileSystem::{
        FILE_ADD_FILE, FILE_ALL_ACCESS, FILE_SHARE_DELETE, FILE_SHARE_WRITE,
    };

    fn test_directory_security() -> SecureDirectorySecurity {
        SecureDirectorySecurity::from_sddl(
            w!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;AU)"),
            false,
        )
        .expect("test directory security")
    }

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

    #[test]
    fn rejected_installer_preserves_the_last_successful_install_log() {
        let directory = tempdir().unwrap();
        let installer_path = directory.path().join("update.msi");
        let log_path = directory.path().join("install.log");
        let contents = b"invalid Windows Installer fixture";
        let previous_log = b"previous successful transaction evidence";
        fs::write(&installer_path, contents).unwrap();
        fs::write(&log_path, previous_log).unwrap();

        let private = SigningKey::from_bytes(&[9_u8; 32]);
        let invalid_signature = STANDARD.encode(private.sign(b"different bytes").to_bytes());
        let public = STANDARD.encode(private.verifying_key().to_bytes());
        assert!(matches!(
            verify_installer_before_rotating_log(
                &installer_path,
                contents.len() as u64,
                &invalid_signature,
                &public,
                &log_path,
            ),
            Err(UpdateError::InvalidSignature)
        ));
        assert_eq!(fs::read(log_path).unwrap(), previous_log);
    }

    #[test]
    fn production_directory_descriptor_has_only_system_and_administrator_aces() {
        let security = SecureDirectorySecurity::system_and_administrators().unwrap();
        assert!(sid_matches_sddl_owner(security.owner, w!("O:SY")).unwrap());
        assert!(sid_matches_sddl_owner(security.group, w!("O:SY")).unwrap());
        // SAFETY: dacl belongs to the live descriptor.
        assert_eq!(unsafe { (*security.dacl).AceCount }, 2);

        let mut found_system = false;
        let mut found_administrators = false;
        for index in 0..2 {
            let mut raw_ace = ptr::null_mut::<c_void>();
            // SAFETY: both indexes are below AceCount and output is writable.
            unsafe { GetAce(security.dacl, index, &raw mut raw_ace) }.unwrap();
            // SAFETY: the production SDDL contains ACCESS_ALLOWED_ACE records.
            let ace = unsafe { &*raw_ace.cast::<ACCESS_ALLOWED_ACE>() };
            assert_eq!(ace.Header.AceType, 0);
            assert_eq!(ace.Mask, FILE_ALL_ACCESS.0);
            let inheritance = (OBJECT_INHERIT_ACE.0 | CONTAINER_INHERIT_ACE.0) as u8;
            assert_eq!(ace.Header.AceFlags & inheritance, inheritance);
            let sid = PSID(ptr::from_ref(&ace.SidStart).cast_mut().cast());
            found_system |= sid_matches_sddl_owner(sid, w!("O:SY")).unwrap();
            found_administrators |= sid_matches_sddl_owner(sid, w!("O:BA")).unwrap();
        }
        assert!(found_system);
        assert!(found_administrators);
    }

    #[test]
    fn preexisting_regular_directory_is_hardened_before_new_files_are_created() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("Updates");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("existing.txt"), b"preserved").unwrap();

        let guard = prepare_secure_directory(&root, &test_directory_security()).unwrap();
        let second_guard = prepare_secure_directory(&root, &test_directory_security()).unwrap();
        fs::write(root.join("created-after-hardening.txt"), b"safe").unwrap();
        assert_eq!(fs::read(root.join("existing.txt")).unwrap(), b"preserved");
        drop(second_guard);
        drop(guard);
    }

    #[test]
    fn empty_untrusted_directory_is_rejected_even_with_a_preopened_handle() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("Updates");
        fs::create_dir(&root).unwrap();
        let path_wide = wide_null(root.as_os_str());
        let inspection = open_directory_for_protection(&path_wide).unwrap();
        if directory_owner_is_system_or_administrator(inspection.0).unwrap() {
            return;
        }
        drop(inspection);
        // SAFETY: path is live/null-terminated and the returned handle is owned
        // by the guard below. This models a write-capable handle opened before
        // the service attempts to harden the directory.
        let attacker_handle = OwnedHandle(
            unsafe {
                CreateFileW(
                    PCWSTR(path_wide.as_ptr()),
                    FILE_ADD_FILE.0,
                    FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
                    None,
                )
            }
            .unwrap(),
        );

        let security = SecureDirectorySecurity::system_and_administrators().unwrap();
        assert!(matches!(
            prepare_secure_directory(&root, &security),
            Err(UpdateError::UntrustedStagingDirectory) | Err(UpdateError::StagingProtection(_))
        ));
        assert!(root.is_dir());
        drop(attacker_handle);
    }

    #[test]
    fn directory_path_that_is_a_regular_file_fails_closed() {
        let parent = tempdir().unwrap();
        let root = parent.path().join("Updates");
        fs::write(&root, b"not a directory").unwrap();

        assert!(matches!(
            prepare_secure_directory(&root, &test_directory_security()),
            Err(UpdateError::UnsafeStagingDirectory) | Err(UpdateError::StagingProtection(_))
        ));
        assert_eq!(fs::read(&root).unwrap(), b"not a directory");
    }

    #[test]
    fn directory_reparse_point_is_rejected_without_touching_its_target() {
        let parent = tempdir().unwrap();
        let target = parent.path().join("target");
        let root = parent.path().join("Updates");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("sentinel.txt"), b"keep").unwrap();
        if let Err(error) = symlink_dir(&target, &root) {
            eprintln!(
                "skipping reparse assertion because symlink creation is unavailable: {error}"
            );
            return;
        }

        assert!(prepare_secure_directory(&root, &test_directory_security()).is_err());
        assert_eq!(fs::read(target.join("sentinel.txt")).unwrap(), b"keep");
        assert!(
            root.symlink_metadata().unwrap().file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0
                != 0
        );
        fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn stale_child_reparse_point_is_removed_without_touching_its_target() {
        let parent = tempdir().unwrap();
        let target = parent.path().join("outside.msi");
        let stale = parent.path().join("update.msi.partial");
        fs::write(&target, b"outside").unwrap();
        if let Err(error) = symlink_file(&target, &stale) {
            eprintln!(
                "skipping reparse assertion because symlink creation is unavailable: {error}"
            );
            return;
        }

        remove_staging_entry_if_present(&stale).unwrap();
        assert!(!stale.exists());
        assert_eq!(fs::read(&target).unwrap(), b"outside");
    }

    #[test]
    fn final_verified_installer_cannot_be_replaced_or_modified_while_locked() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("update.msi");
        let contents = b"final signed Windows Installer fixture";
        fs::write(&path, contents).unwrap();
        let private = SigningKey::from_bytes(&[11_u8; 32]);
        let signature = STANDARD.encode(private.sign(contents).to_bytes());
        let public = STANDARD.encode(private.verifying_key().to_bytes());

        let verified =
            VerifiedInstaller::open(&path, contents.len() as u64, &signature, &public).unwrap();
        assert!(
            OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .is_err()
        );
        assert!(fs::remove_file(&path).is_err());
        drop(verified);
        fs::write(&path, b"replacement").unwrap();
    }

    #[test]
    fn known_service_file_with_an_untrusted_owner_fails_closed() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("state.db");
        fs::write(&path, b"attacker controlled").unwrap();
        let path_wide = wide_null(path.as_os_str());
        let handle = open_file_for_protection(&path_wide).unwrap();
        if directory_owner_is_system_or_administrator(handle.0).unwrap() {
            return;
        }
        drop(handle);

        let security = SecureDirectorySecurity::system_and_administrators_file().unwrap();
        assert!(matches!(
            prepare_existing_secure_file(&path, &security),
            Err(UpdateError::UntrustedServiceDataEntry)
        ));
        assert_eq!(fs::read(&path).unwrap(), b"attacker controlled");
    }

    #[cfg(windows)]
    #[test]
    fn installer_wait_kills_a_process_that_exceeds_its_deadline() {
        let mut child = Command::new("powershell.exe")
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Start-Sleep -Seconds 30",
            ])
            .spawn()
            .unwrap();

        assert!(matches!(
            wait_for_installer(&mut child, Instant::now() + Duration::from_millis(50)),
            Err(UpdateError::InstallerTimeout)
        ));
        assert!(child.try_wait().unwrap().is_some());
        assert!(UpdateError::InstallerTimeout.installer_state_is_indeterminate());
        assert_eq!(
            UpdateError::InstallerTimeout.code(),
            "update_installer_indeterminate"
        );
        assert!(!UpdateError::UpdateDeadlineExpired.installer_state_is_indeterminate());
        assert_eq!(
            UpdateError::UpdateDeadlineExpired.code(),
            "update_deadline_exceeded"
        );
        let directory = tempdir().unwrap();
        let installer_path = directory.path().join("not-launched.msi");
        fs::write(&installer_path, b"not launched").unwrap();
        let installer = VerifiedInstaller {
            path: installer_path,
            _lock: File::open(directory.path().join("not-launched.msi")).unwrap(),
        };
        assert!(matches!(
            launch_msi(
                &installer,
                Path::new("not-created.log"),
                Instant::now() - Duration::from_millis(1),
            ),
            Err(UpdateError::UpdateDeadlineExpired)
        ));
    }
}
