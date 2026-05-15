//! Per-entity append-only event store shared by the provider downloaders.
//!
//! Layout: `<out_dir>/<entity>/<stream>/events.jsonl` where `stream` is
//! `created` (first-sightings) or `updated` (first-sighting + every change).
//! Tail `updated` to get the latest snapshot per key.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Local;
use serde_json::{Map, Value};

pub fn events_path(out_dir: &Path, entity: &str, stream: &str) -> PathBuf {
    out_dir.join(entity).join(stream).join("events.jsonl")
}

pub fn append_jsonl(path: &Path, records: &[Value]) -> Result<()> {
    if records.is_empty() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let mut f = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("open {}", path.display()))?;
    for r in records {
        let line = serde_json::to_string(r)?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
    }
    Ok(())
}

pub fn now_iso() -> String {
    Local::now().to_rfc3339()
}

/// Wrap a provider payload with its denormalized key + a recorded-at stamp.
/// `key_fields` is spread alongside `raw` so the JSONL is greppable.
pub fn make_record(key_fields: &[(&str, Value)], raw: Value) -> Value {
    let mut m = Map::new();
    m.insert("_recorded_at".to_string(), Value::String(now_iso()));
    for (k, v) in key_fields {
        m.insert((*k).to_string(), v.clone());
    }
    m.insert("raw".to_string(), raw);
    Value::Object(m)
}

/// Walks `created/` then `updated/` so updated/ entries shadow earlier ones.
/// Returns a map keyed by the user-supplied key string.
pub fn load_latest_by_key<F>(
    out_dir: &Path,
    entity: &str,
    key_of: F,
) -> Result<BTreeMap<String, Value>>
where
    F: Fn(&Value) -> String,
{
    let mut latest: BTreeMap<String, Value> = BTreeMap::new();
    for stream in ["created", "updated"] {
        let path = events_path(out_dir, entity, stream);
        if !path.exists() {
            continue;
        }
        let f = std::fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let v: Value =
                serde_json::from_str(&line).with_context(|| format!("parse {}", path.display()))?;
            let k = key_of(&v);
            latest.insert(k, v);
        }
    }
    Ok(latest)
}

/// Append new records to `created/` and (new + changed) records to `updated/`.
/// Returns `(new_count, updated_count)`.
pub fn diff_and_save<F>(
    out_dir: &Path,
    entity: &str,
    fresh: &[Value],
    existing_by_key: &BTreeMap<String, Value>,
    key_of: F,
) -> Result<(usize, usize)>
where
    F: Fn(&Value) -> String,
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
    let combined: Vec<Value> = new_records
        .iter()
        .chain(updated_records.iter())
        .cloned()
        .collect();
    append_jsonl(&events_path(out_dir, entity, "updated"), &combined)?;
    Ok((new_records.len(), updated_records.len()))
}
