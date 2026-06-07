// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live end-to-end golden test for `frankweiler-sync`.
//!
//! Spawns the sync binary against `configs/thad_tiny.yaml` (with a few
//! test-only tweaks: tempdir `data_root`, `qmd.skip=true`, slack
//! `refresh_window_days=30`), hitting real provider APIs through
//! `latchkey curl`. Then snapshots the produced data tree, one
//! `.snap` per file under `tests/snapshots/`, mirroring the data layout:
//!
//!   tests/snapshots/
//!     manifest.snap                              ← list of paths
//!     raw/tiny-slack/raw_api/auth.test/run-_.snap
//!     raw/notion-api/notion_official_page/created/events.snap
//!     rendered_md/slack/.../threads/<uuid>.md.snap
//!     rendered_md/slack/.../threads/<uuid>.grid_rows.json.snap
//!     …
//!
//! Per-file `.snap`s mean `cargo insta review` walks them one at a time,
//! diffs stay local to the file that actually changed, and the file
//! layout under `snapshots/` mirrors what frankweiler-sync wrote.
//!
//! Normalization:
//!   * `_recorded_at`, `duration_ms`, `_item_hashes`, `request_id`,
//!     `fetched_at`, `last_edited_time`, `created_time`, `cache_ts`,
//!     `updated`, and `source_fingerprint` keys keep their position but
//!     get their value replaced with `"[redacted]"`.
//!   * `source_fingerprint:` lines in `.md` frontmatter get the same
//!     treatment.
//!   * `run-<timestamp>` filename segments collapse to `run-_`.
//!   * `conversations.list` and `users.list` slack endpoints are dropped
//!     entirely — they're workspace-wide listings that leak unrelated
//!     channels/users and churn on every join/leave.
//!   * Binary media files become `<binary N bytes>` markers.
//!
//! Dolt + qmd are deliberately skipped — too noisy / not deterministic.
//!
//! Tagged `manual` in Bazel and `#[ignore]` in cargo. Run with:
//!
//! ```sh
//! export LATCHKEY_CURL=$(pwd)/frankweiler/backend/target/debug/latchkey-curl-shim
//! cargo test -p frankweiler-sync --test manual_e2e_live_sync_golden -- --ignored --nocapture
//! # then to accept changes:
//! cargo insta review
//! ```

use std::path::{Path, PathBuf};
use std::process::Command;

use insta::{assert_json_snapshot, assert_snapshot};
use serde_json::Value;
use walkdir::WalkDir;

/// Keys whose value is an array we want sorted before snapshotting.
/// Use only for arrays known to be set-like (order is not meaningful).
const SORTED_ARRAY_KEYS: &[&str] = &["safe_urls"];

const VOLATILE_KEYS: &[&str] = &[
    "_recorded_at",
    "duration_ms",
    "_item_hashes",
    "request_id",
    "fetched_at",
    "last_edited_time",
    "created_time",
    "cache_ts",
    "updated",
    // Fields of `sync_summary_<now>.json` that don't reproduce
    // byte-identically across runs.
    "started_at",
    "finished_at",
    "duration_secs",
    "data_root",
    // Per-source stats are provider-specific and capture counts of
    // items fetched; on a live API they jitter with whatever new
    // activity has happened. Snapshot the structure, not the values.
    "stats",
    // qmd's `status` text embeds the index file path, byte sizes, and
    // a relative "updated N seconds ago" — none of which reproduce
    // across runs. Snapshot presence, not contents.
    "qmd_status",
    // Renderer-derived hash. Stable inputs produce a stable value, but
    // any volatile field upstream (which we redact above) would flip it,
    // so redact here too — a real algorithm change will surface as
    // every sidecar/markdown header churning at once.
    "source_fingerprint",
    // Per-row bookkeeping in the doltlite raw stores. Stamped to
    // "now" on every fetch attempt, so they churn on every run even
    // when the upstream payload is byte-identical.
    "last_attempt_at",
    "captured_at",
];

