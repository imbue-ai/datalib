// HTTP daemon — runs as its own process via `frankweiler-http`, not
// inside `frankweiler-sync`. No MultiProgress / no indicatif bars in
// this process; request-error logging legitimately writes to stderr.
// Exempt from the workspace-wide ban defined in clippy.toml. (If this
// ever gets embedded into a process that *does* have bars, switch
// these to `tracing::warn!` / `error!`.)
#![allow(clippy::disallowed_macros)]

//! axum router for the Frankweiler HTTP API.
//!
//! Endpoints:
//!   GET /api/health
//!   GET /api/search?q=…&limit=…  → grid_rows query against the managed Dolt repo
//!   GET /api/columns             → grid column metadata
//!   GET /api/chat/{uuid}         → conversation header (from grid_rows) + raw QMD body
//!
//! Dolt is the source of truth. **QMDs are write-only output** — the
//! `/api/chat` endpoint serves the file body verbatim (sans frontmatter)
//! and lets the UI render markdown once. We never parse a QMD back into
//! structured data; structured fields come from `grid_rows`.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::Json,
    routing::{get, post},
    Router,
};
use frankweiler_core::qmd::{GridIndex, QmdDaemon, QmdRunner, QmdRunnerConfig, QueryMode};
use frankweiler_core::query::{parse_query, FreeTextMode, ParsedQuery};
use frankweiler_core::repo::{DynRepo, RepoError};
use frankweiler_core::search::SearchRow;
use frankweiler_core::version::git_hash;
use frankweiler_schema::feedback::FeedbackRow;
use frankweiler_schema::sync_jobs::SyncJobRow;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

mod embed;

#[derive(Clone)]
pub struct AppState {
    /// Data root on disk — drives the static `/api/media/*` mount and
    /// the `accounts.json` lookup. The SQL store is reached through
    /// [`AppState::repo`].
    pub root: Arc<PathBuf>,
    /// All SQL flows through this seam.
    /// [`frankweiler_core::dolt_repo::DoltRepo`] against a single
    /// doltlite file is the only impl today.
    pub repo: DynRepo,
    /// Long-lived `qmd mcp` child for sub-second searches. `None` when
    /// no qmd index is materialized at startup (or its spawn check
    /// failed) — `run_qmd_search` then falls back to the per-call
    /// `npx … query` shell-out path so search still works.
    pub qmd_daemon: Option<Arc<QmdDaemon>>,
}

#[derive(Debug, Serialize)]
pub struct Health {
    pub ok: bool,
    pub version: &'static str,
    pub root: String,
    pub root_exists: bool,
}

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query_echo: serde_json::Value,
    pub rows: Vec<SearchRow>,
    pub columns: Vec<ColumnSpec>,
    pub total_estimated: u64,
}

#[derive(Debug, Serialize, Clone)]
pub struct ColumnSpec {
    pub field: String,
    pub header: String,
    pub default_visible: bool,
}

/// Response shape for `/api/chat/{markdown_uuid}`. The body is the raw
/// QMD content minus the YAML frontmatter — the UI runs markdown-it on
/// it directly. We do **not** ship a structured `messages[]` array;
/// per-message scrolling uses the
/// `<div id="m-{uuid}" data-section-uuid="…">` wrappers the renderer
/// emits in the body.
#[derive(Debug, Serialize)]
pub struct ChatResponse {
    pub markdown_uuid: String,
    pub name: Option<String>,
    pub account: Option<String>,
    pub project: Option<String>,
    pub channel: Option<String>,
    pub created_at: Option<String>,
    pub source_label: Option<String>,
    pub source_url: Option<String>,
    pub body: String,
}

/// Client-supplied portion of a feedback submission. The server stamps
/// the rest (UUID, timestamp, app_version, git_hash) at insert time, so
/// the client only has to describe what was being clicked on and what the
/// user typed. `context` is whatever shape `feedback/context.ts` produced;
/// we round-trip it as JSON straight into the `context_json` column.
#[derive(Debug, Deserialize)]
pub struct FeedbackRequest {
    /// Optional thumb up/down — `null` when the user submitted just a
    /// comment without choosing a direction.
    pub sentiment: Option<String>,
    /// Required free-form comment. The UI disables Submit until non-empty;
    /// the server enforces the same constraint defensively.
    pub comment: String,
    /// Decoded `FeedbackContext`. Re-serialized into the row's
    /// `context_json` column verbatim.
    pub context: serde_json::Value,
}

