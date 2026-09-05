// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live end-to-end golden test for the `datalib-dag` pipeline.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use insta::{assert_json_snapshot, assert_snapshot};
use serde_json::Value;
use walkdir::WalkDir;

/// The run's tempdir `data_root`, captured once. Snapshots replace its
/// volatile absolute prefix (`/var/folders/…/.tmpXXXX/data`) with a stable
/// `<data_root>` token — preserving the meaningful suffix (which db / rendered
/// file the path points at) while killing the per-run tempdir churn.
static DATA_ROOT: OnceLock<String> = OnceLock::new();

fn norm_data_root(s: &str) -> String {
    match DATA_ROOT.get() {
        Some(dr) => s.replace(dr.as_str(), "<data_root>"),
        None => s.to_string(),
    }
}

/// Collapse the volatile query string of an AWS S3 pre-signed URL to a stable
/// `?<presigned>` token, keeping the base URL. S3-backed providers re-sign on
/// every fetch, so the signature and expiry rotate while the path stays put.
/// Handles a bare value and one embedded in `![alt](…)`.
fn scrub_presigned(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(q) = rest.find("?X-Amz-") {
        out.push_str(&rest[..q]);
        out.push_str("?<presigned>");
        let after = &rest[q..];
        let end = after
            .find(|c: char| c == '"' || c == ')' || c.is_whitespace())
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

fn normalize_str(s: &str) -> String {
    scrub_presigned(&norm_data_root(s))
}

/// Redact dolt `commit=<hash>` substrings embedded in a human-readable string.
/// The run-2 incrementality snapshot preserves each source's `stats` line (for
/// its counts), but that line ends in `commit=<40-hex>` which is per-run
/// volatile. Run-1 / per-file snapshots redact the whole `stats` key instead,
/// so this is only wired into the incrementality path.
fn scrub_commit(s: &str) -> String {
    const KEY: &str = "commit=";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(KEY) {
        out.push_str(&rest[..i + KEY.len()]);
        out.push_str(REDACTED);
        let after = &rest[i + KEY.len()..];
        let end = after
            .find(|c: char| !c.is_ascii_hexdigit())
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

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
    // NB: `updated` is deliberately NOT redacted. It is per-fetch bookkeeping
    // that the pipeline splits into the `volatile_payload` sidecar (which this
    // dump excludes), so it never reaches a content table. If a new provider's
    // volatile `updated` shows up as golden churn, split it at the source —
    // don't re-add it here.

    // Fields of `sync_summary_<now>.json` that don't reproduce byte-identically.
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
    // every document's header churning at once.
    "source_fingerprint",
    // Per-row bookkeeping in the doltlite raw stores. Stamped to
    // "now" on every fetch attempt, so they churn on every run even
    // when the upstream payload is byte-identical.
    "last_attempt_at",
    // NB: `captured_at` was here under the same heading and should not be —
    // the only emitter is `media_visual.captured_at`, the moment the shutter
    // opened, parsed out of fixed EXIF bytes. Redacting it meant the golden
    // could not catch a regression in EXIF timestamp parsing.

    // CAS blob "first stored" wall-clock stamp. Identical bytes land at the
    // same PK, but the timestamp is whenever this run first wrote them.
    "first_seen_at",
    // Resume-cursor / bookkeeping wall-clock stamps: `sync_scope_state`'s
    // `last_finished_at` + `after` (real now when the scope ran, not the
    // `--now` arg), and `last_seen_at` (when a row was last fetched). The
    // cursor `before` is a `--now`-relative window start and stays put, as do
    // genuine upstream content times (`last_sign_in_at`, `latest_build_*`).
    "last_finished_at",
    "after",
    "last_seen_at",
    // GitLab's `local_time` is the user's *current* local time, so it ticks
    // every minute; `last_activity_on` is the same class one granularity
    // coarser. Genuine upstream data that tracks the observer, not the
    // observed.
    "local_time",
    "last_activity_on",
    // Notion file blocks carry a pre-signed S3 link the API re-signs on every
    // fetch: `expiry_time` is its rotating expiry; the sibling `url`'s volatile
    // query string is collapsed by `scrub_presigned` (the base URL stays).
    "expiry_time",
    // Per-provider `_render_cursor.json` skip-check bookkeeping: `last_render_at`
    // is wall-clock, and `last_rendered_hash` is a digest over inputs that
    // include volatile fields (same reason `source_fingerprint` is redacted).
    "last_render_at",
    "last_rendered_hash",
    // Dolt commit hashes (`load.commit_hash`, per-source `commit`) fold in a
    // wall-clock timestamp, so they differ on every run even for identical
    // content. The actual content equality is covered by row counts + the
    // per-file snapshots.
    "commit_hash",
    // `load.write_lock` timings are pure wall-clock jitter; the meaningful
    // `acquisitions` count is preserved.
    "avg_hold_ms",
    "avg_wait_ms",
    "total_hold_ms",
    "total_wait_ms",
    // The extract-metrics report's per-db byte sizes wobble run-to-run
    // (sqlite page layout, ordering) even when row counts match — keep the
    // `rows_*` signal, drop the bytes.
    "bytes_before",
    "bytes_after",
    "bytes_delta",
    // Per-source fetch wall-clock timing recorded in `sync_runs` (every
    // provider's raw db carries it). Pure jitter; the `--now`-derived
    // start/stop timestamps and content fields stay put.
    "elapsed_ms",
    "network_seconds",
    // fsindex's `scan_meta` columns. `last_scan_at` is wall-clock, so it
    // churns run-to-run even when the scanned bytes are identical, and
    // `scanner_version` is redacted so a version bump doesn't churn the
    // golden.
    "mtime_ns",
    "ctime_ns",
    "inode",
    "dev",
    "last_scan_at",
    "scanner_version",
];

/// Live counters on an embedded GitHub *repo* object (a PR payload carries the
/// full head/base repo) that drift as the repo is used. Redacted ONLY inside a
/// repo object — see `is_github_repo_object` — so the same generic names
/// elsewhere (a file's `size`, a comment's `updated_at`) survive as content.
const REPO_VOLATILE_KEYS: &[&str] = &[
    "size",
    "forks",
    "forks_count",
    "watchers",
    "watchers_count",
    "open_issues",
    "open_issues_count",
    "stargazers_count",
    "pushed_at",
    "updated_at",
];

fn is_github_repo_object(map: &serde_json::Map<String, Value>) -> bool {
    map.contains_key("full_name") && map.contains_key("default_branch")
}

/// Per-TABLE volatile columns: `(table, keys)` redacted only in rows of that
/// table. Applied in [`dump_doltlite_db`], which knows the table name for
/// certain — no shape-sniffing required.
const TABLE_VOLATILE_KEYS: &[(&str, &[&str])] = &[("sync_scope_config", &["updated_at"])];

const REDACTED: &str = "[redacted]";

/// Path components whose entire contents we deliberately omit. Slack's
/// workspace-wide listings: every channel the user is in, every user in
/// the workspace. Don't belong in a committed golden.
const SKIP_PATH_SEGMENTS: &[&str] = &["conversations.list", "users.list", "events"];

/// External, out-of-repo home for this test's `config.toml`, its file-based
/// `sources/`, and the golden `snapshots/` — kept outside the repo so the
/// source data is never shared when the repo is open-sourced.
/// `manual_e2e_run.sh` sets `DATALIB_MANUAL_E2E_DIR`; `None` when unset.
fn e2e_dir() -> Option<PathBuf> {
    std::env::var("DATALIB_MANUAL_E2E_DIR")
        .ok()
        .map(PathBuf::from)
}

fn snap_base() -> PathBuf {
    e2e_dir()
        .map(|d| d.join("snapshots"))
        .unwrap_or_else(|| PathBuf::from("snapshots"))
}

fn bin_dir() -> PathBuf {
    if let Ok(p) = std::env::var("DATALIB_BINARY_DIR") {
        return PathBuf::from(p);
    }
    const REL: &str = "_main/datalib/backend/bin";
    let r = runfiles::Runfiles::create().unwrap_or_else(|e| {
        panic!(
            "no runfiles tree ({e}); run this through bazel, or set \
             DATALIB_BINARY_DIR to a directory holding datalib-dag + datalib-step"
        )
    });
    let dir = r
        .rlocation(REL)
        .unwrap_or_else(|| panic!("rlocation failed for {REL}"));
    for name in ["datalib-dag", "datalib-step"] {
        assert!(
            dir.join(name).exists(),
            "{name} missing from the staged binary dir ({})",
            dir.display()
        );
    }
    dir
}

/// Root for this run's `data_root`, persisted pytest-`tmp_path`-style: each
/// run gets its own dir and only the most recent [`KEEP_RUNS`] are kept, so
/// you can inspect the last few runs' doltlite DBs without unbounded disk
/// growth. Unlike a tempdir these survive a panic mid-run.
fn persistent_run_root() -> PathBuf {
    /// How many recent runs to keep (this run + the previous KEEP_RUNS-1).
    const KEEP_RUNS: usize = 3;
    let base = std::env::temp_dir().join("datalib-e2e-runs");
    std::fs::create_dir_all(&base).expect("create e2e runs base");

    // Sortable, chronological dir name: zero-padded millis since the epoch,
    // so a lexical sort is a chronological sort. 13 digits covers ms
    // timestamps through the year ~2286.
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let run_root = base.join(format!("run-{millis:013}"));
    std::fs::create_dir_all(&run_root).expect("create run root");

    // Prune older runs, keeping the newest KEEP_RUNS (this one included).
    let mut runs: Vec<PathBuf> = std::fs::read_dir(&base)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_dir()
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("run-"))
        })
        .collect();
    runs.sort();
    if runs.len() > KEEP_RUNS {
        for old in &runs[..runs.len() - KEEP_RUNS] {
            let _ = std::fs::remove_dir_all(old);
        }
    }
    eprintln!(
        "[test] run_root = {} (keeping newest {KEEP_RUNS} runs under {})",
        run_root.display(),
        base.display()
    );
    run_root
}

