//! Append-only raw-API capture with per-item content dedup.
//!
//! Layout: `<out_dir>/raw_api/<method>/<run_id>.jsonl`. Each fetch
//! invocation gets its own JSONL file per method (lazy-created on first
//! write, so empty runs leave no trace). The directory itself is
//! append-only; files are immutable once a run finishes, which keeps
//! Dropbox/rsync happy and makes "this run produced X" / "translator
//! already consumed file Y" trivial to track downstream. Files can be
//! repacked offline if their count ever becomes inconvenient.
//!
//! Each record wraps one upstream call:
//!
//! ```json
//! {"_recorded_at": "...", "method": "conversations.history",
//!  "params": {...}, "duration_ms": 421,
//!  "_item_hashes": {"<channel>\t<ts>": 1234567890, ...},
//!  "response": {...}}
//! ```
//!
//! On save, the caller hands us a `(item_key, item_value)` list extracted
//! from the response. We hash each item; if every hash matches what's
//! already in the dedup index for that `(method, item_key)`, the page is
//! a no-op and we skip the append. Otherwise the envelope is appended
//! verbatim — with the computed hashes inlined as `_item_hashes` — and
//! the index updated.
//!
//! The inlined hashes turn startup into a linear scan of small JSON maps:
//! [`RawStore::load`] streams the JSONL and copies each envelope's
//! `_item_hashes` directly into the index. No re-extraction of items, no
//! rehashing of response bodies. The raw stream is still the single
//! source of truth — the hashes are derived from `response` and would be
//! reproducible — but storing them inline saves an order of magnitude of
//! work on every startup.
//!
//! Dedup is keyed `(method, item_key)`. The `item_key` namespace is the
//! caller's: e.g. `conversations.history` uses `<channel>\t<ts>`,
//! `users.list` uses `<user_id>`. See `slack::shapes`.
//!
//! See [`canonical_hash`] for the JSON normalization rule — `serde_json`'s
//! default `Map` is a `BTreeMap` (alphabetical key order), so a plain
//! `to_string` is already canonical, but we re-derive the rule explicitly
//! in case `preserve_order` ever gets flipped on.

use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Local;
use serde_json::{Map, Value};

pub type ItemHash = u64;

/// One captured API call, as handed to [`RawStore::save_page`].
pub struct PageCapture<'a> {
    pub method: &'a str,
    pub params: &'a BTreeMap<String, String>,
    pub duration_ms: u64,
    pub response: Value,
    /// `(item_key, item_value)` extracted from `response` by the caller.
    /// If empty, the page is always written (e.g. `auth.test` with a
    /// single top-level key handled by passing the response as the value).
    pub items: Vec<(String, Value)>,
}

pub struct RawStore {
    out_dir: PathBuf,
    /// Stamp shared across all per-method JSONL files written by this
    /// invocation. Filename: `<run_id>.jsonl`.
    run_id: String,
    /// `(method, item_key) -> hash(item_value)`. Sized linearly with the
    /// number of unique items ever seen across all methods.
    seen: BTreeMap<(String, String), ItemHash>,
}

impl RawStore {
    /// Load existing `raw_api/` streams, populating the dedup index from
    /// each envelope's inlined `_item_hashes` map. Glob-fans-in every
    /// `*.jsonl` per method, so prior runs accumulate naturally.
    pub fn load(out_dir: &Path) -> Result<Self> {
        let mut store = Self {
            out_dir: out_dir.to_path_buf(),
            run_id: make_run_id(),
            seen: BTreeMap::new(),
        };
        let raw_root = out_dir.join("raw_api");
        if !raw_root.exists() {
            return Ok(store);
        }
        for method_entry in std::fs::read_dir(&raw_root)
            .with_context(|| format!("read_dir {}", raw_root.display()))?
        {
            let method_entry = method_entry?;
            if !method_entry.file_type()?.is_dir() {
                continue;
            }
            let method = method_entry.file_name().to_string_lossy().to_string();
            let method_dir = method_entry.path();
            let mut files: Vec<PathBuf> = std::fs::read_dir(&method_dir)
                .with_context(|| format!("read_dir {}", method_dir.display()))?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
                .collect();
            files.sort();
            for path in files {
                let f = std::fs::File::open(&path)
                    .with_context(|| format!("open {}", path.display()))?;
                for line in BufReader::new(f).lines() {
                    let line = line?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    let env: Value = serde_json::from_str(&line)
                        .with_context(|| format!("parse {}", path.display()))?;
                    let hashes = env
                        .get("_item_hashes")
                        .and_then(|v| v.as_object())
                        .with_context(|| {
                            format!("{}: envelope missing _item_hashes", path.display())
                        })?;
                    for (k, v) in hashes {
                        if let Some(h) = v.as_u64() {
                            store.seen.insert((method.clone(), k.clone()), h);
                        }
                    }
                }
            }
        }
        Ok(store)
    }

