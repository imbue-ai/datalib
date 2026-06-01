//! Generic Load step: walk a `rendered_md/` tree of `.grid_rows.json`
//! sidecars and upsert their rows into Dolt.
//!
//! Two entry points:
//!
//!   * [`apply_one`] writes a single rendered document into `grid_rows`
//!     and stamps the `documents` row. Called per-doc by sync's render
//!     callback so render+index commit atomically.
//!   * [`load_all`] walks a `rendered_md/` tree and calls `apply_one`
//!     for each sidecar. Used as a rebuild-from-disk tool; not on the
//!     hot path now that sync renders+loads per doc.
//!
//! The sidecar format is the cross-provider contract:
//!
//! ```jsonc
//! {
//!   "header": {
//!     "markdown_uuid": "…",            // primary key for the document
//!     "source_fingerprint": "…",       // hash of upstream payload
//!     "render_version": 1              // renderer-side schema stamp
//!   },
//!   "rows": [GridRow, …]
//! }
//! ```
//!
//! Skip logic: before applying we look up `documents.source_fingerprint`
//! by `markdown_uuid`; if it matches the sidecar header we treat the
//! document as up-to-date and leave `grid_rows` alone. Same delete-then-
//! insert pattern as the Python `populate_grid_rows`, generalized so
//! any provider's Translate step can produce a sidecar tree this loader
//! consumes verbatim.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_schema::grid_rows::{GridRow, DDL as GRID_ROWS_DDL};
use serde::Serialize;
use sha2::{Digest, Sha256};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use crate::sidecar::Sidecar;

/// Per-rendered-markdown metadata projection: one row per `.md` file
/// in `<root>/rendered_md/`. `source_fingerprint` is the renderer's
/// input-hash, set when the markdown + blobs land on disk; subsequent
/// runs compare against it to decide whether to re-render.
/// `row_set_hash` is the load-side hash over the canonical grid_rows,
/// used by tools that walk a stale tree.
///
/// `markdown_uuid` is the canonical addressing primitive for rendered
/// output: every grid_row carries a FK back here, and `/api/chat/{uuid}`
/// dereferences it through `md_path`. Note that for sharded renders
/// (beeper renders one file per period) a single upstream
/// "conversation" maps to N rows here — `conversation_uuid` is not
/// unique in the table.
pub const MARKDOWNS_DDL: &str = r#"CREATE TABLE IF NOT EXISTS markdowns (
    markdown_uuid VARCHAR(96) NOT NULL,
    source_name VARCHAR(64) NOT NULL,
    provider VARCHAR(32) NOT NULL,
    kind VARCHAR(32) NOT NULL,
    title TEXT,
    created_at VARCHAR(40),
    updated_at VARCHAR(40),
    md_path VARCHAR(1024),
    source_fingerprint VARCHAR(64),
    upstream_cursor VARCHAR(64),
    row_set_hash CHAR(64),
    renderer_version VARCHAR(32),
    rendered_at VARCHAR(40),
    PRIMARY KEY (markdown_uuid)
)"#;

/// Stats emitted on every load run. Stable shape so a web UI can poll
/// or stream it without per-provider branches.
#[derive(Debug, Default, Serialize)]
pub struct LoadSummary {
    pub markdowns_total: usize,
    pub markdowns_loaded: usize,
    pub markdowns_skipped: usize,
    pub rows_inserted: usize,
}

/// Apply DDL for `grid_rows` and `markdowns`. The schema is the truth
/// for fresh DBs; no migration support is provided because we don't
/// promise back-compat with pre-`markdowns` databases — wipe and
/// re-ingest if you're upgrading from an older release.
pub async fn init_schema(pool: &SqlitePool) -> Result<()> {
    for (_table, ddl) in GRID_ROWS_DDL {
        sqlx::query(ddl)
            .execute(pool)
            .await
            .context("create grid_rows")?;
    }
    sqlx::query(MARKDOWNS_DDL)
        .execute(pool)
        .await
        .context("create markdowns")?;
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

/// One markdown's payload as handed from render to the indexer. The
/// render-side callback constructs this once md + blobs are durably on
/// disk; [`apply_one`] writes the corresponding `grid_rows` + `markdowns`
/// rows so render+index commit per-doc atomically.
#[derive(Debug, Clone)]
pub struct RenderedMarkdown {
    pub markdown_uuid: String,
    /// User-facing config name (e.g. `tiny-slack`); falls back to the
    /// provider string when sync doesn't have one wired in.
    pub source_name: String,
    pub source_fingerprint: String,
    /// Optional provider-defined cheap-probe value the orchestrator can
    /// use *before* loading payloads to decide whether a markdown has
    /// changed since last run. Examples: slack stamps each thread's
    /// `MAX(fetched_at)` here, so the next run can `GROUP BY
    /// thread_root_uuid` on the existing index and skip loading
    /// untouched threads entirely. None when the provider has no
    /// cheaper-than-fingerprint signal.
    pub upstream_cursor: Option<String>,
    /// Absolute path to the rendered `.md`. Used to derive the
    /// `qmd_path` we stamp into `markdowns.md_path` by stripping the
    /// out-dir prefix.
    pub md_path: PathBuf,
    pub render_version: u32,
    pub rows: Vec<GridRow>,
}

/// Write one rendered document into Dolt unconditionally.
///
/// Skip semantics live in the *render* side now (`prior_fingerprints`
/// gate before each per-doc loop) — by the time we're called here the
/// caller has already decided the doc needs to land. `out_dir` is the
/// prefix stripped off `md_path` to produce a portable `qmd_path`.
pub async fn apply_one(
    pool: &SqlitePool,
    out_dir: &Path,
    md: &RenderedMarkdown,
    now_override: Option<&str>,
) -> Result<usize> {
    let qmd_rel = md
        .md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md.md_path)
        .to_string_lossy()
        .to_string();
    apply_markdown(pool, md, &qmd_rel, now_override).await
}

