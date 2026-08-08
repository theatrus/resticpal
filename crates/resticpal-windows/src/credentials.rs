//! DPAPI-backed credential storage scoped to the process's Windows identity.

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicU64, Ordering, compiler_fence};

use thiserror::Error;
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INSUFFICIENT_BUFFER, HANDLE, HLOCAL, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    SE_FILE_OBJECT, SetNamedSecurityInfoW,
};
use windows::Win32::Security::Cryptography::{
    CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
};
use windows::Win32::Security::{
    ACL, DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, TOKEN_QUERY, TOKEN_USER, TokenUser,
};
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_REPARSE_POINT, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{BOOL, HRESULT, PCWSTR, PWSTR, w};
use zeroize::Zeroizing;

const FILE_MAGIC: &[u8] = b"RESTICPAL-CREDENTIAL\x01";
const FILE_EXTENSION: &str = "credential";
const MAX_SECRET_BYTES: usize = 32 * 1024;
const MAX_PROTECTED_BYTES: usize = 128 * 1024;
// This is application separation, not a hidden key. It must remain stable so
// existing credentials continue to decrypt after upgrades.
const OPTIONAL_ENTROPY: &[u8] = b"resticpal credential store v1";
static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone)]
pub struct DpapiSecretStore {
    root: PathBuf,
}

impl DpapiSecretStore {
    /// Opens or creates a credential directory and replaces its DACL with a
    /// protected ACL for LocalSystem, administrators, and the current identity.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, CredentialStoreError> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        validate_directory(&root)?;
        protect_path(&root)?;

        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            let metadata = entry.path().symlink_metadata()?;
            if is_reparse_point(&metadata) || !metadata.is_file() {
                return Err(CredentialStoreError::UnexpectedStoreEntry);
            }
            protect_path(&entry.path())?;
        }

        Ok(Self { root })
    }

    pub fn put(&self, reference: &str, secret: &[u8]) -> Result<(), CredentialStoreError> {
        validate_reference(reference)?;
        if secret.is_empty() {
            return Err(CredentialStoreError::EmptySecret);
        }
        if secret.len() > MAX_SECRET_BYTES {
            return Err(CredentialStoreError::SecretTooLarge);
        }

        let protected = protect_data(secret)?;
        if protected.len() > MAX_PROTECTED_BYTES {
            return Err(CredentialStoreError::ProtectedDataTooLarge);
        }
        let mut contents = Vec::with_capacity(FILE_MAGIC.len() + protected.len());
        contents.extend_from_slice(FILE_MAGIC);
        contents.extend_from_slice(&protected);
        self.write_atomically(reference, &contents)
    }

    pub fn get(&self, reference: &str) -> Result<Zeroizing<Vec<u8>>, CredentialStoreError> {
        let path = self.path_for(reference)?;
        let metadata = match path.symlink_metadata() {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(CredentialStoreError::NotFound);
            }
            Err(error) => return Err(error.into()),
        };
        if is_reparse_point(&metadata) || !metadata.is_file() {
            return Err(CredentialStoreError::UnexpectedStoreEntry);
        }

        let mut file = File::open(path)?;
        let mut contents = Vec::new();
        Read::by_ref(&mut file)
            .take(u64::try_from(MAX_PROTECTED_BYTES + FILE_MAGIC.len() + 1).unwrap())
            .read_to_end(&mut contents)?;
        if contents.len() > MAX_PROTECTED_BYTES + FILE_MAGIC.len() {
            return Err(CredentialStoreError::ProtectedDataTooLarge);
        }
        let protected = contents
            .strip_prefix(FILE_MAGIC)
            .filter(|value| !value.is_empty())
            .ok_or(CredentialStoreError::InvalidFormat)?;
        let plaintext = unprotect_data(protected)?;
        if plaintext.is_empty() || plaintext.len() > MAX_SECRET_BYTES {
            return Err(CredentialStoreError::InvalidPlaintext);
        }
        Ok(plaintext)
    }

    pub fn remove(&self, reference: &str) -> Result<(), CredentialStoreError> {
        let path = self.path_for(reference)?;
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn path_for(&self, reference: &str) -> Result<PathBuf, CredentialStoreError> {
        validate_reference(reference)?;
        Ok(self.root.join(format!("{reference}.{FILE_EXTENSION}")))
    }

    fn write_atomically(
        &self,
        reference: &str,
        contents: &[u8],
    ) -> Result<(), CredentialStoreError> {
        let target = self.path_for(reference)?;
        let temporary = self.root.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        let cleanup = TemporaryFile::new(temporary.clone());
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        protect_path(&temporary)?;

        move_file_replacing(&temporary, &target)?;
        cleanup.disarm();
        protect_path(&target)?;
        Ok(())
    }
}