const REDACTED: &str = "[redacted]";

/// Path components whose entire contents we deliberately omit. Slack's
/// workspace-wide listings: every channel the user is in, every user in
/// the workspace. Don't belong in a committed golden.
const SKIP_PATH_SEGMENTS: &[&str] = &["conversations.list", "users.list"];

fn workspace_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is .../frankweiler/backend/sync
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("workspace root above sync/")
        .to_path_buf()
}

fn sync_binary() -> PathBuf {
    if let Ok(p) = std::env::var("FRANKWEILER_SYNC_BIN") {
        return PathBuf::from(p);
    }
    if let Some(p) = option_env!("CARGO_BIN_EXE_frankweiler-sync") {
        return PathBuf::from(p);
    }
    panic!("FRANKWEILER_SYNC_BIN not set and no cargo-provided binary path")
}

#[test]
#[ignore]
fn manual_e2e_live_sync_golden() {
    let src_config = match std::env::var("FRANKWEILER_TEST_CONFIG") {
        Ok(p) => PathBuf::from(p),
        Err(_) => workspace_root().join("configs/thad_tiny.yaml"),
    };
    assert!(src_config.exists(), "missing {}", src_config.display());
    let cfg_text = std::fs::read_to_string(&src_config).expect("read config");

    let tmp = tempfile::tempdir().expect("tempdir");
    let data_root = tmp.path().join("data");
    std::fs::create_dir_all(&data_root).unwrap();

    let cfg_out = rewrite_config(&cfg_text, &data_root);
    let cfg_path = tmp.path().join("thad_tiny.yaml");
    std::fs::write(&cfg_path, &cfg_out).unwrap();

    let bin = sync_binary();
    assert!(bin.exists(), "sync binary missing: {}", bin.display());
    eprintln!("[test] sync bin = {}", bin.display());
    eprintln!("[test] data_root = {}", data_root.display());

    let now = "2026-05-21T18:00:00Z";
    let status = Command::new(&bin)
        .arg("--config")
        .arg(&cfg_path)
        .arg("--now")
        .arg(now)
        .status()
        .expect("spawn sync");
    assert!(status.success(), "sync failed: {status:?}");

    // `sync_summary_<now>.json` must exist regardless of how each
    // source went. The path is derived from the `--now` arg with `:`
    // replaced by `-` (see main.rs).
    let safe_now = now.replace(':', "-");
    let summary_path = data_root.join(format!("sync_summary_{safe_now}.json"));
    assert!(
        summary_path.is_file(),
        "expected sync summary at {}",
        summary_path.display()
    );
    let summary_text = std::fs::read_to_string(&summary_path).expect("read summary");
    let mut summary_json: Value = serde_json::from_str(&summary_text).expect("parse summary JSON");
    strip_volatile(&mut summary_json);
    // Sanity: on a clean live run we expect overall_status="ok" and
    // not interrupted. If a source flakes, the snapshot diff will make
    // the failure mode obvious instead of just exiting non-zero.
    insta::with_settings!({
        snapshot_path => "snapshots",
        prepend_module_to_snapshot => false,
        sort_maps => true,
    }, {
        assert_json_snapshot!("sync_summary", summary_json);
    });

    let mut manifest: Vec<String> = Vec::new();
    snapshot_tree(&data_root.join("raw"), "raw", &mut manifest);
    snapshot_tree(&data_root.join("rendered_md"), "rendered_md", &mut manifest);
    manifest.sort();

    // Manifest pins which files we expect to find. Catches additions /
    // removals without having to diff every per-file snapshot.
    insta::with_settings!({
        snapshot_path => "snapshots",
        prepend_module_to_snapshot => false,
    }, {
        assert_snapshot!("manifest", manifest.join("\n"));
    });
}

