// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live end-to-end golden bake for the `datalib-pipeline` pipeline.
//!
//! It runs only in snapshot-update mode, and [`require_update_mode`]
//! says why. The snapshots are a diff to read, not a gate to pass; the
//! `assert!`s around them — step statuses, layout invariants, the
//! run-3 content-stability check — are the pass/fail part, and they
//! hold in update mode exactly as they would in compare mode.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use datalib_contacts::{ContactKind, Store};
use datalib_dag::RunState;
use datalib_handle::Handle;
use datalib_store_dump::{cell_value, open_readonly, store_as_jsonl, Keep};
use insta::{assert_json_snapshot, assert_snapshot};
use serde_json::Value;
use walkdir::WalkDir;

mod manual_e2e_occurrences;

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

/// Blank the `Size` cell of a storage table row (`| 658.4 KiB |`): a
/// store's size on disk wobbles run-to-run at equal row counts, which is
/// why `source_measurements.bytes` is redacted too. The row's `Count`
/// cell (`16 rows`, `4 files`) is the signal and stays.
fn scrub_sizes(s: &str) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let is_size = |cell: &str| {
        let cell = cell.trim();
        UNITS.iter().any(|u| {
            cell.strip_suffix(u)
                .is_some_and(|n| !n.is_empty() && n.trim_end().parse::<f64>().is_ok())
        })
    };
    s.lines()
        .map(|line| {
            if !line.starts_with('|') || !line.split('|').any(is_size) {
                return line.to_string();
            }
            line.split('|')
                .map(|cell| if is_size(cell) { " <size> " } else { cell })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn normalize_str(s: &str) -> String {
    scrub_sizes(&scrub_presigned(&norm_data_root(s)))
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

const VOLATILE_KEYS: &[&str] = &[
    "_recorded_at",
    "duration_ms",
    "_item_hashes",
    "request_id",
    "fetched_at_utc",
    "last_edited_time",
    "created_time",
    "cache_ts",
    // NB: `updated` is deliberately NOT redacted. It is per-fetch bookkeeping
    // that the pipeline splits into the `volatile_payload` sidecar (which this
    // dump excludes), so it never reaches a content table. If a new provider's
    // volatile `updated` shows up as golden churn, split it at the source —
    // don't re-add it here.

    // Fields of `sync_summary_<now>.json` that don't reproduce byte-identically,
    // and `sync_runs`' wall-clock pair in every raw store.
    "started_at",
    "finished_at",
    "started_at_utc",
    "finished_at_utc",
    "duration_secs",
    "data_root",
    // Per-source stats are provider-specific and capture counts of
    // items fetched; on a live API they jitter with whatever new
    // activity has happened. Snapshot the structure, not the values.
    "stats",
    // Per-row bookkeeping in the doltlite raw stores. Stamped to
    // "now" on every fetch attempt, so they churn on every run even
    // when the upstream payload is byte-identical.
    "last_attempt_at_utc",
    // NB: `captured_at` was here under the same heading and should not be —
    // the only emitter is `media_visual.captured_at`, the moment the shutter
    // opened, parsed out of fixed EXIF bytes. Redacting it meant the golden
    // could not catch a regression in EXIF timestamp parsing.

    // CAS blob "first stored" wall-clock stamp. Identical bytes land at the
    // same PK, but the timestamp is whenever this run first wrote them.
    // A problem's two stamps are wall-clock too.
    "first_seen_at_utc",
    "changed_at_utc",
    // Resume-cursor / bookkeeping wall-clock stamps: `sync_scope_state`'s
    // `last_finished_at_utc` + `after` (real now when the scope ran, not the
    // `--now` arg), and `last_seen_at_utc` (when a row was last fetched). The
    // cursor `before` is a `--now`-relative window start and stays put, as do
    // genuine upstream content times (`last_sign_in_at`, `latest_build_*`).
    "last_finished_at_utc",
    "after",
    "last_seen_at_utc",
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
    // file_inventory's `scan_meta` columns. `last_scan_at_utc` is wall-clock, so it
    // churns run-to-run even when the scanned bytes are identical, and
    // `scanner_version` is redacted so a version bump doesn't churn the
    // golden.
    "mtime_ns",
    "ctime_ns",
    "inode",
    "dev",
    "last_scan_at_utc",
    "scanner_version",
    // The offset the clock was in when a stamp above was minted (AGENTS.md
    // "Timestamp convention"): a property of the machine that baked, not
    // of the data, and it rides beside stamps that are already redacted.
    "tz_offset",
    // The raw-store commit a render store's `render_cursor` row was
    // rendered from: a dolt hash, so it moves on every run like
    // `commit_hash`.
    "raw_commit",
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

/// A `grid_rows` row of the storage page whose `byte_size` is a store's
/// size on disk — the same number `source_measurements.bytes` and the
/// page's `Size` cell carry, redacted for the same reason. A row of any
/// other kind keeps its `byte_size`: there it measures the content.
fn is_storage_row(map: &serde_json::Map<String, Value>) -> bool {
    matches!(
        map.get("kind").and_then(Value::as_str),
        Some("Source Size") | Some("Store")
    )
}

/// Per-TABLE volatile columns: `(table, keys)` redacted only in rows of that
/// table. Applied in [`redact_row`], which knows the table name for
/// certain — no shape-sniffing required.
const TABLE_VOLATILE_KEYS: &[(&str, &[&str])] = &[
    // The store's record of which build wrote it: the stamp is the wall
    // clock, and the `git_hash` row (redacted below, by key) is a new
    // commit on every bake. The versions and the schema hash stay.
    ("_datalib_meta", &["written_at_utc"]),
    // A store's size on disk wobbles run-to-run at equal row counts
    // (page layout, chunk ordering), the same way the extract-metrics
    // `bytes_*` did; `items` carries the signal.
    ("source_measurements", &["bytes"]),
];

/// Stores dumped as one row count per table instead of row by row. A
/// mirror source's store is every table of an application's own
/// database — 78 for Photos, 113 for Lightroom, ~275 for WhatsApp — and
/// whether the copy is faithful is not this test's question:
/// `sqlite_mirror`'s roundtrip tests, `lightroom`'s `real_catalogs` and
/// `apple_photos`'s `photos_roundtrip` pin that, byte for byte. What this
/// golden has to show is that the step ran and what it produced, and for
/// that the set of tables and their sizes is the whole story: a table
/// gone from the schema, a shadow table let back in, or a count that
/// moved all diff, without twenty thousand lines around them. WhatsApp's
/// message content still lands in the golden — through its
/// `render_markdown` tree.
///
/// The two whole calendars' raw stores are thousands of events each;
/// what they are here for is their occurrences, which `occurrences.snap`
/// lists one a line and `occurrences_vs_service` checks against the
/// service, so their stores are counted too.
///
/// Keyed by `(stanza, step)` — the group id in dag.toml, which is also
/// what names the snapshot directory, and the step whose tree the store
/// sits in; `"*"` is every step of that stanza. A store not listed is
/// dumped in full. Lightroom names only its `ingest`, because its render
/// store is not a mirror of anything: it is the grid row per photo that
/// this golden is here to show.
const ROW_COUNT_ONLY_STEPS: &[(&str, &str)] = &[
    ("apple_photos", "*"),
    ("lightroom", "ingest"),
    ("whatsapp", "*"),
    ("google_calendar_primary", "*"),
    ("fastmail_calendar_team", "*"),
];

/// Tables dumped as a row count inside an otherwise full dump, as
/// `(stanza, table)`; `"*"` is every stanza. Three reasons a table is
/// here, none of them "it is big":
///
/// - Slack's `channels` and `users` are the workspace-wide listings —
///   every channel and every member — that `SKIP_PATH_SEGMENTS` keeps
///   out of the golden as files; the tables must not let them back in.
///   Linear's lists of people, teams, states, labels, statuses and
///   initiatives are the same: read whole whatever projects are chosen.
///   So are Discord's channels, threads and roles of a server, whatever
///   channels are chosen.
/// - A sensor series (`airvisual_samples`, `garmin_daily`) is thousands
///   of rows whose values the provider's own fixtures pin; here the
///   count says the step read the files, and the rendered plots say
///   what it made of them.
/// - `render_inputs`, `sections`, `drawings` and the agent transcripts'
///   `records` repeat, row by row, what the documents' `.md` snapshots, the
///   `index_text` snapshots and `grid_rows` beside them already show.
///
/// Notion's `tree` and `listed_pages` are the first kind, and the one
/// pair here that is also a tripwire. A roots source walks down from its
/// roots, so they hold what sits under them and nothing more; a source
/// mirroring a whole workspace, or a store an older build left, holds
/// the id, title and parent of every object the credential can see.
/// Counting them means a run that goes back to listing a whole workspace
/// reads as one number jumping from a handful to tens of thousands,
/// rather than as a hundred thousand new lines of somebody's workspace
/// committed to this repo.
const ROW_COUNT_ONLY_TABLES: &[(&str, &str)] = &[
    ("tiny-slack", "channels"),
    ("tiny-slack", "channels_bookkeeping"),
    ("tiny-slack", "users"),
    ("tiny-slack", "users_bookkeeping"),
    ("linear", "users"),
    ("linear", "users_bookkeeping"),
    ("linear", "teams"),
    ("linear", "teams_bookkeeping"),
    ("linear", "workflow_states"),
    ("linear", "workflow_states_bookkeeping"),
    ("linear", "issue_labels"),
    ("linear", "issue_labels_bookkeeping"),
    ("linear", "project_statuses"),
    ("linear", "project_statuses_bookkeeping"),
    ("linear", "initiatives"),
    ("linear", "initiatives_bookkeeping"),
    ("discord", "channels"),
    ("discord", "channels_bookkeeping"),
    ("discord", "roles"),
    ("discord", "roles_bookkeeping"),
    ("airvisual", "airvisual_samples"),
    ("garmin", "garmin_daily"),
    ("garmin", "garmin_daily_bookkeeping"),
    ("notion-api", "tree"),
    ("notion-api", "tree_bookkeeping"),
    ("notion-api", "listed_pages"),
    ("claude_code", "records"),
    ("codex", "records"),
    ("*", "render_inputs"),
    ("*", "sections"),
    ("*", "drawings"),
];

/// The step a store sits in: the `<sub>` of
/// `<data_root>/<stanza>/<sub>/<file>`.
fn step_of(path: &Path) -> Option<String> {
    Some(path.parent()?.file_name()?.to_string_lossy().into_owned())
}

/// The stanza a store belongs to: `<data_root>/<stanza>/<sub>/<file>`.
fn stanza_of(path: &Path) -> Option<String> {
    Some(
        path.parent()?
            .parent()?
            .file_name()?
            .to_string_lossy()
            .into_owned(),
    )
}

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
             DATALIB_BINARY_DIR to a directory holding datalib-pipeline + datalib-step"
        )
    });
    let dir = r
        .rlocation(REL)
        .unwrap_or_else(|| panic!("rlocation failed for {REL}"));
    for name in ["datalib-pipeline", "datalib-step"] {
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

/// Refuse to run outside snapshot-update mode, before anything is
/// fetched.
///
/// A compare run costs three full pipeline passes against real
/// provider APIs and then reports a diff that was always going to be
/// accepted: upstream moves on its own, so these snapshots record what
/// the world looked like at the last bake rather than a contract the
/// code has to meet. Baking and reading the diff is the only way this
/// test has ever been used.
///
/// The modes that write a `.snap` in place and pass are `always` and
/// `force`; `auto` / `new` / `unseen` write `.snap.new` beside it and
/// fail, which is the same waste with extra litter.
fn require_update_mode() {
    let mode = std::env::var("INSTA_UPDATE").unwrap_or_default();
    if matches!(mode.as_str(), "always" | "1" | "force") {
        return;
    }
    let saw = if mode.is_empty() {
        "unset".to_string()
    } else {
        format!("{mode:?}")
    };
    panic!(
        "this test only runs in snapshot-update mode (INSTA_UPDATE is {saw}).\n\
         \n\
         It is a bake-and-eyeball tool, not a pass/fail gate — see the module \
         header. Run it as:\n\
         \n    datalib/backend/dag/manual_e2e_run.sh\n\
         \n\
         which is `bazel run \
         //datalib/backend/dag:manual_e2e_live_sync_golden.update`, and then \
         read the diff in $DATALIB_MANUAL_E2E_DIR/snapshots."
    );
}

#[test]
#[ignore]
fn manual_e2e_live_sync_golden() {
    require_update_mode();

    let explicit = std::env::var("DATALIB_TEST_CONFIG").ok();
    let src_config = match &explicit {
        Some(p) => PathBuf::from(p),
        None => e2e_dir()
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
    let cfg_text = upgraded_config(
        &src_config,
        std::fs::read_to_string(&src_config).expect("read config"),
        explicit.is_none(),
    );

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

    // The config lives inside the data root, where `datalib-http` looks
    // for it, and names no `data_root` (the loader defaults to the
    // config's directory). That makes `data/` a complete root the app
    // can be pointed at after the bake — and one that can be moved.
    let cfg_out = rewrite_config(&cfg_text);
    let cfg_path = data_root.join("config.toml");
    std::fs::write(&cfg_path, &cfg_out).unwrap();

    // `//datalib/backend:bin` stages datalib-pipeline and datalib-step side by
    // side under their public names, so the runner finds the step binary via
    // its own-directory fallback — no `--binary-dir` needed.
    let bin = bin_dir().join("datalib-pipeline");
    eprintln!("[test] dag bin = {}", bin.display());
    eprintln!("[test] data_root = {}", data_root.display());
    eprintln!(
        "[test] after the bake: datalib-http {}",
        data_root.display()
    );

    let now = "2026-05-21T18:00:00Z";
    let run1 = run_pipeline(&bin, &cfg_path, &run_root, now, &["sync"]);
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

    // Layout invariant: data_root is a flat set of group dirs plus the one
    // reserved `system/` dir — never the old top-level `raw/` or
    // `rendered_md/`, and the aggregate index DB lives under the
    // `unified_grid_and_search_index` group. Guards against a regression to the
    // pre-grouping layout.
    assert!(
        !data_root.join("raw").exists() && !data_root.join("rendered_md").exists(),
        "old top-level raw/ or rendered_md/ found — data_root must be grouped by group id"
    );
    assert!(
        data_root
            .join("unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite")
            .is_file(),
        "backend index DB must live at unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite"
    );
    // Only the genuinely-derived index dirs are tagged as rebuildable cache so
    // `--exclude-caches` backups skip them (the per-group `render_markdown/` tags
    // are checked implicitly via the manifest — CACHEDIR.TAG is skipped in the
    // walk below). `system/` must NOT be tagged — job logs are operational
    // history, not rebuildable from raw.
    assert!(
        data_root
            .join("unified_grid_and_search_index/CACHEDIR.TAG")
            .is_file(),
        "unified_grid_and_search_index/ must carry a CACHEDIR.TAG marking the index tree as derived cache"
    );
    assert!(
        !data_root.join("system/CACHEDIR.TAG").exists(),
        "system/ (operational history) must NOT be tagged as cache"
    );

    // Snapshot each group's stores, mirroring the on-disk per-group layout:
    // `ingest/`, `render_markdown/`, a time-series source's
    // `render_time_series/` and the time-series index's `time_series_index/`.
    // `system/` and the grid and search index are skipped.
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
        for sub in TREE_SNAPSHOTS {
            let dir = data_root.join(stanza).join(sub);
            snapshot_tree(&dir, &format!("{stanza}/{sub}"), &mut manifest);
        }
    }
    for stanza in &stanzas {
        snapshot_documents(&data_root, stanza, &mut manifest);
    }
    snapshot_index_text(&data_root, &cfg_out, &mut manifest);
    manifest.sort();

    // Prune snapshots orphaned when the set of produced files changes.
    // insta's `INSTA_UNREFERENCED=delete` is a cargo-insta feature that does
    // not fire under `bazel run` and would not scan our external snapshot
    // tree anyway. Update mode only.
    prune_orphan_snapshots(&manifest, &data_root);

    // Manifest pins which files we expect to find. Catches additions /
    // removals without having to diff every per-file snapshot.
    insta::with_settings!({
        snapshot_path => snap_base().display().to_string(),
        prepend_module_to_snapshot => false,
    }, {
        assert_snapshot!("manifest", manifest.join("\n"));
    });

    // The index itself is the union of the render stores snapshotted
    // above and is not dumped again. A step's problems stay in its own
    // stores (etl/README.md § "Problems stay in the step that found
    // them"), so each store's dump above shows them.

    // The search index: what each searched source holds, and what its
    // embed step left.
    insta::with_settings!({
        snapshot_path => snap_base().join("unified_grid_and_search_index").display().to_string(),
        prepend_module_to_snapshot => false,
        sort_maps => true,
        description => "the search index: documents and chunks left to embed, per source",
    }, {
        assert_json_snapshot!("search_index", search_index_report(&data_root, &cfg_out));
    });

    // The search terms: what each row answers to when an id or a handle is
    // pasted into the search. Derived from `grid_rows`, so a change here
    // with no change above is a change in the derivation.
    insta::with_settings!({
        snapshot_path => snap_base().join("unified_grid_and_search_index").display().to_string(),
        prepend_module_to_snapshot => false,
        description => "unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite#terms, one term a line: kind, value, uuid",
    }, {
        assert_snapshot!("search_terms", search_terms(&data_root));
    });

    // Every calendar's occurrences, one a line, and each calendar
    // mirrored whole beside what the service lists for the same days
    // (`manual_e2e_occurrences.rs`).
    for stanza in stanzas
        .iter()
        .filter(|s| data_root.join(s).join("render_occurrences").is_dir())
    {
        insta::with_settings!({
            snapshot_path => snap_base().join(stanza).display().to_string(),
            prepend_module_to_snapshot => false,
            description => "render_occurrences, one occurrence a line (JSONL), payload left out",
        }, {
            assert_snapshot!("occurrences", manual_e2e_occurrences::listing(&data_root, stanza));
        });
    }
    for (stanza, method, account) in manual_e2e_occurrences::whole_calendar_sources(&cfg_out) {
        let report = manual_e2e_occurrences::compare(&data_root, &stanza, &method, &account);
        insta::with_settings!({
            snapshot_path => snap_base().join(&stanza).display().to_string(),
            prepend_module_to_snapshot => false,
            description => "render_occurrences beside the service's own listing of the same window",
        }, {
            assert_json_snapshot!("occurrences_vs_service", report);
        });
    }

    // ── The contacts app: links a person made, which nothing rebuilds ──
    //
    // Made once run 1 has put the handles in the index, and checked after
    // run 3's reset: no step may write `datalib_curated/`, so the store
    // must come through runs 2 and 3 row for row, and each handle it links
    // must still be in the re-downloaded data.
    let contact_specs = read_contact_specs(&src_config.with_file_name("contacts.toml"));
    assert_handles_indexed(&data_root, &contact_specs, "run 1");
    let contacts = make_contacts(&data_root, &contact_specs);
    let contacts_store = datalib_contacts::store_path(&data_root);
    let contacts_before = dump_store(&contacts_store);
    insta::with_settings!({
        snapshot_path => snap_base().join(datalib_contacts::CURATED_DIR).display().to_string(),
        prepend_module_to_snapshot => false,
        description => "datalib_curated/datalib_contacts/contacts.doltlite_db",
    }, {
        assert_json_snapshot!(
            datalib_contacts::APP_DIR,
            contacts_for_snapshot(contacts_before.clone(), &contacts)
        );
    });

    // ── Second run: incrementality check ──────────────────────────────
    let ingest_steps: Vec<String> = stanzas
        .iter()
        .filter(|s| data_root.join(s).join("ingest").is_dir())
        .map(|s| format!("{s}/ingest"))
        .collect();
    assert!(
        !ingest_steps.is_empty(),
        "no <group>/ingest under {}",
        data_root.display()
    );
    let ingest_stores_at = |data_root: &Path| -> Vec<StoreAtCommit> {
        ingest_steps
            .iter()
            .map(|step| data_root.join(step).join("entities.doltlite_db"))
            .filter(|p| p.is_file())
            .map(|p| StoreAtCommit::head(data_root, &p))
            .collect()
    };
    let stores_before_run2 = ingest_stores_at(&data_root);

    let now2 = "2026-05-21T18:05:00Z";
    let run2 = run_pipeline(&bin, &cfg_path, &run_root, now2, &["sync"]);
    assert!(
        run2.status.success(),
        "pipeline run 2 failed (exit {:?}). Last stderr:\n{}",
        run2.status.code(),
        run2.stderr_tail(40)
    );
    let summary2 = run2.run_summary().expect("run 2 run_summary");
    assert_step_statuses_ok(&summary2);

    // Run 1's problems are all still standing minutes later, and a problem
    // recorded again unchanged keeps its row, stamps and all, so run 2
    // must leave every store's `problems` table as it found it. A row
    // here is either a stamp moving on every sync again or an upstream
    // that changed what the problem says; the scope key says which one.
    let problems_moved: Vec<String> = stores_before_run2
        .iter()
        .filter_map(StoreAtCommit::problems_moved_to_head)
        .collect();
    assert!(
        problems_moved.is_empty(),
        "run 2 changed problems that were standing since run 1:\n{}",
        problems_moved.join("\n")
    );

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

    // ── Third run: reset every source, then re-download every one ─────
    //
    // Every ingest step is emptied with its CAS and synced again, and
    // each store's own history is the witness: the diff from the commit
    // before the reset to the commit after the refill must be empty on
    // every content table. Re-fetching an unchanged upstream object
    // must land identical bytes at the same key, so a row that differs
    // is per-fetch bookkeeping leaking into a content payload.
    let stores_before = ingest_stores_at(&data_root);

    let now3 = "2026-05-21T18:10:00Z";
    let reset_all = ingest_steps.join(",");
    let sync_all = ingest_steps.join(",");
    let reset = run_pipeline(&bin, &cfg_path, &run_root, now3, &["reset", &reset_all]);
    assert!(
        reset.status.success(),
        "pipeline run 3's reset failed (exit {:?}). Last stderr:\n{}",
        reset.status.code(),
        reset.stderr_tail(40)
    );
    let run3 = run_pipeline(&bin, &cfg_path, &run_root, now3, &["sync", &sync_all]);
    assert!(
        run3.status.success(),
        "pipeline run 3 (reset) failed (exit {:?}). Last stderr:\n{}",
        run3.status.code(),
        run3.stderr_tail(40)
    );
    assert_step_statuses_ok(&run3.run_summary().expect("run 3 run_summary"));

    assert!(
        dump_store(&contacts_store) == contacts_before,
        "the contacts store changed across run 2 and run 3's reset; no step may \
         write {}",
        contacts_store.display()
    );
    assert_handles_indexed(&data_root, &contact_specs, "run 3's re-download");
    assert_contacts_resolve(&contacts_store, &contacts);

    let drifts: Vec<String> = stores_before
        .iter()
        .flat_map(StoreAtCommit::content_drift_to_head)
        .collect();
    assert!(
        drifts.is_empty(),
        "content tables drifted across a reset and re-download.\n\
         Re-fetching an unchanged upstream object must land identical \
         bytes, so a drifting field is per-fetch bookkeeping leaking into \
         a content payload — declare it in that entity's \
         *_VOLATILE_PATHS and route the upsert through \
         bulk_upsert_with_tape_split (see data_architecture_ingestion.md \
         §\"Volatile-field split\").\n\n{}",
        drifts.join("\n")
    );
}

/// One raw store and the commit it was at before a run; what the store's
/// own diff says moved between then and `HEAD` is the finding — its
/// problems across run 2, its content across run 3's reset.
struct StoreAtCommit {
    name: String,
    path: PathBuf,
    commit: String,
    tables: Vec<String>,
}

impl StoreAtCommit {
    fn head(data_root: &Path, path: &Path) -> Self {
        let name = path
            .strip_prefix(data_root)
            .unwrap_or(path)
            .to_string_lossy()
            .into_owned();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime for store head");
        let (commit, tables) = rt.block_on(async {
            use sqlx::Row;

            let pool = open_readonly(path).await;
            let commit: String = sqlx::query("SELECT commit_hash FROM dolt_log() LIMIT 1")
                .fetch_one(&pool)
                .await
                .unwrap_or_else(|e| panic!("{name}: dolt_log: {e}"))
                .get(0);
            let tables = content_table_names(&pool).await;
            pool.close().await;
            (commit, tables)
        });
        StoreAtCommit {
            name,
            path: path.to_path_buf(),
            commit,
            tables,
        }
    }

    /// The `problems` rows that changed between this commit and `HEAD`,
    /// by scope key; `None` when none did.
    fn problems_moved_to_head(&self) -> Option<String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime for store diff");
        rt.block_on(async {
            use sqlx::Row;

            let pool = open_readonly(&self.path).await;
            let has_problems: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='problems'",
            )
            .fetch_optional(&pool)
            .await
            .expect("read sqlite_master");
            if has_problems.is_none() {
                pool.close().await;
                return None;
            }
            let rows = sqlx::query(
                "SELECT diff_type, COALESCE(to_scope_key, from_scope_key) AS scope_key \
                   FROM dolt_diff_problems WHERE from_ref = ? AND to_ref = 'HEAD' \
                  ORDER BY scope_key",
            )
            .bind(&self.commit)
            .fetch_all(&pool)
            .await
            .unwrap_or_else(|e| panic!("{}#problems: dolt_diff_problems: {e}", self.name));
            pool.close().await;
            let moved: Vec<String> = rows
                .iter()
                .map(|r| {
                    let kind: String = r.get("diff_type");
                    let key: Option<String> = r.get("scope_key");
                    format!("{kind} {}", key.unwrap_or_default())
                })
                .collect();
            (!moved.is_empty()).then(|| format!("{}#problems: {}", self.name, moved.join(", ")))
        })
    }

    fn content_drift_to_head(&self) -> Vec<String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime for store diff");
        rt.block_on(self.content_drift_to_head_async())
    }

    async fn content_drift_to_head_async(&self) -> Vec<String> {
        use sqlx::{Column, Row};

        let pool = open_readonly(&self.path).await;
        let mut findings = Vec::new();
        let now = content_table_names(&pool).await;
        for t in &self.tables {
            if !now.contains(t) {
                findings.push(format!("{}#{t}: gone after the re-download", self.name));
            }
        }
        for t in &now {
            if !self.tables.contains(t) {
                findings.push(format!("{}#{t}: new after the re-download", self.name));
                continue;
            }
            let stat = sqlx::query(
                "SELECT rows_added, rows_deleted, rows_modified FROM dolt_diff_stat(?, 'HEAD', ?)",
            )
            .bind(&self.commit)
            .bind(t)
            .fetch_optional(&pool)
            .await
            .unwrap_or_else(|e| panic!("{}#{t}: dolt_diff_stat: {e}", self.name));
            // No row at all is how an unchanged table answers.
            let Some(stat) = stat else {
                continue;
            };
            let (added, deleted, modified): (i64, i64, i64) =
                (stat.get(0), stat.get(1), stat.get(2));
            if added + deleted + modified == 0 {
                continue;
            }
            // Which columns moved, row by row: `dolt_diff_<t>` pairs every
            // column as `from_<c>` / `to_<c>`. A modified row whose only
            // differences are known stamp leaks is not a finding. The
            // table name was read out of this store's own `sqlite_master`,
            // and the refs are bound.
            // `payload` is JSONB, so it is read back through `json()`.
            let info = sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info(\"{t}\")")))
                .fetch_all(&pool)
                .await
                .expect("table_info");
            let select_list = info
                .iter()
                .map(|r| r.get::<String, _>("name"))
                .flat_map(|c| {
                    ["from", "to"].map(|side| {
                        if c == "payload" {
                            format!("json({side}_payload) AS {side}_payload")
                        } else {
                            format!("\"{side}_{c}\"")
                        }
                    })
                })
                .chain(["diff_type".to_string()])
                .collect::<Vec<_>>()
                .join(", ");
            let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT {select_list} FROM \"dolt_diff_{t}\" WHERE from_ref = ? AND to_ref = 'HEAD'"
            )))
            .bind(&self.commit)
            .fetch_all(&pool)
            .await
            .unwrap_or_else(|e| panic!("{}#{t}: dolt_diff_{t}: {e}", self.name));
            let exempt = |c: &str| KNOWN_STAMP_LEAKS.contains(&(t.as_str(), c));
            let mut moved: std::collections::BTreeMap<String, usize> =
                std::collections::BTreeMap::new();
            let mut leaked_only = 0usize;
            let mut example: Option<String> = None;
            for row in &rows {
                let kind: String = row.try_get("diff_type").unwrap_or_default();
                let mut here: Vec<String> = Vec::new();
                for col in row.columns() {
                    let Some(c) = col.name().strip_prefix("to_") else {
                        continue;
                    };
                    if c == "commit" || c == "commit_date" {
                        continue;
                    }
                    let from = format!("from_{c}");
                    if row.columns().iter().all(|k| k.name() != from) {
                        continue;
                    }
                    let a = cell_for_diff(row, &from);
                    let b = cell_for_diff(row, col.name());
                    if a == b || exempt(c) {
                        continue;
                    }
                    let (a, b) = if c == "payload" {
                        let keys: Vec<&str> = KNOWN_STAMP_LEAKS
                            .iter()
                            .filter(|(table, _)| *table == t)
                            .filter_map(|(_, path)| path.strip_prefix("payload."))
                            .collect();
                        (without_keys(&a, &keys), without_keys(&b, &keys))
                    } else {
                        (a, b)
                    };
                    if a == b {
                        continue;
                    }
                    *moved.entry(c.to_string()).or_default() += 1;
                    if here.len() < 4 {
                        here.push(format!("{c}: {} → {}", short_cell(&a), short_cell(&b)));
                    }
                }
                if kind == "modified" && here.is_empty() {
                    leaked_only += 1;
                    continue;
                }
                if example.is_none() {
                    example = Some(format!("{kind}: {}", here.join("; ")));
                }
            }
            if added + deleted == 0 && leaked_only as i64 == modified {
                eprintln!(
                    "[test] {}#{t}: {modified} row(s) differ only in a known stamp leak",
                    self.name
                );
                continue;
            }
            findings.push(format!(
                "{}#{t}: added={added} removed={deleted} modified={modified} \
                 (of which {leaked_only} only in a known stamp leak); columns \
                 differing: {}; e.g. {}",
                self.name,
                moved
                    .iter()
                    .map(|(c, n)| format!("{c}×{n}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                example.unwrap_or_default()
            ));
        }
        pool.close().await;
        findings
    }
}

