//! Per-entity append-only event store shared by provider downloaders
//! that mirror many entities (notion, github, gitlab).
//!
//! Layout:
//! ```text
//! <out_dir>/<entity>/<stream>/events.jsonl
//! ```
//! where `stream` is either:
//!
//! - `created` — append-only first-sightings of each key.
//! - `updated` — every first-sighting plus every subsequent change
//!   (so tailing `updated` yields the latest snapshot per key).
//!
//! Records are JSON objects with a `_recorded_at` ISO-8601 stamp, the
//! caller's denormalized key fields spread at the top level (so the
//! files are `grep`-pable without `jq`), and a nested `raw` carrying
//! the full upstream payload.
//!
//! Port of `src/event_store.py`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Local;
use serde::Serialize;
use serde_json::{Map, Value};

/// Path to the events.jsonl for one (entity, stream) pair.
pub fn events_path(out_dir: &Path, entity: &str, stream: &str) -> PathBuf {
    out_dir.join(entity).join(stream).join("events.jsonl")
}

/// Append a batch of records to `path`. Creates parent dirs as needed.
/// No-op if `records` is empty.
pub fn append_jsonl(path: &Path, records: &[Value]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create dir {}", parent.display()))?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    for r in records {
        let line = serde_json::to_string(r).context("serialize event record")?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
    }
    Ok(())
}

/// Current local-time ISO-8601 with offset, matching Python's
/// `datetime.now().astimezone().isoformat()` shape.
pub fn now_iso() -> String {
    Local::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false)
}

/// Wrap an upstream payload with its denormalized key + a `_recorded_at`
/// stamp. Key fields are spread at the top level alongside `raw`.
pub fn make_record(key: Map<String, Value>, raw: Value) -> Value {
    let mut obj = Map::new();
    obj.insert("_recorded_at".into(), Value::String(now_iso()));
    for (k, v) in key {
        obj.insert(k, v);
    }
    obj.insert("raw".into(), raw);
    Value::Object(obj)
}

/// Result of one `diff_and_save` call.
#[derive(Debug, Default, Serialize, Clone, Copy, PartialEq, Eq)]
pub struct DiffCounts {
    pub new: usize,
    pub updated: usize,
}

/// Append new records to `created/` and (new + changed) to `updated/`.
///
/// `key_of` extracts the dedup key from each fresh record (typically by
/// reading a top-level field). `existing_by_key` is the snapshot returned
/// from a prior `load_latest_by_key` call.
pub fn diff_and_save<F>(
    out_dir: &Path,
    entity: &str,
    fresh: &[Value],
    existing_by_key: &HashMap<String, Value>,
    mut key_of: F,
) -> Result<DiffCounts>
where
    F: FnMut(&Value) -> String,
{
    let mut new_records: Vec<Value> = Vec::new();
    let mut updated_records: Vec<Value> = Vec::new();
    for rec in fresh {
        let k = key_of(rec);
        match existing_by_key.get(&k) {
            None => new_records.push(rec.clone()),
            Some(prior) => {
                if prior.get("raw") != rec.get("raw") {
                    updated_records.push(rec.clone());
                }
            }
        }
    }
    append_jsonl(&events_path(out_dir, entity, "created"), &new_records)?;
    let mut combined = new_records.clone();
    combined.extend(updated_records.iter().cloned());
    append_jsonl(&events_path(out_dir, entity, "updated"), &combined)?;
    Ok(DiffCounts {
        new: new_records.len(),
        updated: updated_records.len(),
    })
}

/// Walk `created/` then `updated/`, returning the most recent record
/// keyed by `key_of`. `updated/` entries shadow `created/` entries for
/// the same key.
pub fn load_latest_by_key<F>(
    out_dir: &Path,
    entity: &str,
    mut key_of: F,
) -> Result<HashMap<String, Value>>
where
    F: FnMut(&Value) -> String,
{
    let mut latest: HashMap<String, Value> = HashMap::new();
    for stream in ["created", "updated"] {
        let path = events_path(out_dir, entity, stream);
        if !path.exists() {
            continue;
        }
        let f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        let reader = BufReader::new(f);
        for (lineno, line) in reader.lines().enumerate() {
            let line = line.with_context(|| format!("read {}:{}", path.display(), lineno + 1))?;
            if line.trim().is_empty() {
                continue;
            }
            let rec: Value = serde_json::from_str(&line)
                .with_context(|| format!("parse {}:{}", path.display(), lineno + 1))?;
            latest.insert(key_of(&rec), rec);
        }
    }
    Ok(latest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::tempdir;

    fn key_id(v: &Value) -> String {
        v.get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string()
    }

    #[test]
    fn make_record_spreads_key_alongside_raw() {
        let mut k = Map::new();
        k.insert("id".into(), Value::String("abc".into()));
        let rec = make_record(k, json!({"hello": "world"}));
        assert_eq!(rec["id"], "abc");
        assert_eq!(rec["raw"]["hello"], "world");
        assert!(rec["_recorded_at"].is_string());
    }

    #[test]
    fn diff_and_save_appends_created_and_updated_streams() {
        let dir = tempdir().unwrap();
        let out = dir.path();
        // Round 1: two brand-new records.
        let mut k1 = Map::new();
        k1.insert("id".into(), Value::String("p1".into()));
        let mut k2 = Map::new();
        k2.insert("id".into(), Value::String("p2".into()));
        let r1 = make_record(k1.clone(), json!({"title": "a"}));
        let r2 = make_record(k2.clone(), json!({"title": "b"}));
        let counts = diff_and_save(
            out,
            "ent",
            &[r1.clone(), r2.clone()],
            &HashMap::new(),
            key_id,
        )
        .unwrap();
        assert_eq!(counts.new, 2);
        assert_eq!(counts.updated, 0);
        // Round 2: same p1, changed p2.
        let mut existing: HashMap<String, Value> = HashMap::new();
        existing.insert("p1".into(), r1.clone());
        existing.insert("p2".into(), r2.clone());
        let r2b = make_record(k2.clone(), json!({"title": "b2"}));
        let counts =
            diff_and_save(out, "ent", &[r1.clone(), r2b.clone()], &existing, key_id).unwrap();
        assert_eq!(counts.new, 0);
        assert_eq!(counts.updated, 1);
        // Walk back via load_latest_by_key — p2 must be the updated version.
        let latest = load_latest_by_key(out, "ent", key_id).unwrap();
        assert_eq!(latest["p2"]["raw"]["title"], "b2");
        assert_eq!(latest["p1"]["raw"]["title"], "a");
        // created/ should have 2 lines (round 1 only), updated/ should have 3.
        let created = std::fs::read_to_string(events_path(out, "ent", "created")).unwrap();
        let updated = std::fs::read_to_string(events_path(out, "ent", "updated")).unwrap();
        assert_eq!(created.lines().count(), 2);
        assert_eq!(updated.lines().count(), 3);
    }
}
