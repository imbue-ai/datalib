//! Append-only JSONL mirror of every upsert that hits the raw store.
//!
//! See `docs/dev/data_architecture_ingestion.md` § "Wire-event tape (JSONL)" for the
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

use crate::bulk::EventBatch;

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
        let row = [(id, payload)];
        self.append_batch(&EventBatch { table, rows: &row })
    }

    /// Bulk-append one [`EventBatch`] to its table's tape file. Opens
    /// the file once, writes every line, flushes once at the end. Same
    /// `EventBatch` shape the [`crate::doltlite_raw::bulk_upsert_events`]
    /// chokepoint uses on the bookkeeping side, so a chokepoint can
    /// hand the same batch through both.
    pub fn append_batch(&self, batch: &EventBatch<'_>) -> Result<()> {
        let mut writers = self.writers.lock().expect("event tape mutex poisoned");
        let w = match writers.get_mut(batch.table) {
            Some(w) => w,
            None => {
                std::fs::create_dir_all(&self.dir)
                    .with_context(|| format!("mkdir {}", self.dir.display()))?;
                let path = self.dir.join(format!("{}.jsonl", batch.table));
                let f = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&path)
                    .with_context(|| format!("open {}", path.display()))?;
                writers.insert(batch.table.to_string(), BufWriter::new(f));
                writers.get_mut(batch.table).expect("just inserted")
            }
        };
        let now = IsoOffsetTimestamp::now_local().to_rfc3339_micros();
        for (id, payload) in batch.rows {
            let line = json!({
                "_recorded_at": &now,
                "table": batch.table,
                "id": id,
                "payload": payload,
            });
            let mut text = serde_json::to_string(&line).context("serialize event tape line")?;
            text.push('\n');
            w.write_all(text.as_bytes())
                .with_context(|| format!("append to tape {}", batch.table))?;
        }
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

    #[test]
    fn append_batch_writes_one_line_per_row() {
        let d = tempdir().unwrap();
        let tape = EventTape::new(d.path().to_path_buf());
        let p1 = json!({"text": "hi"});
        let p2 = json!({"text": "bye"});
        let p3 = json!({"text": "later"});
        let rows = [("C1:1.0", &p1), ("C1:2.0", &p2), ("C1:3.0", &p3)];
        tape.append_batch(&EventBatch {
            table: "messages",
            rows: &rows,
        })
        .unwrap();
        let m = std::fs::read_to_string(d.path().join("messages.jsonl")).unwrap();
        assert_eq!(m.lines().count(), 3);
    }
}
