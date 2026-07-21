//! Snapshot of the TNG fixture's `backend_index.doltlite_db` contents.
//!
//! Catches silent data regressions in the load pipeline. Most recently
//! the "WAL not checkpointed at sync close" bug, where the genrule
//! shipped a 4 KB empty `backend_index.doltlite_db` while all the
//! actual rows sat in the discarded `backend_index.doltlite_db-wal` —
//! every e2e test got back zero rows
//! and we only noticed via the UI failures. A snapshot at the SQL
//! level fails immediately on that kind of drift, and shows a
//! reviewable diff of exactly what changed.
//!
//! Snapshot contents: `grid_rows`, `documents`, and `markdowns_loaded`
//! tables, each dumped as one JSON object per row, sorted by their
//! primary key for stability. Long `text` bodies are truncated +
//! hashed to keep the snapshot diff-friendly (a one-character change
//! in a chat body changes one digest, not 50 lines). The fixture
//! genrule is deterministic given a fixed `--now`, so every column
//! including timestamps is reproducible run-to-run.
//!
//! How to update: change something that affects the output, then run
//! `cargo insta review` (or just delete `tests/snapshots/*.snap.new`
//! to discard).

use std::path::PathBuf;
use std::str::FromStr;

use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

/// Locate `backend_index.doltlite_db`. Two paths into this resolution:
///
/// 1. **Under bazel test**: the fixture is in the test's runfiles tree,
///    declared as a `data` dep in BUILD.bazel. We resolve it via the
///    `runfiles` crate's `rlocation()` lookup, which is portable across
///    compilation modes (fastbuild/opt) and operating systems and
///    doesn't depend on the test's CWD or workspace-relative
///    `$(rootpath ...)` resolution against an inconsistent base. (An
///    earlier version of this code used `FW_FIXTURE_DB=$(rootpath ...)`
///    and worked locally under fastbuild but broke on CI under opt —
///    the path was workspace-relative but the test's CWD wasn't the
///    workspace root.)
///
/// 2. **Under plain `cargo test`**: no runfiles tree exists. Fall back
///    to the workspace's `bazel-bin/` convenience symlink — the
///    developer must have run `bazelisk build //tests/fixtures:ingested_tng`
///    at least once. Panics with a clear message if the file isn't
///    there, instead of silently snapshotting an empty DB.
fn fixture_db_path() -> PathBuf {
    if let Ok(r) = runfiles::Runfiles::create() {
        if let Some(candidate) =
            r.rlocation("_main/tests/fixtures/ingested/backend_index.doltlite_db")
        {
            if candidate.exists() {
                return candidate;
            }
        }
    }
    let cargo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let bazel_bin = cargo_root
        .join("../../../bazel-bin/tests/fixtures/ingested/backend_index.doltlite_db")
        .canonicalize()
        .unwrap_or_else(|_| {
            panic!(
                "fixture backend_index.doltlite_db not found. Run \
                 `bazelisk build //tests/fixtures:ingested_tng` first."
            )
        });
    bazel_bin
}

async fn open_readonly(path: &std::path::Path) -> SqlitePool {
    // Doltlite-format databases reject `immutable=1` (and the WAL-mode
    // open machinery in general) — the prolly chunk store doesn't
    // model a frozen-bytes view the way stock SQLite's pager does. So
    // we open with `read_only=true` only.
    //
    // We canonicalize the path before passing it to sqlx because
    // doltlite's chunk_store does NOT resolve symlinks — it stat()s
    // the path the caller passed and fails with SQLITE_CANTOPEN when
    // that path is a symlink (even one pointing at a perfectly valid
    // doltlite file). Bazel's runfiles tree is entirely symlinks, so
    // every test that opens a `data`-dep'd doltlite file would fail
    // without this. `canonicalize` follows the symlinks at the OS
    // level so doltlite sees the real on-disk path.
    let real = path
        .canonicalize()
        .unwrap_or_else(|e| panic!("canonicalize {}: {e}", path.display()));
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", real.display()))
        .expect("parse url")
        .read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap_or_else(|e| panic!("open {}: {e}", real.display()))
}