#[test]
#[ignore]
fn manual_e2e_live_sync_golden() {
    let src_config = match std::env::var("DATALIB_TEST_CONFIG") {
        Ok(p) => PathBuf::from(p),
        Err(_) => e2e_dir()
            .expect(
                "set DATALIB_MANUAL_E2E_DIR to the external test-data dir \
                 (holding dag.toml + sources/ + snapshots/), or set \
                 DATALIB_TEST_CONFIG to a config explicitly",
            )
            .join("dag.toml"),
    };
    assert!(
        src_config.exists(),
        "missing {}. Point DATALIB_MANUAL_E2E_DIR at the external test-data \
         dir (holding dag.toml + sources/ + snapshots/), or set \
         DATALIB_TEST_CONFIG to a config explicitly.",
        src_config.display()
    );
    let cfg_text = std::fs::read_to_string(&src_config).expect("read config");

    // Persist the run dir pytest-`tmp_path`-style (see `persistent_run_root`)
    // rather than a self-deleting tempdir, so you can always peek at the last
    // few runs' working doltlite DBs / rendered output afterward — e.g. to
    // inspect a `dolt_diff_<table>` warning with the doltlite client.
    let run_root = persistent_run_root();
    let data_root = run_root.join("data");
    std::fs::create_dir_all(&data_root).unwrap();
    // Capture the data_root so snapshots can normalize its absolute
    // prefix out of every embedded path (see `norm_data_root`).
    DATA_ROOT.set(data_root.to_string_lossy().into_owned()).ok();

    let cfg_out = rewrite_config(&cfg_text, &data_root);
    let cfg_path = run_root.join("config.toml");
    std::fs::write(&cfg_path, &cfg_out).unwrap();

    // `//datalib/backend:bin` stages datalib-dag and datalib-step side by
    // side under their public names, so the runner finds the step binary via
    // its own-directory fallback — no `--binary-dir` needed.
    let bin = bin_dir().join("datalib-dag");
    eprintln!("[test] dag bin = {}", bin.display());
    eprintln!("[test] data_root = {}", data_root.display());

    let now = "2026-05-21T18:00:00Z";
    let run1 = run_pipeline(&bin, &cfg_path, now, &[]);
    assert!(
        run1.status.success(),
        "pipeline run 1 failed (exit {:?}). Last stderr:\n{}",
        run1.status.code(),
        run1.stderr_tail(40)
    );

    // The `run_summary` event is the machine-readable run record — one
    // NDJSON line on stderr, emitted exactly once, last. It replaces the
    // old `sync_summary_<now>.json` file (see the module header).
    let summary1 = run1.run_summary().expect(
        "no run_summary event on stderr — the runner must emit one per run; \
         did it die before the scheduler finished?",
    );

    // The old aggregate summary snapshot pinned every source's shape in one
    // file. `run_summary` is far thinner, so snapshotting it wholesale would
    // buy little; assert the two things it CAN still tell us instead, both of
    // which fail loudly rather than silently.
    assert_step_statuses_ok(&summary1);
    // 2. The step-id set matches the config. Catches the reverse: a step
    //    silently added or renamed.
    insta::with_settings!({
        snapshot_path => snap_base().display().to_string(),
        prepend_module_to_snapshot => false,
    }, {
        assert_snapshot!("run_summary_steps", step_id_list(&summary1));
    });

    // Layout invariant: data_root is a flat set of stanza dirs plus the one
    // reserved `system/` dir — never the old top-level `raw/` or
    // `rendered_md/`, and the aggregate index DB lives under `system/`. Guards
    // against a regression to the pre-grouping layout.
    assert!(
        !data_root.join("raw").exists() && !data_root.join("rendered_md").exists(),
        "old top-level raw/ or rendered_md/ found — data_root must be grouped by stanza"
    );
    assert!(
        data_root
            .join("unified_index/grid/db.doltlite_db")
            .is_file(),
        "backend index DB must live at unified_index/grid/db.doltlite_db"
    );
    // Only the genuinely-derived index dirs are tagged as rebuildable cache so
    // `--exclude-caches` backups skip them (the per-stanza `rendered_md/` tags
    // are checked implicitly via the manifest — CACHEDIR.TAG is skipped in the
    // walk below). `system/` must NOT be tagged — job logs are operational
    // history, not rebuildable from raw.
    assert!(
        data_root.join("unified_index/CACHEDIR.TAG").is_file(),
        "unified_index/ must carry a CACHEDIR.TAG marking the index tree as derived cache"
    );
    assert!(
        !data_root.join("system/CACHEDIR.TAG").exists(),
        "system/ (operational history) must NOT be tagged as cache"
    );

    // Snapshot each stanza's `raw/` and `rendered_md/` trees, mirroring the
    // on-disk per-stanza layout. `system/` (the aggregate index + qmd) is
    // skipped — see the module header.
    let mut stanzas: Vec<String> = std::fs::read_dir(&data_root)
        .expect("read data_root")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "system")
        .collect();
    stanzas.sort();

    let mut manifest: Vec<String> = Vec::new();
    for stanza in &stanzas {
        for sub in ["raw", "rendered_md"] {
            let dir = data_root.join(stanza).join(sub);
            snapshot_tree(&dir, &format!("{stanza}/{sub}"), &mut manifest);
        }
    }
    manifest.sort();

    // Prune snapshots orphaned when the set of produced files changes.
    // insta's `INSTA_UNREFERENCED=delete` is a cargo-insta feature that does
    // not fire under `bazel run` and would not scan our external snapshot
    // tree anyway. Update mode only.
    prune_orphan_snapshots(&manifest);

    // Manifest pins which files we expect to find. Catches additions /
    // removals without having to diff every per-file snapshot.
    insta::with_settings!({
        snapshot_path => snap_base().display().to_string(),
        prepend_module_to_snapshot => false,
    }, {
        assert_snapshot!("manifest", manifest.join("\n"));
    });

    // ── Second run: incrementality check ──────────────────────────────
    let now2 = "2026-05-21T18:05:00Z";
    let run2 = run_pipeline(&bin, &cfg_path, now2, &[]);
    assert!(
        run2.status.success(),
        "pipeline run 2 failed (exit {:?}). Last stderr:\n{}",
        run2.status.code(),
        run2.stderr_tail(40)
    );
    assert_step_statuses_ok(&run2.run_summary().expect("run 2 run_summary"));

    // The incrementality signal comes from each stanza's own `sync_runs`
    // table, not the runner's summary. `strip_volatile_for_incrementality`
    // redacts wall-clock jitter while preserving the counts, which are the
    // whole point of this snapshot.
    let mut report = incrementality_report(&data_root, &stanzas);
    strip_volatile_for_incrementality(&mut report);
    insta::with_settings!({
        snapshot_path => snap_base().display().to_string(),
        prepend_module_to_snapshot => false,
        sort_maps => true,
    }, {
        assert_json_snapshot!("sync_summary_run2_incrementality", report);
    });
    // On slack, read `messages` with care: it is the one count here that
    // moves without anything being wrong, because `refresh_window_days = 30`
    // makes run 2 re-query the trailing 30 days and whatever traffic fell
    // inside that window on the day of the bake.

    // ── Third run: --reset-and-redownload content stability ───────────
    let stability_dbs = ["tiny-slack/raw/entities.doltlite_db"];
    // Skip (loudly) any db this config didn't produce, so a reduced config via
    // DATALIB_TEST_CONFIG doesn't crash here. On the full config a missing db
    // means its source failed — which run 1's status assertion already caught.
    let before: Vec<(&str, Value)> = stability_dbs
        .iter()
        .filter(|name| {
            let present = data_root.join(name).is_file();
            if !present {
                eprintln!("[test] WARNING: stability db absent, skipping: {name}");
            }
            present
        })
        .map(|name| (*name, content_tables(&data_root.join(name))))
        .collect();

    let now3 = "2026-05-21T18:10:00Z";
    let run3 = run_pipeline(&bin, &cfg_path, now3, &["--reset-and-redownload"]);
    assert!(
        run3.status.success(),
        "pipeline run 3 (reset) failed (exit {:?}). Last stderr:\n{}",
        run3.status.code(),
        run3.stderr_tail(40)
    );
    assert_step_statuses_ok(&run3.run_summary().expect("run 3 run_summary"));

    for (name, before_v) in &before {
        let after_v = content_tables(&data_root.join(name));
        // A path-level diff, not `assert_eq!` on two whole Values: these
        // are multi-megabyte structures, and eyeballing two offset 2MB
        // dumps invites you to "find" differences that are only
        // misalignment.
        let drifts = json_diff_paths(before_v, &after_v, DRIFT_REPORT_LIMIT);
        assert!(
            drifts.is_empty(),
            "{name}: content tables drifted across --reset-and-redownload.\n\
             Re-fetching an unchanged upstream object must land identical \
             bytes, so a drifting field is per-fetch bookkeeping leaking into \
             a content payload — declare it in that entity's \
             *_VOLATILE_PATHS and route the upsert through \
             bulk_upsert_with_tape_split (see data_architecture_ingestion.md \
             §\"Volatile-field split\").\n\n{}",
            drifts.join("\n")
        );
    }
}

