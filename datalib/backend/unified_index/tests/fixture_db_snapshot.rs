//! Snapshot of the TNG fixture's `backend_index.doltlite_db` contents.

use std::path::PathBuf;
use std::str::FromStr;

use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

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

fn stable_source_url(v: Option<String>) -> Option<String> {
    let u = v?;
    let Some(rest) = u.strip_prefix("file://") else {
        return Some(u);
    };
    let tail = rest.rsplit('/').next().unwrap_or(rest);
    Some(format!("file://…/{tail}"))
}

fn stable_row_set_hash(provider: Option<&str>, v: Option<String>) -> Option<String> {
    match provider {
        Some("pdf") => Some("<machine-specific: rows embed an absolute path>".to_string()),
        _ => v,
    }
}

#[tokio::test]
async fn snapshot_grid_rows_and_documents() {
    let db = fixture_db_path();
    let pool = open_readonly(&db).await;

    // ── grid_rows ────────────────────────────────────────────────
    let rows = sqlx::query(
        "SELECT uuid, provider, kind, source_label, when_ts, author, account, \
                project, org_uuid, org_name, channel, conversation_name, conversation_uuid, \
                message_index, entire_chat, text, slack_link, qmd_path, \
                source_url, git_sha, upstream_id, upstream_entity_kind, upstream_scope, \
                notion_page_uuid, \
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
                // `Option<String>`, not `String`: `when_ts` is nullable, and
                // reading it as a bare `String` rendered SQL NULL as `""` —
                // so this golden could not tell "upstream gave us no
                // timestamp" from "upstream gave us an empty one". That is
                // exactly the distinction §6 turns on, and it was invisible
                // here until a fixture finally had an undated record.
                "when_ts": r.try_get::<Option<String>, _>("when_ts").ok().flatten(),
                "author": r.try_get::<Option<String>, _>("author").ok().flatten(),
                "account": r.try_get::<Option<String>, _>("account").ok().flatten(),
                "project": r.try_get::<Option<String>, _>("project").ok().flatten(),
                "org_uuid": r.try_get::<Option<String>, _>("org_uuid").ok().flatten(),
                "org_name": r.try_get::<Option<String>, _>("org_name").ok().flatten(),
                "channel": r.try_get::<Option<String>, _>("channel").ok().flatten(),
                "conversation_name": r.try_get::<Option<String>, _>("conversation_name").ok().flatten(),
                "conversation_uuid": r.try_get::<String, _>("conversation_uuid").ok(),
                "message_index": r.try_get::<Option<i64>, _>("message_index").ok().flatten(),
                "text_len": text.chars().count(),
                "text_sha": digest(&text),
                "entire_chat": entire_chat,
                "slack_link": r.try_get::<Option<String>, _>("slack_link").ok().flatten(),
                "qmd_path": r.try_get::<Option<String>, _>("qmd_path").ok().flatten(),
                "source_url": stable_source_url(
                    r.try_get::<Option<String>, _>("source_url").ok().flatten(),
                ),
                "git_sha": r.try_get::<Option<String>, _>("git_sha").ok().flatten(),
                "upstream_id": r.try_get::<Option<String>, _>("upstream_id").ok().flatten(),
                "upstream_entity_kind": r.try_get::<Option<String>, _>("upstream_entity_kind").ok().flatten(),
                "upstream_scope": r.try_get::<Option<String>, _>("upstream_scope").ok().flatten(),
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
                "row_set_hash": stable_row_set_hash(
                    r.try_get::<String, _>("provider").ok().as_deref(),
                    r.try_get::<Option<String>, _>("row_set_hash").ok().flatten(),
                ),
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