/// A cell for comparison: a BLOB that is text — a JSON payload — is
/// compared as text, and any other BLOB by its bytes.
fn cell_for_diff(row: &sqlx::sqlite::SqliteRow, name: &str) -> Value {
    use sqlx::{Row, TypeInfo, ValueRef};

    let raw = row.try_get_raw(name).expect("try_get_raw");
    if !raw.is_null() && raw.type_info().into_owned().name() == "BLOB" {
        let bytes: Vec<u8> = row.try_get(name).unwrap_or_default();
        return match String::from_utf8(bytes) {
            Ok(text) => Value::String(text),
            Err(e) => {
                let bytes = e.into_bytes();
                use std::hash::{Hash, Hasher};
                let mut h = std::hash::DefaultHasher::new();
                bytes.hash(&mut h);
                Value::String(format!("<bytes {} hash={:016x}>", bytes.len(), h.finish()))
            }
        };
    }
    cell_value(row, name, false)
}

/// The JSON text with the named top-level keys removed; anything that is
/// not a JSON object comes back as it was.
fn without_keys(v: &Value, keys: &[&str]) -> Value {
    let Value::String(text) = v else {
        return v.clone();
    };
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(mut map)) => {
            for k in keys {
                map.remove(*k);
            }
            Value::Object(map)
        }
        _ => v.clone(),
    }
}