/// Walk `root` and emit one snapshot per file. Each snapshot lives at
/// `tests/snapshots/<top>/<rel_dir>/<filename>.snap`, mirroring the
/// data layout. `manifest` collects the snapshot key (top + rel path)
/// for the overall manifest assertion.
fn snapshot_tree(root: &Path, top: &str, manifest: &mut Vec<String>) {
    if !root.is_dir() {
        return;
    }
    for entry in WalkDir::new(root).sort_by_file_name() {
        let entry = entry.expect("walk tree");
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(root).unwrap();
        if rel
            .components()
            .any(|c| SKIP_PATH_SEGMENTS.contains(&c.as_os_str().to_string_lossy().as_ref()))
        {
            continue;
        }
        // Doltlite leaves behind sidecar lock files (e.g.
        // `.foo.doltlite_db-lock`) for in-process flock coordination.
        // They're ephemeral, content-free, and would clutter goldens
        // with hidden-dotfile noise. Skip anything whose name ends in
        // `-lock`.
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.ends_with("-lock"))
        {
            continue;
        }
        let canonical_rel = canonicalize_path(rel);
        let manifest_key = format!("{top}/{canonical_rel}");
        manifest.push(manifest_key.clone());

        // snapshot_path is relative to the test source file. Insta
        // creates the directories as needed.
        let canonical_rel_path = PathBuf::from(&canonical_rel);
        let snap_parent = canonical_rel_path.parent().unwrap_or(Path::new(""));
        let snap_dir = PathBuf::from("snapshots").join(top).join(snap_parent);
        let snap_name = canonical_rel_path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_string();

        let value = summarize_file(entry.path());
        insta::with_settings!({
            snapshot_path => snap_dir.display().to_string(),
            prepend_module_to_snapshot => false,
            sort_maps => true,
            description => manifest_key,
        }, {
            match value {
                SnapValue::Json(v) => assert_json_snapshot!(snap_name, v),
                SnapValue::Text(s) => assert_snapshot!(snap_name, s),
            }
        });
    }
}

enum SnapValue {
    Json(Value),
    Text(String),
}

/// File → snapshot payload. JSONL is parsed line-by-line, sorted, and
/// stripped of volatile fields. JSON likewise. Markdown is text with
/// frontmatter redactions. `.doltlite_db` files are opened and dumped
/// as `{table_name: [rows]}` JSON so the goldens carry the actual raw
/// payload contents (lets the TNG fixture pipeline reuse the captured
/// rows as seed data). Anything else (media) becomes a size marker.
fn summarize_file(path: &Path) -> SnapValue {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.ends_with(".doltlite_db") {
        let mut v = dump_doltlite_db(path);
        strip_volatile(&mut v);
        return SnapValue::Json(v);
    }
    if name.ends_with(".jsonl") {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut lines: Vec<Value> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| match serde_json::from_str::<Value>(l) {
                Ok(mut v) => {
                    strip_volatile(&mut v);
                    v
                }
                Err(_) => Value::String(l.to_string()),
            })
            .collect();
        lines.sort_by_key(|v| v.to_string());
        SnapValue::Json(Value::Array(lines))
    } else if name.ends_with(".json") {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        match serde_json::from_str::<Value>(&text) {
            Ok(mut v) => {
                strip_volatile(&mut v);
                SnapValue::Json(v)
            }
            Err(_) => SnapValue::Text(text),
        }
    } else if name.ends_with(".md") {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        SnapValue::Text(redact_markdown(&text))
    } else {
        // Try text first, fall back to a size marker for binary.
        match std::fs::read_to_string(path) {
            Ok(t) => SnapValue::Text(t),
            Err(_) => {
                let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                SnapValue::Text(format!("<binary {size} bytes>"))
            }
        }
    }
}