/// How many drifting paths to name before truncating. Enough to see whether
/// the drift is one stray field or a systemic shape change; not so many that
/// the message becomes the thing that hides the answer.
const DRIFT_REPORT_LIMIT: usize = 40;

fn json_diff_paths(a: &Value, b: &Value, limit: usize) -> Vec<String> {
    fn short(v: &Value) -> String {
        let s = match v {
            Value::String(s) => format!("{s:?}"),
            other => other.to_string(),
        };
        if s.chars().count() > 80 {
            let head: String = s.chars().take(77).collect();
            format!("{head}...")
        } else {
            s
        }
    }
    fn row_id(v: &Value) -> Option<&str> {
        v.get("id").and_then(Value::as_str)
    }
    fn walk(path: &str, a: &Value, b: &Value, out: &mut Vec<String>, limit: usize) {
        if out.len() >= limit {
            return;
        }
        match (a, b) {
            (Value::Object(ma), Value::Object(mb)) => {
                let mut keys: Vec<&String> = ma.keys().chain(mb.keys()).collect();
                keys.sort_unstable();
                keys.dedup();
                for k in keys {
                    let p = if path.is_empty() {
                        k.clone()
                    } else {
                        format!("{path}.{k}")
                    };
                    match (ma.get(k), mb.get(k)) {
                        (Some(x), Some(y)) => walk(&p, x, y, out, limit),
                        (Some(x), None) => {
                            out.push(format!("  {p}: before={} after=<missing>", short(x)))
                        }
                        (None, Some(y)) => {
                            out.push(format!("  {p}: before=<missing> after={}", short(y)))
                        }
                        (None, None) => {}
                    }
                    if out.len() >= limit {
                        return;
                    }
                }
            }
            (Value::Array(xa), Value::Array(xb)) => {
                // Match by `id` when every element has one; else positionally.
                let ida: Option<Vec<&str>> = xa.iter().map(row_id).collect();
                let idb: Option<Vec<&str>> = xb.iter().map(row_id).collect();
                if let (Some(ia), Some(ib)) = (ida, idb) {
                    let ma: std::collections::BTreeMap<&str, &Value> =
                        ia.into_iter().zip(xa.iter()).collect();
                    let mb: std::collections::BTreeMap<&str, &Value> =
                        ib.into_iter().zip(xb.iter()).collect();
                    let mut ids: Vec<&&str> = ma.keys().chain(mb.keys()).collect();
                    ids.sort_unstable();
                    ids.dedup();
                    for id in ids {
                        let p = format!("{path}[id={id}]");
                        match (ma.get(*id), mb.get(*id)) {
                            (Some(x), Some(y)) => walk(&p, x, y, out, limit),
                            (Some(_), None) => {
                                out.push(format!("  {p}: row present before, gone after"))
                            }
                            (None, Some(_)) => {
                                out.push(format!("  {p}: row absent before, present after"))
                            }
                            (None, None) => {}
                        }
                        if out.len() >= limit {
                            return;
                        }
                    }
                } else {
                    if xa.len() != xb.len() {
                        out.push(format!(
                            "  {path}: array length {} -> {}",
                            xa.len(),
                            xb.len()
                        ));
                    }
                    for (i, (x, y)) in xa.iter().zip(xb.iter()).enumerate() {
                        walk(&format!("{path}[{i}]"), x, y, out, limit);
                        if out.len() >= limit {
                            return;
                        }
                    }
                }
            }
            _ => {
                if a != b {
                    out.push(format!("  {path}: before={} after={}", short(a), short(b)));
                }
            }
        }
    }
    let mut out = Vec::new();
    walk("", a, b, &mut out, limit);
    if out.len() >= limit {
        out.push(format!("  … truncated at {limit} differing paths"));
    }
    out
}

