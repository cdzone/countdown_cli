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

/// Cap, in characters, for caller-supplied strings copied into audit lines
/// (caller id, title, path segments, on_behalf_of, …).
pub const MAX_AUDIT_STR: usize = 64;

/// Cap for operator-facing `detail` (error chains, serde paths). Longer than
/// [`MAX_AUDIT_STR`] so a URL or title cannot eat the whole budget.
pub const MAX_AUDIT_DETAIL: usize = 512;

/// The first `max` characters of `s`; cuts on char boundaries, so multi-byte
/// UTF-8 is never split.
pub fn truncated_at(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// The first [`MAX_AUDIT_STR`] characters of `s`.
pub fn truncated(s: &str) -> String {
    truncated_at(s, MAX_AUDIT_STR)
}

/// The first [`MAX_AUDIT_DETAIL`] characters of `s`.
pub fn truncated_detail(s: &str) -> String {
    truncated_at(s, MAX_AUDIT_DETAIL)
}

/// `Display` of `err` plus each `source()`, joined with `": "`.
pub fn error_chain(err: &dyn std::error::Error) -> String {
    let mut parts = vec![err.to_string()];
    let mut cur = err.source();
    while let Some(e) = cur {
        parts.push(e.to_string());
        cur = e.source();
    }
    parts.join(": ")
}

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

    #[test]
    fn truncated_keeps_first_64_chars_on_char_boundaries() {
        assert_eq!(truncated("short"), "short");
        let ascii = "a".repeat(100);
        assert_eq!(truncated(&ascii), "a".repeat(64));
        let wide = "倒".repeat(70);
        let cut = truncated(&wide);
        assert_eq!(cut.chars().count(), 64);
        assert_eq!(cut, "倒".repeat(64));
    }

    #[test]
    fn truncated_detail_allows_512_chars() {
        let ascii = "b".repeat(600);
        assert_eq!(truncated_detail(&ascii).chars().count(), MAX_AUDIT_DETAIL);
        assert_eq!(truncated_at("xy", 1), "x");
    }

    #[test]
    fn error_chain_joins_sources() {
        #[derive(Debug)]
        struct Leaf;
        impl std::fmt::Display for Leaf {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "leaf-reason")
            }
        }
        impl std::error::Error for Leaf {}

        #[derive(Debug)]
        struct Wrap(Leaf);
        impl std::fmt::Display for Wrap {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "wrap")
            }
        }
        impl std::error::Error for Wrap {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        assert_eq!(error_chain(&Wrap(Leaf)), "wrap: leaf-reason");
    }
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