fn validate_reference(reference: &str) -> Result<(), CredentialStoreError> {
    if reference.is_empty()
        || reference.len() > 64
        || !reference
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        Err(CredentialStoreError::InvalidReference)
    } else {
        Ok(())
    }
}

fn validate_directory(path: &Path) -> Result<(), CredentialStoreError> {
    let metadata = path.symlink_metadata()?;
    if metadata.is_dir() && !is_reparse_point(&metadata) {
        Ok(())
    } else {
        Err(CredentialStoreError::UnsafeStoreDirectory)
    }
}

fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
}

fn protect_data(plaintext: &[u8]) -> Result<Vec<u8>, CredentialStoreError> {
    let input = blob_for(plaintext)?;
    let entropy = blob_for(OPTIONAL_ENTROPY)?;
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input and entropy point to live slices, output is writable, and
    // DPAPI allocates output with LocalAlloc for this process to release.
    unsafe {
        CryptProtectData(
            &raw const input,
            w!("resticpal credential"),
            Some(&raw const entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut output,
        )
    }?;
    let output = LocalBlob::new(output, false);
    Ok(output.as_slice().to_vec())
}

fn unprotect_data(protected: &[u8]) -> Result<Zeroizing<Vec<u8>>, CredentialStoreError> {
    let input = blob_for(protected)?;
    let entropy = blob_for(OPTIONAL_ENTROPY)?;
    let mut output = CRYPT_INTEGER_BLOB::default();
    // SAFETY: input and entropy point to live slices, output is writable, no UI
    // is permitted, and the returned allocation is owned below.
    unsafe {
        CryptUnprotectData(
            &raw const input,
            None,
            Some(&raw const entropy),
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut output,
        )
    }?;
    let output = LocalBlob::new(output, true);
    Ok(Zeroizing::new(output.as_slice().to_vec()))
}

fn blob_for(bytes: &[u8]) -> Result<CRYPT_INTEGER_BLOB, CredentialStoreError> {
    Ok(CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(bytes.len()).map_err(|_| CredentialStoreError::SecretTooLarge)?,
        pbData: bytes.as_ptr().cast_mut(),
    })
}

struct LocalBlob {
    blob: CRYPT_INTEGER_BLOB,
    zero_on_drop: bool,
}

impl LocalBlob {
    fn new(blob: CRYPT_INTEGER_BLOB, zero_on_drop: bool) -> Self {
        Self { blob, zero_on_drop }
    }

    fn as_slice(&self) -> &[u8] {
        if self.blob.pbData.is_null() || self.blob.cbData == 0 {
            &[]
        } else {
            // SAFETY: DPAPI returned this allocation and cbData is its readable length.
            unsafe { slice::from_raw_parts(self.blob.pbData, self.blob.cbData as usize) }
        }
    }
}

impl Drop for LocalBlob {
    fn drop(&mut self) {
        if self.blob.pbData.is_null() {
            return;
        }
        if self.zero_on_drop {
            for index in 0..self.blob.cbData as usize {
                // SAFETY: the DPAPI allocation is writable for cbData bytes and
                // remains owned until LocalFree below.
                unsafe { self.blob.pbData.add(index).write_volatile(0) };
            }
            compiler_fence(Ordering::SeqCst);
        }
        // SAFETY: DPAPI allocated pbData with LocalAlloc and ownership is unique.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.blob.pbData.cast()))) };
    }
}

fn protect_path(path: &Path) -> Result<(), CredentialStoreError> {
    let acl = CredentialAcl::for_current_identity()?;
    let mut path = wide_null(path.as_os_str());
    // SAFETY: path is writable and null terminated, the DACL remains live for
    // the call, and no owner, group, or SACL mutation is requested.
    unsafe {
        SetNamedSecurityInfoW(
            PWSTR(path.as_mut_ptr()),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            None,
            None,
            Some(acl.dacl.cast_const()),
            None,
        )
    }
    .ok()?;
    Ok(())
}

