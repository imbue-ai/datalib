// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live end-to-end golden test for the `datalib-dag` pipeline.
//!
//! Config, file-based source data, and golden snapshots live OUTSIDE this
//! repo — in the dir named by `DATALIB_MANUAL_E2E_DIR` (e.g.
//! `~/data_liberation_manual_e2e_test_data`), versioned in a private repo so
//! the (slightly sensitive) source data is never shared when this repo is
//! open-sourced. That dir holds:
//!
//!   <DATALIB_MANUAL_E2E_DIR>/
//!     dag.toml             ← the pipeline config (file sources point at sources/)
//!     sources/             ← LinkedIn / Takeout / SMS … export data
//!     snapshots/           ← the golden .snap tree (below)
//!
//! The runner is `manual_e2e_run.sh`, which lives next to this test in the code
//! repo (it's code, not data); it sets `DATALIB_MANUAL_E2E_DIR` (defaulting
//! to the canonical checkout) and invokes the test / `.update`.
//!
//! # History: what this is a port OF
//!
//! This test previously lived at `frankweiler/backend/sync/tests/` and drove
//! the monolithic `frankweiler-sync` binary. That crate — and this test with
//! it — was deleted in e905d252 when the pipeline moved to the DAG runner.
//! Everything below the "Normalization" heading is carried over verbatim: it
//! operates on the produced data tree, whose layout the migration did not
//! change, which is why the golden `snapshots/` tree keeps the same shape.
//!
//! Two things genuinely had to change:
//!
//!   * **Invocation.** `datalib-dag <config> [--now] [--reset-and-redownload]`,
//!     with the config as a POSITIONAL arg (there is no `--config`). The runner
//!     resolves step commands like `datalib-step download slack_api` purely
//!     through the child `PATH`, falling back to its own directory — so we run
//!     it out of `//datalib/backend:bin`, which stages every binary under its
//!     public dash-separated name, and pass no `--binary-dir`. See [`bin_dir`].
//!   * **Where the run record comes from.** The old binary wrote an aggregate
//!     `sync_summary_<now>.json` carrying per-source `stats`. `datalib-dag`
//!     emits a `run_summary` NDJSON event on stderr instead, and it is
//!     deliberately thin — per step: status, attempts, error, and each
//!     output's `{path, version, changed}`. No per-source counts, because the
//!     orchestrator is storage-agnostic by design and does not know sources
//!     persist to doltlite.
//!
//!     The counts did not disappear, they moved to the source of record: each
//!     stanza's `<stanza>/raw/entities.doltlite_db` has a `sync_runs` table
//!     whose `summary` column carries `deltas` (per-table added/modified/
//!     removed, computed from `dolt_diff_<table>`) and `cursors` (which
//!     `sync_scope_state` scopes moved). The run-2 incrementality snapshot
//!     therefore reads `sync_runs` per stanza rather than one aggregate file —
//!     see [`incrementality_report`]. Same signal, correct grain.
//!
//! Spawns the runner against that `dag.toml` (with two test-only tweaks:
//! per-run `data_root` and slack `refresh_window_days=30`), hitting real
//! provider APIs through `latchkey curl`. Then snapshots the produced data
//! tree, one `.snap` per file under `<DATALIB_MANUAL_E2E_DIR>/snapshots/`,
//! mirroring the layout:
//!
//! The data root is grouped by stanza (`<stanza>/raw`, `<stanza>/rendered_md`)
//! with aggregates under `system/`; the snapshot tree mirrors that:
//!
//!   snapshots/
//!     manifest.snap                                       ← list of paths
//!     tiny-slack/raw/raw_api/auth.test/run-_.snap
//!     tiny-slack/rendered_md/<chat_uuid>/all.md.snap
//!     tiny-slack/rendered_md/<chat_uuid>/all.grid_rows.json.snap
//!     notion-api/raw/notion_official_page/created/events.snap
//!     …
//!
//! Per-file `.snap`s mean `cargo insta review` walks them one at a time,
//! diffs stay local to the file that actually changed, and the file
//! layout under `snapshots/` mirrors what the pipeline wrote.
//!
//! Normalization:
//!   * `_recorded_at`, `duration_ms`, `_item_hashes`, `request_id`,
//!     `fetched_at`, `last_edited_time`, `created_time`, `cache_ts`,
//!     `updated`, and `source_fingerprint` keys keep their position but
//!     get their value replaced with `"[redacted]"`. Same for the
//!     non-deterministic dolt `commit_hash`, the `load.write_lock` timings
//!     (`avg/total_hold_ms`, `avg/total_wait_ms`), and the extract-metrics
//!     per-db byte sizes (`bytes_before/after/delta`) — row counts carry the
//!     real signal there.
//!   * The tempdir `data_root` prefix is replaced with the stable token
//!     `<data_root>` wherever it appears in a path string, so a path keeps its
//!     meaningful suffix (`<data_root>/tiny-slack/raw/entities.doltlite_db`) without
//!     the per-run `/var/folders/…/.tmpXXXX` churn.
//!   * `source_fingerprint:` lines in `.md` frontmatter get the same
//!     treatment.
//!   * `run-<timestamp>` filename segments collapse to `run-_`.
//!   * `conversations.list` and `users.list` slack endpoints are dropped
//!     entirely — they're workspace-wide listings that leak unrelated
//!     channels/users and churn on every join/leave.
//!   * Binary media files become `<binary N bytes>` markers.
//!
//! The aggregate index + qmd under `system/` are deliberately skipped —
//! too noisy / not deterministic (the doltlite commit hashes churn).
//!
//! Tagged `manual` + `external` in Bazel and `#[ignore]` in cargo. Easiest
//! path is `manual_e2e_run.sh` next to this test (it sets the env var and
//! forwards creds):
//!
//! ```sh
//! datalib/backend/dag/manual_e2e_run.sh           # run + diff
//! datalib/backend/dag/manual_e2e_run.sh --update  # accept new goldens
//! ```
//!
//! Bazel-only — there is no cargo path, because `datalib-step` is a
//! bazel-only target and the runner needs it on `PATH`.
//!
//! To check the config alone — no network, no credentials, seconds not
//! minutes — run the offline sibling instead:
//!
//! ```sh
//! bazel test //datalib/backend/ingest_config:config_examples_test \
//!     --test_arg=--ignored --test_env=DATALIB_MANUAL_E2E_DIR
//! ```

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

