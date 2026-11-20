//! Every statement that read or wrote rows, once per statement and file, as a
//! JSON line in `<dir>/<pid>.jsonl`, with whether it walked a whole table or
//! index as it ran. Only a test sets the folder. Whether a statement would
//! scan is read from its plan afterwards (`tests/fixtures/sql_scans.py`): a
//! scan of an empty table walks nothing, so what ran depends on the order
//! steps ran in, and the plan does not.

use std::collections::HashSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

pub(crate) fn dir() -> Option<&'static Path> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        std::env::var_os(super::CENSUS_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    })
    .as_deref()
}

pub(crate) fn note(dir: &Path, sql: &str, file: &str, scanned: bool) {
    static NOTED: OnceLock<Mutex<HashSet<(String, String, bool)>>> = OnceLock::new();
    let key = (sql.to_string(), file.to_string(), scanned);
    let new = NOTED
        .get_or_init(Default::default)
        .lock()
        .map(|mut noted| noted.insert(key))
        .unwrap_or(false);
    if !new {
        return;
    }
    let line = serde_json::json!({ "statement": sql, "file": file, "scanned": scanned })
        .to_string()
        + "\n";
    let path = dir.join(format!("{}.jsonl", std::process::id()));
    let wrote = std::fs::create_dir_all(dir).and_then(|()| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?
            .write_all(line.as_bytes())
    });
    if let Err(e) = wrote {
        tracing::error!(
            target: super::TARGET,
            path = %path.display(),
            "could not record a statement in the census: {e}"
        );
    }
}