/// What the client gets back after a successful POST. Mostly a confirmation
/// that the row landed; the UUID is useful for showing a "filed as X" toast
/// and for cross-referencing in `dolt log`.
#[derive(Debug, Serialize)]
pub struct FeedbackResponse {
    pub feedback_uuid: String,
    pub created_at: String,
    pub git_hash: &'static str,
}

pub fn router(state: AppState) -> Router {
    // Slack image attachments are symlinked into `<root>/media/slack/<file_id>/`
    // by ingest; serve them verbatim so QMD-embedded `![](...)` URLs resolve.
    let media_dir = state.root.join("media");
    Router::new()
        .route("/api/health", get(health))
        .route("/api/search", get(search_handler))
        .route("/api/columns", get(columns))
        .route("/api/accounts", get(accounts))
        .route("/api/chat/{markdown_uuid}", get(chat))
        .route("/api/feedback", post(submit_feedback))
        .route("/api/sync/sources", get(sync_sources))
        .route("/api/sync/jobs", get(sync_jobs_active).post(sync_enqueue))
        .route("/api/sync/jobs/all", get(sync_jobs_all))
        .route("/api/sync/jobs/{id}", get(sync_job_get))
        .route("/api/sync/jobs/{id}/cancel", post(sync_job_cancel))
        .route("/api/sync/jobs/{id}/log", get(sync_job_log))
        .nest_service("/api/media", ServeDir::new(media_dir))
        // SPA fallback — anything not matched above is served from the
        // embedded Vite bundle. Client-side routing turns unknown paths
        // into `index.html`.
        .fallback(embed::serve_ui)
        .with_state(state)
        .layer(CorsLayer::permissive())
}

async fn accounts(State(s): State<AppState>) -> Json<serde_json::Value> {
    // Ingest writes `<root>/accounts.json` mapping account UUIDs → display
    // names. We surface it verbatim so the UI can do UUID → label lookups
    // late, in render code, with the UUID still in hand.
    let path = s.root.join("accounts.json");
    let v: serde_json::Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    Json(v)
}

async fn health(State(s): State<AppState>) -> Json<Health> {
    Json(Health {
        ok: true,
        version: env!("CARGO_PKG_VERSION"),
        root: s.root.display().to_string(),
        root_exists: s.root.exists(),
    })
}

async fn search_handler(
    State(s): State<AppState>,
    Query(p): Query<SearchParams>,
) -> Json<SearchResponse> {
    let parsed = parse_query(p.q.as_deref().unwrap_or(""));
    let limit = p.limit.unwrap_or(200).min(100_000);
    // Three routing cases:
    //   1. Empty free-text — pure structured query, route through repo.search.
    //   2. Non-empty free-text + qmd index present — shell out to qmd, map
    //      hits to row uuids via the repo's grid_row_refs, then fetch full
    //      rows via repo.search_by_uuids preserving rank order.
    //   3. Non-empty free-text but no qmd index — degrade gracefully: surface
    //      the error in `query_echo.qmd_error` and fall back to repo.search
    //      (SQL substring LIKE) so the UI isn't dead.
    let mut qmd_error: Option<String> = None;
    let rows = if parsed.free_text.is_empty() {
        s.repo.search(&parsed, limit).await.unwrap_or_default()
    } else {
        match run_qmd_search(&s.root, &s.repo, s.qmd_daemon.as_ref(), &parsed, limit).await {
            Ok(rows) => rows,
            Err(e) => {
                qmd_error = Some(format!("{e:#}"));
                s.repo.search(&parsed, limit).await.unwrap_or_default()
            }
        }
    };

    let total = rows.len() as u64;
    Json(SearchResponse {
        query_echo: serde_json::json!({
            "free_text": parsed.free_text,
            "free_text_mode": match parsed.free_text_mode {
                FreeTextMode::Hybrid => "hybrid",
                FreeTextMode::Vsearch => "vsearch",
            },
            "resolved_type": format!("{:?}", parsed.resolved_type),
            "filters": parsed.filters.iter()
                .map(|(k, v)| (format!("{:?}", k), v.clone()))
                .collect::<Vec<_>>(),
            "qmd_error": qmd_error,
        }),
        rows,
        columns: default_columns(),
        total_estimated: total,
    })
}