struct CredentialAcl {
    descriptor: PSECURITY_DESCRIPTOR,
    dacl: *mut ACL,
}

impl CredentialAcl {
    fn for_current_identity() -> Result<Self, CredentialStoreError> {
        let current_sid = current_user_sid_string()?;
        let sddl = format!("D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;FA;;;{current_sid})");
        let sddl = wide_null(OsStr::new(&sddl));
        let mut descriptor = PSECURITY_DESCRIPTOR::default();
        // SAFETY: SDDL is a valid null-terminated string and Windows allocates
        // the returned self-relative descriptor with LocalAlloc.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &raw mut descriptor,
                None,
            )
        }?;

        let mut dacl_present = BOOL::default();
        let mut dacl_defaulted = BOOL::default();
        let mut dacl = ptr::null_mut();
        // SAFETY: descriptor is live and the output pointers are writable.
        unsafe {
            GetSecurityDescriptorDacl(
                descriptor,
                &raw mut dacl_present,
                &raw mut dacl,
                &raw mut dacl_defaulted,
            )
        }?;
        if !dacl_present.as_bool() || dacl.is_null() {
            let _ = unsafe { LocalFree(Some(HLOCAL(descriptor.0))) };
            return Err(CredentialStoreError::InvalidSecurityDescriptor);
        }

        Ok(Self { descriptor, dacl })
    }
}

impl Drop for CredentialAcl {
    fn drop(&mut self) {
        // SAFETY: conversion allocated descriptor with LocalAlloc. The DACL is
        // part of this allocation and is not freed separately.
        let _ = unsafe { LocalFree(Some(HLOCAL(self.descriptor.0))) };
    }
}

fn current_user_sid_string() -> Result<String, CredentialStoreError> {
    let mut token = HANDLE::default();
    // SAFETY: GetCurrentProcess returns a pseudo-handle and token is writable.
    unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &raw mut token) }?;
    let token = OwnedHandle(token);

    let mut required = 0_u32;
    // The first call intentionally has no buffer and obtains the required size.
    let first = unsafe {
        windows::Win32::Security::GetTokenInformation(
            token.0,
            TokenUser,
            None,
            0,
            &raw mut required,
        )
    };
    match first {
        Err(error)
            if error.code() == HRESULT::from_win32(ERROR_INSUFFICIENT_BUFFER.0) && required > 0 => {
        }
        Err(error) => return Err(error.into()),
        Ok(()) => return Err(CredentialStoreError::InvalidTokenInformation),
    }

    let word_size = size_of::<usize>();
    let mut buffer = vec![0_usize; (required as usize).div_ceil(word_size)];
    // SAFETY: the usize buffer is suitably aligned and has at least required bytes.
    unsafe {
        windows::Win32::Security::GetTokenInformation(
            token.0,
            TokenUser,
            Some(buffer.as_mut_ptr().cast()),
            required,
            &raw mut required,
        )
    }?;
    // SAFETY: GetTokenInformation initialized a TOKEN_USER at the buffer start.
    let token_user = unsafe { &*buffer.as_ptr().cast::<TOKEN_USER>() };

    let mut string_sid = PWSTR::null();
    // SAFETY: the SID points inside the live token-information buffer and the
    // output receives a LocalAlloc-owned null-terminated string.
    unsafe { ConvertSidToStringSidW(token_user.User.Sid, &raw mut string_sid) }?;
    let string_sid = LocalString(string_sid);
    // SAFETY: ConvertSidToStringSidW returned a valid null-terminated UTF-16 SID.
    let value = unsafe { string_sid.0.to_string() }?;
    Ok(value)
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

struct LocalString(PWSTR);

impl Drop for LocalString {
    fn drop(&mut self) {
        if !self.0.is_null() {
            let _ = unsafe { LocalFree(Some(HLOCAL(self.0.0.cast()))) };
        }
    }
}