    /// Stamp identifying this run's output files. One per invocation,
    /// shared across all methods.
    pub fn run_id(&self) -> &str {
        &self.run_id
    }

    /// Number of unique items currently indexed for `method`. Useful for
    /// observability.
    pub fn seen_count(&self, method: &str) -> usize {
        self.seen
            .range((method.to_string(), String::new())..)
            .take_while(|((m, _), _)| m == method)
            .count()
    }

    /// True iff every item in `cap.items` already has a matching hash in
    /// the index. An empty `cap.items` is treated as "not a no-op" — the
    /// caller is asserting there's nothing item-shaped to dedup against,
    /// so the page should always be written.
    fn page_is_noop(&self, method: &str, items: &[(String, ItemHash)]) -> bool {
        if items.is_empty() {
            return false;
        }
        for (k, h) in items {
            let prior = self.seen.get(&(method.to_string(), k.clone()));
            match prior {
                Some(p) if *p == *h => {}
                _ => return false,
            }
        }
        true
    }

    /// Append `cap` to `raw_api/<method>/events.jsonl` unless every item
    /// inside is a content-match for what we already have. Returns the
    /// `(new_items, changed_items)` counts (both zero on no-op).
    pub fn save_page(&mut self, cap: PageCapture<'_>) -> Result<(usize, usize)> {
        let method = cap.method.to_string();
        let hashed: Vec<(String, ItemHash)> = cap
            .items
            .iter()
            .map(|(k, v)| (k.clone(), canonical_hash(v)))
            .collect();
        if self.page_is_noop(&method, &hashed) {
            return Ok((0, 0));
        }

        let (mut new_count, mut changed_count) = (0, 0);
        let mut hash_map = Map::new();
        for (k, h) in &hashed {
            match self.seen.insert((method.clone(), k.clone()), *h) {
                None => new_count += 1,
                Some(prior) if prior != *h => changed_count += 1,
                _ => {}
            }
            hash_map.insert(k.clone(), Value::from(*h));
        }

        let mut env = Map::new();
        env.insert("_recorded_at".to_string(), Value::String(now_iso()));
        env.insert("method".to_string(), Value::String(method.clone()));
        env.insert("params".to_string(), params_to_value(cap.params));
        env.insert("duration_ms".to_string(), Value::from(cap.duration_ms));
        env.insert("_item_hashes".to_string(), Value::Object(hash_map));
        env.insert("response".to_string(), cap.response);

        let path = self
            .out_dir
            .join("raw_api")
            .join(&method)
            .join(format!("{}.jsonl", self.run_id));
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("mkdir -p {}", parent.display()))?;
        }
        let mut f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open {}", path.display()))?;
        let line = serde_json::to_string(&Value::Object(env))?;
        f.write_all(line.as_bytes())?;
        f.write_all(b"\n")?;
        Ok((new_count, changed_count))
    }

    /// Iterate item keys we've seen for `method`. Cheap — just walks the
    /// in-memory index range. Callers build per-method projections (e.g.
    /// max ts per channel) on top of this.
    pub fn keys_for<'a>(&'a self, method: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.seen
            .range((method.to_string(), String::new())..)
            .take_while(move |((m, _), _)| m == method)
            .map(|((_, k), _)| k.as_str())
    }
}

fn params_to_value(params: &BTreeMap<String, String>) -> Value {
    let mut m = Map::new();
    for (k, v) in params {
        m.insert(k.clone(), Value::String(v.clone()));
    }
    Value::Object(m)
}

fn now_iso() -> String {
    frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339()
}

/// Sortable, filesystem-safe run stamp. Millisecond precision so two
/// runs back-to-back don't collide.
fn make_run_id() -> String {
    Local::now().format("run-%Y%m%dT%H%M%S%3f").to_string()
}

/// Hash a JSON value canonically. `serde_json::Map` is a `BTreeMap` in
/// our build (no `preserve_order` feature), so `to_string` is already
/// alphabetically keyed at every level — we just feed it through
/// `DefaultHasher`. 64 bits of SipHash is fine for our scale.
fn canonical_hash(v: &Value) -> ItemHash {
    let s = serde_json::to_string(v).expect("serializable json");
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}
