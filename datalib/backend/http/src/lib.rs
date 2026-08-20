// HTTP daemon — runs as its own process via `datalib-http`, not
// inside the pipeline binaries. No MultiProgress / no indicatif bars in
// this process; request-error logging legitimately writes to stderr.
// Exempt from the workspace-wide ban defined in clippy.toml. (If this
// ever gets embedded into a process that *does* have bars, switch
// these to `tracing::warn!` / `error!`.)
#![allow(clippy::disallowed_macros)]

//! axum router for the Datalib HTTP API.
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

use app_schema::feedback::FeedbackRow;
use app_schema::sync_jobs::SyncJobRow;
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, Response, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    response::Json,
    routing::{any, get, post},
    Router,
};
use datalib_core::qmd::{GridIndex, QmdDaemon, QmdRunner, QmdRunnerConfig, QueryMode};
use datalib_core::query::{parse_query, FreeTextMode, ParsedQuery};
use datalib_core::repo::{DocRow, DynRepo, EdgeRowOut, RepoError};
use datalib_core::search::SearchRow;
use datalib_core::version::git_hash;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::services::ServeDir;

pub mod applets;
pub mod auth;
pub mod boot;
mod embed;
pub mod frontend;
pub mod worker;

pub use auth::ApiToken;
pub use boot::build_state;

#[derive(Clone)]
pub struct AppState {
    /// Data root on disk — drives the static `/api/media/*` mount and
    /// the `accounts.json` lookup. The SQL store is reached through
    /// [`AppState::repo`].
    pub root: Arc<PathBuf>,
    /// All SQL flows through this seam.
    /// [`datalib_core::dolt_repo::DoltRepo`] against a single
    /// doltlite file is the only impl today.
    pub repo: DynRepo,
    /// Long-lived `qmd mcp` child for sub-second searches. Always present:
    /// it resolves its index lazily per query, so a missing index (no
    /// sync yet) or a mid-run rebuild is handled inside `search`. On any
    /// daemon error `run_qmd_search` still falls back to the per-call
    /// `npx … query` shell-out path so search keeps working.
    pub qmd_daemon: Arc<QmdDaemon>,
    /// Fan-out channel for live sync-job progress. The worker (and the
    /// enqueue/cancel handlers) publish [`worker::ProgressEvent`]s here;
    /// `GET /api/sync/stream` subscribes and pushes them to the UI over
    /// SSE, so progress is realtime push, not poll.
    pub progress_tx: worker::ProgressTx,
    /// The configured applets: their components, their gallery
    /// entries, the module store behind `/modules/`, and the
    /// supervisor behind `/applet/`. Empty when the config declares none,
    /// which is the state every data root starts in.
    pub applets: Arc<applets::AppletRegistry>,
    /// The per-process API token every request must carry. Minted at
    /// startup and published to `<root>/system/state/api-token`; see
    /// [`crate::auth`] for the scheme and why it exists.
    pub api_token: ApiToken,
}

impl AppState {
    /// Self-contained config path for this data root:
    /// `<root>/config.toml`. The config + setup endpoints read and
    /// write it, and the sync worker drives `datalib-dag <this>`.
    /// Keeping the config inside the root is what lets the app
    /// bootstrap from an empty directory with no external `~/.config`
    /// file.
    pub fn config_path(&self) -> PathBuf {
        datalib_dag::config::root_config_path(&self.root)
    }
}

#[derive(Debug, Serialize)]
pub struct Health {
    pub ok: bool,
    pub version: &'static str,
    pub root: String,
    pub root_exists: bool,
    /// Where this server published its API token. Surfaced so the UI
    /// can tell a coding agent where to read it (see `handoff.ts`);
    /// reaching this response already required holding the token.
    pub token_file: String,
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
    /// Backend-side errors the user should know about even though we
    /// returned 200 + rows. Populated when a degraded path ran (qmd
    /// fallback) or when a swallowed error would otherwise leave the
    /// UI staring at an empty grid with no signal. The UI surfaces
    /// these as toasts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
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
    /// Outgoing edges from this markdown. The UI uses this to render
    /// the "outgoing destinations" list at the top of the doc preview
    /// AND to resolve `<span data-edge-id>` clicks inside the body to
    /// their destinations. Empty for documents with no edges (or for
    /// data roots without an `edges` table).
    pub outgoing_edges: Vec<EdgeRowOut>,
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

/// `~/.datalib/bin` — the blessed drop spot for user- (and agent-)
/// provided programs a config names by bare command.
///
/// Prepended to the child PATH by both things that run config
/// commands: the sync worker, for step subprocesses
/// ([`worker::run_job`]), and the applet gateway, for applet servers
/// ([`applets`]). Those are the only two, and they must agree —
/// `/agent/config.md` advertises this as *the* predictable install
/// location without qualifying which kind of entry it works for, and a
/// binary that resolves for a step but not for an applet is exactly the
/// surprise that promise rules out.
///
/// `None` when no home directory is discoverable.
pub fn user_bin_dir() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE")?;
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".datalib").join("bin"))
}

