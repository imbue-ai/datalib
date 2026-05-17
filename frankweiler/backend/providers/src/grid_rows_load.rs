//! Generic Load step: walk a `rendered_md/` tree of `.grid_rows.json`
//! sidecars and upsert their rows into Dolt.
//!
//! The sidecar format is the cross-provider contract between Translate
//! and Load:
//!
//! ```jsonc
//! {
//!   "header": {
//!     "document_uuid": "…",            // primary key for the document
//!     "source_fingerprint": "…",       // hash of upstream payload
//!     "render_version": 1              // renderer-side schema stamp
//!   },
//!   "rows": [GridRow, …]
//! }
//! ```
//!
//! Per document we DELETE existing rows by `document_uuid` and INSERT
//! the fresh set, then UPSERT a row into the `documents_loaded` table
//! so the next run can skip unchanged files. Same delete-then-insert
//! pattern as the Python `populate_grid_rows`
//! (`src/ingest/grid_rows.py:824,839-841`), generalized so any
//! provider's Translate step can produce a sidecar tree that this
//! loader consumes verbatim.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_schema::grid_rows::{GridRow, DDL as GRID_ROWS_DDL};
use serde::{Deserialize, Serialize};
use sqlx::mysql::MySqlPool;
use sqlx::Row;

/// `CREATE TABLE` for the loader's own bookkeeping table. DDL is
/// auto-committed even under Dolt's `--no-auto-commit`, so this is
/// idempotent across runs.
pub const DOCUMENTS_LOADED_DDL: &str = r#"CREATE TABLE IF NOT EXISTS documents_loaded (
    qmd_path VARCHAR(512) NOT NULL,
    document_uuid VARCHAR(96) NOT NULL,
    source_fingerprint VARCHAR(64) NOT NULL,
    loaded_at VARCHAR(40) NOT NULL,
    PRIMARY KEY (qmd_path)
)"#;

#[derive(Debug, Serialize, Deserialize)]
pub struct SidecarHeader {
    /// Stable id for the document this sidecar describes. The Slack
    /// Translate step sets this to the thread uuid; other providers
    /// (Notion page, GitHub issue, etc.) plug in their own
    /// document-level uuid.
    pub document_uuid: String,
    pub source_fingerprint: String,
    pub render_version: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Sidecar {
    pub header: SidecarHeader,
    pub rows: Vec<GridRow>,
}

/// Stats emitted on every load run. Stable shape so a web UI can poll
/// or stream it without per-provider branches.
#[derive(Debug, Default, Serialize)]
pub struct LoadSummary {
    pub documents_total: usize,
    pub documents_loaded: usize,
    pub documents_skipped: usize,
    pub rows_inserted: usize,
}

/// Apply DDL for `grid_rows` and `documents_loaded`. Idempotent.
pub async fn init_schema(pool: &MySqlPool) -> Result<()> {
    for (_table, ddl) in GRID_ROWS_DDL {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .context("create grid_rows")?;
    }
    sqlx::query(DOCUMENTS_LOADED_DDL)
        .execute(pool)
        .await
        .context("create documents_loaded")?;
    Ok(())
}

/// Walk `<out>/rendered_md/` for every `*.grid_rows.json` sidecar (any
/// provider) and load each into Dolt. `out_dir` is also the prefix
/// stripped from the sidecar's `.md` path to derive the
/// `documents_loaded.qmd_path` key.
pub async fn load_all(
    pool: &MySqlPool,
    out_dir: &Path,
    progress: impl Fn(&str),
) -> Result<LoadSummary> {
    let rendered_root = out_dir.join("rendered_md");
    let mut sidecars: Vec<PathBuf> = Vec::new();
    if rendered_root.exists() {
        collect_sidecars(&rendered_root, &mut sidecars);
    }
    sidecars.sort();

    let mut summary = LoadSummary {
        documents_total: sidecars.len(),
        ..Default::default()
    };

    for sidecar_path in &sidecars {
        let raw = fs::read_to_string(sidecar_path)
            .with_context(|| format!("read {}", sidecar_path.display()))?;
        let sidecar: Sidecar = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", sidecar_path.display()))?;

        let md_path = derive_md_path(sidecar_path)
            .with_context(|| format!("derive .md path from {}", sidecar_path.display()))?;
        let qmd_rel = md_path
            .strip_prefix(out_dir)
            .unwrap_or(&md_path)
            .to_string_lossy()
            .to_string();

        let document_uuid = sidecar.header.document_uuid.clone();
        let fingerprint = sidecar.header.source_fingerprint.clone();

        if existing_fingerprint(pool, &qmd_rel).await? == Some(fingerprint.clone()) {
            summary.documents_skipped += 1;
            continue;
        }

        let inserted = apply_document(pool, &document_uuid, &qmd_rel, &fingerprint, &sidecar.rows)
            .await
            .with_context(|| format!("load {}", sidecar_path.display()))?;
        summary.rows_inserted += inserted;
        summary.documents_loaded += 1;
        progress(&format!(
            "loaded {}/{}",
            summary.documents_loaded + summary.documents_skipped,
            summary.documents_total
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

async fn apply_document(
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

    let msg = format!("grid-rows-load: {document_uuid} {fingerprint}");
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