fn short_cell(v: &Value) -> String {
    let s = match v {
        Value::String(s) => format!("{s:?}"),
        other => other.to_string(),
    };
    if s.chars().count() > 80 {
        let cut: String = s.chars().take(77).collect();
        format!("{cut}…")
    } else {
        s
    }
}

/// The tables whose rows are the source's content: every user table but
/// the `*_bookkeeping` sidecars and [`NON_CONTENT_TABLES`].
async fn content_table_names(pool: &sqlx::SqlitePool) -> Vec<String> {
    use sqlx::Row;

    sqlx::query(
        "SELECT name FROM sqlite_master \
         WHERE type='table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .expect("list tables")
    .iter()
    .map(|r| r.get::<String, _>(0))
    .filter(|t| !t.ends_with("_bookkeeping") && !NON_CONTENT_TABLES.contains(&t.as_str()))
    .collect()
}

/// One invocation of `datalib-pipeline`, with its stderr captured.
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

fn run_pipeline(
    bin: &Path,
    cfg_path: &Path,
    run_root: &Path,
    now: &str,
    verb_and_args: &[&str],
) -> PipelineRun {
    eprintln!(
        "[test] run: {} {verb_and_args:?} --now {now}",
        bin.display()
    );
    let (verb, extra_args) = verb_and_args.split_first().expect("a verb");
    let out = Command::new(bin)
        .arg(verb)
        .arg(cfg_path)
        .arg("--now")
        .arg(now)
        .args(extra_args)
        // The run's own fingerprint cache, so run 1 is cold on every host
        // and the host cache never sees this test.
        .env("DATALIB_CACHE_DIR", run_root.join("fingerprint_cache"))
        .env("DATALIB_CHILD_ENV", "DATALIB_CACHE_DIR")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn datalib-pipeline");
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    // Persist the raw stream next to the run's data. We capture stderr
    // rather than inheriting it (that's where the NDJSON run record is), so
    // without this the whole event stream is discarded on a *successful*
    // run and only the last 40 lines survive a failure — which is precisely
    // when you want to ask "why did that take 18 minutes?". It goes
    // beside the data root, not in it: the root is what the app serves.
    let log = run_root.join(format!("{}.ndjson", now.replace(':', "-")));
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
    // The runner's own definition of "finished without failing", so a
    // state it adds is judged here the way it judges it: run 2 is full of
    // `skipped_up_to_date` (that is what incrementality looks like), and
    // run 3, a per-source sync, lists only the steps its request reached.
    // A spelling this build cannot parse is a failure, not a pass.
    let bad: Vec<String> = steps
        .iter()
        .filter(|s| {
            !s.get("status")
                .and_then(Value::as_str)
                .and_then(RunState::parse)
                .is_some_and(RunState::is_ok)
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

/// Every searched source holds one document per searchable document in
/// its render store, a source with an `embed` step
/// has nothing left to embed, and one without still has all of it left: the
/// embed stayed in its own source.
fn search_index_report(data_root: &Path, cfg_text: &str) -> Value {
    let embedded = groups_with(cfg_text, "embed");
    let keyword: Vec<String> = searched_groups(cfg_text).into_iter().collect();
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for the search index read");
    let held = rt
        .block_on(datalib_search_index::holdings(data_root, &keyword))
        .expect("read what the search index holds");
    assert_eq!(
        held.len(),
        keyword.len(),
        "the search index was never made, though {} sources are searched",
        keyword.len()
    );

    let mut report = serde_json::Map::new();
    for h in held {
        let rendered = searchable_documents(&data_root.join(&h.source).join("render_markdown"));
        assert_eq!(
            h.documents as u64, rendered,
            "{}: the search index holds {} documents; its render store has {rendered} \
             searchable ones",
            h.source, h.documents
        );
        let embeds = embedded.contains(&h.source);
        if embeds {
            assert_eq!(
                h.not_embedded, 0,
                "{} has an embed step, yet {} of its documents are not embedded",
                h.source, h.not_embedded
            );
        } else if h.documents > 0 {
            assert!(
                h.not_embedded > 0,
                "{} has no embed step, yet all of it is embedded: an embed reached \
                 beyond its own source",
                h.source
            );
        }
        report.insert(
            h.source,
            serde_json::json!({
                "documents": h.documents,
                "embed_step": embeds,
                "documents_not_embedded": h.not_embedded,
            }),
        );
    }
    Value::Object(report)
}

/// The groups that have a step of `function` in the config.
fn groups_with(cfg_text: &str, function: &str) -> std::collections::BTreeSet<String> {
    let cfg: toml::Value = toml::from_str(cfg_text).expect("config parses");
    cfg.get("steps")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.get("id").and_then(toml::Value::as_str))
        .filter_map(|id| id.split_once('/'))
        .filter(|(_, f)| *f == function)
        .map(|(g, _)| g.to_string())
        .collect()
}

/// The sources `unified_grid_and_search_index` puts in the search: every one with a render,
/// but those its `params.not_searched` lists.
fn searched_groups(cfg_text: &str) -> std::collections::BTreeSet<String> {
    let cfg: toml::Value = toml::from_str(cfg_text).expect("config parses");
    let not_searched: Vec<String> = cfg
        .get("steps")
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter(|s| {
            s.get("id")
                .and_then(toml::Value::as_str)
                .and_then(|id| id.split_once('/'))
                .is_some_and(|(_, f)| f == "unified_grid_and_search_index")
        })
        .filter_map(|s| s.get("params")?.get("not_searched")?.as_array().cloned())
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    groups_with(cfg_text, "render_markdown")
        .into_iter()
        .filter(|g| !not_searched.contains(g))
        .collect()
}

/// The documents the search covers in the render store under `dir`: every
/// one its renderer did not keep for the grid alone.
fn searchable_documents(dir: &Path) -> u64 {
    let Some(store) =
        datalib_etl_render::indexed_markdown::IndexedMarkdownStore::open_for_reading(dir, None)
            .expect("open a render store")
    else {
        return 0;
    };
    let rows = store.markdown_rows().expect("read the documents");
    store.close();
    rows.iter().filter(|r| r.searchable).count() as u64
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
        let db = data_root.join(stanza).join("ingest/entities.doltlite_db");
        let v = if db.is_file() {
            rt.block_on(latest_sync_run(&db))
        } else {
            Value::String("<no ingest/entities.doltlite_db>".into())
        };
        out.insert(stanza.clone(), v);
    }
    Value::Object(out)
}

async fn latest_sync_run(path: &Path) -> Value {
    use std::str::FromStr;

    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use sqlx::Row;

    // A plain SQLite file has to say so to the doltlite-linked engine,
    // which would otherwise take it for a doltlite store.
    let opts = if path.extension().is_some_and(|e| e == "sqlite") {
        SqliteConnectOptions::new().filename(datalib_runtime::plain_sqlite::uri(path))
    } else {
        SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display())).expect("sqlite uri")
    }
    .create_if_missing(false)
    .read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
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

    let status: Option<String> = row.try_get("status").expect("sync_runs status");
    let summary: Option<String> = row.try_get("summary").expect("sync_runs summary");
    let summary = summary
        .map(|s| serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)))
        .unwrap_or(Value::Null);
    serde_json::json!({
        "status": status,
        "summary": summary,
    })
}