/// Replace the captured `data_root` prefix in `s` with `<data_root>`.
/// A no-op until `DATA_ROOT` is set at the top of the test.
fn norm_data_root(s: &str) -> String {
    match DATA_ROOT.get() {
        Some(dr) => s.replace(dr.as_str(), "<data_root>"),
        None => s.to_string(),
    }
}

/// Collapse the volatile query string of any AWS S3 pre-signed URL to a stable
/// `?<presigned>` token, preserving the base URL (which file). Notion (and any
/// S3-backed provider) re-signs these on every fetch, so the query — the
/// `X-Amz-Signature`, `-Date`, `-Credential`, `-Security-Token`, `-Expires` —
/// rotates each run while the path stays put. Handles both a bare value (JSON)
/// and a URL embedded in `![alt](…)` markdown (terminated by `)`/`"`/space).
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

/// Per-string snapshot normalization: data_root prefix + pre-signed URLs.
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
    // NB: `updated` is deliberately NOT redacted here. It's per-fetch
    // bookkeeping that providers embed in their payload (Slack's
    // channel/user `updated` epoch); rather than paper over it in the
    // test, the extract pipeline splits it into the
    // `volatile_payload` sidecar (which this dump excludes) so it never
    // reaches a content table. See data_architecture_ingestion.md
    // §"Volatile-field split". If a NEW provider's volatile `updated`
    // shows up as golden churn, split it at the source — don't re-add it
    // here.
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
    // CAS blob "first stored" wall-clock stamp (the `blobs` table's
    // `first_seen_at`). Identical bytes (content-addressed by blake3) land at
    // the same PK, but the timestamp is whenever this run first wrote them.
    "first_seen_at",
    // Resume-cursor / bookkeeping wall-clock stamps: `sync_scope_state`'s
    // `last_finished_at` + `after` (real now when the scope ran, not the
    // `--now` arg), and `last_seen_at` (when a row was last fetched). The
    // cursor `before` is a `--now`-relative window start and stays put, as do
    // genuine upstream content times (`last_sign_in_at`, `latest_build_*`).
    "last_finished_at",
    "after",
    "last_seen_at",
    // GitLab user object's `local_time` — the user's *current* local time
    // ("4:52 PM"), so it ticks every minute. Upstream content, but volatile by
    // nature. `last_activity_on` is the same class one granularity coarser:
    // the date you last used GitLab, so it advances every day you do, and any
    // two runs either side of midnight disagree. Genuine upstream data, but it
    // tracks the observer rather than the observed.
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
    // fsindex's `file_stats` rescan-cursor + `scan_meta` columns. The
    // filesystem-mechanical fields (mtime/ctime, inode, dev) are per-checkout
    // / per-machine, and `last_scan_at` is wall-clock — all churn run-to-run
    // even when the scanned bytes are identical. The deterministic content
    // (kind, size, blake3, path) lives in the `files` table and is preserved.
    // `scanner_version` is redacted so a version bump doesn't churn the golden.
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

