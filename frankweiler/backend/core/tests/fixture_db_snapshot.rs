//! Snapshot of the TNG fixture's `mirror.db` contents.
//!
//! Catches silent data regressions in the load pipeline. Most recently
//! the "WAL not checkpointed at sync close" bug, where the genrule
//! shipped a 4 KB empty `mirror.db` while all the actual rows sat in
//! the discarded `mirror.db-wal` — every e2e test got back zero rows
//! and we only noticed via the UI failures. A snapshot at the SQL
//! level fails immediately on that kind of drift, and shows a
//! reviewable diff of exactly what changed.
//!
//! Snapshot contents: `grid_rows`, `documents`, and `documents_loaded`
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

/// Locate `mirror.db`. Under Bazel, the test sees it at the path
/// rules_rust resolves via `$(rootpath)` (passed via the `FW_FIXTURE_DB`
/// env in BUILD.bazel). Under plain `cargo test`, fall back to the
/// workspace's `bazel-bin/` convenience symlink — the developer has to
/// have run `bazelisk build //tests/fixtures:ingested_tng` at least
/// once. The fallback panics with a clear message if the file isn't
/// there, instead of silently snapshotting an empty DB.
fn fixture_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("FW_FIXTURE_DB") {
        let pb = PathBuf::from(p);
        if pb.exists() {
            return pb;
        }
    }
    let cargo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let bazel_bin = cargo_root
        .join("../../../bazel-bin/tests/fixtures/ingested/mirror.db")
        .canonicalize()
        .unwrap_or_else(|_| {
            panic!(
                "fixture mirror.db not found. Run \
                 `bazelisk build //tests/fixtures:ingested_tng` first."
            )
        });
    bazel_bin
}

async fn open_readonly(path: &std::path::Path) -> SqlitePool {
    // Open read-only so the test can't accidentally write to the
    // shared bazel-bin artifact. `synchronous = NORMAL` is fine for a
    // ro open; we set it anyway to silence sqlx's default.
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .expect("parse url")
        .read_only(true)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap_or_else(|e| panic!("open {}: {e}", path.display()))
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
                notion_block_uuid, document_uuid \
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
                "document_uuid": r.try_get::<Option<String>, _>("document_uuid").ok().flatten(),
            })
        })
        .collect();

    // ── documents ────────────────────────────────────────────────
    let drows = sqlx::query(
        "SELECT document_uuid, source_name, provider, kind, title, \
                created_at, updated_at, md_path, row_set_hash, \
                renderer_version, rendered_at \
         FROM documents ORDER BY document_uuid",
    )
    .fetch_all(&pool)
    .await
    .expect("read documents");

    let documents: Vec<serde_json::Value> = drows
        .iter()
        .map(|r| {
            json!({
                "document_uuid": r.try_get::<String, _>("document_uuid").ok(),
                "source_name": r.try_get::<String, _>("source_name").ok(),
                "provider": r.try_get::<String, _>("provider").ok(),
                "kind": r.try_get::<String, _>("kind").ok(),
                "title": r.try_get::<Option<String>, _>("title").ok().flatten(),
                "created_at": r.try_get::<Option<String>, _>("created_at").ok().flatten(),
                "updated_at": r.try_get::<Option<String>, _>("updated_at").ok().flatten(),
                "md_path": r.try_get::<Option<String>, _>("md_path").ok().flatten(),
                "row_set_hash": r.try_get::<String, _>("row_set_hash").ok(),
                "renderer_version": r.try_get::<String, _>("renderer_version").ok(),
                "rendered_at": r.try_get::<Option<String>, _>("rendered_at").ok().flatten(),
            })
        })
        .collect();

    // ── documents_loaded ─────────────────────────────────────────
    let lrows = sqlx::query(
        "SELECT qmd_path, document_uuid, source_fingerprint, loaded_at \
         FROM documents_loaded ORDER BY qmd_path",
    )
    .fetch_all(&pool)
    .await
    .expect("read documents_loaded");

    let documents_loaded: Vec<serde_json::Value> = lrows
        .iter()
        .map(|r| {
            json!({
                "qmd_path": r.try_get::<String, _>("qmd_path").ok(),
                "document_uuid": r.try_get::<String, _>("document_uuid").ok(),
                "source_fingerprint": r.try_get::<String, _>("source_fingerprint").ok(),
                "loaded_at": r.try_get::<String, _>("loaded_at").ok(),
            })
        })
        .collect();

    let bundle = json!({
        "summary": {
            "grid_rows_count": grid_rows.len(),
            "documents_count": documents.len(),
            "documents_loaded_count": documents_loaded.len(),
        },
        "grid_rows": grid_rows,
        "documents": documents,
        "documents_loaded": documents_loaded,
    });

    // Pretty-printed JSON is the most diff-friendly representation —
    // one field per line, sorted keys, no insta-yaml quoting surprises.
    let snapshot = serde_json::to_string_pretty(&bundle).expect("serialize");
    insta::assert_snapshot!("fixture_mirror_db", snapshot);
}