/// Every term in the grid index's search terms, decoded from its dictionary, one
/// line each as `kind<TAB>value<TAB>uuid`, sorted.
fn search_terms(data_root: &Path) -> String {
    let path = datalib_runtime::layout::grid_index_db(data_root);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for the terms read");
    let coded: Vec<(i64, String, String)> = rt.block_on(async {
        let pool = open_readonly(&path).await;
        let rows = sqlx::query_as(
            "SELECT t.kind, v.value, r.uuid FROM terms t \
             JOIN vals v ON v.val_id = t.val_id JOIN rows r ON r.row_id = t.row_id",
        )
        .fetch_all(&pool)
        .await
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
        pool.close().await;
        rows
    });
    let mut lines: Vec<String> = coded
        .into_iter()
        .map(|(code, value, uuid)| {
            let kind = datalib_schema::search_terms::SearchTermKind::from_code(code)
                .map_or_else(|| format!("kind {code}"), |k| k.as_str().to_string());
            format!("{kind}\t{value}\t{uuid}")
        })
        .collect();
    lines.sort();
    lines.join("\n")
}

/// One contact the bake makes, as `contacts.toml` beside the config
/// writes it. The handles are real, which is why they live in the
/// private data repo and not here.
struct ContactSpec {
    name: String,
    handles: Vec<Handle>,
    stopped_working: Vec<(Handle, String)>,
}

