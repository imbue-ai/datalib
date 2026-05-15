//! Load rendered Slack grid_rows sidecars into Dolt.
//!
//! Reads `<out>/rendered_md/slack/**/*.grid_rows.json`, written by the
//! `slack-render` binary. The sidecar header carries `source_fingerprint`
//! and `thread_uuid` (== `document_uuid`); we use the fingerprint to
//! skip already-loaded threads via a `documents_loaded(qmd_path PK,
//! source_fingerprint)` table.
//!
//! Per changed thread:
//!   1. `DELETE FROM grid_rows WHERE document_uuid = ?`
//!   2. `INSERT INTO grid_rows ...` for every row in the sidecar
//!   3. UPSERT into `documents_loaded`
//!   4. `CALL DOLT_COMMIT('-Am', '...')` to publish across connections
//!      (Dolt's working set is session-scoped under `--no-auto-commit`).
//!
//! The loader never reads `raw_api/` — its contract is the `.grid_rows.json`
//! sidecar, full stop.
//!
//! Match the Python `populate_grid_rows` delete-then-insert pattern
//! (`src/ingest/grid_rows.py:824,839-841`).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_schema::grid_rows::{GridRow, DDL as GRID_ROWS_DDL};
use sqlx::mysql::MySqlPool;
use sqlx::Row;

use super::render::Sidecar;

/// `CREATE TABLE` for the loader's own bookkeeping table. The Dolt
/// `--no-auto-commit` flag affects rows in the working set; DDL is
/// committed automatically, so this is safe to run on every startup.
pub const DOCUMENTS_LOADED_DDL: &str = r#"CREATE TABLE IF NOT EXISTS documents_loaded (
    qmd_path VARCHAR(512) NOT NULL,
    document_uuid VARCHAR(96) NOT NULL,
    source_fingerprint VARCHAR(64) NOT NULL,
    loaded_at VARCHAR(40) NOT NULL,
    PRIMARY KEY (qmd_path)
)"#;

#[derive(Debug, Default)]
pub struct LoadSummary {
    pub threads_total: usize,
    pub threads_loaded: usize,
    pub threads_skipped: usize,
    pub rows_inserted: usize,
}

/// Apply DDL for `grid_rows` and `documents_loaded`. Idempotent.
pub async fn init_schema(pool: &MySqlPool) -> Result<()> {
    for (_table, ddl) in GRID_ROWS_DDL {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .with_context(|| "create grid_rows")?;
    }
    sqlx::query(DOCUMENTS_LOADED_DDL)
        .execute(pool)
        .await
        .with_context(|| "create documents_loaded")?;
    Ok(())
}

/// Walk `<out>/rendered_md/slack/` for `.grid_rows.json` sidecars and
/// load each one into Dolt. `out_dir` is also the prefix for the
/// `qmd_path` stored in `grid_rows` (relative to the data root).
pub async fn load_all(
    pool: &MySqlPool,
    out_dir: &Path,
    progress: impl Fn(&str),
) -> Result<LoadSummary> {
    let slack_root = out_dir.join("rendered_md").join("slack");
    let mut sidecars: Vec<PathBuf> = Vec::new();
    if slack_root.exists() {
        collect_sidecars(&slack_root, &mut sidecars);
    }
    sidecars.sort();

    let mut summary = LoadSummary {
        threads_total: sidecars.len(),
        ..Default::default()
    };

    for sidecar_path in &sidecars {
        let raw = fs::read_to_string(sidecar_path)
            .with_context(|| format!("read {}", sidecar_path.display()))?;
        let sidecar: Sidecar = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", sidecar_path.display()))?;

        // qmd_path key: the .md file's path relative to `out_dir`,
        // matching what render.rs stamps into `grid_rows.qmd_path`.
        let md_path = sidecar_path.with_extension("").with_extension("md");
        // `.with_extension("").with_extension("md")` strips `.json` then
        // `.grid_rows`, leaving `<stem>.md`. Belt-and-braces: derive from
        // the sidecar filename explicitly.
        let md_path = derive_md_path(sidecar_path).unwrap_or(md_path);
        let qmd_rel = md_path
            .strip_prefix(out_dir)
            .unwrap_or(&md_path)
            .to_string_lossy()
            .to_string();

        let document_uuid = sidecar.header.thread_uuid.clone();
        let fingerprint = sidecar.header.source_fingerprint.clone();

        if existing_fingerprint(pool, &qmd_rel).await? == Some(fingerprint.clone()) {
            summary.threads_skipped += 1;
            continue;
        }

        let inserted = apply_thread(pool, &document_uuid, &qmd_rel, &fingerprint, &sidecar.rows)
            .await
            .with_context(|| format!("load {}", sidecar_path.display()))?;
        summary.rows_inserted += inserted;
        summary.threads_loaded += 1;
        progress(&format!(
            "loaded {}/{}",
            summary.threads_loaded + summary.threads_skipped,
            summary.threads_total
        ));
    }
    Ok(summary)
}