/// Walk `<out>/rendered_md/` for every `*.grid_rows.json` sidecar and
/// rebuild the index by calling [`apply_one`] for each. Off the hot
/// path now — sync's translate step writes through `apply_one` per doc
/// directly — but useful as a disaster-recovery / "reindex from disk"
/// tool.
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
        markdowns_total: sidecars.len(),
        ..Default::default()
    };

    for sidecar_path in &sidecars {
        let raw = fs::read_to_string(sidecar_path)
            .with_context(|| format!("read {}", sidecar_path.display()))?;
        let sidecar: Sidecar = serde_json::from_str(&raw)
            .with_context(|| format!("parse {}", sidecar_path.display()))?;

        let md_path = derive_md_path(sidecar_path)
            .with_context(|| format!("derive .md path from {}", sidecar_path.display()))?;

        let markdown_uuid = sidecar.header.markdown_uuid.clone();
        let fingerprint = sidecar.header.source_fingerprint.clone();

        if existing_fingerprint(pool, &markdown_uuid).await? == Some(fingerprint.clone()) {
            summary.markdowns_skipped += 1;
            continue;
        }

        // load_all has no access to the config-level source name, so we
        // fall back to the canonical row's provider (same default as the
        // pre-callback code path).
        let source_name = sidecar
            .rows
            .first()
            .map(|r| r.provider.clone())
            .unwrap_or_default();
        let md = RenderedMarkdown {
            markdown_uuid,
            source_name,
            source_fingerprint: fingerprint,
            // load_all rebuilds the index from sidecars on disk, which
            // don't carry the cheap-probe cursor (it lives in the
            // indexer only). Leaving it None forces the next live sync
            // to fall back to the fingerprint check for these markdowns
            // — safe, just not as fast as the cursor short-circuit.
            upstream_cursor: None,
            md_path,
            render_version: sidecar.header.render_version,
            rows: sidecar.rows,
        };
        let inserted = apply_one(pool, out_dir, &md, now_override)
            .await
            .with_context(|| format!("load {}", sidecar_path.display()))?;
        summary.rows_inserted += inserted;
        summary.markdowns_loaded += 1;
        progress(&format!(
            "loaded {}/{}",
            summary.markdowns_loaded + summary.markdowns_skipped,
            summary.markdowns_total
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

/// Look up the on-disk render's fingerprint for one markdown. Caller
/// uses this to decide whether to skip work (sync builds a bulk
/// `HashMap<uuid, fingerprint>` once per source via [`load_fingerprints`]
/// rather than calling this in a loop).
pub async fn existing_fingerprint(
    pool: &SqlitePool,
    markdown_uuid: &str,
) -> Result<Option<String>> {
    let row = sqlx::query("SELECT source_fingerprint FROM markdowns WHERE markdown_uuid = ?")
        .bind(markdown_uuid)
        .fetch_optional(pool)
        .await?;
    Ok(row.and_then(|r| r.try_get::<String, _>("source_fingerprint").ok()))
}

/// Bulk fingerprint snapshot. Used once per sync to populate the
/// `prior_fingerprints` map every renderer consults at per-markdown
/// skip time. Rows whose `source_fingerprint` is NULL are omitted so
/// the caller treats them as "not rendered".
pub async fn load_fingerprints(pool: &SqlitePool) -> Result<HashMap<String, String>> {
    let rows = sqlx::query(
        "SELECT markdown_uuid, source_fingerprint \
         FROM markdowns WHERE source_fingerprint IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .context("load_fingerprints")?;
    let mut out: HashMap<String, String> = HashMap::with_capacity(rows.len());
    for r in rows {
        let uuid: String = r.try_get("markdown_uuid")?;
        let fp: String = r.try_get("source_fingerprint")?;
        out.insert(uuid, fp);
    }
    Ok(out)
}

/// Bulk upstream-cursor snapshot, used the same way as
/// [`load_fingerprints`] but for the cheap-probe shortcut a few
/// providers use. Today only slack writes a non-NULL cursor (each
/// thread's `MAX(fetched_at)`); other providers' rows are omitted.
pub async fn load_cursors(pool: &SqlitePool) -> Result<HashMap<String, String>> {
    let rows = sqlx::query(
        "SELECT markdown_uuid, upstream_cursor \
         FROM markdowns WHERE upstream_cursor IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .context("load_cursors")?;
    let mut out: HashMap<String, String> = HashMap::with_capacity(rows.len());
    for r in rows {
        let uuid: String = r.try_get("markdown_uuid")?;
        let cur: String = r.try_get("upstream_cursor")?;
        out.insert(uuid, cur);
    }
    Ok(out)
}

async fn apply_markdown(
    pool: &SqlitePool,
    md: &RenderedMarkdown,
    qmd_path: &str,
    now_override: Option<&str>,
) -> Result<usize> {
    let mut conn = pool.acquire().await.context("acquire conn")?;

    sqlx::query("DELETE FROM grid_rows WHERE markdown_uuid = ?")
        .bind(&md.markdown_uuid)
        .execute(&mut *conn)
        .await
        .context("delete prior rows")?;

    for row in &md.rows {
        insert_grid_row(&mut conn, row).await?;
    }

    let rendered_at = now_override
        .map(str::to_string)
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    upsert_markdown(&mut conn, md, qmd_path, &rendered_at)
        .await
        .context("upsert markdowns")?;

    // Stamp the markdown as its own dolt_log entry so re-ingests are
    // human-auditable. doltlite exposes `dolt_commit` as a SQLite
    // scalar function — same semantics as the dolt-sql-server's
    // `CALL DOLT_COMMIT(...)`, just SELECT-shaped. With stock libsqlite3
    // the function isn't registered, so we skip the call silently;
    // production runs against doltlite will populate dolt_log normally.
    if has_dolt_extensions(&mut conn).await {
        let msg = format!(
            "grid-rows-load: {} {}",
            md.markdown_uuid, md.source_fingerprint
        );
        sqlx::query("SELECT dolt_commit('-Am', ?)")
            .bind(&msg)
            .execute(&mut *conn)
            .await
            .context("dolt_commit")?;
    }

    Ok(md.rows.len())
}

/// Pick the canonical row for a markdown — the row whose `uuid` matches
/// `markdown_uuid` (the chat/thread/PR/page row). Fallback to the first
/// row if nothing matches.
fn pick_canonical<'a>(rows: &'a [GridRow], markdown_uuid: &str) -> Option<&'a GridRow> {
    rows.iter()
        .find(|r| r.uuid == markdown_uuid)
        .or_else(|| rows.first())
}

async fn upsert_markdown(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    md: &RenderedMarkdown,
    qmd_path: &str,
    rendered_at: &str,
) -> Result<()> {
    let Some(canonical) = pick_canonical(&md.rows, &md.markdown_uuid) else {
        return Ok(());
    };
    let kind = doc_kind_for(&canonical.kind);
    let timestamps: Vec<&str> = md
        .rows
        .iter()
        .map(|r| r.when_ts.as_str())
        .filter(|s| !s.is_empty())
        .collect();
    let created_at = timestamps.iter().min().copied();
    let updated_at = timestamps.iter().max().copied();
    let row_set_hash = compute_row_set_hash(&md.rows);
    let version_str = format!("{RENDERER_VERSION}.{}", md.render_version);
    // Prefer the user-facing source_name the renderer was invoked with
    // (config.sources[].name in sync). Fall back to the canonical row's
    // provider when load_all rebuilds from disk without that context.
    let source_name = if md.source_name.is_empty() {
        canonical.provider.clone()
    } else {
        md.source_name.clone()
    };

    sqlx::query("DELETE FROM markdowns WHERE markdown_uuid = ?")
        .bind(&md.markdown_uuid)
        .execute(&mut **conn)
        .await
        .context("delete prior markdowns row")?;
    sqlx::query(
        "INSERT INTO markdowns \
         (markdown_uuid, source_name, provider, kind, title, created_at, updated_at, \
          md_path, source_fingerprint, upstream_cursor, row_set_hash, renderer_version, rendered_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&md.markdown_uuid)
    .bind(&source_name)
    .bind(&canonical.provider)
    .bind(kind)
    .bind(&canonical.conversation_name)
    .bind(created_at)
    .bind(updated_at)
    .bind(qmd_path)
    .bind(&md.source_fingerprint)
    .bind(md.upstream_cursor.as_deref())
    .bind(&row_set_hash)
    .bind(&version_str)
    .bind(rendered_at)
    .execute(&mut **conn)
    .await
    .context("insert markdowns row")?;
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
          markdown_uuid) \
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
    .bind(&row.markdown_uuid)
    .execute(&mut **conn)
    .await
    .with_context(|| format!("insert grid_row {}", row.uuid))?;
    Ok(())
}