/// A contact the bake made, under the id the store gave it.
struct MadeContact {
    name: String,
    contact_id: String,
    handles: Vec<Handle>,
}

fn read_contact_specs(path: &Path) -> Vec<ContactSpec> {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "read {}: {e}. The bake links handles in the contacts app from this \
             file; add it beside the config.",
            path.display()
        )
    });
    let doc: toml::Table = toml::from_str(&text).expect("contacts.toml parses");
    let handle = |v: &toml::Value| {
        let s = v.as_str().expect("a handle is a string");
        Handle::parse(s).unwrap_or_else(|| panic!("{s:?} is not a handle as datalib spells one"))
    };
    let specs: Vec<ContactSpec> = doc
        .get("contacts")
        .and_then(toml::Value::as_array)
        .expect("contacts.toml has [[contacts]]")
        .iter()
        .map(|c| ContactSpec {
            name: c["name"].as_str().expect("a contact's name").to_string(),
            handles: c["handles"]
                .as_array()
                .expect("a contact's handles")
                .iter()
                .map(handle)
                .collect(),
            stopped_working: c
                .get("stopped_working")
                .and_then(toml::Value::as_table)
                .into_iter()
                .flatten()
                .map(|(h, by)| {
                    let by = by.as_str().expect("stopped_working is a date");
                    (handle(&toml::Value::String(h.clone())), by.to_string())
                })
                .collect(),
        })
        .collect();
    assert!(!specs.is_empty(), "{}: no contacts", path.display());
    specs
}