/// A JSON object is a GitHub repo object if it carries both `full_name` and
/// `default_branch` — distinctive enough to scope `REPO_VOLATILE_KEYS` to it.
fn is_github_repo_object(map: &serde_json::Map<String, Value>) -> bool {
    map.contains_key("full_name") && map.contains_key("default_branch")
}

/// Per-TABLE volatile columns: `(table, keys)` redacted only in rows of that
/// table. Applied in [`dump_doltlite_db`], which knows the table name for
/// certain — no shape-sniffing required.
///
/// This exists because the wall-clock stamps that churn worst live in the
/// shared bookkeeping tables (`SHARED_DDL`: `sync_runs`, `sync_scope_state`,
/// `sync_scope_config`), where a globally-scoped redaction would be wrong.
/// `updated_at` is a perfectly good *content* field elsewhere — a GitHub
/// comment's edit time, a GitLab note's — so blanket-redacting it would mask
/// real upstream change. Same reasoning as `REPO_VOLATILE_KEYS` being scoped
/// to repo objects, but keyed on the table rather than guessed from the row.
///
/// `sync_scope_config.updated_at` is re-stamped on every run that satisfies
/// its scope config, so without this every API-backed source contributes a
/// spurious one-line diff to every single bake.
const TABLE_VOLATILE_KEYS: &[(&str, &[&str])] = &[("sync_scope_config", &["updated_at"])];

const REDACTED: &str = "[redacted]";

/// Path components whose entire contents we deliberately omit. Slack's
/// workspace-wide listings: every channel the user is in, every user in
/// the workspace. Don't belong in a committed golden.
///
/// `events/` is the per-source JSONL wire-event tape (see
/// `docs/dev/data_architecture_ingestion.md` § "Wire-event tape (JSONL)"). Every line
/// carries a wall-clock `_recorded_at`, so the files are non-deterministic
/// across runs and would dominate a diff with churn that says nothing
/// about extract correctness.
const SKIP_PATH_SEGMENTS: &[&str] = &["conversations.list", "users.list", "events"];

/// External, out-of-repo home for this manual test's `config.toml`, the
/// file-based `sources/`, and the golden `snapshots/`. Kept outside the repo
/// so the (slightly sensitive) source data is never shared when the repo is
/// open-sourced; versioned separately in a private repo. `manual_e2e_run.sh`
/// (in the code repo) sets `DATALIB_MANUAL_E2E_DIR` and invokes this test.
/// `None` when unset.
///
/// Exactly one name, deliberately. The pre-rename `FRANKWEILER_MANUAL_E2E_DIR`
/// is NOT accepted: a fallback to a stale name keeps a stale shell profile
/// silently working, which is how you end up with two documented spellings
/// and no signal that one is wrong.
fn e2e_dir() -> Option<PathBuf> {
    std::env::var("DATALIB_MANUAL_E2E_DIR")
        .ok()
        .map(PathBuf::from)
}

/// Base directory for golden snapshots. Resolves to `<e2e_dir>/snapshots`
/// (absolute, so insta reads/writes there directly) when the data dir is
/// configured, else the legacy in-tree `snapshots/` next to this file.
fn snap_base() -> PathBuf {
    e2e_dir()
        .map(|d| d.join("snapshots"))
        .unwrap_or_else(|| PathBuf::from("snapshots"))
}

