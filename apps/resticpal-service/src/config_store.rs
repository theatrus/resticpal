use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use resticpal_core::config::{LocalConfig, LocalConfigError};
use thiserror::Error;

use crate::atomic_file::{self, AtomicFileError};

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub struct LocalConfigStore {
    path: PathBuf,
}

impl LocalConfigStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<LocalConfig, ConfigStoreError> {
        let mut file = match fs::File::open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(LocalConfig::default());
            }
            Err(error) => return Err(error.into()),
        };
        let mut contents = String::new();
        Read::by_ref(&mut file)
            .take(MAX_CONFIG_BYTES + 1)
            .read_to_string(&mut contents)?;
        if contents.len() as u64 > MAX_CONFIG_BYTES {
            return Err(ConfigStoreError::TooLarge);
        }
        Ok(LocalConfig::from_toml(&contents)?)
    }

    pub fn save(&self, config: &LocalConfig) -> Result<(), ConfigStoreError> {
        let mut contents = config.to_toml_pretty()?.into_bytes();
        contents.push(b'\n');
        if contents.len() as u64 > MAX_CONFIG_BYTES {
            return Err(ConfigStoreError::TooLarge);
        }
        atomic_file::replace(&self.path, &contents, "config")?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ConfigStoreError {
    #[error("local configuration exceeds its size limit")]
    TooLarge,
    #[error("local configuration I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    LocalConfig(#[from] LocalConfigError),
    #[error(transparent)]
    AtomicFile(#[from] AtomicFileError),
}

#[cfg(test)]
mod tests {
    use resticpal_core::config::{CONFIG_SCHEMA_VERSION, LocalBackupConfig};

    use super::*;

    #[test]
    fn missing_configuration_loads_product_defaults() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = LocalConfigStore::new(directory.path().join("config.toml"));

        assert_eq!(
            store.load().expect("missing config is valid"),
            LocalConfig::default()
        );
    }

    #[test]
    fn configuration_round_trips_and_is_replaced() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let store = LocalConfigStore::new(directory.path().join("config.toml"));
        let mut config = LocalConfig {
            schema_version: CONFIG_SCHEMA_VERSION,
            backup: LocalBackupConfig {
                paths: Some(vec![PathBuf::from(r"C:\Users\Example\Documents")]),
                exclusions: Some(Vec::new()),
            },
            ..LocalConfig::default()
        };

        store.save(&config).expect("first save");
        config.backup.paths = Some(vec![PathBuf::from(r"D:\Data")]);
        store.save(&config).expect("replacement save");

        assert_eq!(store.load().expect("load"), config);
    }
}
