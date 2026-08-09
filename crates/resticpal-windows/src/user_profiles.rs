//! Discovery of standard backup folders for local Windows user profiles.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

#[cfg(test)]
use std::fs;

use resticpal_core::config::{MAX_BACKUP_PATHS, MAX_PATH_CHARACTERS};
use resticpal_protocol::{DiscoveredBackupSource, DiscoveredSourceKind};
use thiserror::Error;
use windows::Win32::Foundation::ERROR_NO_MORE_ITEMS;
use windows::Win32::System::Environment::ExpandEnvironmentStringsW;
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, RRF_RT_REG_EXPAND_SZ, RRF_RT_REG_SZ, RRF_ZEROONFAILURE,
    RegEnumKeyExW, RegGetValueW, RegOpenKeyExW,
};
use windows::core::{Error as WindowsError, PCWSTR, PWSTR, w};

const PROFILE_LIST_KEY: PCWSTR = w!(r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\ProfileList");
const MAX_PROFILE_KEYS: u32 = 256;
const MAX_REGISTRY_STRING_BYTES: u32 = 64 * 1024;

pub fn discover_backup_sources() -> Result<Vec<DiscoveredBackupSource>, ProfileDiscoveryError> {
    discover_from_profiles(enumerate_profile_paths()?)
}

fn enumerate_profile_paths() -> Result<Vec<PathBuf>, ProfileDiscoveryError> {
    let mut raw_key = HKEY::default();
    // SAFETY: PROFILE_LIST_KEY is a valid static path and raw_key is writable.
    unsafe {
        RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            PROFILE_LIST_KEY,
            None,
            KEY_READ,
            &raw mut raw_key,
        )
    }
    .ok()?;
    let key = OwnedRegistryKey(raw_key);
    let mut profiles = Vec::new();

    for index in 0..MAX_PROFILE_KEYS {
        let mut name = [0_u16; 256];
        let mut name_length = u32::try_from(name.len()).expect("registry buffer fits in u32");
        // SAFETY: name is writable for name_length UTF-16 units and all other
        // optional outputs are intentionally omitted.
        let result = unsafe {
            RegEnumKeyExW(
                key.0,
                index,
                Some(PWSTR(name.as_mut_ptr())),
                &raw mut name_length,
                None,
                None,
                None,
                None,
            )
        };
        if result == ERROR_NO_MORE_ITEMS {
            break;
        }
        result.ok()?;
        let name = String::from_utf16(&name[..name_length as usize])?;
        if let Ok(value) = read_profile_path(key.0, &name)
            && is_candidate_profile(&value)
        {
            profiles.push(value);
        }
    }

    Ok(profiles)
}

fn read_profile_path(parent: HKEY, subkey: &str) -> Result<PathBuf, ProfileDiscoveryError> {
    let subkey = wide_null(subkey);
    let flags = RRF_RT_REG_SZ | RRF_RT_REG_EXPAND_SZ | RRF_ZEROONFAILURE;
    let mut byte_count = 0_u32;
    // SAFETY: strings are null terminated and byte_count is writable. The
    // first call intentionally requests only the required value size.
    unsafe {
        RegGetValueW(
            parent,
            PCWSTR(subkey.as_ptr()),
            w!("ProfileImagePath"),
            flags,
            None,
            None,
            Some(&raw mut byte_count),
        )
    }
    .ok()?;
    if byte_count == 0 || byte_count > MAX_REGISTRY_STRING_BYTES {
        return Err(ProfileDiscoveryError::InvalidRegistryString);
    }

    let mut value = vec![0_u16; (byte_count as usize).div_ceil(size_of::<u16>())];
    // SAFETY: value is writable for byte_count bytes and both strings remain live.
    unsafe {
        RegGetValueW(
            parent,
            PCWSTR(subkey.as_ptr()),
            w!("ProfileImagePath"),
            flags,
            None,
            Some(value.as_mut_ptr().cast()),
            Some(&raw mut byte_count),
        )
    }
    .ok()?;
    let value = String::from_utf16(trim_null(&value))?;
    Ok(PathBuf::from(expand_environment(&value)?))
}

fn expand_environment(value: &str) -> Result<String, ProfileDiscoveryError> {
    let value = wide_null(value);
    // SAFETY: value is a live null-terminated string; no destination requests
    // the number of UTF-16 units required, including the terminator.
    let required = unsafe { ExpandEnvironmentStringsW(PCWSTR(value.as_ptr()), None) };
    if required == 0 || required > MAX_REGISTRY_STRING_BYTES / 2 {
        return Err(WindowsError::from_thread().into());
    }
    let mut expanded = vec![0_u16; required as usize];
    // SAFETY: expanded has the size returned by the first call.
    let written =
        unsafe { ExpandEnvironmentStringsW(PCWSTR(value.as_ptr()), Some(expanded.as_mut_slice())) };
    if written == 0 || written > required {
        return Err(WindowsError::from_thread().into());
    }
    Ok(String::from_utf16(trim_null(&expanded))?)
}

