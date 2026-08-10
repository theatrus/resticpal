//! Bounded, redacted operational diagnostics stored next to the service config.

use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::Utc;
use resticpal_protocol::{DiagnosticEntry, DiagnosticLevel};

const MAX_LOG_BYTES: u64 = 1024 * 1024;
const MAX_ARCHIVES: usize = 3;
pub const MAX_DIAGNOSTIC_RESULTS: usize = 200;
const MAX_CODE_CHARACTERS: usize = 96;

#[derive(Debug, Clone)]
pub struct DiagnosticLog {
    path: PathBuf,
    write_lock: Arc<Mutex<()>>,
}

impl DiagnosticLog {
    #[must_use]
    pub fn next_to_config(config_path: &Path) -> Self {
        let parent = config_path.parent().unwrap_or_else(|| Path::new("."));
        Self {
            path: parent.join("Logs").join("service.jsonl"),
            write_lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn record(
        &self,
        level: DiagnosticLevel,
        event_id: &'static str,
        message: &'static str,
        code: Option<&str>,
    ) -> io::Result<()> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let entry = DiagnosticEntry {
            timestamp: Utc::now(),
            level,
            event_id: event_id.to_owned(),
            message: message.to_owned(),
            code: code.and_then(sanitize_code),
        };
        let mut encoded = serde_json::to_vec(&entry).map_err(io::Error::other)?;
        encoded.push(b'\n');
        fs::create_dir_all(
            self.path
                .parent()
                .expect("diagnostic log always has a parent"),
        )?;
        let current_size = fs::metadata(&self.path).map_or(0, |metadata| metadata.len());
        if current_size.saturating_add(encoded.len() as u64) > MAX_LOG_BYTES {
            self.rotate()?;
        }
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?
            .write_all(&encoded)
    }

    pub fn recent(&self, limit: usize) -> io::Result<Vec<DiagnosticEntry>> {
        let _guard = self
            .write_lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let mut entries = Vec::new();
        for index in (1..=MAX_ARCHIVES).rev() {
            read_entries(&archive_path(&self.path, index), &mut entries)?;
        }
        read_entries(&self.path, &mut entries)?;
        let start = entries.len().saturating_sub(limit);
        Ok(entries.split_off(start))
    }

    fn rotate(&self) -> io::Result<()> {
        let oldest = archive_path(&self.path, MAX_ARCHIVES);
        match fs::remove_file(oldest) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        for index in (1..MAX_ARCHIVES).rev() {
            let source = archive_path(&self.path, index);
            let destination = archive_path(&self.path, index + 1);
            match fs::rename(source, destination) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        match fs::rename(&self.path, archive_path(&self.path, 1)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn archive_path(path: &Path, index: usize) -> PathBuf {
    path.with_extension(format!("jsonl.{index}"))
}

fn read_entries(path: &Path, entries: &mut Vec<DiagnosticEntry>) -> io::Result<()> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    for line in BufReader::new(file).lines() {
        let line = line?;
        if let Ok(entry) = serde_json::from_str::<DiagnosticEntry>(&line) {
            entries.push(entry);
        }
    }
    Ok(())
}

fn sanitize_code(value: &str) -> Option<String> {
    if value.is_empty()
        || value.len() > MAX_CODE_CHARACTERS
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"_-.".contains(&byte)
        })
    {
        return Some("unclassified_error".to_owned());
    }
    Some(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_is_structured_bounded_and_sanitizes_codes() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let log = DiagnosticLog::next_to_config(&directory.path().join("config.toml"));
        log.record(
            DiagnosticLevel::Error,
            "backup.failed",
            "Backup failed.",
            Some("Access denied: C:\\Users\\Secret"),
        )
        .expect("diagnostic entry");

        let entries = log.recent(10).expect("recent entries");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].event_id, "backup.failed");
        assert_eq!(entries[0].code.as_deref(), Some("unclassified_error"));
        let serialized = serde_json::to_string(&entries).expect("serialize entries");
        assert!(!serialized.contains(r"C:\\Users\\Secret"));
    }

    #[test]
    fn recent_results_obey_the_requested_limit() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let log = DiagnosticLog::next_to_config(&directory.path().join("config.toml"));
        for _ in 0..5 {
            log.record(
                DiagnosticLevel::Information,
                "service.test",
                "Test event.",
                None,
            )
            .expect("diagnostic entry");
        }

        assert_eq!(log.recent(2).expect("recent entries").len(), 2);
    }
}
