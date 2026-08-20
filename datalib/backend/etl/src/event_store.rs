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

/// Current local-time ISO-8601 with explicit offset, matching Python's
/// `datetime.now().astimezone().isoformat()` shape. Funnels through
/// `datalib-time` so the local-offset policy lives in one place.
pub fn now_iso() -> String {
    datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339_micros()
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
///
/// # Order is part of the contract
///
/// Returns a `Vec` in **first-seen order** — the order records appear in
/// `created/`, with an `updated/` record replacing its predecessor *in
/// place* rather than moving it to the end. For an append-only event
/// stream that is document order, which is what a synthesizer replaying
/// a listing endpoint has to reproduce.
///
/// This used to return a `HashMap`, and the ordering was silently
/// whatever Rust's per-process hash seed produced. It cost the notion
/// fixture its reproducibility: the synthesizer packs these records into
/// `results` arrays, so the replayed `/children` listing came back
/// shuffled, the downloader's BFS assigned different `blocks.page_order`
/// values every run, and the rendered markdown emitted the same blocks
/// in a different order each time — visible in the preview pane and in
/// what qmd indexes. Found 2026-08-20 by diffing two fixture builds.
///
/// A `Vec` rather than a `BTreeMap` because sorting by key is *not* the
/// same as document order and would silently reshape the page: in
/// `tests/fixtures/notion_web` the block ids diverge from file order at
/// index 34. Callers that want a lookup table should build one; callers
/// that want a canonical order should sort explicitly.
///
/// # An unkeyable record is an error, not a skip
///
/// Every `key_of` in this tree is built from `unwrap_or_default()` over
/// a few field lookups, so a record whose fields don't match what the
/// key function expects yields `""`. Tolerating that loses data twice:
/// every unkeyable record collapses onto the same `""` entry, and
/// callers then skip the empty key — so a whole entity stream reads as
/// "no records", the synthesizer writes no fixtures, the downloader
/// replays an empty listing, nothing renders, and every step reports
/// success.
///
/// That is not hypothetical. `tests/fixtures/gitlab_api` spelled the
/// project path `project_path` while every consumer had moved to
/// `project_full_path`; gitlab contributed zero rows to the fixture
/// pipeline for three months without one failing test. The error below
/// names the file, the line, and the fields the record actually has,
/// which is enough to spot a renamed field on sight.
pub fn load_latest_by_key<F>(
    out_dir: &Path,
    entity: &str,
    mut key_of: F,
) -> Result<Vec<(String, Value)>>
where
    F: FnMut(&Value) -> String,
{
    // `latest` holds the records in first-seen order; `at` maps a key to
    // its slot so an `updated/` record overwrites in place.
    let mut latest: Vec<(String, Value)> = Vec::new();
    let mut at: HashMap<String, usize> = HashMap::new();
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
            let key = key_of(&rec);
            if key.is_empty() {
                let fields = rec
                    .as_object()
                    .map(|m| m.keys().cloned().collect::<Vec<_>>().join(", "))
                    .unwrap_or_else(|| "<not a JSON object>".to_string());
                anyhow::bail!(
                    "{}:{}: could not derive a key for entity {entity:?} — the key \
                     function found none of the fields it needs. The record has: \
                     [{fields}]. A stale fixture whose field names predate a rename \
                     is the usual cause; left unchecked this silently yields an \
                     empty entity stream.",
                    path.display(),
                    lineno + 1,
                );
            }
            match at.get(&key) {
                Some(&i) => latest[i].1 = rec,
                None => {
                    at.insert(key.clone(), latest.len());
                    latest.push((key, rec));
                }
            }
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

    fn by_key<'a>(recs: &'a [(String, Value)], k: &str) -> &'a Value {
        &recs
            .iter()
            .find(|(key, _)| key == k)
            .expect("key present")
            .1
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
        assert_eq!(by_key(&latest, "p2")["raw"]["title"], "b2");
        assert_eq!(by_key(&latest, "p1")["raw"]["title"], "a");
        // ...and the update must not have moved p2 behind p1.
        assert_eq!(
            latest.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["p1", "p2"],
        );
        // created/ should have 2 lines (round 1 only), updated/ should have 3.
        let created = std::fs::read_to_string(events_path(out, "ent", "created")).unwrap();
        let updated = std::fs::read_to_string(events_path(out, "ent", "updated")).unwrap();
        assert_eq!(created.lines().count(), 2);
        assert_eq!(updated.lines().count(), 3);
    }

    /// `load_latest_by_key` must hand records back in the order the
    /// stream recorded them.
    ///
    /// This is a regression test with a specific bug behind it. The
    /// function returned a `HashMap`, so iteration order was whatever
    /// Rust's per-process hash seed produced. notion's synthesizer packs
    /// these records into `results` arrays, so its replayed `/children`
    /// listing came back shuffled, the downloader's BFS wrote different
    /// `blocks.page_order` values every run, and the rendered markdown
    /// emitted the same blocks in a different order each time.
    ///
    /// Twenty keys, in an order that is neither sorted nor reverse
    /// sorted: a `HashMap` reproducing this exact sequence by chance is
    /// a 1-in-20! event. Note the bug is invisible *within* one process
    /// — the seed is fixed per process, so a "render twice and compare"
    /// test would have passed. Asserting the order explicitly is what
    /// catches it.
    #[test]
    fn records_come_back_in_stream_order() {
        let dir = tempdir().unwrap();
        let out = dir.path();
        let ids: Vec<String> = [
            13, 7, 20, 1, 15, 4, 19, 8, 2, 11, 17, 5, 9, 14, 3, 18, 6, 12, 10, 16,
        ]
        .iter()
        .map(|n| format!("blk-{n:02}"))
        .collect();
        let recs: Vec<Value> = ids
            .iter()
            .map(|id| {
                let mut k = Map::new();
                k.insert("id".into(), Value::String(id.clone()));
                make_record(k, json!({"title": id}))
            })
            .collect();
        append_jsonl(&events_path(out, "ent", "created"), &recs).unwrap();

        let latest = load_latest_by_key(out, "ent", key_id).unwrap();
        assert_eq!(
            latest.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>(),
            ids,
            "records must come back in the order the stream recorded them",
        );
    }

    /// An `updated/` record replaces its predecessor **in place**.
    ///
    /// Appending it instead would reorder the document every time any
    /// one block was edited — a subtler version of the same bug, and one
    /// that only shows up on the second sync.
    #[test]
    fn an_update_does_not_move_its_record_to_the_end() {
        let dir = tempdir().unwrap();
        let out = dir.path();
        let mk = |id: &str, title: &str| {
            let mut k = Map::new();
            k.insert("id".into(), Value::String(id.into()));
            make_record(k, json!({"title": title}))
        };
        append_jsonl(
            &events_path(out, "ent", "created"),
            &[mk("a", "1"), mk("b", "1"), mk("c", "1")],
        )
        .unwrap();
        // `a` is edited later, so it appears in the `updated/` stream.
        append_jsonl(&events_path(out, "ent", "updated"), &[mk("a", "2")]).unwrap();

        let latest = load_latest_by_key(out, "ent", key_id).unwrap();
        assert_eq!(
            latest.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(),
            ["a", "b", "c"],
            "an updated record must keep its original position",
        );
        assert_eq!(by_key(&latest, "a")["raw"]["title"], "2");
    }

    /// A record the key function can't identify must fail loudly. This
    /// is the gitlab `project_path` / `project_full_path` bug in
    /// miniature: before this check the stream read as empty and the
    /// whole provider silently produced nothing.
    #[test]
    fn unkeyable_record_is_an_error_naming_its_fields() {
        let dir = tempdir().unwrap();
        let out = dir.path();
        let mut k = Map::new();
        // The writer spells it `renamed_id`; `key_id` looks for `id`.
        k.insert("renamed_id".into(), Value::String("p1".into()));
        let rec = make_record(k, json!({"title": "a"}));
        append_jsonl(&events_path(out, "ent", "created"), &[rec]).unwrap();

        let err = load_latest_by_key(out, "ent", key_id)
            .expect_err("an unkeyable record must not read as an empty stream");
        let msg = err.to_string();
        // The message has to be actionable on sight: which entity, and
        // what the record actually carries.
        assert!(msg.contains("\"ent\""), "should name the entity: {msg}");
        assert!(msg.contains("renamed_id"), "should list the fields: {msg}");
        assert!(msg.contains(":1:"), "should name the line: {msg}");
    }

    /// The guard must not fire on well-formed records.
    #[test]
    fn keyable_records_load_without_error() {
        let dir = tempdir().unwrap();
        let out = dir.path();
        let mut k = Map::new();
        k.insert("id".into(), Value::String("p1".into()));
        let rec = make_record(k, json!({"title": "a"}));
        append_jsonl(&events_path(out, "ent", "created"), &[rec]).unwrap();

        let latest = load_latest_by_key(out, "ent", key_id).unwrap();
        assert_eq!(by_key(&latest, "p1")["raw"]["title"], "a");
    }
}