/// Run a qmd-routed search. qmd itself is shelled out via `npx` on a
/// blocking thread; the row-resolution layer is async and goes through
/// the repo trait so both Dolt and SQLite backends work.
async fn run_qmd_search(
    root: &std::sync::Arc<PathBuf>,
    repo: &DynRepo,
    daemon: Option<&Arc<QmdDaemon>>,
    parsed: &ParsedQuery,
    limit: usize,
) -> anyhow::Result<Vec<SearchRow>> {
    let root_owned = root.as_ref().clone();
    let parsed_for_qmd = parsed.clone();
    let daemon = daemon.cloned();
    // Ask qmd for a generous hit count: a single qmd hit (e.g. a
    // conversation-level snippet) can resolve to many grid rows. We then
    // truncate to `limit` after row expansion.
    let qmd_limit = std::cmp::min(limit.saturating_mul(2).max(50), 1_000);
    let hits = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let mode = match parsed_for_qmd.free_text_mode {
            FreeTextMode::Hybrid => QueryMode::Hybrid,
            FreeTextMode::Vsearch => QueryMode::Vsearch,
        };
        // Prefer the long-lived MCP daemon (sub-second). On any I/O
        // error we drop down to a fresh `npx … query` shell-out so a
        // misbehaving daemon doesn't kill search entirely.
        if let Some(d) = daemon.as_ref() {
            match d.search(mode, &parsed_for_qmd.free_text, qmd_limit) {
                Ok(hits) => return Ok(hits),
                Err(e) => {
                    eprintln!("qmd daemon search failed, falling back to CLI: {e:#}");
                }
            }
        }
        let cfg = QmdRunnerConfig::new(root_owned);
        let runner = QmdRunner::new(cfg)?;
        runner.search(mode, &parsed_for_qmd.free_text, qmd_limit)
    })
    .await
    .map_err(|e| anyhow::anyhow!("qmd task join error: {e}"))??;

    let refs = repo
        .grid_row_refs()
        .await
        .map_err(|e| anyhow::anyhow!("grid_row_refs: {e}"))?;
    let idx = GridIndex::new(refs);
    // Walk hits in rank order, mapping each to its grid rows and stamping
    // the hit's score onto every row it produces. A single qmd hit can fan
    // out to many rows; if a row is produced by more than one hit we keep
    // the first (highest-ranked) score we saw for it.
    let mut uuids: Vec<String> = Vec::new();
    let mut scores: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for h in &hits {
        for row in idx.rows_for_hit(h) {
            if !scores.contains_key(&row.uuid) {
                scores.insert(row.uuid.clone(), h.score);
                uuids.push(row.uuid);
            }
        }
    }
    let mut rows = repo
        .search_by_uuids(parsed, &uuids, limit)
        .await
        .map_err(|e| anyhow::anyhow!("search_by_uuids: {e}"))?;
    for r in rows.iter_mut() {
        r.score = scores.get(&r.uuid).copied();
    }
    Ok(rows)
}

async fn columns() -> Json<Vec<ColumnSpec>> {
    Json(default_columns())
}