/// Patch the YAML: swap `data_root`, force `qmd.skip=true`, and bump
/// slack `refresh_window_days` so a fresh data_root re-downloads media.
fn rewrite_config(text: &str, data_root: &Path) -> String {
    let mut doc: serde_yaml::Value = serde_yaml::from_str(text).expect("parse config yaml");
    let map = doc.as_mapping_mut().expect("config root mapping");
    map.insert(
        serde_yaml::Value::String("data_root".into()),
        serde_yaml::Value::String(data_root.display().to_string()),
    );
    let qmd = serde_yaml::Mapping::from_iter([(
        serde_yaml::Value::String("skip".into()),
        serde_yaml::Value::Bool(true),
    )]);
    map.insert(
        serde_yaml::Value::String("qmd".into()),
        serde_yaml::Value::Mapping(qmd),
    );
    if let Some(sources) = map
        .get_mut(serde_yaml::Value::String("sources".into()))
        .and_then(|v| v.as_sequence_mut())
    {
        for src in sources {
            let Some(m) = src.as_mapping_mut() else {
                continue;
            };
            let is_slack = m
                .get(serde_yaml::Value::String("type".into()))
                .and_then(|v| v.as_str())
                == Some("slack_api");
            if !is_slack {
                continue;
            }
            let sync_entry = m
                .entry(serde_yaml::Value::String("sync".into()))
                .or_insert(serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
            if let Some(sync_map) = sync_entry.as_mapping_mut() {
                sync_map.insert(
                    serde_yaml::Value::String("refresh_window_days".into()),
                    serde_yaml::Value::Number(30.into()),
                );
            }
        }
    }
    serde_yaml::to_string(&doc).expect("serialize yaml")
}

/// Replace `run-<timestamp>` segments in a path with `run-_` so per-run
/// filenames don't churn the snapshot layout.
fn canonicalize_path(rel: &Path) -> String {
    let parts: Vec<String> = rel
        .components()
        .map(|c| {
            let s = c.as_os_str().to_string_lossy().to_string();
            if let Some(rest) = s.strip_prefix("run-") {
                let tail_ext = rest.find('.').map(|i| &rest[i..]).unwrap_or("");
                format!("run-_{tail_ext}")
            } else {
                s
            }
        })
        .collect();
    parts.join("/")
}

/// Line-level redaction for rendered markdown. Frontmatter lines like
/// `source_fingerprint: ba02d4dd774685c7` get their value replaced.
fn redact_markdown(text: &str) -> String {
    let prefixes = ["source_fingerprint:"];
    let mut out = text
        .lines()
        .map(|line| {
            let trimmed = line.trim_start();
            for p in &prefixes {
                if trimmed.starts_with(p) {
                    let indent = &line[..line.len() - trimmed.len()];
                    return format!("{indent}{p} [redacted]");
                }
            }
            line.to_string()
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Open a `.doltlite_db` sqlite file and dump every user table as a
/// JSON object of the shape `{table_name: [{col: value, ...}, ...]}`.
///
/// - `payload` columns are unwrapped via `json(payload)` so JSONB blobs
///   come back as text JSON, then re-parsed to a [`Value`] so the
///   snapshot diff stays at JSON granularity rather than embedded
///   string granularity. Other JSON-bearing text columns (`config`,
///   `summary`, `example_headers`, `example_envelope_skeleton`) get the
///   same parse treatment.
/// - `BLOB` columns (e.g. `blobs.bytes`) collapse to `<bytes N>` markers
///   — we don't want literal binary in goldens, but a row's *presence*
///   and approximate size are signal worth keeping.
/// - Rows are ordered by the table's natural primary key (`id`,
///   `run_id`, `scope`, or `endpoint` depending on the table) so the
///   snapshot is deterministic.
fn dump_doltlite_db(path: &Path) -> Value {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for doltlite dump");
    rt.block_on(dump_doltlite_db_async(path))
}

async fn dump_doltlite_db_async(path: &Path) -> Value {
    use std::str::FromStr;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::{Column, Row, TypeInfo, ValueRef};

    // JSON-bearing TEXT columns: parse to Value rather than leaving as
    // an embedded string. Everything else passes through verbatim.
    const JSON_TEXT_COLUMNS: &[&str] = &[
        "payload",
        "config",
        "summary",
        "example_headers",
        "example_envelope_skeleton",
    ];

    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .expect("sqlite uri")
        .create_if_missing(false)
        .read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("open doltlite db");

    // Tables to walk, in alphabetical order so the snapshot is stable.
    let table_rows = sqlx::query(
        "SELECT name FROM sqlite_master \
         WHERE type='table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )
    .fetch_all(&pool)
    .await
    .expect("list tables");
    let tables: Vec<String> = table_rows
        .iter()
        .map(|r| r.try_get::<String, _>(0).unwrap_or_default())
        .collect();

    let mut out = serde_json::Map::new();
    for t in tables {
        // Pull column names so we know whether to wrap `payload` in
        // `json(...)` and which column to ORDER BY.
        let info = sqlx::query(&format!("PRAGMA table_info(\"{t}\")"))
            .fetch_all(&pool)
            .await
            .expect("table_info");
        let columns: Vec<String> = info
            .iter()
            .map(|r| r.try_get::<String, _>("name").unwrap_or_default())
            .collect();

        let select_list = columns
            .iter()
            .map(|c| {
                if c == "payload" {
                    "json(payload) AS payload".to_string()
                } else {
                    format!("\"{c}\"")
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let order_by = if columns.iter().any(|c| c == "id") {
            "ORDER BY id"
        } else if columns.iter().any(|c| c == "run_id") {
            "ORDER BY run_id"
        } else if columns.iter().any(|c| c == "scope") {
            "ORDER BY scope"
        } else if columns.iter().any(|c| c == "endpoint") {
            "ORDER BY endpoint"
        } else {
            ""
        };
        let q = format!("SELECT {select_list} FROM \"{t}\" {order_by}");
        let rows = sqlx::query(&q)
            .fetch_all(&pool)
            .await
            .unwrap_or_else(|e| panic!("select {t}: {e}"));

        let mut row_vals = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut obj = serde_json::Map::new();
            for col in row.columns() {
                let name = col.name();
                // Per-row dynamic type. We can't trust the *column*'s
                // declared type because aliased function results (e.g.
                // `json(payload) AS payload`) have no declared type and
                // would fall through to NULL. ValueRef gives the actual
                // SQLite storage class for *this* cell.
                let raw = row.try_get_raw(name).expect("try_get_raw");
                let value: Value = if raw.is_null() {
                    Value::Null
                } else {
                    let type_info = raw.type_info().into_owned();
                    let kind = type_info.name(); // TEXT/INTEGER/REAL/BLOB
                    match kind {
                        "TEXT" => row
                            .try_get::<String, _>(name)
                            .ok()
                            .map(|s| {
                                if JSON_TEXT_COLUMNS.contains(&name) {
                                    serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s))
                                } else {
                                    Value::String(s)
                                }
                            })
                            .unwrap_or(Value::Null),
                        "INTEGER" => row
                            .try_get::<i64, _>(name)
                            .ok()
                            .map(Value::from)
                            .unwrap_or(Value::Null),
                        "REAL" => row
                            .try_get::<f64, _>(name)
                            .ok()
                            .and_then(|n| serde_json::Number::from_f64(n).map(Value::Number))
                            .unwrap_or(Value::Null),
                        "BLOB" => row
                            .try_get::<Vec<u8>, _>(name)
                            .ok()
                            .map(|b| Value::String(format!("<bytes {}>", b.len())))
                            .unwrap_or(Value::Null),
                        _ => Value::Null,
                    }
                };
                obj.insert(name.to_string(), value);
            }
            row_vals.push(Value::Object(obj));
        }
        out.insert(t, Value::Array(row_vals));
    }
    Value::Object(out)
}

fn strip_volatile(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, child) in map.iter_mut() {
                if VOLATILE_KEYS.contains(&k.as_str()) {
                    *child = Value::String(REDACTED.into());
                    continue;
                }
                strip_volatile(child);
                if SORTED_ARRAY_KEYS.contains(&k.as_str()) {
                    if let Value::Array(items) = child {
                        items.sort_by_key(|a| a.to_string());
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                strip_volatile(item);
            }
        }
        _ => {}
    }
}