fn discover_from_profiles(
    profiles: impl IntoIterator<Item = PathBuf>,
) -> Result<Vec<DiscoveredBackupSource>, ProfileDiscoveryError> {
    let mut profiles: Vec<_> = profiles.into_iter().collect();
    profiles.sort_by_key(|path| path.to_string_lossy().to_lowercase());
    let mut seen = BTreeSet::new();
    let mut sources = Vec::new();

    'profiles: for profile in profiles {
        if !is_candidate_profile(&profile) || !profile.is_dir() {
            continue;
        }
        let profile_name = profile
            .file_name()
            .map_or_else(|| "User".to_owned(), |name| name.to_string_lossy().into());
        for (kind, folder_name) in standard_folders() {
            let path = profile.join(folder_name);
            let key = path.to_string_lossy().to_lowercase();
            if path.is_dir()
                && path.to_string_lossy().encode_utf16().count() <= MAX_PATH_CHARACTERS
                && seen.insert(key)
            {
                sources.push(DiscoveredBackupSource {
                    profile_name: profile_name.clone(),
                    kind,
                    path,
                });
                if sources.len() == MAX_BACKUP_PATHS {
                    break 'profiles;
                }
            }
        }
    }

    Ok(sources)
}

fn standard_folders() -> [(DiscoveredSourceKind, &'static str); 5] {
    [
        (DiscoveredSourceKind::Desktop, "Desktop"),
        (DiscoveredSourceKind::Documents, "Documents"),
        (DiscoveredSourceKind::Pictures, "Pictures"),
        (DiscoveredSourceKind::Videos, "Videos"),
        (DiscoveredSourceKind::Music, "Music"),
    ]
}

fn is_candidate_profile(path: &Path) -> bool {
    if !path.is_absolute() {
        return false;
    }
    let value = path.to_string_lossy().replace('/', r"\").to_lowercase();
    !value.starts_with(r"\\")
        && !value.contains(r"\windows\system32\config\systemprofile")
        && !value.contains(r"\windows\serviceprofiles\")
        && !matches!(
            path.file_name()
                .map(|name| name.to_string_lossy().to_lowercase())
                .as_deref(),
            Some("default" | "default user" | "public")
        )
}

fn trim_null(value: &[u16]) -> &[u16] {
    let length = value
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(value.len());
    &value[..length]
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

struct OwnedRegistryKey(HKEY);

impl Drop for OwnedRegistryKey {
    fn drop(&mut self) {
        let _ = unsafe { windows::Win32::System::Registry::RegCloseKey(self.0) };
    }
}

#[derive(Debug, Error)]
pub enum ProfileDiscoveryError {
    #[error("Windows profile registry operation failed: {0}")]
    Windows(#[from] WindowsError),
    #[error("Windows profile registry string was invalid: {0}")]
    Utf16(#[from] std::string::FromUtf16Error),
    #[error("Windows profile registry value was empty or too large")]
    InvalidRegistryString,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_only_existing_default_folders() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let profile = directory.path().join("Example");
        fs::create_dir_all(profile.join("Desktop")).expect("desktop");
        fs::create_dir_all(profile.join("Documents")).expect("documents");

        let sources = discover_from_profiles([profile]).expect("discovery");

        assert_eq!(sources.len(), 2);
        assert_eq!(sources[0].kind, DiscoveredSourceKind::Desktop);
        assert_eq!(sources[1].kind, DiscoveredSourceKind::Documents);
    }

    #[test]
    fn excludes_system_and_template_profiles() {
        assert!(!is_candidate_profile(Path::new(
            r"C:\Windows\System32\config\systemprofile"
        )));
        assert!(!is_candidate_profile(Path::new(r"C:\Users\Public")));
        assert!(!is_candidate_profile(Path::new(
            r"\\server\profiles\Example"
        )));
        assert!(is_candidate_profile(Path::new(r"D:\Profiles\Yann")));
    }

    #[test]
    fn trims_registry_string_terminators() {
        assert_eq!(trim_null(&[b'a' as u16, b'b' as u16, 0, 0]), &[97, 98]);
    }

    #[test]
    fn discovery_is_bounded_to_the_configuration_limit() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let mut profiles = Vec::new();
        for index in 0..=MAX_BACKUP_PATHS {
            let profile = directory.path().join(format!("User{index:03}"));
            fs::create_dir_all(profile.join("Desktop")).expect("desktop");
            profiles.push(profile);
        }

        let sources = discover_from_profiles(profiles).expect("discovery");

        assert_eq!(sources.len(), MAX_BACKUP_PATHS);
    }
}