/// One invocation of `datalib-dag`, with its stderr captured.
struct PipelineRun {
    status: std::process::ExitStatus,
    stderr: String,
}

impl PipelineRun {
    fn run_summary(&self) -> Option<Value> {
        self.stderr
            .lines()
            .rev() // emitted last; scan from the end
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v.get("event").and_then(Value::as_str) == Some("run_summary"))
    }

    fn stderr_tail(&self, n: usize) -> String {
        let lines: Vec<&str> = self.stderr.lines().collect();
        lines[lines.len().saturating_sub(n)..].join("\n")
    }
}

fn run_pipeline(bin: &Path, cfg_path: &Path, now: &str, extra_args: &[&str]) -> PipelineRun {
    eprintln!("[test] run: {} --now {now} {extra_args:?}", bin.display());
    let out = Command::new(bin)
        .arg(cfg_path)
        .arg("--now")
        .arg(now)
        .args(extra_args)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn datalib-dag");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // Persist the raw stream next to the run's data. We capture stderr
    // rather than inheriting it (that's where the NDJSON run record is), so
    // without this the whole event stream is discarded on a *successful*
    // run and only the last 40 lines survive a failure — which is precisely
    // when you want to ask "why did that take 18 minutes?".
    let log = cfg_path
        .parent()
        .unwrap_or(Path::new("."))
        .join(format!("{}.ndjson", now.replace(':', "-")));
    if let Err(e) = std::fs::write(&log, &stderr) {
        eprintln!("[test] WARNING: could not write {}: {e}", log.display());
    } else {
        eprintln!("[test] stream: {}", log.display());
    }

    PipelineRun {
        status: out.status,
        stderr,
    }
}