async fn chat(
    State(s): State<AppState>,
    Path(markdown_uuid): Path<String>,
) -> Result<Json<ChatResponse>, StatusCode> {
    // QMDs are write-only output. We read the file just to ship its body
    // to the UI as-is; structured metadata comes from grid_rows. Per-section
    // anchors in the body (`<div id="m-{uuid}" data-section-uuid="…">`)
    // let the UI scroll-and-highlight without a structured chat schema.
    // One UUID → one file: no enumeration, no fallbacks.
    let path = s
        .repo
        .qmd_path_for_markdown(&markdown_uuid)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let raw = std::fs::read_to_string(&path).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let body = strip_frontmatter(&raw).to_string();
    let meta = s
        .repo
        .chat_meta(&markdown_uuid)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    // Synthesize page-level URLs for providers that don't carry one in
    // `source_url`. Claude/ChatGPT use the conversation UUID directly
    // in their public URL scheme — and for those providers
    // markdown_uuid == conversation_uuid (one rendered file per chat),
    // so we can drop it straight in.
    let source_url = meta
        .source_url
        .or_else(|| match meta.source_label.as_deref() {
            Some("Claude") => Some(format!("https://claude.ai/chat/{markdown_uuid}")),
            Some("ChatGPT") => Some(format!("https://chatgpt.com/c/{markdown_uuid}")),
            _ => None,
        });
    Ok(Json(ChatResponse {
        markdown_uuid,
        name: meta.name,
        account: meta.account,
        project: meta.project,
        channel: meta.channel,
        created_at: meta.when_ts,
        source_label: meta.source_label,
        source_url,
        body,
    }))
}

