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
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use crate::sidecar::Sidecar;

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

/// Document-level metadata projection, one row per renderable document.
/// Mirrors the Python `documents` schema (see `schemas/documents.schema.json`).
pub const DOCUMENTS_DDL: &str = r#"CREATE TABLE IF NOT EXISTS documents (
    document_uuid VARCHAR(96) NOT NULL,
    source_name VARCHAR(64) NOT NULL,
    provider VARCHAR(32) NOT NULL,
    kind VARCHAR(32) NOT NULL,
    title TEXT,
    created_at VARCHAR(40),
    updated_at VARCHAR(40),
    md_path VARCHAR(1024),
    row_set_hash CHAR(64) NOT NULL,
    renderer_version VARCHAR(32) NOT NULL,
    rendered_at VARCHAR(40),
    PRIMARY KEY (document_uuid)
)"#;

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
pub async fn init_schema(pool: &SqlitePool) -> Result<()> {
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
    sqlx::query(DOCUMENTS_DDL)
        .execute(pool)
        .await
        .context("create documents")?;
    Ok(())
}

/// Renderer-side cache stamp. Bump when the canonical-tuple shape in
/// `compute_row_set_hash` or the rendered `.md` layout changes — every
/// `documents.row_set_hash` is invalidated and the next ingest will
/// re-render. `rust-v1` is the clean break from the Python `"v1"` since
/// the hash encoding differs.
pub const RENDERER_VERSION: &str = "rust-v1";

/// Map a grid_rows `kind` (string used in the UI) to the
/// `documents.kind` enum (chat/thread/page/pr/mr). Anything not in this
/// map is a child row and shouldn't be picked as the canonical document
/// row — but if it ends up being the only candidate we fall back to
/// `"chat"`, matching the Python behavior.
fn doc_kind_for(grid_kind: &str) -> &'static str {
    match grid_kind {
        "Chat" => "chat",
        "Slack Thread" => "thread",
        "GitHub PR" => "pr",
        "GitLab MR" => "mr",
        "Notion Page" | "Notion Database" => "page",
        "Notion Comment Thread" => "thread",
        _ => "chat",
    }
}

/// SHA-256 over the canonical per-row tuple, sorted by `(when_ts, uuid)`
/// so the hash is independent of producer order. Encoding is a
/// `\0`-delimited concatenation of length-prefixed fields — stable across
/// Rust versions (unlike `Debug`), unlike Python's `repr` but that's
/// fine: bumping `RENDERER_VERSION` invalidates the old hashes anyway.
pub fn compute_row_set_hash(rows: &[GridRow]) -> String {
    let mut sorted: Vec<&GridRow> = rows.iter().collect();
    sorted.sort_by(|a, b| a.when_ts.cmp(&b.when_ts).then_with(|| a.uuid.cmp(&b.uuid)));
    let mut h = Sha256::new();
    let push = |h: &mut Sha256, v: Option<&str>| {
        match v {
            Some(s) => {
                h.update(b"S");
                h.update((s.len() as u64).to_le_bytes());
                h.update(s.as_bytes());
            }
            None => h.update(b"N"),
        }
        h.update(b"\x00");
    };
    let push_i = |h: &mut Sha256, v: Option<i64>| {
        match v {
            Some(n) => {
                h.update(b"I");
                h.update(n.to_le_bytes());
            }
            None => h.update(b"N"),
        }
        h.update(b"\x00");
    };
    for r in sorted {
        push(&mut h, Some(&r.uuid));
        push(&mut h, Some(&r.kind));
        push(&mut h, Some(&r.when_ts));
        push(&mut h, r.author.as_deref());
        push_i(&mut h, r.message_index);
        push(&mut h, Some(&r.text));
        push(&mut h, r.source_url.as_deref());
        push(&mut h, r.slack_link.as_deref());
        push(&mut h, r.git_sha.as_deref());
        push(&mut h, r.external_id.as_deref());
        push(&mut h, r.notion_page_uuid.as_deref());
        push(&mut h, r.notion_block_uuid.as_deref());
    }
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Walk `<out>/rendered_md/` for every `*.grid_rows.json` sidecar (any
/// provider) and load each into Dolt. `out_dir` is also the prefix
/// stripped from the sidecar's `.md` path to derive the
/// `documents_loaded.qmd_path` key.
pub async fn load_all(
    pool: &SqlitePool,
    out_dir: &Path,
    progress: impl Fn(&str),
    now_override: Option<&str>,
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

        let inserted = apply_document(
            pool,
            &document_uuid,
            &qmd_rel,
            &fingerprint,
            sidecar.header.render_version,
            &sidecar.rows,
            now_override,
        )
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

/// Probe the connection's libsqlite3 for the `dolt_commit` scalar
/// function. Same probe as [`frankweiler_core::dolt_repo`] uses at
/// connect-time, but at the connection level so the loader doesn't
/// need a `DoltRepo` handle.
async fn has_dolt_extensions(conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>) -> bool {
    let res = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM pragma_function_list WHERE name = 'dolt_commit'",
    )
    .fetch_one(&mut **conn)
    .await;
    matches!(res, Ok(n) if n > 0)
}

async fn existing_fingerprint(pool: &SqlitePool, qmd_path: &str) -> Result<Option<String>> {
    let row = sqlx::query("SELECT source_fingerprint FROM documents_loaded WHERE qmd_path = ?")
        .bind(qmd_path)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|r| r.try_get::<String, _>("source_fingerprint").ok()))
}