fn assert_step_statuses_ok(summary: &Value) {
    let steps = summary
        .get("steps")
        .and_then(Value::as_array)
        .expect("run_summary.steps");
    assert!(!steps.is_empty(), "run_summary carried zero steps");
    // `skipped_up_to_date` is a SUCCESS: the scheduler content-hashed the
    // step's inputs, found them unchanged, and correctly did nothing. Runs 2
    // and 3 are expected to be full of these — that is what incrementality
    // looks like. Only `failed` and `blocked` are real problems.
    const OK_STATUSES: &[&str] = &["succeeded", "skipped_up_to_date"];
    let bad: Vec<String> = steps
        .iter()
        .filter(|s| {
            !s.get("status")
                .and_then(Value::as_str)
                .is_some_and(|st| OK_STATUSES.contains(&st))
        })
        .map(|s| {
            format!(
                "  {} → {} {}",
                s.get("step").and_then(Value::as_str).unwrap_or("?"),
                s.get("status").and_then(Value::as_str).unwrap_or("?"),
                s.get("error").and_then(Value::as_str).unwrap_or(""),
            )
        })
        .collect();
    assert!(
        bad.is_empty(),
        "not every step succeeded:\n{}",
        bad.join("\n")
    );
}

fn step_id_list(summary: &Value) -> String {
    let mut ids: Vec<&str> = summary
        .get("steps")
        .and_then(Value::as_array)
        .expect("run_summary.steps")
        .iter()
        .filter_map(|s| s.get("step").and_then(Value::as_str))
        .collect();
    ids.sort_unstable();
    ids.join("\n")
}