/// SHA-256 of a long string, truncated to 16 hex chars. We snapshot
/// the digest of long body fields (`text`, `entire_chat`) instead of
/// the body itself: the body changes break ~50 lines of diff for what
/// is conceptually a one-row update, and reading a 5KB markdown chat
/// in a `.snap` file isn't actually useful. The digest still catches
/// the regression — if a row's body changes, its digest changes.
fn digest(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    let d = h.finalize();
    format!("sha256:{:.16x}", BytesAsHex(&d[..]))
}

struct BytesAsHex<'a>(&'a [u8]);
impl<'a> std::fmt::LowerHex for BytesAsHex<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.0 {
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

#[tokio::test]
async fn snapshot_grid_rows_and_documents() {
    let db = fixture_db_path();
    let pool = open_readonly(&db).await;

    // ── grid_rows ────────────────────────────────────────────────
    let rows = sqlx::query(
        "SELECT uuid, provider, kind, source_label, when_ts, author, account, \
                project, channel, conversation_name, conversation_uuid, \
                message_index, entire_chat, text, slack_link, qmd_path, \
                source_url, git_sha, external_id, notion_page_uuid, \
                notion_block_uuid, markdown_uuid \
         FROM grid_rows ORDER BY uuid",
    )
    .fetch_all(&pool)
    .await
    .expect("read grid_rows");

    let grid_rows: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let text: String = r.try_get("text").unwrap_or_default();
            let entire_chat: String = r.try_get("entire_chat").unwrap_or_default();
            json!({
                "uuid": r.try_get::<String, _>("uuid").ok(),
                "provider": r.try_get::<String, _>("provider").ok(),
                "kind": r.try_get::<String, _>("kind").ok(),
                "source_label": r.try_get::<String, _>("source_label").ok(),
                "when_ts": r.try_get::<String, _>("when_ts").ok(),
                "author": r.try_get::<Option<String>, _>("author").ok().flatten(),
                "account": r.try_get::<Option<String>, _>("account").ok().flatten(),
                "project": r.try_get::<Option<String>, _>("project").ok().flatten(),
                "channel": r.try_get::<Option<String>, _>("channel").ok().flatten(),
                "conversation_name": r.try_get::<Option<String>, _>("conversation_name").ok().flatten(),
                "conversation_uuid": r.try_get::<String, _>("conversation_uuid").ok(),
                "message_index": r.try_get::<Option<i64>, _>("message_index").ok().flatten(),
                "text_len": text.chars().count(),
                "text_sha": digest(&text),
                "entire_chat": entire_chat,
                "slack_link": r.try_get::<Option<String>, _>("slack_link").ok().flatten(),
                "qmd_path": r.try_get::<Option<String>, _>("qmd_path").ok().flatten(),
                "source_url": r.try_get::<Option<String>, _>("source_url").ok().flatten(),
                "git_sha": r.try_get::<Option<String>, _>("git_sha").ok().flatten(),
                "external_id": r.try_get::<Option<String>, _>("external_id").ok().flatten(),
                "notion_page_uuid": r.try_get::<Option<String>, _>("notion_page_uuid").ok().flatten(),
                "notion_block_uuid": r.try_get::<Option<String>, _>("notion_block_uuid").ok().flatten(),
                "markdown_uuid": r.try_get::<Option<String>, _>("markdown_uuid").ok().flatten(),
            })
        })
        .collect();

    // ── documents ────────────────────────────────────────────────
    // Includes source_fingerprint (render's input-hash) since the
    // markdowns_loaded table merged into documents.
    let drows = sqlx::query(
        "SELECT markdown_uuid, source_name, provider, kind, title, \
                created_at, updated_at, md_path, source_fingerprint, \
                row_set_hash, renderer_version, rendered_at \
         FROM markdowns ORDER BY markdown_uuid",
    )
    .fetch_all(&pool)
    .await
    .expect("read documents");

    let documents: Vec<serde_json::Value> = drows
        .iter()
        .map(|r| {
            json!({
                "markdown_uuid": r.try_get::<String, _>("markdown_uuid").ok(),
                "source_name": r.try_get::<String, _>("source_name").ok(),
                "provider": r.try_get::<String, _>("provider").ok(),
                "kind": r.try_get::<String, _>("kind").ok(),
                "title": r.try_get::<Option<String>, _>("title").ok().flatten(),
                "created_at": r.try_get::<Option<String>, _>("created_at").ok().flatten(),
                "updated_at": r.try_get::<Option<String>, _>("updated_at").ok().flatten(),
                "md_path": r.try_get::<Option<String>, _>("md_path").ok().flatten(),
                "source_fingerprint": r.try_get::<Option<String>, _>("source_fingerprint").ok().flatten(),
                "row_set_hash": r.try_get::<Option<String>, _>("row_set_hash").ok().flatten(),
                "renderer_version": r.try_get::<Option<String>, _>("renderer_version").ok().flatten(),
                "rendered_at": r.try_get::<Option<String>, _>("rendered_at").ok().flatten(),
            })
        })
        .collect();

    // ── dolt_log ─────────────────────────────────────────────────
    // doltlite stamps every `dolt_commit` call into `dolt_log`. The
    // grid_index step issues exactly ONE commit per run for the
    // index DB (see datalib_step::grid_index);
    // that, plus doltlite's own "Initialize data repository" boot
    // commit, is what we expect to see here. Snapshotting the
    // commit-message column catches:
    //
    //   * Regression to the per-doc commit pattern we removed — the
    //     log would balloon from 2 entries to hundreds.
    //   * The orchestrator silently skipping the commit (e.g. if a
    //     future refactor drops the closing commit) — the log
    //     would shrink to 1 entry.
    //   * Format drift in the commit-message template (the stats
    //     string would change shape).
    //
    // We don't snapshot the commit hashes themselves — they're
    // content-addressed and would change on any byte-level data
    // change. Author/email is also not snapshotted: it comes from
    // host `git config` which differs between dev machines and CI.
    //
    // Tiebreak by `message`, NOT by `commit_hash`. With the fixture's
    // fixed `--now`, the boot "Initialize data repository" commit and
    // the sync stats commit land with identical `date` values, and
    // SQLite's ordering on ties is unspecified — without an explicit
    // tiebreaker, small unrelated changes (e.g. adding a provider) can
    // flip the order and produce spurious snapshot churn. An earlier
    // version of this test reached for `commit_hash` as the tiebreak,
    // but doltlite's hashes are content-addressed and change between
    // doltlite versions (0.11.4 → 0.11.5 silently swapped the order),
    // so they're the wrong axis to anchor a snapshot on. `message` is
    // stable across doltlite bumps and a real change to either commit
    // message is something we'd want the snapshot to surface anyway.
    // If two distinct commits ever produce truly identical messages
    // (e.g. two back-to-back empty-stats sync runs), the snapshot will
    // start being order-unstable on `message` too and the test author
    // will pick a better discriminator at that point.
    let log_rows = sqlx::query("SELECT message FROM dolt_log() ORDER BY date ASC, message ASC")
        .fetch_all(&pool)
        .await
        .expect("read dolt_log");

    let dolt_log: Vec<serde_json::Value> = log_rows
        .iter()
        .map(|r| json!({"message": r.try_get::<String, _>("message").ok()}))
        .collect();

    let bundle = json!({
        "summary": {
            "grid_rows_count": grid_rows.len(),
            "documents_count": documents.len(),
            "dolt_log_count": dolt_log.len(),
        },
        "grid_rows": grid_rows,
        "documents": documents,
        "dolt_log": dolt_log,
    });

    // Pretty-printed JSON is the most diff-friendly representation —
    // one field per line, sorted keys, no insta-yaml quoting surprises.
    let snapshot = serde_json::to_string_pretty(&bundle).expect("serialize");
    insta::assert_snapshot!("fixture_backend_index", snapshot);
}