async fn apply_document(
    pool: &SqlitePool,
    document_uuid: &str,
    qmd_path: &str,
    fingerprint: &str,
    render_version: u32,
    rows: &[GridRow],
    now_override: Option<&str>,
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

    let loaded_at = now_override
        .map(str::to_string)
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    sqlx::query(
        "INSERT INTO documents_loaded (qmd_path, document_uuid, source_fingerprint, loaded_at) \
         VALUES (?, ?, ?, ?) \
         ON CONFLICT(qmd_path) DO UPDATE SET document_uuid = excluded.document_uuid, \
                                             source_fingerprint = excluded.source_fingerprint, \
                                             loaded_at = excluded.loaded_at",
    )
    .bind(qmd_path)
    .bind(document_uuid)
    .bind(fingerprint)
    .bind(&loaded_at)
    .execute(&mut *conn)
    .await
    .context("upsert documents_loaded")?;

    upsert_document(
        &mut conn,
        document_uuid,
        qmd_path,
        render_version,
        &loaded_at,
        rows,
    )
    .await
    .context("upsert documents")?;

    // Stamp the document as its own dolt_log entry so re-ingests are
    // human-auditable. doltlite exposes `dolt_commit` as a SQLite
    // scalar function — same semantics as the dolt-sql-server's
    // `CALL DOLT_COMMIT(...)`, just SELECT-shaped. With stock libsqlite3
    // the function isn't registered, so we skip the call silently;
    // production runs against doltlite will populate dolt_log normally.
    if has_dolt_extensions(&mut conn).await {
        let msg = format!("grid-rows-load: {document_uuid} {fingerprint}");
        sqlx::query("SELECT dolt_commit('-Am', ?)")
            .bind(&msg)
            .execute(&mut *conn)
            .await
            .context("dolt_commit")?;
    }

    Ok(rows.len())
}

/// Pick the canonical row for a document — the row whose `uuid` matches
/// `document_uuid` (the chat/thread/PR/page row). Fallback to the first
/// row if nothing matches, mirroring the Python `documents.py:175`
/// behavior.
fn pick_canonical<'a>(rows: &'a [GridRow], document_uuid: &str) -> Option<&'a GridRow> {
    rows.iter()
        .find(|r| r.uuid == document_uuid)
        .or_else(|| rows.first())
}

async fn upsert_document(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    document_uuid: &str,
    qmd_path: &str,
    render_version: u32,
    rendered_at: &str,
    rows: &[GridRow],
) -> Result<()> {
    let Some(canonical) = pick_canonical(rows, document_uuid) else {
        return Ok(());
    };
    let kind = doc_kind_for(&canonical.kind);
    let timestamps: Vec<&str> = rows
        .iter()
        .map(|r| r.when_ts.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    let created_at = timestamps.iter().min().copied();
    let updated_at = timestamps.iter().max().copied();
    let row_set_hash = compute_row_set_hash(rows);
    let version_str = format!("{RENDERER_VERSION}.{render_version}");
    // source_name is the human-friendly name from config.sources[].name in
    // the Python loader. We don't have that wiring yet on the Rust side,
    // so we default to the provider — callers can refine when sources
    // config lands in frankweiler-core::config.
    let source_name = canonical.provider.clone();

    sqlx::query("DELETE FROM documents WHERE document_uuid = ?")
        .bind(document_uuid)
        .execute(&mut **conn)
        .await
        .context("delete prior documents row")?;
    sqlx::query(
        "INSERT INTO documents \
         (document_uuid, source_name, provider, kind, title, created_at, updated_at, \
          md_path, row_set_hash, renderer_version, rendered_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(document_uuid)
    .bind(&source_name)
    .bind(&canonical.provider)
    .bind(kind)
    .bind(&canonical.conversation_name)
    .bind(created_at)
    .bind(updated_at)
    .bind(qmd_path)
    .bind(&row_set_hash)
    .bind(&version_str)
    .bind(rendered_at)
    .execute(&mut **conn)
    .await
    .context("insert documents row")?;
    Ok(())
}

async fn insert_grid_row(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
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
