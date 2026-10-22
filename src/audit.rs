//! Append-only JSON Lines audit file.
//!
//! Every record opens the file afresh, writes one complete line with a single
//! `write_all`, and closes it again. Holding no handle means an external
//! `logrotate` that renames the file needs no signal: the next record simply
//! creates a new file at the configured path. One `write_all` per line keeps
//! a concurrent rename from splitting a record across two files.

use std::{
    fs::OpenOptions,
    io::{self, Write},
    path::{Path, PathBuf},
};

use serde::Serialize;

#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// Checks that `path` can be appended to (creating it with mode 0600 if
    /// missing), so a bad path fails at startup rather than on first request.
    pub fn open(path: &Path) -> io::Result<Self> {
        let log = AuditLog {
            path: path.to_owned(),
        };
        log.open_file()?;
        Ok(log)
    }

    /// Appends `record` as one JSON line.
    pub fn append<T: Serialize>(&self, record: &T) -> io::Result<()> {
        let mut line = serde_json::to_vec(record)?;
        line.push(b'\n');
        self.open_file()?.write_all(&line)
    }

    fn open_file(&self) -> io::Result<std::fs::File> {
        let mut options = OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
        options.open(&self.path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn lines(path: &Path) -> Vec<Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn each_record_is_one_parseable_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(&path).unwrap();
        log.append(&json!({"n": 1, "s": "a\nb"})).unwrap();
        log.append(&json!({"n": 2})).unwrap();
        assert_eq!(
            lines(&path),
            vec![json!({"n":1,"s":"a\nb"}), json!({"n":2})]
        );
    }

    #[test]
    fn record_after_rename_lands_in_a_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let rotated = dir.path().join("audit.jsonl.1");
        let log = AuditLog::open(&path).unwrap();
        log.append(&json!({"n": 1})).unwrap();
        std::fs::rename(&path, &rotated).unwrap();
        log.append(&json!({"n": 2})).unwrap();
        assert_eq!(lines(&rotated), vec![json!({"n": 1})]);
        assert_eq!(lines(&path), vec![json!({"n": 2})]);
    }

    #[cfg(unix)]
    #[test]
    fn new_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        AuditLog::open(&path).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn unwritable_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(AuditLog::open(dir.path()).is_err());
        assert!(AuditLog::open(&dir.path().join("missing/audit.jsonl")).is_err());
    }
}
