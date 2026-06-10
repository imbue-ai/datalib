//! Append-only JSONL mirror of every upsert that hits the raw store.
//!
//! See `docs/data_architecture.md` § "Wire-event tape (JSONL)" for the
//! principle. This module is the plumbing: a small handle that owns a
//! directory and lazily opens one append-mode file per entity table.
//!
//! The pipeline never reads from these files. They exist so a human
//! (or `tail -f`, `grep`, `jq`) can watch the wire payload come off
//! the upstream without opening doltlite.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use frankweiler_time::IsoOffsetTimestamp;
use serde_json::{json, Value};

/// One source's tape. Cheap to clone via `Arc` if you need to share it
/// across tasks; the inner state is `Mutex`-guarded.
#[derive(Debug)]
pub struct EventTape {
    dir: PathBuf,
    writers: Mutex<HashMap<String, BufWriter<File>>>,
}

impl EventTape {
    /// Create (or attach to) a tape at `<dir>/`. Files appear under
    /// `<dir>/<table>.jsonl` on first append.
    pub fn new(dir: PathBuf) -> Self {
        Self {
            dir,
            writers: Mutex::new(HashMap::new()),
        }
    }

    /// The directory we write into. Useful for tests and for logging.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Append one line for the given (table, id) and payload. The line
    /// shape is `{"_recorded_at", "table", "id", "payload"}`. Flushed
    /// after every line so a `tail -f` watcher sees rows promptly.
    pub fn append(&self, table: &str, id: &str, payload: &Value) -> Result<()> {
        let line = json!({
            "_recorded_at": IsoOffsetTimestamp::now_local().to_rfc3339_micros(),
            "table": table,
            "id": id,
            "payload": payload,
        });
        let mut text = serde_json::to_string(&line).context("serialize event tape line")?;
        text.push('\n');

        let mut writers = self.writers.lock().expect("event tape mutex poisoned");
        let w = match writers.get_mut(table) {
            Some(w) => w,
            None => {
                std::fs::create_dir_all(&self.dir)
                    .with_context(|| format!("mkdir {}", self.dir.display()))?;
                let path = self.dir.join(format!("{table}.jsonl"));
                let f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("open {}", path.display()))?;
                writers.insert(table.to_string(), BufWriter::new(f));
                writers.get_mut(table).expect("just inserted")
            }
        };
        w.write_all(text.as_bytes())
            .with_context(|| format!("append to tape {}", table))?;
        w.flush().context("flush event tape")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn append_creates_file_and_writes_lines() {
        let d = tempdir().unwrap();
        let tape = EventTape::new(d.path().to_path_buf());
        tape.append("messages", "C1:1.0", &json!({"text": "hi"}))
            .unwrap();
        tape.append("messages", "C1:2.0", &json!({"text": "bye"}))
            .unwrap();
        tape.append("users", "U1", &json!({"name": "picard"}))
            .unwrap();

        let m = std::fs::read_to_string(d.path().join("messages.jsonl")).unwrap();
        assert_eq!(m.lines().count(), 2);
        let first: Value = serde_json::from_str(m.lines().next().unwrap()).unwrap();
        assert_eq!(first["table"], "messages");
        assert_eq!(first["id"], "C1:1.0");
        assert_eq!(first["payload"]["text"], "hi");
        assert!(first["_recorded_at"].is_string());

        let u = std::fs::read_to_string(d.path().join("users.jsonl")).unwrap();
        assert_eq!(u.lines().count(), 1);
    }
}