async fn submit_feedback(
    State(s): State<AppState>,
    Json(req): Json<FeedbackRequest>,
) -> Result<Json<FeedbackResponse>, StatusCode> {
    // The UI also disables Submit until non-empty, but enforce it here so
    // a hand-crafted POST can't slip an all-whitespace row past the audit
    // trail.
    if req.comment.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let context_json = serde_json::to_string(&req.context).map_err(|_| StatusCode::BAD_REQUEST)?;
    // Server-stamped fields. We mint these here rather than trusting the
    // client so each row carries a server-vouched provenance and so
    // `feedback_uuid` collisions are impossible from the wire.
    let feedback_uuid = uuid::Uuid::new_v4().to_string();
    let created_at = chrono::Local::now().to_rfc3339();
    let app_version = env!("CARGO_PKG_VERSION").to_string();
    let git_hash_str = git_hash().to_string();
    let row = FeedbackRow {
        feedback_uuid: feedback_uuid.clone(),
        created_at: created_at.clone(),
        sentiment: req.sentiment,
        comment: req.comment,
        app_version,
        git_hash: git_hash_str,
        context_json,
        // Resolution metadata is filled in by hand later, never at submit time.
        fixed_in_git_hash: None,
        notes: None,
    };
    match s.repo.insert_feedback(row).await {
        Ok(()) => Ok(Json(FeedbackResponse {
            feedback_uuid,
            created_at,
            git_hash: git_hash(),
        })),
        Err(RepoError::ReadOnly) => Err(StatusCode::SERVICE_UNAVAILABLE),
        Err(e) => {
            eprintln!("feedback insert failed: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// Strip a leading `---\n…\n---\n` YAML frontmatter block. This is text
/// trimming, not parsing — we don't look at the YAML contents and we don't
/// care if it's malformed; the body is whatever's after the closing `---`.
fn strip_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    let Some(end) = rest.find("\n---") else {
        return text;
    };
    let after = &rest[end + 4..];
    after.strip_prefix('\n').unwrap_or(after)
}

fn default_columns() -> Vec<ColumnSpec> {
    vec![
        col("score", "Score", true),
        col("source", "Source", true),
        col("kind", "Type", true),
        col("when", "Time", true),
        col("snippet", "Contents", true),
        col("author", "Author", true),
        col("account", "Account", true),
        col("conversation_name", "Conversation Name", false),
        col("project", "Project", false),
        col("entire_chat", "Entire Chat", false),
    ]
}

fn col(field: &str, header: &str, default_visible: bool) -> ColumnSpec {
    ColumnSpec {
        field: field.into(),
        header: header.into(),
        default_visible,
    }
}

/// One entry in `GET /api/sync/sources`. Derived from the config file
/// at the data root — the backend never persists this list to SQL.
#[derive(Debug, Serialize)]
pub struct SourceInfo {
    pub name: String,
    /// Discriminator from the config (e.g. `claude_api`, `notion_api`,
    /// `claude_export`). Carries both the provider and the
    /// provenance — the UI splits it on `_` when it needs either piece.
    #[serde(rename = "type")]
    pub type_: String,
    /// True when the source has a `sync:` block — i.e. the worker can
    /// drive a downloader for it. Derived; not stored in YAML.
    pub managed: bool,
}

#[derive(Debug, Deserialize)]
pub struct EnqueueJobRequest {
    pub kind: String,
    #[serde(default)]
    pub source_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct JobsAllParams {
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Surface the typed `Config.sources` list to the UI as
/// `{name, type, managed}` entries. We re-load the config on every call
/// so a user editing the YAML doesn't have to restart the backend.
/// Returns an empty list (rather than 500) when the file is missing or
/// fails to parse, mirroring the previous behavior.
async fn sync_sources(State(s): State<AppState>) -> Json<Vec<SourceInfo>> {
    let _ = s; // unused; included for symmetry with other handlers.
    let path = frankweiler_core::config::default_config_path();
    let cfg = match frankweiler_core::config::load_config(Some(&path)) {
        Ok(c) => c,
        Err(_) => return Json(Vec::new()),
    };
    let out: Vec<SourceInfo> = cfg
        .sources
        .iter()
        .map(|src| SourceInfo {
            name: src.name().to_string(),
            type_: src.type_str().to_string(),
            managed: src.is_managed(),
        })
        .collect();
    Json(out)
}

async fn sync_jobs_active(State(s): State<AppState>) -> Result<Json<Vec<SyncJobRow>>, StatusCode> {
    s.repo
        .list_jobs(true, 200)
        .await
        .map(Json)
        .map_err(repo_err_to_status)
}

async fn sync_jobs_all(
    State(s): State<AppState>,
    Query(p): Query<JobsAllParams>,
) -> Result<Json<Vec<SyncJobRow>>, StatusCode> {
    let limit = p.limit.unwrap_or(200).min(10_000);
    s.repo
        .list_jobs(false, limit)
        .await
        .map(Json)
        .map_err(repo_err_to_status)
}

async fn sync_job_get(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<SyncJobRow>, StatusCode> {
    match s.repo.get_job(&id).await {
        Ok(Some(row)) => Ok(Json(row)),
        Ok(None) => Err(StatusCode::NOT_FOUND),
        Err(e) => Err(repo_err_to_status(e)),
    }
}

async fn sync_enqueue(
    State(s): State<AppState>,
    Json(req): Json<EnqueueJobRequest>,
) -> Result<Json<SyncJobRow>, StatusCode> {
    // Validate the discriminator client-side; the DB column is a
    // VARCHAR with no enum constraint so we'd otherwise accept anything.
    match req.kind.as_str() {
        "download" | "ingest" | "render" | "all" => {}
        _ => return Err(StatusCode::BAD_REQUEST),
    }
    s.repo
        .enqueue_job(&req.kind, req.source_name.as_deref())
        .await
        .map(Json)
        .map_err(repo_err_to_status)
}

async fn sync_job_cancel(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    s.repo
        .request_cancel_job(&id)
        .await
        .map(|_| StatusCode::NO_CONTENT)
        .map_err(repo_err_to_status)
}

/// Tail the per-job log written by the worker at `<root>/state/job-logs/{id}.log`.
/// 404 when the file doesn't exist yet — the UI polls `/jobs/{id}` for state
/// and only follows the log link once it appears.
async fn sync_job_log(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<
    (
        StatusCode,
        [(axum::http::HeaderName, &'static str); 1],
        String,
    ),
    StatusCode,
> {
    // Defensive: reject anything that could traverse outside the logs dir.
    if id.contains('/') || id.contains('\\') || id.contains("..") {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = s.root.join("state/job-logs").join(format!("{id}.log"));
    match std::fs::read_to_string(&path) {
        Ok(body) => Ok((
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            body,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

fn repo_err_to_status(e: RepoError) -> StatusCode {
    match e {
        RepoError::ReadOnly => StatusCode::SERVICE_UNAVAILABLE,
        _ => {
            eprintln!("repo error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_columns_listed() {
        assert_eq!(default_columns().len(), 10);
    }
}