fn incrementality_report(data_root: &Path, stanzas: &[String]) -> Value {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for sync_runs read");
    let mut out = serde_json::Map::new();
    for stanza in stanzas {
        let db = data_root.join(stanza).join("raw/entities.doltlite_db");
        let v = if db.is_file() {
            rt.block_on(latest_sync_run(&db))
        } else {
            Value::String("<no raw/entities.doltlite_db>".into())
        };
        out.insert(stanza.clone(), v);
    }
    Value::Object(out)
}

async fn latest_sync_run(path: &Path) -> Value {
    use std::str::FromStr;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::Row;

    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .expect("sqlite uri")
        .create_if_missing(false)
        .read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap_or_else(|e| panic!("open {} for sync_runs: {e}", path.display()));

    let row = sqlx::query("SELECT status, summary FROM sync_runs ORDER BY run_id DESC LIMIT 1")
        .fetch_optional(&pool)
        .await
        .unwrap_or_else(|e| panic!("select sync_runs from {}: {e}", path.display()));
    let Some(row) = row else {
        // Table present, no rows: a file-backed source that never opened a
        // DownloadRun. See `incrementality_report`.
        return Value::String("<no sync_runs rows — file-backed source>".into());
    };

    let status: Option<String> = row.try_get("status").ok();
    let summary: Option<String> = row.try_get("summary").ok();
    let summary = summary
        .map(|s| serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)))
        .unwrap_or(Value::Null);
    serde_json::json!({
        "status": status,
        "summary": summary,
    })
}

/// Whole-table bookkeeping that legitimately changes across a reset, so it
/// is excluded from the content-stability comparison:
const NON_CONTENT_TABLES: &[&str] = &["sync_runs", "sync_scope_state", "sync_scope_config"];

/// Dump only the entity *content* tables of a doltlite DB for the
/// --reset-and-redownload stability assertion: drops every
/// `*_bookkeeping` sidecar (per-fetch stamps + the `volatile_payload`
/// split-outs) plus [`NON_CONTENT_TABLES`].
fn content_tables(path: &Path) -> Value {
    let mut v = dump_doltlite_db(path);
    if let Value::Object(map) = &mut v {
        map.retain(|table, _| {
            !table.ends_with("_bookkeeping") && !NON_CONTENT_TABLES.contains(&table.as_str())
        });
    }
    v
}

