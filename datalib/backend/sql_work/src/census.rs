//! Every statement that walked a whole table or index, once per statement
//! and file, as a JSON line in `<dir>/<pid>.jsonl`. Only a test sets the
//! folder; on a fixture's few rows a scan costs nothing, so every scan is
//! recorded, and the list is what a later, larger store will pay for.

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

pub(crate) fn note(dir: &Path, sql: &str, file: &str) {
    static NOTED: OnceLock<Mutex<HashSet<(String, String)>>> = OnceLock::new();
    let key = (sql.to_string(), file.to_string());
    let new = NOTED
        .get_or_init(Default::default)
        .lock()
        .map(|mut noted| noted.insert(key))
        .unwrap_or(false);
    if !new {
        return;
    }
    let line = serde_json::json!({ "statement": sql, "file": file }).to_string() + "\n";
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
            "could not record a scan in the census: {e}"
        );
    }
}
