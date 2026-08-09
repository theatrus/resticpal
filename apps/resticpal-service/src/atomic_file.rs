use std::ffi::OsStr;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use thiserror::Error;
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::core::PCWSTR;

static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

pub fn replace(path: &Path, contents: &[u8], label: &str) -> Result<(), AtomicFileError> {
    let parent = path.parent().ok_or(AtomicFileError::MissingParent)?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".{label}-{}-{}.tmp",
        std::process::id(),
        NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
    ));
    let cleanup = TemporaryFile(temporary.clone());
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);

    let source = wide_null(temporary.as_os_str());
    let target = wide_null(path.as_os_str());
    // SAFETY: both buffers are live null-terminated paths, and the source is a
    // newly created file in the same directory as the target.
    unsafe {
        MoveFileExW(
            PCWSTR(source.as_ptr()),
            PCWSTR(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }?;
    cleanup.disarm();
    Ok(())
}

fn wide_null(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(std::iter::once(0)).collect()
}

struct TemporaryFile(PathBuf);

impl TemporaryFile {
    fn disarm(mut self) {
        self.0 = PathBuf::new();
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if !self.0.as_os_str().is_empty() {
            let _ = fs::remove_file(&self.0);
        }
    }
}

#[derive(Debug, Error)]
pub enum AtomicFileError {
    #[error("atomic file target has no parent directory")]
    MissingParent,
    #[error("atomic file I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("could not atomically replace the target file: {0}")]
    Windows(#[from] windows::core::Error),
}