pub fn router(state: AppState) -> Router {
    // Slack image attachments are symlinked into
    // `<root>/system/media/slack/<file_id>/` by ingest; serve them verbatim so
    // QMD-embedded `![](...)` URLs resolve.
    let media_dir = datalib_core::layout::media_dir(&state.root);
    // Served attachments are re-materializable from the raw blob CAS,
    // so mark the tree as derived cache for `--exclude-caches` backups.
    // Nothing writes media yet (see layout.rs), so this usually no-ops;
    // it's here (rather than in a pipeline step) because no step owns
    // the dir and the server is its one consumer.
    datalib_core::layout::mark_derived_cache(&media_dir);
    // Cloned out before `state` is moved into `with_state` below.
    let api_token = state.api_token.clone();
    Router::new()
        .route("/api/health", get(health))
        .route("/api/search", get(search_handler))
        .route("/api/columns", get(columns))
        .route("/api/accounts", get(accounts))
        .route("/api/chat/{markdown_uuid}", get(chat))
        .route("/api/docs", get(list_docs))
        .route("/api/asset/{markdown_uuid}/{*rel}", get(asset))
        .route("/api/feedback", post(submit_feedback))
        .route("/api/card", post(create_card))
        .route("/api/card/{hash}", get(get_card))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/config/scaffold", get(config_scaffold))
        .route("/api/dag", get(get_dag))
        .route("/api/lib/{name}", get(get_lib).put(put_lib))
        .route("/api/lib/{name}/rename", post(rename_lib))
        .route("/agent/cards.md", get(agent_cards_guide))
        .route("/agent/config.md", get(agent_config_guide))
        // The pre-split guide URL; wayfinders copied before the split
        // may still reference it.
        .route(
            "/agent.md",
            get(|| async { axum::response::Redirect::permanent("/agent/cards.md") }),
        )
        .route("/api/sync/sources", get(sync_sources))
        .route("/api/sync/jobs", get(sync_jobs_active).post(sync_enqueue))
        .route("/api/sync/jobs/all", get(sync_jobs_all))
        .route("/api/sync/jobs/{id}", get(sync_job_get))
        .route("/api/sync/jobs/{id}/cancel", post(sync_job_cancel))
        .route("/api/sync/jobs/{id}/log", get(sync_job_log))
        .route("/api/sync/stream", get(sync_stream))
        .route("/api/frontend", get(get_frontend))
        // Component code, addressed by content. Flat across every
        // namespace, so byte-identical components resolve to one URL
        // and the browser evaluates them once. See frontend.rs.
        .route("/modules/{hash}", get(get_module))
        // The applet proxy. `{*rest}` keeps the whole remaining path,
        // which the applet sees verbatim.
        .route("/applet/{id}/{*rest}", any(proxy_applet))
        .route("/applet/{id}/", any(proxy_applet_root))
        .nest_service("/api/media", ServeDir::new(media_dir))
        // SPA fallback — anything not matched above is served from the
        // embedded Vite bundle. Client-side routing turns unknown paths
        // into `index.html`.
        .fallback(embed::serve_ui)
        .with_state(state)
        // Outermost, so it covers every route above plus the SPA
        // fallback and the `/api/media` static mount. `CorsLayer::
        // permissive()` used to sit here; it is gone deliberately.
        // With the token gate in front, a cross-origin page cannot get
        // a usable response anyway, and `Access-Control-Allow-Origin:
        // *` on a 401 is just a misleading advertisement. Nothing
        // in-tree needs cross-origin access: the browser UI is
        // same-origin in every packaging, and `pnpm dev` reaches the
        // API through Vite's *server-side* proxy, which no CORS policy
        // applies to.
        .layer(axum::middleware::from_fn_with_state(
            api_token,
            auth::require_token,
        ))
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
        token_file: s.api_token.token_file().display().to_string(),
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
    let mut errors: Vec<String> = Vec::new();
    // Run repo.search but collect any error instead of swallowing it.
    // The previous `unwrap_or_default()` hid schema mismatches and
    // connection failures behind an empty grid with no signal.
    let rows = if parsed.free_text.is_empty() {
        match s.repo.search(&parsed, limit).await {
            Ok(rows) => rows,
            Err(e) => {
                let msg = format!("structured search: {e}");
                eprintln!("search: {msg}");
                errors.push(msg);
                Vec::new()
            }
        }
    } else {
        match run_qmd_search(&s.root, &s.repo, &s.qmd_daemon, &parsed, limit).await {
            Ok(rows) => rows,
            Err(e) => {
                qmd_error = Some(format!("{e:#}"));
                match s.repo.search(&parsed, limit).await {
                    Ok(rows) => rows,
                    Err(e2) => {
                        let msg = format!("LIKE fallback: {e2}");
                        eprintln!("search: {msg}");
                        errors.push(msg);
                        Vec::new()
                    }
                }
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
        errors,
    })
}

/// Run a qmd-routed search. qmd itself is shelled out via `npx` on a
/// blocking thread; the row-resolution layer is async and goes through
/// the repo trait so both Dolt and SQLite backends work.
async fn run_qmd_search(
    root: &std::sync::Arc<PathBuf>,
    repo: &DynRepo,
    daemon: &Arc<QmdDaemon>,
    parsed: &ParsedQuery,
    limit: usize,
) -> anyhow::Result<Vec<SearchRow>> {
    let root_owned = root.as_ref().clone();
    let parsed_for_qmd = parsed.clone();
    let daemon = daemon.clone();
    // Ask qmd for a generous hit count: a single qmd hit (e.g. a
    // conversation-level snippet) can resolve to many grid rows. We then
    // truncate to `limit` after row expansion.
    let qmd_limit = std::cmp::min(limit.saturating_mul(2).max(50), 1_000);
    let hits = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let mode = match parsed_for_qmd.free_text_mode {
            FreeTextMode::Hybrid => QueryMode::Hybrid,
            FreeTextMode::Vsearch => QueryMode::Vsearch,
        };
        // Prefer the long-lived MCP daemon (sub-second). On any error —
        // including a not-yet-built index — we drop down to a fresh
        // `npx … query` shell-out so a missing or misbehaving daemon
        // doesn't kill search entirely.
        match daemon.search(mode, &parsed_for_qmd.free_text, qmd_limit) {
            Ok(hits) => return Ok(hits),
            Err(e) => {
                eprintln!("qmd daemon search failed, falling back to CLI: {e:#}");
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
    let idx = GridIndex::new((**root).clone(), refs);
    // Map hits to grid rows in rank order, keeping only the top hit per
    // markdown document so the result list stays concise — a single chat that
    // matches in several places shows up once, at its best rank. Orphan hits
    // (a path the grid doesn't know about, e.g. a stale render under an old
    // layout) resolve to no rows; flag them loudly so their dropped score is
    // visible. (ERROR level; this file logs via eprintln!.)
    let ranked = idx.ranked_rows_one_per_doc(&hits, |h| {
        eprintln!(
            "ERROR search: qmd hit resolved to no grid rows: path={:?} score={}",
            h.path, h.score
        );
    });
    let uuids: Vec<String> = ranked.iter().map(|(row, _)| row.uuid.clone()).collect();
    let scores: std::collections::HashMap<String, f64> = ranked
        .iter()
        .map(|(row, score)| (row.uuid.clone(), *score))
        .collect();
    drop(idx);
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

/// List rendered documents for the document-picker card, newest first.
/// The row shape is [`DocRow`] straight from the repo; 500 is plenty
/// for a pick-from-a-list UI without paging machinery.
async fn list_docs(State(s): State<AppState>) -> Result<Json<Vec<DocRow>>, StatusCode> {
    match s.repo.list_docs(500).await {
        Ok(rows) => Ok(Json(rows)),
        Err(e) => {
            eprintln!("list_docs: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
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
    let outgoing_edges = s
        .repo
        .outgoing_edges(&markdown_uuid)
        .await
        .unwrap_or_default();
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
        outgoing_edges,
    }))
}

/// Serve a file living next to (or under) a rendered markdown. Relative
/// `![](blobs/foo.png)` references in the markdown body become
/// `/api/asset/{markdown_uuid}/blobs/foo.png` once the UI rewrites them;
/// this handler resolves them by looking up the markdown's on-disk path
/// and joining `rel` against its parent directory.
///
/// Path-traversal guard: canonicalize both the parent dir and the target,
/// reject the request if the target escapes the parent.
async fn asset(
    State(s): State<AppState>,
    Path((markdown_uuid, rel)): Path<(String, String)>,
) -> Result<Response<Body>, StatusCode> {
    let md_path = s
        .repo
        .qmd_path_for_markdown(&markdown_uuid)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let parent = md_path.parent().ok_or(StatusCode::NOT_FOUND)?.to_path_buf();
    let target = parent.join(&rel);
    let parent_canon = parent
        .canonicalize()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let target_canon = target.canonicalize().map_err(|_| StatusCode::NOT_FOUND)?;
    if !target_canon.starts_with(&parent_canon) {
        return Err(StatusCode::FORBIDDEN);
    }
    let bytes = std::fs::read(&target_canon).map_err(|_| StatusCode::NOT_FOUND)?;
    let mime = mime_guess::from_path(&target_canon)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    Response::builder()
        .header(header::CONTENT_TYPE, mime)
        .body(Body::from(bytes))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
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
    let created_at = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
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

/// Body of `POST /api/card`. The user-authored JS source goes in
/// verbatim; the server hashes it to derive the storage key. Bigger
/// scripts (single-file Observable-style cells) are fine — the body
/// is bounded by axum's default body limit.
#[derive(Debug, Deserialize)]
pub struct CreateCardRequest {
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct CreateCardResponse {
    pub hash: String,
}

// ---------------------------------------------------------------------------
// Applets
// ---------------------------------------------------------------------------

/// The frontend as the UI consumes it: every namespace the store found
/// — `user` plus one per applet — and the applets whose write failed.
/// A broken applet is named rather than merely absent, since an empty
/// gallery looks the same as a config that never saved.
async fn get_frontend(State(s): State<AppState>) -> Json<applets::FrontendView> {
    // Pick up a config edit before answering. Cheap when nothing moved
    // (one `stat`); blocking when it did, since a rebuild execs one
    // child per applet — hence the blocking thread. The UI polls this
    // endpoint, so a saved config becomes a live gallery update.
    let registry = s.applets.clone();
    let _ = tokio::task::spawn_blocking(move || registry.refresh_if_config_changed()).await;
    Json(s.applets.frontend_view())
}

/// Serve one component by content hash.
///
/// Immutable forever: the URL names the bytes, so changed code is a
/// different URL. That is also what makes the browser's
/// one-module-per-URL rule do the deduplication for us — across
/// namespaces, not just across applet instances.
async fn get_module(
    State(s): State<AppState>,
    Path(hash): Path<String>,
) -> Result<Response<Body>, StatusCode> {
    let bytes = s
        .applets
        .read_component(&hash)
        .ok_or(StatusCode::NOT_FOUND)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/javascript; charset=utf-8")
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .body(Body::from(bytes))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn proxy_applet(
    State(s): State<AppState>,
    Path((id, rest)): Path<(String, String)>,
    req: axum::extract::Request,
) -> Response<Body> {
    proxy_impl(s, id, format!("/{rest}"), req).await
}

async fn proxy_applet_root(
    State(s): State<AppState>,
    Path(id): Path<String>,
    req: axum::extract::Request,
) -> Response<Body> {
    proxy_impl(s, id, "/".to_string(), req).await
}

/// Forward to the applet, spawning it if this is the first request.
///
/// Runs on a blocking thread because the proxy is synchronous socket
/// I/O (see applets.rs for why there is no HTTP client crate here).
async fn proxy_impl(
    s: AppState,
    id: String,
    path: String,
    req: axum::extract::Request,
) -> Response<Body> {
    let method = req.method().to_string();
    let query = req
        .uri()
        .query()
        .map(|q| format!("?{q}"))
        .unwrap_or_default();
    // Carry the caller's content type through. Inventing one would
    // mislabel any non-JSON body, and this route accepts any method.
    let content_type = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let body = match axum::body::to_bytes(req.into_body(), 8 * 1024 * 1024).await {
        Ok(b) => b.to_vec(),
        Err(e) => return applet_error(StatusCode::BAD_REQUEST, &format!("read body: {e}")),
    };
    let target = format!("{path}{query}");
    let registry = s.applets.clone();
    let result = tokio::task::spawn_blocking(move || {
        // A card may reference an applet added since boot, and an
        // applet whose params changed must not keep serving the old
        // ones — so the same refresh guards the data path.
        registry.refresh_if_config_changed();
        registry.proxy(&id, &method, &target, content_type.as_deref(), &body)
    })
    .await;
    match result {
        Ok(Ok(r)) => Response::builder()
            .status(StatusCode::from_u16(r.status).unwrap_or(StatusCode::BAD_GATEWAY))
            .header(header::CONTENT_TYPE, r.content_type)
            .body(Body::from(r.body))
            .unwrap_or_else(|_| applet_error(StatusCode::BAD_GATEWAY, "malformed applet response")),
        // The applet is configured but not answering. Hand the card
        // the reason rather than an empty body it would render as "no
        // data" — the same instinct as a failed step's last stderr
        // lines becoming its error message.
        Ok(Err(e)) => applet_error(StatusCode::BAD_GATEWAY, &e),
        Err(e) => applet_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("proxy task: {e}"),
        ),
    }
}

fn applet_error(status: StatusCode, msg: &str) -> Response<Body> {
    eprintln!("applet proxy: {msg}");
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::json!({ "error": msg }).to_string()))
        .expect("static response builds")
}

/// Content-addressed JS store under `<root>/.datalib/cards/<hash>.js`.
/// Writes are idempotent: identical sources produce the same hash, and
/// re-POSTing returns the same hash without touching the file.
async fn create_card(
    State(s): State<AppState>,
    Json(req): Json<CreateCardRequest>,
) -> Result<Json<CreateCardResponse>, StatusCode> {
    let hash = sha256_hex(req.source.as_bytes());
    let dir = s.root.join(".datalib/cards");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("create_card: mkdir {}: {e}", dir.display());
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let path = dir.join(format!("{hash}.js"));
    if !path.exists() {
        if let Err(e) = std::fs::write(&path, req.source.as_bytes()) {
            eprintln!("create_card: write {}: {e}", path.display());
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    Ok(Json(CreateCardResponse { hash }))
}

/// Serve a stored card's JS body. The hash is validated to be 64 hex
/// chars so the path can't traverse out of the cards directory.
async fn get_card(
    State(s): State<AppState>,
    Path(hash): Path<String>,
) -> Result<
    (
        StatusCode,
        [(axum::http::HeaderName, &'static str); 1],
        String,
    ),
    StatusCode,
> {
    // Same guard the module store applies: validate the digest's
    // shape before it is ever joined onto a directory.
    if !frontend::is_sha256_hex(&hash) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = s.root.join(".datalib/cards").join(format!("{hash}.js"));
    match std::fs::read_to_string(&path) {
        Ok(body) => Ok((
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/javascript; charset=utf-8",
            )],
            body,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

// --- Authoring the `user` namespace ----------------------------------------
//
// `/api/lib` is how a person or an agent puts a component into the
// store. It is *only* a writer: everything read back — by the gallery,
// by a card resolving `comp.user.foo` — comes from
// `system/frontend/user/` through [`frontend::FrontendStore`], the same
// scan that reads an applet's namespace. There is one component
// mechanism, and this endpoint is a convenience for filling one corner
// of it without a text editor.
//
// A PUT writes two files, which is the whole storage format:
//
//   system/frontend/user/<sha256>.js   the source, addressed by content
//   system/frontend/user/<name>.json   { title, description,
//                                        component_hash, component_args }
//
// Re-PUTting a name repoints its `.json` at new bytes. The old `.js`
// is left in place — it is content-addressed, so it is still a correct
// answer for anything mid-render, and a later refresh does not sweep
// `user`.

#[derive(Debug, Deserialize)]
pub struct PutLibRequest {
    pub source: String,
    /// Gallery description. Omitted = keep what is stored; empty
    /// string = clear.
    #[serde(default)]
    pub description: Option<String>,
    /// Human-readable display name, same keep/clear semantics.
    #[serde(default)]
    pub title: Option<String>,
    /// Arguments the gallery entry should pass, as JSON values. This is
    /// what lets an authored component appear in the gallery *with*
    /// arguments — the thing the old name-only format could not express,
    /// and the reason `documentPickerView` had to exist as a stand-in.
    #[serde(default)]
    pub component_args: Option<Vec<serde_json::Value>>,
}

/// What a write returns: the name, the content hash, and the metadata
/// document as stored.
#[derive(Debug, Serialize)]
pub struct LibEntry {
    pub name: String,
    pub hash: String,
    pub meta: frontend::Meta,
}

/// Lowercase hex sha256. The single definition in this crate: the card
/// store and the frontend store name content the same way.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut hash = String::with_capacity(64);
    for b in digest.iter() {
        hash.push_str(&format!("{b:02x}"));
    }
    hash
}

/// `GET /api/lib/{name}` — the source behind `comp.user.{name}`.
///
/// A convenience for an agent about to edit a component: the store
/// addresses code by hash, and this resolves the name for you.
async fn get_lib(
    State(s): State<AppState>,
    Path(name): Path<String>,
) -> Result<
    (
        StatusCode,
        [(axum::http::HeaderName, &'static str); 1],
        String,
    ),
    StatusCode,
> {
    if !frontend::valid_name(&name) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let store = frontend::FrontendStore::scan(&s.root);
    let ns = store
        .view()
        .get(frontend::USER_NAMESPACE)
        .ok_or(StatusCode::NOT_FOUND)?;
    let hash = match ns.entries.get(&name) {
        Some(frontend::Meta::Component { component_hash, .. }) => component_hash.clone(),
        // A tombstone is not a component; the caller should follow it.
        Some(frontend::Meta::Renamed { .. }) | None => return Err(StatusCode::NOT_FOUND),
    };
    let bytes = store.read_component(&hash).ok_or(StatusCode::NOT_FOUND)?;
    let body = String::from_utf8(bytes).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok((
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/javascript; charset=utf-8",
        )],
        body,
    ))
}

/// `PUT /api/lib/{name}` — write a component into the `user` namespace.
async fn put_lib(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<PutLibRequest>,
) -> Result<Json<LibEntry>, StatusCode> {
    if !frontend::valid_name(&name) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let dir = frontend::frontend_dir(&s.root).join(frontend::USER_NAMESPACE);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("put_lib: mkdir {}: {e}", dir.display());
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }

    let hash = sha256_hex(req.source.as_bytes());
    let js = dir.join(format!("{hash}.js"));
    // Content-addressed: identical source is already the right file.
    if !js.exists() {
        if let Err(e) = std::fs::write(&js, req.source.as_bytes()) {
            eprintln!("put_lib: write {}: {e}", js.display());
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }

    // Absent title/description keep whatever is stored (so a plain
    // source re-PUT doesn't wipe them); an empty string clears.
    let prior = read_user_meta(&dir, &name);
    let merge = |req_field: Option<String>, stored: Option<String>| match req_field {
        None => stored,
        Some(v) if v.trim().is_empty() => None,
        Some(v) => Some(v),
    };
    let (prior_title, prior_desc, prior_args) = match prior {
        Some(frontend::Meta::Component {
            title,
            description,
            component_args,
            ..
        }) => (Some(title), Some(description), Some(component_args)),
        _ => (None, None, None),
    };
    let meta = frontend::Meta::Component {
        title: merge(req.title, prior_title).unwrap_or_default(),
        description: merge(req.description, prior_desc).unwrap_or_default(),
        component_hash: hash.clone(),
        component_args: req.component_args.or(prior_args).unwrap_or_default(),
    };
    // Writing the metadata also retires any tombstone at this name: the
    // name holds a real component again.
    let meta_path = dir.join(format!("{name}.json"));
    match serde_json::to_string_pretty(&meta) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&meta_path, text) {
                eprintln!("put_lib: meta {}: {e}", meta_path.display());
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
        Err(e) => {
            eprintln!("put_lib: encode meta {name}: {e}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    Ok(Json(LibEntry { name, hash, meta }))
}

/// Read a `user` metadata document, if it parses.
fn read_user_meta(dir: &std::path::Path, name: &str) -> Option<frontend::Meta> {
    let text = std::fs::read_to_string(dir.join(format!("{name}.json"))).ok()?;
    serde_json::from_str(&text).ok()
}

#[derive(Debug, Deserialize)]
pub struct RenameLibRequest {
    pub new_name: String,
}

/// `POST /api/lib/{name}/rename` — move a component to a new name,
/// leaving `{"renamed_to": …}` behind so cards still saying
/// `comp.user.{name}(…)` can follow. This is how an agent gives a
/// placeholder its formal name once the component works.
///
/// 404 when `name` holds no component, 409 when `new_name` is taken,
/// 400 when either name is not an identifier.
async fn rename_lib(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<RenameLibRequest>,
) -> Result<Json<LibEntry>, StatusCode> {
    let new_name = req.new_name;
    if !frontend::valid_name(&name) || !frontend::valid_name(&new_name) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if name == new_name {
        return Err(StatusCode::BAD_REQUEST);
    }
    let dir = frontend::frontend_dir(&s.root).join(frontend::USER_NAMESPACE);
    let meta = match read_user_meta(&dir, &name) {
        Some(m @ frontend::Meta::Component { .. }) => m,
        _ => return Err(StatusCode::NOT_FOUND),
    };
    if read_user_meta(&dir, &new_name).is_some() {
        return Err(StatusCode::CONFLICT);
    }

    let target = dir.join(format!("{new_name}.json"));
    let encoded = serde_json::to_string_pretty(&meta).map_err(|e| {
        eprintln!("rename_lib: encode {new_name}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    if let Err(e) = std::fs::write(&target, encoded) {
        eprintln!("rename_lib: write {}: {e}", target.display());
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    // The tombstone replaces the old document, so the old name resolves
    // to a redirect rather than to code. `component_hash` is untouched:
    // both names point at the same bytes until cards catch up.
    let tomb = frontend::Meta::Renamed {
        renamed_to: new_name.clone(),
    };
    let tomb_path = dir.join(format!("{name}.json"));
    match serde_json::to_string_pretty(&tomb) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&tomb_path, text) {
                eprintln!("rename_lib: tombstone {}: {e}", tomb_path.display());
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
        }
        Err(e) => {
            eprintln!("rename_lib: encode tombstone {name}: {e}");
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    let hash = match &meta {
        frontend::Meta::Component { component_hash, .. } => component_hash.clone(),
        frontend::Meta::Renamed { .. } => String::new(),
    };
    Ok(Json(LibEntry {
        name: new_name,
        hash,
        meta,
    }))
}

fn markdown_doc(
    body: &'static str,
) -> (
    StatusCode,
    [(axum::http::HeaderName, &'static str); 1],
    &'static str,
) {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/markdown; charset=utf-8",
        )],
        body,
    )
}

async fn agent_cards_guide() -> (
    StatusCode,
    [(axum::http::HeaderName, &'static str); 1],
    &'static str,
) {
    markdown_doc(include_str!("agent_cards_guide.md"))
}

async fn agent_config_guide() -> (
    StatusCode,
    [(axum::http::HeaderName, &'static str); 1],
    &'static str,
) {
    markdown_doc(include_str!("agent_config_guide.md"))
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
        col("org_name", "Org", false),
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
/// at the data root — the backend never persists this list to SQL. A
/// source is any step with no declared inputs (a fringe step — the
/// thing `--sync` can target), identified by its step id.
#[derive(Debug, Serialize)]
pub struct SourceInfo {
    pub id: String,
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

// --- Config / setup --------------------------------------------------------
//
// These three endpoints make the data root self-contained: the app reads
// and writes its own `<root>/config.toml` instead of relying on a
// separate external file. An empty data root opens with no config; the
// UI's Setup tab scaffolds one, lets the user edit it, and saves it
// back here, after which `/api/sync/*` lights up.
//
// TOML is the only format any of this handles. A data root predating
// the switch is converted once, out of band, by the separate
// `datalib-migrate-config` program; all this side does is notice the
// stray `config.yaml` and say so (`legacy_yaml_path`).

/// Parse + validate the DAG config at `path` the same way the runner
/// does (`config::load` → `to_specs` → `Graph::build`, so cycle /
/// ownership / bad-command errors are caught, not just TOML syntax),
/// and derive the source step ids.
///
/// A source is any step with no declared `inputs` — a fringe step,
/// which is exactly what the runner's `--sync` can target (its real
/// input is outside the DAG: a remote service, a user-staged tree).
/// Nothing about the step's command matters here; the derivation is
/// fully generic.
fn load_dag_config(
    path: &std::path::Path,
) -> anyhow::Result<(datalib_dag::config::DagConfig, Vec<String>)> {
    let (cfg, _root) = datalib_dag::config::load(path)?;
    let sources = check_dag_config(&cfg)?;
    Ok((cfg, sources))
}

/// [`load_dag_config`] against text that isn't on disk yet — the
/// about-to-be-saved config in `put_config`.
fn validate_config_text(text: &str) -> anyhow::Result<Vec<String>> {
    check_dag_config(&datalib_dag::config::parse(text)?)
}

/// Run the runner's own validation over a parsed config and return its
/// source step ids. Nothing is executed here.
fn check_dag_config(cfg: &datalib_dag::config::DagConfig) -> anyhow::Result<Vec<String>> {
    let specs = datalib_dag::config::to_specs(cfg)?;
    datalib_dag::Graph::build(specs)?;
    Ok(cfg
        .steps
        .iter()
        .filter(|e| e.inputs.is_empty())
        .map(|e| e.id.clone())
        .collect())
}

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    /// Absolute path of `<root>/config.toml` — shown in the UI so the
    /// user knows exactly which file they're editing.
    pub path: String,
    /// Whether that file exists yet. `false` on a fresh data root.
    pub exists: bool,
    /// Raw config text (empty string when the file doesn't exist).
    pub text: String,
    /// Whether the current bytes parse + validate as a `DagConfig`.
    pub parsed_ok: bool,
    /// Loader error message when `parsed_ok` is false.
    pub error: Option<String>,
    /// Number of configured sources (0 when invalid/missing).
    pub source_count: usize,
    /// How the user should invoke the latchkey CLI on this install:
    /// the app-bundled launcher's absolute path (shell-quoted if
    /// needed) when running from the packaged app, else
    /// `npx -y latchkey@<pin>`. The Setup UI splices this into its
    /// copy-pasteable credential-setup snippets.
    pub latchkey_cli: String,
    /// Absolute path of a pre-TOML `<root>/config.yaml`, when one is
    /// sitting there and `config.toml` is not. Purely a signpost: the
    /// UI tells the user to convert it rather than leaving them
    /// staring at an apparently-empty data root that visibly has a
    /// config in it. Nothing here reads or parses the file — this is
    /// an fs::exists check, and the legacy schemas live in the
    /// migration tool alone.
    pub legacy_yaml_path: Option<String>,
    /// The exact command that converts it, set whenever
    /// `legacy_yaml_path` is. Resolved rather than hard-coded because
    /// the packaged desktop app installs `datalib-migrate-config`
    /// inside the bundle, where it is not on the user's `$PATH` — a
    /// bare command name would be a dead end there.
    pub legacy_migrate_cmd: Option<String>,
}

/// A stray pre-TOML config in this root, if any, plus the command to
/// convert it. `None` once `config.toml` exists, so the hint retires
/// itself after a migration without the user having to delete the old
/// file.
fn legacy_yaml_hint(root: &std::path::Path) -> Option<(String, String)> {
    if datalib_dag::config::root_config_path(root).exists() {
        return None;
    }
    let yaml = root.join("config.yaml");
    if !yaml.exists() {
        return None;
    }
    Some((yaml.display().to_string(), migrate_cmd(root)))
}

/// `datalib-migrate-config <root>`, with the tool's absolute path when
/// it sits next to this binary — which is how every distribution lays
/// it out (release tarball, Docker image, and the app bundle's
/// `Resources/binaries`, the one case where `$PATH` won't find it).
fn migrate_cmd(root: &std::path::Path) -> String {
    use datalib_core::node_runtime::shell_quote;
    const NAMES: [&str; 2] = ["datalib-migrate-config", "datalib_migrate_config_bin"];
    let sibling = std::env::current_exe().ok().and_then(|exe| {
        let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
        let dir = exe.parent()?.to_path_buf();
        NAMES.into_iter().map(|n| dir.join(n)).find(|p| p.is_file())
    });
    let prog = match sibling {
        Some(p) => shell_quote(&p.to_string_lossy()),
        None => NAMES[0].to_string(),
    };
    format!("{prog} {}", shell_quote(&root.to_string_lossy()))
}

/// `GET /api/config` — current `<root>/config.toml` plus a parse check.
async fn get_config(State(s): State<AppState>) -> Json<ConfigResponse> {
    let path = s.config_path();
    let legacy = legacy_yaml_hint(&s.root);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let (parsed_ok, error, source_count) = match load_dag_config(&path) {
        Ok((_cfg, sources)) => (true, None, sources.len()),
        Err(e) => (false, Some(format!("{e:#}")), 0),
    };
    Json(ConfigResponse {
        exists: path.exists(),
        path: path.display().to_string(),
        text,
        parsed_ok,
        error,
        source_count,
        latchkey_cli: datalib_core::node_runtime::latchkey_cli_hint(),
        legacy_yaml_path: legacy.clone().map(|(p, _)| p),
        legacy_migrate_cmd: legacy.map(|(_, c)| c),
    })
}

#[derive(Debug, Deserialize)]
pub struct PutConfigRequest {
    pub text: String,
}

#[derive(Debug, Serialize)]
pub struct PutConfigResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub source_count: usize,
}

/// `PUT /api/config` — validate then atomically write
/// `<root>/config.toml`.
///
/// We validate with the real loader first (so cycle / ownership /
/// bad-command errors are caught, not just syntax), then write via a
/// sibling `.tmp` + `rename` so a rejected — or half-written — config
/// never clobbers the existing one. Validation failures return
/// `200 {ok:false, error}` (the UI shows it inline); only genuine I/O
/// failures are 5xx.
async fn put_config(
    State(s): State<AppState>,
    Json(req): Json<PutConfigRequest>,
) -> Result<Json<PutConfigResponse>, StatusCode> {
    let path = s.config_path();

    // Validate the submitted text before it touches the filesystem, so
    // a rejected config never gets written even transiently.
    let sources = match validate_config_text(&req.text) {
        Ok(sources) => sources,
        Err(e) => {
            return Ok(Json(PutConfigResponse {
                ok: false,
                error: Some(format!("{e:#}")),
                source_count: 0,
            }))
        }
    };

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            eprintln!("put_config: mkdir {}: {e}", parent.display());
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }
    let tmp = path.with_extension("tmp");
    if let Err(e) = std::fs::write(&tmp, req.text.as_bytes()) {
        eprintln!("put_config: write {}: {e}", tmp.display());
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    if let Err(e) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        eprintln!("put_config: rename {}: {e}", path.display());
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok(Json(PutConfigResponse {
        ok: true,
        error: None,
        source_count: sources.len(),
    }))
}

/// `GET /api/config/scaffold` — a minimal starter `config.toml` for this
/// data root. The UI drops it into the editor when the root has no config
/// yet; the user then fills in sources via the Setup tab's buttons.
async fn config_scaffold(State(s): State<AppState>) -> Json<ConfigResponse> {
    let path = s.config_path();
    let legacy = legacy_yaml_hint(&s.root);
    Json(ConfigResponse {
        exists: path.exists(),
        path: path.display().to_string(),
        text: scaffold_toml(),
        parsed_ok: true,
        error: None,
        source_count: 0,
        latchkey_cli: datalib_core::node_runtime::latchkey_cli_hint(),
        legacy_yaml_path: legacy.clone().map(|(p, _)| p),
        legacy_migrate_cmd: legacy.map(|(_, c)| c),
    })
}

/// One step in `GET /api/dag`, in topological order.
#[derive(Debug, Serialize)]
pub struct DagStepInfo {
    pub id: String,
    /// The step's `command:` as written in the config.
    pub command: String,
    /// Declared input artifact patterns (may contain wildcards).
    pub inputs: Vec<String>,
    /// Declared output artifact paths.
    pub outputs: Vec<String>,
    /// Ids of the steps this one depends on (derived from artifact
    /// overlap — the actual DAG edges).
    pub deps: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DagResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub steps: Vec<DagStepInfo>,
}

/// `GET /api/dag` — the step DAG derived from the data root's config,
/// exactly as the runner would build it (same load → to_specs →
/// Graph::build chain), so the visualization can never drift from
/// execution. Steps come back in topological order.
async fn get_dag(State(s): State<AppState>) -> Json<DagResponse> {
    use datalib_dag::config;
    let build = || -> anyhow::Result<Vec<DagStepInfo>> {
        let (cfg, _root) = config::load(&s.config_path())?;
        let commands: std::collections::HashMap<String, String> = cfg
            .steps
            .iter()
            .map(|e| (e.id.clone(), e.command.clone()))
            .collect();
        let specs = config::to_specs(&cfg)?;
        let graph = datalib_dag::Graph::build(specs)?;
        Ok(graph
            .topo
            .iter()
            .map(|&i| {
                let sp = &graph.steps[i];
                DagStepInfo {
                    id: sp.id.clone(),
                    command: commands.get(&sp.id).cloned().unwrap_or_default(),
                    inputs: sp.inputs.iter().map(|a| a.as_str().to_string()).collect(),
                    outputs: sp.outputs.iter().map(|a| a.as_str().to_string()).collect(),
                    deps: graph.deps[i]
                        .iter()
                        .map(|&d| graph.steps[d].id.clone())
                        .collect(),
                }
            })
            .collect())
    };
    match build() {
        Ok(steps) => Json(DagResponse {
            ok: true,
            error: None,
            steps,
        }),
        Err(e) => Json(DagResponse {
            ok: false,
            error: Some(format!("{e:#}")),
            steps: Vec::new(),
        }),
    }
}

/// Starter DAG config: the shared fan-in steps that every source's
/// rendered markdown feeds. Non-empty on purpose — `index`/`qmd` are
/// source-independent and belong in every pipeline; their wildcard
/// input matches nothing until the first source's steps are added
/// (which the UI's "Add a source" buttons append as a
/// `<name>.download` + `<name>.render` pair). `data_root` is omitted:
/// it defaults to this file's own directory, keeping the root
/// self-contained.
fn scaffold_toml() -> String {
    "\
# ── shared fan-in steps ────────────────────────────────────────────────
# Every source's rendered markdown feeds these.

[[steps]]
id = \"grid_index\"
command = \"datalib-step grid_index\"
inputs = [\"**/rendered_md\"]
outputs = [\"system/backend_index\"]

[[steps]]
id = \"qmd_index\"
command = \"datalib-step qmd_index\"
inputs = [\"**/rendered_md\"]
outputs = [\"system/qmd\"]

# Source steps go below. Anything you add above the first [[steps]]
# is a top-level key (data_root, binary_dir), not part of a step.
# `[[applets]]` is the other top-level array — servers that contribute
# UI + endpoints rather than artifacts; see <origin>/agent/config.md.
# ───────────────────────────────────────────────────────────────────────
"
    .to_string()
}

/// Surface the DAG config's source steps to the UI as `{id}` entries —
/// the steps with no declared inputs, i.e. what a sync can target.
/// Re-loaded on every call so a config edit in the Setup tab shows up
/// without a backend restart. Returns an empty list (rather than 500)
/// when the file is missing or fails to parse, mirroring the previous
/// behavior.
async fn sync_sources(State(s): State<AppState>) -> Json<Vec<SourceInfo>> {
    let sources = match load_dag_config(&s.config_path()) {
        Ok((_cfg, sources)) => sources,
        Err(_) => return Json(Vec::new()),
    };
    Json(sources.into_iter().map(|id| SourceInfo { id }).collect())
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
    // Validate the discriminator server-side; the DB column is a
    // VARCHAR with no enum constraint so we'd otherwise accept anything.
    // `all` (one DAG run, `source_name` optionally selecting a subset)
    // is the only live kind — the legacy `download`/`ingest`/`render`
    // kinds died with the fixed-phase orchestrator and are rejected;
    // historical rows keep whatever kind they were written with.
    match req.kind.as_str() {
        "all" => {}
        _ => return Err(StatusCode::BAD_REQUEST),
    }
    let row = s
        .repo
        .enqueue_job(&req.kind, req.source_name.as_deref())
        .await
        .map_err(repo_err_to_status)?;
    // Push the new (pending) job so SSE clients show it immediately,
    // before the worker even claims it.
    let _ = s.progress_tx.send(worker::ProgressEvent {
        id: row.id.clone(),
        kind: row.kind.clone(),
        source_name: row.source_name.clone(),
        state: row.state.clone(),
        progress_pct: row.progress_pct,
        progress_msg: row.progress_msg.clone(),
        tasks: None,
    });
    Ok(Json(row))
}

/// SSE stream of live job progress. Each `message` event is a JSON
/// [`worker::ProgressEvent`]. The UI keeps its job list patched from this
/// instead of polling; a slow poll remains as a reconnect fallback.
async fn sync_stream(
    State(s): State<AppState>,
) -> Sse<impl futures::Stream<Item = Result<Event, std::convert::Infallible>>> {
    use tokio::sync::broadcast::error::RecvError;
    let rx = s.progress_tx.subscribe();
    let stream = futures::stream::unfold(rx, |mut rx| async move {
        loop {
            match rx.recv().await {
                Ok(ev) => {
                    let data = serde_json::to_string(&ev).unwrap_or_default();
                    return Some((Ok(Event::default().data(data)), rx));
                }
                // Slow consumer dropped some events; keep going with the next.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return None,
            }
        }
    });
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn sync_job_cancel(
    State(s): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, StatusCode> {
    s.repo
        .request_cancel_job(&id)
        .await
        .map_err(repo_err_to_status)?;
    // A pending job that's canceled is never claimed by the worker, so
    // it would emit nothing — push a terminal event ourselves so the UI
    // updates. (A running job will also get the worker's own event.)
    let _ = s.progress_tx.send(worker::ProgressEvent {
        id,
        kind: String::new(),
        source_name: None,
        state: "canceled".to_string(),
        progress_pct: None,
        progress_msg: None,
        tasks: None,
    });
    Ok(StatusCode::NO_CONTENT)
}

/// Tail the per-job log written by the worker at
/// `<root>/system/state/job-logs/{id}.log`.
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
    let path = datalib_core::layout::state_dir(&s.root)
        .join("job-logs")
        .join(format!("{id}.log"));
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
        assert_eq!(default_columns().len(), 11);
    }

    /// Whatever the scaffold emits has to survive the round trip the
    /// user is about to put it through: parse as TOML, then pass the
    /// runner's own validation.
    #[test]
    fn scaffold_is_valid_toml() {
        let text = scaffold_toml();
        let sources = validate_config_text(&text)
            .unwrap_or_else(|e| panic!("scaffold rejected: {e:#}\n---\n{text}"));
        // The two fan-in steps both declare inputs, so neither is a
        // source — a scaffolded root has nothing to sync yet.
        assert!(sources.is_empty(), "{sources:?}");
    }

    /// TOML is the only format the server accepts; a legacy config
    /// PUT here is a parse error, not a silent reinterpretation.
    #[test]
    fn only_toml_is_accepted() {
        assert_eq!(
            validate_config_text("[[steps]]\nid = \"x\"\ncommand = \"s\"\noutputs = [\"x/raw\"]\n")
                .unwrap(),
            ["x"]
        );
        assert!(validate_config_text("steps:\n  - id: x\n    command: s\n").is_err());
        assert!(validate_config_text("sources:\n  - name: x\n").is_err());
    }

    /// The migrator hint is a signpost, not a fallback: it appears only
    /// while a stray config.yaml is the *only* config, and retires
    /// itself once config.toml exists — the user never has to delete
    /// the old file to clear it.
    #[test]
    fn legacy_yaml_hint_retires_itself() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        assert_eq!(legacy_yaml_hint(root), None, "empty root");

        std::fs::write(root.join("config.yaml"), "steps: []\n").unwrap();
        let (path, cmd) = legacy_yaml_hint(root).expect("stray config.yaml");
        assert_eq!(path, root.join("config.yaml").display().to_string());
        // The command names the tool and the data root, so it's
        // copy-pasteable as shown.
        assert!(cmd.contains("datalib-migrate-config"), "{cmd}");
        assert!(cmd.contains(&root.display().to_string()), "{cmd}");

        std::fs::write(root.join("config.toml"), "steps = []\n").unwrap();
        assert_eq!(legacy_yaml_hint(root), None, "config.toml wins");
    }
}