/// Walk `root` and emit one snapshot per file. Each snapshot lives at
/// `<snap_base()>/<top>/<rel_dir>/<filename>.snap` (i.e. under
/// `$DATALIB_MANUAL_E2E_DIR/snapshots`), mirroring the data layout.
/// `manifest` collects the snapshot key (top + rel path)
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
        // Doltlite's sidecar `-lock` files are ephemeral and
        // content-free; `CACHEDIR.TAG` is a backup hint asserted
        // separately. Neither belongs in a golden.
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.ends_with("-lock") || n == "CACHEDIR.TAG")
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
        let snap_dir = snap_base().join(top).join(snap_parent);
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

/// Delete per-stanza `.snap` files that no longer correspond to a manifest
/// key — orphans from a run whose produced paths have since changed. No-op
/// outside update mode, which must never mutate the version-controlled golden.
fn prune_orphan_snapshots(manifest: &[String]) {
    if !insta_update_mode() {
        return;
    }
    let base = snap_base();
    if !base.is_dir() {
        return;
    }
    let keys: std::collections::HashSet<&str> = manifest.iter().map(String::as_str).collect();
    for entry in WalkDir::new(&base) {
        let entry = entry.expect("walk snapshot tree");
        if !entry.file_type().is_file() {
            continue;
        }
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("snap") {
            continue;
        }
        // Snapshot path mirrors the data layout, so only the per-stanza
        // tree snaps are manifest-keyed — hence pruning only under a `raw/`
        // or `rendered_md/` segment.
        let rel = p.strip_prefix(&base).unwrap().to_string_lossy().to_string();
        let key = rel.strip_suffix(".snap").unwrap_or(&rel);
        let is_tree_snap = key
            .split('/')
            .any(|seg| seg == "raw" || seg == "rendered_md");
        if is_tree_snap && !keys.contains(key) {
            std::fs::remove_file(p)
                .unwrap_or_else(|e| panic!("delete orphan snapshot {}: {e}", p.display()));
        }
    }
    remove_empty_dirs(&base);
}

/// True when insta is writing snapshots (the `.update` target sets
/// `INSTA_UPDATE=always`).
fn insta_update_mode() -> bool {
    matches!(
        std::env::var("INSTA_UPDATE").ok().as_deref(),
        Some("always") | Some("force") | Some("new") | Some("unseen") | Some("1")
    )
}

fn remove_empty_dirs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        if p.is_dir() {
            remove_empty_dirs(&p);
            let _ = std::fs::remove_dir(&p); // succeeds only when empty
        }
    }
}

enum SnapValue {
    Json(Value),
    Text(String),
}

/// File → snapshot payload. JSONL and JSON are parsed, sorted and stripped of
/// volatile fields; markdown is text with frontmatter redactions;
/// `.doltlite_db` files are dumped as `{table: [rows]}` so the goldens carry
/// the actual raw payloads. Anything else becomes a size marker.
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
            Err(_) => SnapValue::Text(normalize_str(&text)),
        }
    } else if name.ends_with(".md") {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        SnapValue::Text(normalize_str(&redact_markdown(&text)))
    } else {
        // Try text first, fall back to a size marker for binary.
        match std::fs::read_to_string(path) {
            Ok(t) => SnapValue::Text(normalize_str(&t)),
            Err(_) => {
                let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
                SnapValue::Text(format!("<binary {size} bytes>"))
            }
        }
    }
}

fn rewrite_config(text: &str, data_root: &Path) -> String {
    let mut doc: toml::Table = toml::from_str(text).expect("parse config toml");
    doc.insert(
        "data_root".into(),
        toml::Value::String(data_root.display().to_string()),
    );

    let steps = doc
        .get_mut("steps")
        .and_then(|v| v.as_array_mut())
        .expect("config must have a `[[steps]]` array (DAG format)");

    let mut patched_slack = 0usize;
    for step in steps.iter_mut() {
        let Some(m) = step.as_table_mut() else {
            continue;
        };
        let command = m
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        assert!(
            !command.starts_with("datalib-step qmd_index"),
            "config declares a qmd_index step; the golden test deliberately \
             excludes qmd (non-deterministic status text) — drop the step"
        );
        if command != "datalib-step download slack_api" {
            continue;
        }
        let params = m
            .entry("params")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(params_map) = params.as_table_mut() else {
            continue;
        };
        let sync_entry = params_map
            .entry("sync")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(sync_map) = sync_entry.as_table_mut() {
            sync_map.insert("refresh_window_days".into(), toml::Value::Integer(30));
            patched_slack += 1;
        }
    }
    // Warn rather than fail: a reduced config (one provider, for debugging via
    // DATALIB_TEST_CONFIG) legitimately has no slack step, and a hard assert
    // here would make the test unusable for exactly that. On the real config
    // this line firing means the tweak silently did nothing — the command
    // string drifted — and you'd see spurious media churn in the goldens.
    if patched_slack == 0 {
        eprintln!(
            "[test] WARNING: no `datalib-step download slack_api` step; \
             refresh_window_days tweak not applied"
        );
    }

    toml::to_string(&doc).expect("serialize toml")
}

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
        // Golden-test dump: `t` is a table name this test just read out of the
        // store's own `sqlite_master`.
        let info = sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info(\"{t}\")")))
            .fetch_all(&pool)
            .await
            .expect("table_info");
        let columns: Vec<String> = info
            .iter()
            .map(|r| r.try_get::<String, _>("name").unwrap_or_default())
            // `volatile_payload` holds churn by definition — that is why
            // it was split off the content payload. Snapshotting it would
            // make the golden non-deterministic.
            .filter(|c| c != "volatile_payload")
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
        // Same: `select_list` is built from the columns just introspected and
        // `order_by` is one of a closed set of literals.
        let rows = sqlx::query(sqlx::AssertSqlSafe(q))
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
        // Table-scoped redaction, applied here because this is the one place
        // that knows the table name for certain (see `TABLE_VOLATILE_KEYS`).
        if let Some((_, keys)) = TABLE_VOLATILE_KEYS.iter().find(|(name, _)| *name == t) {
            for row in row_vals.iter_mut() {
                if let Value::Object(map) = row {
                    for k in *keys {
                        if let Some(slot) = map.get_mut(*k) {
                            *slot = Value::String(REDACTED.into());
                        }
                    }
                }
            }
        }
        out.insert(t, Value::Array(row_vals));
    }
    Value::Object(out)
}