/// Resolve a bazel-built binary from the test's runfiles tree, where it is
/// staged by a `data` dep in BUILD.bazel.
///
/// Deliberately runfiles-based rather than a `$(rootpath …)`-in-`env` string:
/// a rootpath is workspace-relative, and the test's cwd is not the workspace
/// root under every bazel config — the same trap documented at length in
/// `datalib/backend/core/tests/fixture_db_snapshot.rs`. The env override is
/// kept as an escape hatch for running against a hand-built binary.
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
/// run gets its own dir under a stable base, and only the most recent
/// [`KEEP_RUNS`] are retained. Lets you peek at the last few runs' working
/// doltlite DBs / rendered output (e.g. to inspect a `dolt_diff_<table>`
/// warning with the doltlite client) without unbounded disk growth. Unlike a
/// `tempfile` tempdir these survive the process — including when a snapshot
/// assertion panics mid-run.
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
    //
    // 1. Every declared step ran and succeeded. Catches a source that got
    //    skipped, blocked behind a failed dep, or quietly dropped from the
    //    config — none of which a per-file snapshot diff would flag as
    //    anything but "files went missing".
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
            .join("system/backend_index/db.doltlite_db")
            .is_file(),
        "backend index DB must live at system/backend_index/db.doltlite_db"
    );
    // Only the genuinely-derived index dirs are tagged as rebuildable cache so
    // `--exclude-caches` backups skip them (the per-stanza `rendered_md/` tags
    // are checked implicitly via the manifest — CACHEDIR.TAG is skipped in the
    // walk below). The backend index is always produced here; qmd is skipped in
    // this test so its tag may be absent. `system/state/` must NOT be tagged —
    // job logs are operational history, not rebuildable from raw.
    assert!(
        data_root
            .join("system/backend_index/CACHEDIR.TAG")
            .is_file(),
        "system/backend_index/ must carry a CACHEDIR.TAG marking it as derived cache"
    );
    assert!(
        !data_root.join("system/state/CACHEDIR.TAG").exists(),
        "system/state/ (operational history) must NOT be tagged as cache"
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

    // Prune orphaned `.snap` files left behind when the set of produced
    // files changes (e.g. a renderer migration that changes output
    // paths). insta's own `INSTA_UNREFERENCED=delete` is a cargo-insta
    // feature that doesn't fire under `bazel run` and wouldn't scan our
    // external `snapshot_path` tree anyway — so we prune ourselves,
    // keyed off the manifest above. Only in update mode: a check run
    // surfaces the change through the `manifest` snapshot diff instead.
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
    //
    // Run the same sync again against the now-populated data_root.
    // A healthy incremental sync should make a small number of upstream
    // requests, produce small `deltas` per source, and advance every
    // `sync_scope_state` scope by the same wall-clock interval. A
    // regression that broke incrementality (e.g. the gitlab+github
    // `full_sync: true` override that bit us recently) would show up
    // here as `requests` and `deltas.<table>.added` ballooning back to
    // first-run scale.
    //
    // Snapshot redactions are tuned for this purpose: timestamps
    // (`elapsed_ms`, `network_seconds`, cursor `before`/`after`) get
    // [redacted], but per-source `stats` counts are PRESERVED — those
    // are precisely the numbers that prove (or break) incrementality.
    let now2 = "2026-05-21T18:05:00Z";
    let run2 = run_pipeline(&bin, &cfg_path, now2, &[]);
    assert!(
        run2.status.success(),
        "pipeline run 2 failed (exit {:?}). Last stderr:\n{}",
        run2.status.code(),
        run2.stderr_tail(40)
    );
    assert_step_statuses_ok(&run2.run_summary().expect("run 2 run_summary"));

    // Read the incrementality signal from each stanza's own `sync_runs` table
    // rather than the runner's summary — see the module header for why it
    // lives there now. `strip_volatile_for_incrementality` is unchanged from
    // the pre-DAG test: it redacts wall-clock jitter (`elapsed_ms`,
    // `network_seconds`, cursor `before`/`after`, `commit_hash`) while
    // PRESERVING the counts, which are the whole point of this snapshot.
    let mut report = incrementality_report(&data_root, &stanzas);
    strip_volatile_for_incrementality(&mut report);
    insta::with_settings!({
        snapshot_path => snap_base().display().to_string(),
        prepend_module_to_snapshot => false,
        sort_maps => true,
    }, {
        assert_json_snapshot!("sync_summary_run2_incrementality", report);
    });

    // ── Third run: --reset-and-redownload content stability ───────────
    //
    // Wipe every entity + bookkeeping table and re-download every row
    // from upstream. The CONTENT tables must come back byte-identical:
    // re-fetching the same upstream object must land the same bytes at
    // the same PK, so `dolt_diff_<t>` (and incremental render) reflect
    // real upstream change only. Any field that drifts across an
    // identical re-fetch is per-fetch bookkeeping leaking into a content
    // payload — it belongs in the `volatile_payload` sidecar (see
    // data_architecture_ingestion.md §"Volatile-field split"), and THIS
    // is the check that surfaces it.
    //
    // We compare RAW (un-redacted) content so a field that should have
    // been split — but wasn't — shows up as drift instead of being
    // masked by `strip_volatile`. Bookkeeping sidecars, `sync_runs`, and
    // `sync_scope_state` legitimately change across a reset and are
    // excluded by `content_tables`.
    //
    // Scoped to the providers that have adopted the split. Add a DB here
    // as each provider migrates; the long-term goal is every provider.
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
        // Report a PATH-LEVEL diff, not `assert_eq!` on the two whole Values.
        // These are multi-megabyte structures; the stock assertion prints both
        // in full, which is unreadable — and worse, actively misleading, since
        // eyeballing two offset 2MB dumps invites you to "find" differences
        // that are only misalignment. Say exactly which row and which JSON
        // path moved.
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

/// Structural diff of two JSON values: returns human-readable
/// `path: before=… after=…` lines for every leaf that differs.
///
/// Arrays of objects carrying an `id` are matched BY ID rather than by
/// position, so one inserted row doesn't cascade into "everything after this
/// differs". That cascade is exactly the failure mode that makes a naive diff
/// of these dumps untrustworthy.
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
///
/// stderr carries both the NDJSON event stream and the steps' forwarded
/// logs, so we keep it whole: [`PipelineRun::run_summary`] mines the one
/// line we assert on, and [`PipelineRun::stderr_tail`] surfaces the rest
/// when something fails.
struct PipelineRun {
    status: std::process::ExitStatus,
    stderr: String,
}

impl PipelineRun {
    /// The single `{"event":"run_summary",…}` line. `None` if the runner died
    /// before the scheduler finished (a crash, or a second SIGINT).
    fn run_summary(&self) -> Option<Value> {
        self.stderr
            .lines()
            .rev() // emitted last; scan from the end
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .find(|v| v.get("event").and_then(Value::as_str) == Some("run_summary"))
    }

    /// Last `n` stderr lines, for failure messages. The runner forwards every
    /// step's stderr here, so this is where the actual provider error is.
    ///
    /// A tail answers "how did this end", which is the wrong question for a
    /// run that wedged in the middle: a 14-minute stall at minute 4 of 18
    /// leaves a tail that looks perfectly healthy. Issue #136 tracks adding
    /// the complementary view — the log lines followed by the longest
    /// silences, with context — to this report. Until then, the run's full
    /// stream is persisted next to the data (see [`run_pipeline`]) and
    /// `scripts/dag_profile.py` will find the gaps.
    fn stderr_tail(&self, n: usize) -> String {
        let lines: Vec<&str> = self.stderr.lines().collect();
        lines[lines.len().saturating_sub(n)..].join("\n")
    }
}

/// Spawn `datalib-dag <config> --now <now> [extra…]`.
///
/// The config is a POSITIONAL arg — there is no `--config` flag. stderr is
/// captured (not inherited) because that is where the NDJSON run record goes;
/// it is echoed on failure via [`PipelineRun::stderr_tail`], and in full when
/// `--nocapture` is in play and the test fails.
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
    //
    //   scripts/dag_profile.py <run_root>/<now>.ndjson
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

/// Fail unless every step in a `run_summary` reached `succeeded`.
///
/// A non-zero exit already fails the run, but this catches the subtler
/// shapes the exit code alone would let through on a green run — and names
/// the offending step instead of leaving you to diff 2000 snapshots to find
/// which source went quiet.
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

/// Sorted step ids from a `run_summary`, one per line — the snapshot body that
/// pins which steps the config declared.
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

/// Per-stanza incrementality record, built from each source's own
/// `sync_runs` table — `{stanza: {status, summary}}` for that stanza's most
/// recent run.
///
/// `sync_runs.summary` is where the counts live: `deltas` (per-table
/// added/modified/removed, from `dolt_diff_<table>`) plus `cursors` (the
/// `sync_scope_state` scopes that moved). A regression that broke
/// incrementality — the `full_sync: true` override that bit us once — shows up
/// as `deltas.<table>.added` back at first-run scale.
///
/// Only the API-backed providers stamp `sync_runs` — as of writing, the eight
/// that route their download through `DownloadRun`: slack, github, gitlab,
/// notion, anthropic, chatgpt, email, beeper. File-backed sources (carddav,
/// linkedin, google_takeout, sms_backup_restore, fsindex) read a local export
/// and never open a run, so their table is present but empty. That is the
/// right answer rather than a gap: incrementality here means "didn't re-fetch
/// from upstream", and there is no upstream to be incremental about.
///
/// Every stanza is recorded either way — a source going quiet must move this
/// snapshot, not vanish from it — but with an explicit marker string instead
/// of a bare `null`, so a reader can tell "nothing to report" from "something
/// broke". The two are distinguished.
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

/// The highest-`run_id` row of `sync_runs`, as `{status, summary}` with
/// `summary` parsed from its JSON text so the diff stays at JSON granularity.
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
///
/// * `sync_runs` — the audit log; a reset adds a row by definition.
/// * `sync_scope_state` — the resume cursor.
/// * `sync_scope_config` — the recorded scope config, carrying a wall-clock
///   `updated_at` that is re-stamped on every run. Missing from this list
///   originally, which made the stability assertion fail on a pure
///   timestamp — a false positive that looked like a real content leak.
const NON_CONTENT_TABLES: &[&str] = &["sync_runs", "sync_scope_state", "sync_scope_config"];

/// Dump only the entity *content* tables of a doltlite DB for the
/// --reset-and-redownload stability assertion: drops every
/// `*_bookkeeping` sidecar (per-fetch stamps + the `volatile_payload`
/// split-outs) plus [`NON_CONTENT_TABLES`].
///
/// Deliberately NOT volatile-redacted: the whole point is to catch a
/// field that drifts on re-fetch, which `strip_volatile` would mask.
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
        // Doltlite leaves behind sidecar lock files (e.g.
        // `.foo.doltlite_db-lock`) for in-process flock coordination.
        // They're ephemeral, content-free, and would clutter goldens
        // with hidden-dotfile noise. Skip anything whose name ends in
        // `-lock`. Also skip the constant `CACHEDIR.TAG` marker (backup
        // hint, not rendered content — asserted separately above).
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

/// Delete per-stanza `.snap` files (those under a `raw/` or `rendered_md/`
/// segment) that don't correspond to a key in `manifest` — orphans from a
/// prior run whose produced paths have since changed. The top-level meta snaps
/// (`manifest`, `sync_summary*`) are left untouched. No-op outside update mode
/// (a check run surfaces the change via the `manifest` snapshot diff instead,
/// and must never mutate the version-controlled golden).
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
        // Snapshot path mirrors the data layout: a file at
        // `<base>/<stanza>/<sub>/<canonical_rel>.snap` corresponds to manifest
        // key `<stanza>/<sub>/<canonical_rel>`. Only the per-stanza tree snaps
        // are managed by the manifest; the top-level meta snaps aren't keyed
        // there, so we only ever prune snaps living under a `raw/` or
        // `rendered_md/` path segment.
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

/// Recursively remove directories under `dir` that became empty after
/// pruning (leaves `dir` itself in place).
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

/// Patch the DAG config with the two test-only tweaks: point `data_root` at
/// this run's directory, and bump slack `refresh_window_days` so a fresh
/// data_root re-downloads media.
///
/// The pre-DAG version also forced `qmd.skip=true`; in the steps format that
/// is expressed by the config simply not declaring a `qmd_index` step, so
/// there is nothing to override here. We assert it rather than assume it — a
/// `qmd_index` step would make the golden non-deterministic (its status embeds
/// byte sizes and a relative "updated N seconds ago").
///
/// Slack's knob is reached by *step id* (`<stanza>.download` whose command is
/// `datalib-step download slack_api`), not by a `type` field — in the steps
/// format the provider type lives in the `command` string.
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
            // `volatile_payload` (the bookkeeping sidecar's split-out
            // per-fetch fields, e.g. Slack's `updated`) holds churn BY
            // DEFINITION — that's why it was split off the content
            // payload. Never snapshot it: it would make the golden
            // non-deterministic across runs. See
            // data_architecture_ingestion.md §"Volatile-field split".
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

/// Stricter-on-time-fields, looser-on-stats redaction used for the
/// run 2 incrementality snapshot. Unlike [`strip_volatile`] this
/// preserves per-source `stats` counts (since the whole point of the
/// run 2 snapshot is to assert those counts stayed *small*) while
/// redacting the per-run jitter fields that can't reproduce
/// byte-identically (wall-clock timings, cursor advancement
/// timestamps).
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
        "captured_at",
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