fn collect_sidecars(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let p = entry.path();
        if p.is_dir() {
            collect_sidecars(&p, out);
        } else if p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(".grid_rows.json"))
        {
            out.push(p);
        }
    }
}

fn derive_md_path(sidecar: &Path) -> Option<PathBuf> {
    let name = sidecar.file_name()?.to_str()?;
    let stem = name.strip_suffix(".grid_rows.json")?;
    Some(sidecar.with_file_name(format!("{stem}.md")))
}

async fn existing_fingerprint(pool: &MySqlPool, qmd_path: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT source_fingerprint FROM documents_loaded WHERE qmd_path = ?")
        .bind(qmd_path)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|r| r.try_get::<String, _>("source_fingerprint").ok()))
}

async fn apply_thread(
    pool: &MySqlPool,
    document_uuid: &str,
    qmd_path: &str,
    fingerprint: &str,
    rows: &[GridRow],
) -> Result<usize> {
    let mut conn = pool.acquire().await.context("acquire conn")?;

    sqlx::query("DELETE FROM grid_rows WHERE document_uuid = ?")
        .bind(document_uuid)
        .execute(&mut *conn)
        .await
        .context("delete prior rows")?;

    for row in rows {
        insert_grid_row(&mut conn, row).await?;
    }

    let loaded_at = chrono::Utc::now().to_rfc3339();
    sqlx::query(
        "INSERT INTO documents_loaded (qmd_path, document_uuid, source_fingerprint, loaded_at) \
         VALUES (?, ?, ?, ?) \
         ON DUPLICATE KEY UPDATE document_uuid = VALUES(document_uuid), \
                                 source_fingerprint = VALUES(source_fingerprint), \
                                 loaded_at = VALUES(loaded_at)",
    )
    .bind(qmd_path)
    .bind(document_uuid)
    .bind(fingerprint)
    .bind(&loaded_at)
    .execute(&mut *conn)
    .await
    .context("upsert documents_loaded")?;

    let msg = format!("slack-load: {document_uuid} {fingerprint}");
    sqlx::query("CALL DOLT_COMMIT('-Am', ?)")
        .bind(&msg)
        .execute(&mut *conn)
        .await
        .context("dolt_commit")?;

    Ok(rows.len())
}

async fn insert_grid_row(
    conn: &mut sqlx::pool::PoolConnection<sqlx::MySql>,
    row: &GridRow,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO grid_rows \
         (uuid, provider, kind, source_label, when_ts, author, account, project, channel, \
          conversation_name, conversation_uuid, message_index, entire_chat, text, slack_link, \
          qmd_path, source_url, git_sha, external_id, notion_page_uuid, notion_block_uuid, \
          document_uuid) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&row.uuid)
    .bind(&row.provider)
    .bind(&row.kind)
    .bind(&row.source_label)
    .bind(&row.when_ts)
    .bind(&row.author)
    .bind(&row.account)
    .bind(&row.project)
    .bind(&row.channel)
    .bind(&row.conversation_name)
    .bind(&row.conversation_uuid)
    .bind(row.message_index)
    .bind(&row.entire_chat)
    .bind(&row.text)
    .bind(&row.slack_link)
    .bind(&row.qmd_path)
    .bind(&row.source_url)
    .bind(&row.git_sha)
    .bind(&row.external_id)
    .bind(&row.notion_page_uuid)
    .bind(&row.notion_block_uuid)
    .bind(&row.document_uuid)
    .execute(&mut **conn)
    .await
    .with_context(|| format!("insert grid_row {}", row.uuid))?;
    Ok(())
}