fn strip_volatile(v: &mut Value) {
    match v {
        Value::Object(map) => {
            let repo = is_github_repo_object(map);
            for (k, child) in map.iter_mut() {
                if VOLATILE_KEYS.contains(&k.as_str())
                    || (repo && REPO_VOLATILE_KEYS.contains(&k.as_str()))
                {
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
        // Normalize the tempdir data_root out of any embedded path string.
        Value::String(s) => *s = normalize_str(s),
        _ => {}
    }
}

/// Redaction for the run-2 incrementality snapshot: stricter on time fields,
/// looser on stats. Unlike [`strip_volatile`] it preserves per-source counts,
/// since asserting those stayed small is the whole point.
fn strip_volatile_for_incrementality(v: &mut Value) {
    // Reuse most of `VOLATILE_KEYS` but drop `stats` (preserved) and
    // add timing/jitter fields specific to per-source summaries.
    const RUN2_VOLATILE_KEYS: &[&str] = &[
        // shared with strip_volatile
        "_recorded_at",
        "duration_ms",
        "_item_hashes",
        "request_id",
        "fetched_at",
        "last_edited_time",
        "created_time",
        "cache_ts",
        // `updated` intentionally omitted — split into the volatile_payload
        // sidecar at extract time, not redacted here (see strip_volatile).
        "started_at",
        "finished_at",
        "duration_secs",
        "data_root",
        "qmd_status",
        "source_fingerprint",
        "last_attempt_at",
        "first_seen_at",
        "last_finished_at",
        "last_seen_at",
        "local_time",
        "expiry_time",
        // run-2-specific jitter inside the per-source `stats`:
        // wall-clock timings + the cursor `before`/`after` ISO
        // timestamps + network/elapsed jitter. The cursor `scope`
        // names are NOT redacted — those are the signal proving the
        // scope advanced.
        "elapsed_ms",
        "network_seconds",
        "before",
        "after",
        // The `commit_hash` flips between any two runs even when the
        // data is byte-identical, since dolt commit hashes include
        // wall-clock timestamps. The content-equality is already
        // covered by `deltas` showing few/no rows changed.
        "commit_hash",
        // `load.write_lock` timings + the extract-metrics per-db byte sizes
        // are wall-clock / layout jitter; row counts carry the real signal.
        "avg_hold_ms",
        "avg_wait_ms",
        "total_hold_ms",
        "total_wait_ms",
        "bytes_before",
        "bytes_after",
        "bytes_delta",
    ];
    match v {
        Value::Object(map) => {
            let repo = is_github_repo_object(map);
            for (k, child) in map.iter_mut() {
                if RUN2_VOLATILE_KEYS.contains(&k.as_str())
                    || (repo && REPO_VOLATILE_KEYS.contains(&k.as_str()))
                {
                    *child = Value::String(REDACTED.into());
                    continue;
                }
                strip_volatile_for_incrementality(child);
                if SORTED_ARRAY_KEYS.contains(&k.as_str()) {
                    if let Value::Array(items) = child {
                        items.sort_by_key(|a| a.to_string());
                    }
                }
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                strip_volatile_for_incrementality(item);
            }
        }
        // Normalize the data_root prefix, and scrub the volatile `commit=<hash>`
        // out of the preserved per-source `stats` line.
        Value::String(s) => *s = scrub_commit(&normalize_str(s)),
        _ => {}
    }
}