/// Every handle the specs link is an author in the index, so each link
/// lands on documents rather than on nothing.
fn assert_handles_indexed(data_root: &Path, specs: &[ContactSpec], after: &str) {
    let db =
        data_root.join("unified_grid_and_search_index/unified_grid_and_search_index/db.sqlite");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for the index read");
    let indexed: std::collections::BTreeSet<String> = rt.block_on(async {
        let pool = open_readonly(&db).await;
        let handles = sqlx::query_scalar(
            "SELECT DISTINCT author_handle FROM grid_rows WHERE author_handle IS NOT NULL",
        )
        .fetch_all(&pool)
        .await
        .expect("read the index's author handles");
        pool.close().await;
        handles.into_iter().collect()
    });
    let missing: Vec<&str> = specs
        .iter()
        .flat_map(|s| &s.handles)
        .map(Handle::as_str)
        .filter(|h| !indexed.contains(*h))
        .collect();
    assert!(
        missing.is_empty(),
        "after {after}, contacts.toml links handles no document is written by: {missing:?}. \
         If upstream moved, pick handles that are in the data."
    );
}

fn make_contacts(data_root: &Path, specs: &[ContactSpec]) -> Vec<MadeContact> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for the contacts store");
    rt.block_on(async {
        let store = Store::open(&datalib_contacts::store_path(data_root))
            .await
            .expect("open the contacts store");
        let mut made = Vec::new();
        for spec in specs {
            let contact_id = store
                .create(&spec.name, ContactKind::Person, &spec.handles)
                .await
                .unwrap_or_else(|e| panic!("make contact {:?}: {e:#}", spec.name));
            for (handle, by) in &spec.stopped_working {
                let marked = store
                    .set_stopped_working(handle, Some(by))
                    .await
                    .unwrap_or_else(|e| panic!("mark {handle} stopped working: {e:#}"));
                assert!(marked, "{handle} is not one of {:?}'s handles", spec.name);
            }
            made.push(MadeContact {
                name: spec.name.clone(),
                contact_id,
                handles: spec.handles.clone(),
            });
        }
        store.close().await;
        made
    })
}