fn move_file_replacing(source: &Path, target: &Path) -> Result<(), CredentialStoreError> {
    let source = wide_null(source.as_os_str());
    let target = wide_null(target.as_os_str());
    // SAFETY: both paths are live, null-terminated strings. The operation is
    // confined to two validated paths within the credential directory.
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }?;
    Ok(())
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

struct TemporaryFile {
    path: Option<PathBuf>,
}

impl TemporaryFile {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(mut self) {
        self.path = None;
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            let _ = fs::remove_file(path);
        }
    }
}

#[derive(Debug, Error)]
pub enum CredentialStoreError {
    #[error("credential reference must contain 1-64 lowercase letters, digits, or hyphens")]
    InvalidReference,
    #[error("credential value cannot be empty")]
    EmptySecret,
    #[error("credential value exceeds the storage limit")]
    SecretTooLarge,
    #[error("protected credential exceeds the storage limit")]
    ProtectedDataTooLarge,
    #[error("credential does not exist")]
    NotFound,
    #[error("credential file has an invalid format")]
    InvalidFormat,
    #[error("decrypted credential has an invalid size")]
    InvalidPlaintext,
    #[error("credential directory is not a safe local directory")]
    UnsafeStoreDirectory,
    #[error("credential directory contains an unexpected entry")]
    UnexpectedStoreEntry,
    #[error("credential ACL descriptor did not contain a DACL")]
    InvalidSecurityDescriptor,
    #[error("Windows token did not return current-user information")]
    InvalidTokenInformation,
    #[error("credential I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("Windows credential protection failed: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("Windows SID text was invalid: {0}")]
    SidEncoding(#[from] std::string::FromUtf16Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_identity_round_trips_and_replaces_a_secret() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = DpapiSecretStore::open(directory.path().join("credentials"))
            .expect("store should open");

        store
            .put("repository-password", b"first-secret")
            .expect("secret should be protected");
        assert_eq!(
            store.get("repository-password").expect("secret").as_slice(),
            b"first-secret"
        );

        store
            .put("repository-password", b"replacement-secret")
            .expect("secret should be replaced atomically");
        drop(store);
        let store = DpapiSecretStore::open(directory.path().join("credentials"))
            .expect("protected store should reopen");
        assert_eq!(
            store.get("repository-password").expect("secret").as_slice(),
            b"replacement-secret"
        );
    }

    #[test]
    fn persisted_file_does_not_contain_plaintext() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().join("credentials");
        let store = DpapiSecretStore::open(&root).expect("store should open");
        let plaintext = b"unique-plaintext-value-for-resticpal";

        store
            .put("s3-secret-access-key", plaintext)
            .expect("secret should be protected");
        let contents = fs::read(root.join("s3-secret-access-key.credential"))
            .expect("credential file should exist");

        assert!(contents.starts_with(FILE_MAGIC));
        assert!(
            !contents
                .windows(plaintext.len())
                .any(|value| value == plaintext)
        );
    }

    #[test]
    fn invalid_references_cannot_escape_the_store() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = DpapiSecretStore::open(directory.path().join("credentials"))
            .expect("store should open");

        for reference in ["", "../outside", "UPPERCASE", "has_underscore", "has.dot"] {
            assert!(matches!(
                store.put(reference, b"secret"),
                Err(CredentialStoreError::InvalidReference)
            ));
            assert!(matches!(
                store.get(reference),
                Err(CredentialStoreError::InvalidReference)
            ));
        }
    }

    #[test]
    fn corrupted_ciphertext_fails_closed() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path().join("credentials");
        let store = DpapiSecretStore::open(&root).expect("store should open");
        store.put("token", b"secret").expect("secret should store");
        fs::write(
            root.join("token.credential"),
            [FILE_MAGIC, b"not-dpapi"].concat(),
        )
        .expect("test should corrupt credential");

        assert!(matches!(
            store.get("token"),
            Err(CredentialStoreError::Windows(_))
        ));
    }

    #[test]
    fn remove_is_idempotent() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = DpapiSecretStore::open(directory.path().join("credentials"))
            .expect("store should open");
        store.put("token", b"secret").expect("secret should store");

        store.remove("token").expect("first remove should work");
        store.remove("token").expect("second remove should work");
        assert!(matches!(
            store.get("token"),
            Err(CredentialStoreError::NotFound)
        ));
    }
}