/// The store still answers each handle with the contact it was linked to.
fn assert_contacts_resolve(store_path: &Path, contacts: &[MadeContact]) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for the contacts store");
    rt.block_on(async {
        let store = Store::open(store_path)
            .await
            .expect("open the contacts store");
        for c in contacts {
            let resolved = store.resolve(&c.handles).await.expect("resolve handles");
            for h in &c.handles {
                assert_eq!(
                    resolved.get(h.as_str()).map(|n| n.key.as_str()),
                    Some(c.contact_id.as_str()),
                    "{h} no longer resolves to {:?}",
                    c.name
                );
            }
        }
        store.close().await;
    });
}

/// The contacts store's dump with each contact's random id replaced by
/// its name and the stamps the wall clock wrote redacted, so a bake diffs
/// only when what the store holds does.
fn contacts_for_snapshot(mut dump: Value, contacts: &[MadeContact]) -> Value {
    const STAMPS: &[&str] = &[
        "created_at_utc",
        "updated_at_utc",
        "linked_at_utc",
        "added_at_utc",
        "set_at_utc",
        "tz_offset",
    ];
    fn walk(v: &mut Value, contacts: &[MadeContact]) {
        match v {
            Value::Object(map) => {
                for (k, child) in map.iter_mut() {
                    if STAMPS.contains(&k.as_str()) {
                        *child = Value::String(REDACTED.into());
                    } else {
                        walk(child, contacts);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|i| walk(i, contacts)),
            Value::String(s) => {
                for c in contacts {
                    *s = s.replace(&c.contact_id, &format!("<contact {}>", c.name));
                }
            }
            _ => {}
        }
    }
    walk(&mut dump, contacts);
    // These tables are keyed by the random id, so the store's order is
    // random too; re-sort on the names that replaced it.
    for table in ["contacts", "members", "photos"] {
        if let Some(Value::Array(rows)) = dump.get_mut(table) {
            rows.sort_by_cached_key(|r| r.to_string());
        }
    }
    dump
}

/// Whole-table bookkeeping that legitimately changes across a reset, so it
/// is excluded from the content-stability comparison: the store's record
/// of itself, of its runs and cursors, of the spans a walk has looked at
/// (whose newest end is the run's clock) and when it last found nothing
/// newer, of the token each listed
/// message was last named under (Gmail's is account-wide, so any new mail
/// restamps every row a re-list names), and of the files or scans a
/// file-backed ingest has already been through.
const NON_CONTENT_TABLES: &[&str] = &[
    "_datalib_meta",
    "sync_runs",
    "sync_scope_state",
    "coverage",
    "coverage_checked",
    "listed_messages",
    "problems",
    "ingested_files",
    "scan_meta",
    "media_scan_meta",
    "pdf_scan_meta",
];

/// Stamps a store mints inside a content row, each a known leak: the
/// row reads as modified on every re-fetch, so a render that diffs the
/// raw store re-renders it every run. A `payload.<key>` entry names a
/// key inside the JSON payload. An entry is deleted when its stamp
/// moves to a bookkeeping sidecar or out of the payload; the check
/// then holds that table to the byte. Empty is the goal.
const KNOWN_STAMP_LEAKS: &[(&str, &str)] = &[];

/// Every document `stanza`'s render store holds, as the store spells it
/// (what the viewer shows and `datalib-step markdown` exports), at
/// `<stanza>/render_markdown/<id>.md`, beside the store's own snapshot.
/// Which documents search covers is that snapshot's `markdowns.searchable`.
fn snapshot_documents(data_root: &Path, stanza: &str, manifest: &mut Vec<String>) {
    let dir = data_root.join(stanza).join("render_markdown");
    if !dir.is_dir() {
        return;
    }
    let Some(store) =
        datalib_etl_render::indexed_markdown::IndexedMarkdownStore::open_for_reading(&dir, None)
            .expect("open a render store")
    else {
        return;
    };
    let texts = store.document_texts().expect("read the documents");
    store.close();
    for (uuid, s) in texts {
        let key = format!("{stanza}/render_markdown/{uuid}.md");
        manifest.push(key.clone());
        let s = normalize_str(&s);
        insta::with_settings!({
            snapshot_path => snap_base().join(stanza).join("render_markdown").display().to_string(),
            prepend_module_to_snapshot => false,
            description => key,
        }, {
            assert_snapshot!(format!("{uuid}.md"), s);
        });
    }
}

/// Every searchable document's text as the search index embeds it, chunk
/// by chunk, beside its rendered page: `<source>/index_text/<id>.txt`,
/// for each source `unified_grid_and_search_index` searches.
/// Drawn by `datalib-step index-text --chunks`, the call the keyword and
/// embed passes make, so a change to what search reads (the envelope line,
/// a flattened link, where a chunk is cut, the prompt) is a diff here even
/// when the page looks the same.
fn snapshot_index_text(data_root: &Path, cfg_text: &str, manifest: &mut Vec<String>) {
    let searched = searched_groups(cfg_text);
    let out = Command::new(bin_dir().join("datalib-step"))
        .arg("index-text")
        .arg(data_root)
        .arg("--chunks")
        .output()
        .expect("run datalib-step index-text");
    assert!(
        out.status.success(),
        "datalib-step index-text failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8(out.stdout).expect("index-text writes UTF-8");
    let docs = index_text_documents(&text);
    assert!(
        !docs.is_empty(),
        "index-text drew no document, though the bake renders searchable sources"
    );
    for (source, uuid, body) in docs {
        if !searched.contains(&source) {
            continue;
        }
        let key = format!("{source}/index_text/{uuid}.txt");
        manifest.push(key.clone());
        insta::with_settings!({
            snapshot_path => snap_base().join(&source).join("index_text").display().to_string(),
            prepend_module_to_snapshot => false,
            description => key,
        }, {
            assert_snapshot!(format!("{uuid}.txt"), normalize_str(&body));
        });
    }
}

/// `index-text`'s output split at each `=== <source> <id> <title>` header,
/// the header kept as the document's first line.
fn index_text_documents(text: &str) -> Vec<(String, String, String)> {
    let mut docs: Vec<(String, String, String)> = Vec::new();
    for line in text.split_inclusive('\n') {
        let header = line
            .strip_prefix("=== ")
            .and_then(|rest| {
                let mut parts = rest.splitn(3, ' ');
                Some((parts.next()?, parts.next()?))
            })
            .filter(|(_, uuid)| {
                uuid.len() == 36 && uuid.chars().filter(|c| *c == '-').count() == 4
            });
        match header {
            Some((source, uuid)) => {
                docs.push((source.to_string(), uuid.to_string(), line.to_string()))
            }
            None => {
                if let Some(doc) = docs.last_mut() {
                    doc.2.push_str(line);
                }
            }
        }
    }
    docs
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
        // Doltlite's `-lock` sidecars and our own per-file `.lock`s
        // (etl README § "Connection pools") are ephemeral and
        // content-free; `CACHEDIR.TAG` is a backup hint asserted
        // separately. None belongs in a golden.
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| n.ends_with("-lock") || n.ends_with(".lock") || n == "CACHEDIR.TAG")
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
/// The step trees snapshotted file by file, under each group.
const TREE_SNAPSHOTS: [&str; 4] = [
    "ingest",
    "render_markdown",
    "render_time_series",
    "time_series_index",
];

/// Deletes the snapshots this bake no longer writes: a tree snapshot whose
/// file is gone, and a top-level folder whose group is gone from the data
/// root, which is what a renamed group leaves.
fn prune_orphan_snapshots(manifest: &[String], data_root: &Path) {
    if !insta_update_mode() {
        return;
    }
    let base = snap_base();
    if !base.is_dir() {
        return;
    }
    for entry in std::fs::read_dir(&base)
        .expect("read snapshot tree")
        .flatten()
    {
        let name = entry.file_name();
        let written_later = name == datalib_contacts::CURATED_DIR;
        if entry.path().is_dir() && !written_later && !data_root.join(&name).is_dir() {
            std::fs::remove_dir_all(entry.path()).unwrap_or_else(|e| {
                panic!("delete orphan snapshots {}: {e}", entry.path().display())
            });
        }
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
        // Snapshot path mirrors the data layout, so only the per-group
        // tree snaps are manifest-keyed — hence pruning only under one of
        // `TREE_SNAPSHOTS`, `render_occurrences/` or `index_text/`.
        let rel = p.strip_prefix(&base).unwrap().to_string_lossy().to_string();
        let key = rel.strip_suffix(".snap").unwrap_or(&rel);
        let is_tree_snap = key.split('/').any(|seg| {
            TREE_SNAPSHOTS.contains(&seg) || matches!(seg, "render_occurrences" | "index_text")
        });
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
/// volatile fields; markdown is text with frontmatter redactions; a store —
/// `.doltlite_db`, or a plain `.sqlite` like the blob CAS — is dumped a row a
/// line by `store_as_jsonl` so the goldens carry the actual raw payloads.
/// Anything else becomes a size marker.
fn summarize_file(path: &Path) -> SnapValue {
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if name.ends_with(".doltlite_db") || name.ends_with(".sqlite") {
        let mut v = dump_store(path);
        strip_volatile(&mut v);
        return SnapValue::Text(store_as_jsonl(&v));
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
        SnapValue::Text(normalize_str(&text))
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

/// The config as `datalib-http` would leave it. The bake runs
/// `datalib-pipeline`, which loads a config but never upgrades one, so a shape
/// the app rewrites on sight (the index group's old name) would fail the
/// bake rather than be carried forward. A rewrite that refuses fails it
/// here, by name, since the app would then run the config as it is. With
/// `write_back`, the data repo's `dag.toml` takes the upgrade, so its diff
/// shows it and git keeps what it replaced.
fn upgraded_config(path: &Path, text: String, write_back: bool) -> String {
    let up = datalib_migrate_config::upgrade_unattended(&text);
    assert!(
        up.refused.is_empty(),
        "{}: the app would leave this config as it is: {}",
        path.display(),
        up.refused
            .iter()
            .map(|(why, e)| format!("it {why}, and the rewrite refused: {e:#}"))
            .collect::<Vec<_>>()
            .join("; ")
    );
    if up.done.is_empty() {
        return text;
    }
    eprintln!("[test] upgraded {}: {}", path.display(), up.done.join("; "));
    if write_back {
        std::fs::write(path, &up.text).expect("write the upgraded config back");
    }
    up.text
}

fn rewrite_config(text: &str) -> String {
    let mut doc: toml::Table = toml::from_str(text).expect("parse config toml");
    doc.remove("data_root");

    // A step's id is `<group>/<function>`; its provider is the group's `type`.
    let slack_groups: Vec<String> = doc
        .get("groups")
        .and_then(|v| v.as_array())
        .map(|groups| {
            groups
                .iter()
                .filter_map(|g| g.as_table())
                .filter(|g| g.get("type").and_then(|t| t.as_str()) == Some("slack"))
                .filter_map(|g| g.get("id").and_then(|i| i.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();

    let steps = doc
        .get_mut("steps")
        .and_then(|v| v.as_array_mut())
        .expect("config must have a `[[steps]]` array (DAG format)");

    let mut patched_slack = 0usize;
    for step in steps.iter_mut() {
        let Some(m) = step.as_table_mut() else {
            continue;
        };
        let id = m.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let (group, function) = id.split_once('/').unwrap_or((id, ""));
        if function != "ingest" || !slack_groups.iter().any(|g| g == group) {
            continue;
        }
        let params = m
            .entry("params")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let Some(params_map) = params.as_table_mut() else {
            continue;
        };
        let api_entry = params_map
            .entry("api")
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        if let Some(api_map) = api_entry.as_table_mut() {
            api_map.insert("refresh_window_days".into(), toml::Value::Integer(30));
            patched_slack += 1;
        }
    }
    // Warn rather than fail: a reduced config (one provider, for debugging via
    // DATALIB_TEST_CONFIG) legitimately has no slack step, and a hard assert
    // here would make the test unusable for exactly that. On the real config
    // this line firing means the tweak silently did nothing — the config
    // shape drifted — and you'd see spurious media churn in the goldens.
    if patched_slack == 0 {
        eprintln!(
            "[test] WARNING: no ingest step under a `slack` group; \
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

fn dump_store(path: &Path) -> Value {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime for a store dump");
    rt.block_on(dump_store_async(path))
}

async fn dump_store_async(path: &Path) -> Value {
    let stanza = stanza_of(path).unwrap_or_default();
    let step = step_of(path).unwrap_or_default();
    let store_counts_only = ROW_COUNT_ONLY_STEPS
        .iter()
        .any(|(s, st)| *s == stanza && (*st == "*" || *st == step));
    let table_counts_only = |t: &str| {
        ROW_COUNT_ONLY_TABLES
            .iter()
            .any(|(s, table)| *table == t && (*s == "*" || *s == stanza))
    };
    let mut dump =
        datalib_store_dump::dump(path, |t| match store_counts_only || table_counts_only(t) {
            true => Keep::Count,
            false => Keep::Rows,
        })
        .await;
    for (table, rows) in dump.as_object_mut().into_iter().flatten() {
        for row in rows.as_array_mut().into_iter().flatten() {
            redact_row(table, row);
        }
    }
    dump
}

fn redact_row(table: &str, row: &mut Value) {
    let Value::Object(map) = row else {
        return;
    };
    let volatile = TABLE_VOLATILE_KEYS.iter().find(|(name, _)| *name == table);
    for k in volatile.into_iter().flat_map(|(_, keys)| *keys) {
        if let Some(slot) = map.get_mut(*k) {
            *slot = Value::String(REDACTED.into());
        }
    }
    if table == "_datalib_meta" && map.get("key").and_then(Value::as_str) == Some("git_hash") {
        map.insert("value".into(), Value::String(REDACTED.into()));
    }
}

fn strip_volatile(v: &mut Value) {
    match v {
        Value::Object(map) => {
            let repo = is_github_repo_object(map);
            let storage = is_storage_row(map);
            for (k, child) in map.iter_mut() {
                if VOLATILE_KEYS.contains(&k.as_str())
                    || (repo && REPO_VOLATILE_KEYS.contains(&k.as_str()))
                    || (storage && k == "byte_size")
                {
                    *child = Value::String(REDACTED.into());
                    continue;
                }
                strip_volatile(child);
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
        "fetched_at_utc",
        "last_edited_time",
        "created_time",
        "cache_ts",
        // `updated` intentionally omitted — split into the volatile_payload
        // sidecar at extract time, not redacted here (see strip_volatile).
        "started_at",
        "finished_at",
        "started_at_utc",
        "finished_at_utc",
        "duration_secs",
        "data_root",
        "last_attempt_at_utc",
        "first_seen_at_utc",
        "changed_at_utc",
        "last_finished_at_utc",
        "last_seen_at_utc",
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
        "raw_commit",
        "tz_offset",
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
