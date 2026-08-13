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
    routing::{get, post},
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
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

pub mod boot;
mod embed;
pub mod worker;

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
}

impl AppState {
    /// Self-contained config path for this data root: `<root>/config.toml`,
    /// or a pre-TOML `<root>/config.yaml` while one is still there. The
    /// config + setup endpoints read and write it, and the sync worker
    /// drives `datalib-dag <this>`. Keeping the config inside the root
    /// is what lets the app bootstrap from an empty directory with no
    /// external `~/.config` file.
    ///
    /// Resolved per call rather than pinned at boot, so converting a
    /// legacy YAML config takes effect the moment the new
    /// `config.toml` lands — no restart.
    pub fn config_path(&self) -> PathBuf {
        datalib_ingest_config::resolve_root_config_path(&self.root)
    }
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
        .route("/api/config/migrate", get(config_migrate))
        .route("/api/dag", get(get_dag))
        .route("/api/lib", get(list_lib))
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

/// Content-addressed JS store under `<root>/.datalib/cards/<hash>.js`.
/// Writes are idempotent: identical sources produce the same hash, and
/// re-POSTing returns the same hash without touching the file.
async fn create_card(
    State(s): State<AppState>,
    Json(req): Json<CreateCardRequest>,
) -> Result<Json<CreateCardResponse>, StatusCode> {
    let mut h = Sha256::new();
    h.update(req.source.as_bytes());
    let digest = h.finalize();
    let mut hash = String::with_capacity(64);
    for b in digest.iter() {
        hash.push_str(&format!("{b:02x}"));
    }
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
    let valid = hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if !valid {
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

// --- Component library (named, mutable card aliases) -----------------------
//
// `/api/lib` is the user-defined component library: named JS "view
// factory" snippets that card source can invoke by bare name, exactly
// like the builtin `gridView`/`documentView`. Unlike `/api/card` (which
// is content-addressed and immutable), a lib entry is a MUTABLE name —
// re-PUTting `foo` overwrites it, and any card whose source references
// `foo()` re-renders. A coding agent is the expected author: it writes
// (or compiles/minifies) a factory and PUTs it under a name the card
// points at.
//
// Stored one-file-per-name under `<root>/.datalib/lib/<name>.js`.
// The name doubles as a JS identifier injected into card scope, so it
// is constrained to a valid bare identifier (see `valid_lib_name`),
// which also makes it path-safe (no `/`, `.`, traversal).

#[derive(Debug, Deserialize)]
pub struct PutLibRequest {
    pub source: String,
    /// Optional gallery metadata: a short human-readable description of
    /// what the component shows. A component with a description appears
    /// in the new-card gallery (it must therefore work with no
    /// arguments). Omitted = keep the stored description (so a plain
    /// source re-PUT doesn't wipe it); empty string = clear it.
    #[serde(default)]
    pub description: Option<String>,
    /// Optional human-readable display name, shown instead of the bare
    /// component name wherever the component is listed (the new-card
    /// gallery, the component-library view). Same keep/clear semantics
    /// as `description`.
    #[serde(default)]
    pub title: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct LibEntry {
    pub name: String,
    /// sha256 of the source — the UI watches this to decide when a card
    /// that depends on this alias needs re-rendering. Empty for rename
    /// tombstones (entries that only carry `renamed_to`).
    pub hash: String,
    /// Human-readable display name (see [`PutLibRequest::title`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Gallery description (see [`PutLibRequest::description`]); `None`
    /// for components that don't advertise themselves in the gallery.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Set on rename tombstones: this name no longer holds a component;
    /// it was renamed to the given name. The UI follows these to
    /// repoint cards that still reference the old name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renamed_to: Option<String>,
}

/// Sidecar shape stored at `<root>/.datalib/lib/<name>.meta.json`,
/// holding the mutable non-source fields of a lib entry. A separate
/// file (rather than frontmatter in the `.js`) keeps the stored source
/// byte-identical to what evaluates, so hashes stay pure content
/// hashes.
#[derive(Debug, Default, Serialize, Deserialize)]
struct LibMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

impl LibMeta {
    fn is_empty(&self) -> bool {
        self.title.is_none() && self.description.is_none()
    }
}

fn lib_meta_path(dir: &std::path::Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.meta.json"))
}

fn read_lib_meta(dir: &std::path::Path, name: &str) -> LibMeta {
    std::fs::read_to_string(lib_meta_path(dir, name))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Rename tombstone stored at `<root>/.datalib/lib/<name>.renamed.json`
/// after `<name>` is renamed away, so clients (and stale card URLs) can
/// follow the move. Removed if the old name is ever re-created by a PUT.
#[derive(Debug, Serialize, Deserialize)]
struct LibRename {
    renamed_to: String,
}

fn lib_rename_path(dir: &std::path::Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.renamed.json"))
}

/// Remove `name`'s rename tombstone if one exists ("already absent" is
/// fine; only real I/O errors surface).
fn clear_lib_rename(dir: &std::path::Path, name: &str) -> std::io::Result<()> {
    match std::fs::remove_file(lib_rename_path(dir, name)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        r => r,
    }
}

/// Names of the builtin view factories injected into card scope
/// (`datalib/ui/src/cards/types.ts` — keep in sync). A stored
/// component may not take one of these names: an alias with a builtin's
/// name would shadow the builtin when cards are compiled.
const BUILTIN_VIEW_NAMES: &[&str] = &[
    "gridView",
    "documentView",
    "documentPickerView",
    "galleryView",
    "agentSeedView",
    "aliasView",
    "dactalView",
    "perseusView",
    "sourceDagView",
];

/// A lib name is injected into card scope as a bare identifier and
/// invoked as `name()`, so it must be a valid ASCII JS identifier. That
/// also makes it path-safe: no `/`, `.`, or `..`, so it can't traverse
/// out of the lib directory.
fn valid_lib_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok =
        matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$');
    first_ok
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    let mut hash = String::with_capacity(64);
    for b in digest.iter() {
        hash.push_str(&format!("{b:02x}"));
    }
    hash
}

/// List every named component with its content hash, plus a tombstone
/// entry (`renamed_to`, empty hash) for every renamed-away name.
async fn list_lib(State(s): State<AppState>) -> Result<Json<Vec<LibEntry>>, StatusCode> {
    let dir = s.root.join(".datalib/lib");
    let mut out = Vec::new();
    match std::fs::read_dir(&dir) {
        Ok(rd) => {
            for ent in rd.flatten() {
                let path = ent.path();
                let Some(fname) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                if let Some(stem) = fname.strip_suffix(".js") {
                    if !valid_lib_name(stem) {
                        continue;
                    }
                    if let Ok(src) = std::fs::read_to_string(&path) {
                        let meta = read_lib_meta(&dir, stem);
                        out.push(LibEntry {
                            name: stem.to_string(),
                            hash: sha256_hex(src.as_bytes()),
                            title: meta.title,
                            description: meta.description,
                            renamed_to: None,
                        });
                    }
                } else if let Some(stem) = fname.strip_suffix(".renamed.json") {
                    if !valid_lib_name(stem) {
                        continue;
                    }
                    let Some(ren) = std::fs::read_to_string(&path)
                        .ok()
                        .and_then(|s| serde_json::from_str::<LibRename>(&s).ok())
                    else {
                        continue;
                    };
                    out.push(LibEntry {
                        name: stem.to_string(),
                        hash: String::new(),
                        title: None,
                        description: None,
                        renamed_to: Some(ren.renamed_to),
                    });
                }
            }
        }
        // No lib dir yet just means an empty library.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            eprintln!("list_lib: read_dir {}: {e}", dir.display());
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Json(out))
}

/// Serve a stored component's JS body as `text/javascript`.
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
    if !valid_lib_name(&name) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let path = s.root.join(".datalib/lib").join(format!("{name}.js"));
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

/// Create or overwrite a named component. Idempotent per content; the
/// returned hash lets the caller confirm what landed.
async fn put_lib(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<PutLibRequest>,
) -> Result<Json<LibEntry>, StatusCode> {
    if !valid_lib_name(&name) || BUILTIN_VIEW_NAMES.contains(&name.as_str()) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let dir = s.root.join(".datalib/lib");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("put_lib: mkdir {}: {e}", dir.display());
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let path = dir.join(format!("{name}.js"));
    if let Err(e) = std::fs::write(&path, req.source.as_bytes()) {
        eprintln!("put_lib: write {}: {e}", path.display());
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    // The name holds a real component again — retire any tombstone left
    // by an earlier rename away from it.
    if let Err(e) = clear_lib_rename(&dir, &name) {
        eprintln!("put_lib: clear rename {name}: {e}");
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    // Title/description semantics: absent = keep what's stored,
    // "" = clear.
    let stored = read_lib_meta(&dir, &name);
    let merge = |req_field: Option<String>, stored_field: Option<String>| match req_field {
        None => stored_field,
        Some(v) if v.trim().is_empty() => None,
        Some(v) => Some(v),
    };
    let meta = LibMeta {
        title: merge(req.title, stored.title),
        description: merge(req.description, stored.description),
    };
    let meta_path = lib_meta_path(&dir, &name);
    let write_res = if meta.is_empty() {
        // No metadata → no sidecar; ignore "already absent".
        match std::fs::remove_file(&meta_path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            r => r,
        }
    } else {
        std::fs::write(&meta_path, serde_json::to_string(&meta).unwrap())
    };
    if let Err(e) = write_res {
        eprintln!("put_lib: meta {}: {e}", meta_path.display());
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok(Json(LibEntry {
        name,
        hash: sha256_hex(req.source.as_bytes()),
        title: meta.title,
        description: meta.description,
        renamed_to: None,
    }))
}

#[derive(Debug, Deserialize)]
pub struct RenameLibRequest {
    pub new_name: String,
}

/// `POST /api/lib/{name}/rename` — move a component to a new name,
/// leaving a tombstone behind so cards that still say `{name}()` can
/// follow (the UI rewrites their source when it sees the tombstone in
/// the manifest). This is how an agent gives the placeholder
/// `card_xxxxx` alias its "formal" name once the component works.
///
/// 404 when `name` doesn't exist, 409 when `new_name` is taken, 400
/// when either name is invalid (including builtin view names).
async fn rename_lib(
    State(s): State<AppState>,
    Path(name): Path<String>,
    Json(req): Json<RenameLibRequest>,
) -> Result<Json<LibEntry>, StatusCode> {
    let new_name = req.new_name;
    if !valid_lib_name(&name)
        || !valid_lib_name(&new_name)
        || BUILTIN_VIEW_NAMES.contains(&new_name.as_str())
        || new_name == name
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let dir = s.root.join(".datalib/lib");
    let old_path = dir.join(format!("{name}.js"));
    let new_path = dir.join(format!("{new_name}.js"));
    if !old_path.is_file() {
        return Err(StatusCode::NOT_FOUND);
    }
    if new_path.exists() {
        return Err(StatusCode::CONFLICT);
    }
    let source = std::fs::read_to_string(&old_path).map_err(|e| {
        eprintln!("rename_lib: read {}: {e}", old_path.display());
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let io = |what: &str, e: std::io::Error| {
        eprintln!("rename_lib: {what}: {e}");
        StatusCode::INTERNAL_SERVER_ERROR
    };
    std::fs::rename(&old_path, &new_path).map_err(|e| io("rename .js", e))?;
    // Carry the sidecar metadata along ("already absent" is fine).
    match std::fs::rename(lib_meta_path(&dir, &name), lib_meta_path(&dir, &new_name)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        r => r.map_err(|e| io("rename meta", e))?,
    }
    // The new name is live again — drop any tombstone parked on it —
    // and the old name becomes a tombstone pointing at the new one.
    clear_lib_rename(&dir, &new_name).map_err(|e| io("clear rename", e))?;
    let tomb = LibRename {
        renamed_to: new_name.clone(),
    };
    std::fs::write(
        lib_rename_path(&dir, &name),
        serde_json::to_string(&tomb).unwrap(),
    )
    .map_err(|e| io("write tombstone", e))?;
    let meta = read_lib_meta(&dir, &new_name);
    Ok(Json(LibEntry {
        name: new_name,
        hash: sha256_hex(source.as_bytes()),
        title: meta.title,
        description: meta.description,
        renamed_to: None,
    }))
}

/// Onboarding docs for a coding agent pointed at this instance. Served
/// as markdown at stable, app-relative URLs so a wayfinder snippet can
/// reference `<origin>/agent/cards.md` (cards) or
/// `<origin>/agent/config.md` (the data-source config) without baking
/// the content into the wayfinder itself.
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
// separate `~/.config/datalib/config.yaml`. An empty data root opens
// with no config; the UI's Setup tab scaffolds one, lets the user edit
// it, and saves it back here, after which `/api/sync/*` lights up.
//
// Two legacy shapes still come through here, both YAML, both read-only:
// a pre-TOML `config.yaml` in the steps format, and the retired
// stanza-based `sources:` format. `/api/config/migrate` converts either
// one to TOML; the UI drops the result in the editor for review.

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
/// about-to-be-saved config in `put_config`. `path` supplies nothing
/// but its extension, which picks the parser.
fn validate_config_text(text: &str, path: &std::path::Path) -> anyhow::Result<Vec<String>> {
    check_dag_config(&datalib_dag::config::parse(text, path)?)
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

/// Which syntax [`ConfigResponse::text`] is in — the editor needs it
/// to pick a parser, and the UI keys its "convert this" prompt off it.
/// Serialized lowercase (`"toml"` / `"yaml"`) as the wire contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ConfigFormat {
    Toml,
    /// A pre-TOML config, in either the steps or the retired `sources:`
    /// format. Read-only as far as new roots are concerned; still
    /// saveable in place so a user can fix one without converting.
    Yaml,
}

impl ConfigFormat {
    /// The format implied by a config path's extension — the same rule
    /// [`datalib_dag::config::parse`] picks its parser by, so the UI
    /// can never disagree with the backend about how bytes are read.
    fn of_path(path: &std::path::Path) -> Self {
        if datalib_dag::config::is_legacy_yaml_path(path) {
            Self::Yaml
        } else {
            Self::Toml
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    /// Absolute path of `<root>/config.toml` (or the legacy
    /// `<root>/config.yaml`) — shown in the UI so the user knows
    /// exactly which file they're editing.
    pub path: String,
    /// Whether that file exists yet. `false` on a fresh data root.
    pub exists: bool,
    /// Raw config text (empty string when the file doesn't exist).
    pub text: String,
    /// The syntax `text` is in, from the path's extension.
    pub format: ConfigFormat,
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
    /// True when the file is an old-style `sources:` config for the
    /// retired sync binary (top-level `sources:` and no `steps:`).
    /// Always YAML. The UI offers a one-click migration to the DAG
    /// format; a `format: yaml` response with `legacy: false` is a
    /// pre-TOML *steps* config, which converts too but through a much
    /// shorter path.
    pub legacy: bool,
}

/// Cheap probe: is this text an old-style `sources:` config? Top-level
/// `sources:` with no `steps:` — the two formats are disjoint on those
/// keys. Only meaningful for YAML text; the `sources:` format predates
/// TOML entirely and was never written in it.
fn looks_legacy(yaml: &str) -> bool {
    let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(yaml) else {
        return false;
    };
    let Some(m) = v.as_mapping() else {
        return false;
    };
    m.contains_key("sources") && !m.contains_key("steps")
}

/// `GET /api/config` — current `<root>/config.toml` plus a parse check.
async fn get_config(State(s): State<AppState>) -> Json<ConfigResponse> {
    let path = s.config_path();
    let exists = path.exists();
    let format = ConfigFormat::of_path(&path);
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let (parsed_ok, error, source_count) = match load_dag_config(&path) {
        Ok((_cfg, sources)) => (true, None, sources.len()),
        Err(e) => (false, Some(format!("{e:#}")), 0),
    };
    let legacy = exists && format == ConfigFormat::Yaml && looks_legacy(&text);
    Json(ConfigResponse {
        path: path.display().to_string(),
        exists,
        text,
        format,
        parsed_ok,
        error,
        source_count,
        latchkey_cli: datalib_core::node_runtime::latchkey_cli_hint(),
        legacy,
    })
}

#[derive(Debug, Deserialize)]
pub struct PutConfigRequest {
    pub text: String,
    /// Which syntax `text` is in, and thus which file it lands in:
    /// `toml` → `<root>/config.toml`, `yaml` → `<root>/config.yaml`.
    /// The client sends it explicitly because saving is exactly when
    /// the format can *change* — accepting a converted config is a PUT
    /// of TOML text against a root whose current file is still YAML,
    /// and inferring the target from what's on disk would write TOML
    /// bytes into a `.yaml` name.
    pub format: ConfigFormat,
}

#[derive(Debug, Serialize)]
pub struct PutConfigResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub source_count: usize,
}

/// `PUT /api/config` — validate then atomically write the data root's
/// config file, `<root>/config.toml` for `format: toml` and
/// `<root>/config.yaml` for `format: yaml`.
///
/// We validate by writing to a sibling `.tmp` file and running the real
/// loader (so cycle / ownership / bad-command errors are caught, not
/// just syntax), then writing via a sibling `.tmp` + `rename` so a
/// rejected — or half-written — config never clobbers the existing
/// one. Validation failures return `200 {ok:false, error}` (the UI
/// shows it inline); only genuine I/O failures are 5xx.
///
/// Saving a converted config therefore *adds* `config.toml` and leaves
/// the old `config.yaml` sitting there. That's deliberate — the user
/// keeps their pre-conversion file as a fallback, and
/// [`AppState::config_path`] prefers the TOML one from the next
/// request onward, so nothing reads the stale copy.
async fn put_config(
    State(s): State<AppState>,
    Json(req): Json<PutConfigRequest>,
) -> Result<Json<PutConfigResponse>, StatusCode> {
    let path = match req.format {
        ConfigFormat::Toml => datalib_ingest_config::root_config_path(&s.root),
        ConfigFormat::Yaml => s.root.join("config.yaml"),
    };

    // Validate the submitted text before it touches the filesystem.
    // Note this can't go through a temp file the way it used to: the
    // parser is chosen by extension, and no `.tmp` name can carry the
    // real one (`config.yaml.tmp` has extension `tmp`, not `yaml`), so
    // a legacy save would be validated as TOML and always fail.
    let sources = match validate_config_text(&req.text, &path) {
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
    // Always TOML, whatever the root currently holds: a scaffold is by
    // definition a fresh config, and fresh configs are TOML.
    let path = datalib_ingest_config::root_config_path(&s.root);
    Json(ConfigResponse {
        exists: path.exists(),
        path: path.display().to_string(),
        text: scaffold_toml(),
        format: ConfigFormat::Toml,
        parsed_ok: true,
        error: None,
        source_count: 0,
        latchkey_cli: datalib_core::node_runtime::latchkey_cli_hint(),
        legacy: false,
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

/// Response for `GET /api/config/migrate`: the current legacy config
/// converted to TOML. Nothing is written — the UI drops the `text`
/// into the editor for review; the user saves explicitly.
#[derive(Debug, Serialize)]
pub struct MigrateResponse {
    pub ok: bool,
    pub text: Option<String>,
    pub error: Option<String>,
}

/// `GET /api/config/migrate` — convert the data root's legacy config to
/// a TOML one. Handles both legacy shapes, distinguished the same way
/// [`get_config`] reports them:
///
/// - the retired stanza-based `sources:` YAML → the steps format, a
///   real schema translation ([`migrate_legacy_config`]);
/// - a pre-TOML `config.yaml` already in the steps format → the same
///   steps, re-emitted as TOML ([`yaml_steps_to_toml`]).
///
/// A config that's already TOML has nothing to do and says so.
async fn config_migrate(State(s): State<AppState>) -> Json<MigrateResponse> {
    let path = s.config_path();
    let fail = |msg: String| {
        Json(MigrateResponse {
            ok: false,
            text: None,
            error: Some(msg),
        })
    };
    if ConfigFormat::of_path(&path) == ConfigFormat::Toml {
        return fail(format!("{} is already TOML", path.display()));
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => return fail(format!("read {}: {e}", path.display())),
    };
    let converted = if looks_legacy(&text) {
        migrate_legacy_config(&text)
    } else {
        yaml_steps_to_toml(&text, &path)
    };
    match converted {
        Ok(text) => Json(MigrateResponse {
            ok: true,
            text: Some(text),
            error: None,
        }),
        Err(e) => fail(format!("{e:#}")),
    }
}

/// Re-emit a pre-TOML `config.yaml` that's already in the steps format
/// as TOML. A straight reserialization of the parsed config: the steps
/// and their params survive exactly, comments and YAML-only spellings
/// (anchors, in particular, which get expanded into their two copies)
/// do not. Same contract as the `sources:` migrator — a reviewable
/// draft, not a byte-faithful rewrite.
fn yaml_steps_to_toml(text: &str, path: &std::path::Path) -> anyhow::Result<String> {
    let cfg = datalib_dag::config::parse(text, path)?;
    let mut doc = toml_edit::DocumentMut::new();
    if let Some(root) = &cfg.data_root {
        doc["data_root"] = toml_edit::value(root.display().to_string());
    }
    if let Some(dir) = &cfg.binary_dir {
        doc["binary_dir"] = toml_edit::value(dir.display().to_string());
    }
    let mut steps = toml_edit::ArrayOfTables::new();
    for e in &cfg.steps {
        let mut t = toml_edit::Table::new();
        t["id"] = toml_edit::value(e.id.as_str());
        t["command"] = toml_edit::value(e.command.as_str());
        if !e.inputs.is_empty() {
            t["inputs"] = toml_edit::value(str_array(&e.inputs));
        }
        if !e.outputs.is_empty() {
            t["outputs"] = toml_edit::value(str_array(&e.outputs));
        }
        if !e.env.is_empty() {
            let mut env = toml_edit::Table::new();
            for (k, v) in &e.env {
                env[k.as_str()] = toml_edit::value(v.as_str());
            }
            t["env"] = toml_edit::Item::Table(env);
        }
        if let Some(params) = &e.params {
            // Sub-tables must follow the plain keys; `params` is set
            // last so the emitted step reads id/command/inputs/outputs
            // and *then* its `[steps.params]` headers.
            t["params"] = params_table(params)
                .map_err(|err| anyhow::anyhow!("step {:?}: params → TOML: {err:#}", e.id))?;
        }
        t.decor_mut().set_prefix("\n");
        steps.push(t);
    }
    doc["steps"] = toml_edit::Item::ArrayOfTables(steps);
    Ok(format!(
        "# Converted from config.yaml — review, save, then delete the old\n\
         # file. Comments and formatting from it are not carried over.\n{doc}"
    ))
}

fn str_array(items: &[String]) -> toml_edit::Array {
    items.iter().map(|s| s.as_str()).collect()
}

/// A step's params subtree as a `[steps.params]` sub-table, so nested
/// structure reads as TOML headers rather than one dense inline table.
/// Round-tripping through text is what gives us that shape for free:
/// `toml::to_string` already orders values before tables, which is the
/// one rule a hand-built document is easy to violate.
fn params_table(params: &toml::Value) -> anyhow::Result<toml_edit::Item> {
    let doc: toml_edit::DocumentMut = toml::to_string(params)?.parse()?;
    let mut t = doc.as_table().clone();
    // Implicit = emit the `[steps.params]` header only if something is
    // directly under it. A params subtree that's all sub-tables would
    // otherwise lead with a bare header holding nothing.
    t.set_implicit(true);
    Ok(toml_edit::Item::Table(t))
}

/// Convert an old-style `sources:` config to the DAG step format:
/// each source becomes a `<name>.download` + `<name>.render` step pair
/// (render-only for unmanaged sources like `claude_export`), preceded
/// by the shared `index`/`qmd` fan-in steps. Comments from the old
/// file are not carried over; the output is a reviewable draft, not a
/// byte-faithful rewrite. Global `defaults:` are folded into each
/// source's `common:` (value-level only — no path resolution), so the
/// per-step params are self-contained.
fn migrate_legacy_config(text: &str) -> anyhow::Result<String> {
    use std::fmt::Write as _;
    // Raw (un-normalized) parse: we want the fields as written, not
    // resolved absolute paths.
    let mut cfg: datalib_ingest_config::Config =
        serde_yaml::from_str(text).map_err(|e| anyhow::anyhow!("parse legacy config: {e}"))?;
    let defaults = cfg.defaults.clone();

    let mut out = String::new();
    out.push_str("# Migrated from the old sources: format — review before saving.\n");
    if !cfg.data_root.as_os_str().is_empty() {
        // Above the first [[steps]], as TOML requires: everything after
        // a table header belongs to that table.
        let _ = writeln!(
            out,
            "data_root = {}\n",
            toml_edit::Value::from(cfg.data_root.display().to_string())
        );
    }
    let _ = write!(out, "{}", divider("shared fan-in steps"));
    out.push_str("# Every source's rendered markdown feeds these.\n");
    out.push_str(&step_block(
        "grid_index",
        "datalib-step grid_index",
        &["**/rendered_md"],
        &["system/backend_index"],
        None,
    )?);
    if !cfg.qmd.skip {
        out.push('\n');
        out.push_str(&step_block(
            "qmd_index",
            "datalib-step qmd_index",
            &["**/rendered_md"],
            &["system/qmd"],
            None,
        )?);
    }

    for entry in &mut cfg.sources {
        entry.source.common_mut().fold_defaults(&defaults);
        let name = entry.name.clone();
        let ty = entry.source.type_str();
        let managed = entry.source.is_managed();

        // The provider subtree, minus the `type:` tag (the command's
        // nested subcommand carries it now) and any nulls serde emitted for
        // unset fields. The old top-level `name:` is gone too — the
        // step derives it from its first declared output. Stripping the
        // nulls isn't cosmetic here: TOML has no null, so a surviving
        // one would fail to serialize at all.
        let mut val = serde_yaml::to_value(&entry.source)
            .map_err(|e| anyhow::anyhow!("serialize source {name:?}: {e}"))?;
        if let Some(m) = val.as_mapping_mut() {
            m.remove("type");
        }
        strip_nulls(&mut val);
        // Per-phase params split: pull the render-wave knobs out of
        // the legacy subtree into the render step's params; the rest
        // is the download step's. An empty side just omits `params`.
        let render_val = split_render_params(&mut val, ty);

        let mut block = format!("\n{}", divider(&name));
        if managed {
            block.push_str(&step_block(
                &format!("{name}.download"),
                &format!("datalib-step download {ty}"),
                &[],
                &[&format!("{name}/raw")],
                Some(&val),
            )?);
            block.push('\n');
        }
        block.push_str(&step_block(
            &format!("{name}.render"),
            &format!("datalib-step render {ty}"),
            &[&format!("{name}/raw")],
            &[&format!("{name}/rendered_md")],
            Some(&render_val),
        )?);

        if entry.enabled {
            out.push_str(&block);
        } else {
            // Disabled sources come over commented out — the new
            // format has no per-source enable flag.
            out.push_str("\n# (was `enabled: false` — uncomment to activate)\n");
            for line in block.trim_start_matches('\n').lines() {
                if line.is_empty() {
                    out.push_str("#\n");
                } else {
                    let _ = writeln!(out, "# {line}");
                }
            }
        }
    }
    Ok(out)
}

/// A full-width `# ── label ───────` section divider, padded to a fixed
/// width so the migrated file's sections are scannable.
fn divider(label: &str) -> String {
    const WIDTH: usize = 68;
    if label.is_empty() {
        return format!("# {}\n", "\u{2500}".repeat(WIDTH + 3));
    }
    let pad = "\u{2500}".repeat(WIDTH.saturating_sub(label.chars().count()));
    format!("# \u{2500}\u{2500} {label} {pad}\n")
}

/// One `[[steps]]` block as text, ready to concatenate. Built through
/// `toml_edit` rather than `format!` so ids, commands, and artifact
/// paths get quoted and escaped properly, and so the `[steps.params]`
/// sub-table lands after the plain keys.
fn step_block(
    id: &str,
    command: &str,
    inputs: &[&str],
    outputs: &[&str],
    params: Option<&serde_yaml::Value>,
) -> anyhow::Result<String> {
    let mut t = toml_edit::Table::new();
    t["id"] = toml_edit::value(id);
    t["command"] = toml_edit::value(command);
    if !inputs.is_empty() {
        t["inputs"] = toml_edit::value(inputs.iter().copied().collect::<toml_edit::Array>());
    }
    if !outputs.is_empty() {
        t["outputs"] = toml_edit::value(outputs.iter().copied().collect::<toml_edit::Array>());
    }
    // An empty subtree means the step takes no params at all; note that
    // an empty-but-present `sync` table is *not* empty by this test,
    // which is what keeps a managed source managed.
    if let Some(v) = params.filter(|v| !v.as_mapping().is_none_or(|m| m.is_empty())) {
        let as_toml = toml::Value::try_from(v)
            .map_err(|e| anyhow::anyhow!("step {id:?}: params → TOML: {e}"))?;
        t["params"] = params_table(&as_toml)
            .map_err(|e| anyhow::anyhow!("step {id:?}: params → TOML: {e:#}"))?;
    }
    let mut steps = toml_edit::ArrayOfTables::new();
    steps.push(t);
    let mut doc = toml_edit::DocumentMut::new();
    doc["steps"] = toml_edit::Item::ArrayOfTables(steps);
    Ok(doc.to_string())
}

/// Pull the render-wave knobs out of a legacy source subtree (post
/// [`strip_nulls`]), returning the render step's params. These fields
/// moved off the shared stanza in the per-phase params split:
/// `sync.period` (beeper/signal) and `sync.alignment_pairs` (perseus)
/// hop out of `sync:` to the render params' top level;
/// `outlink_format` / `only_render_labels` (email) move over
/// verbatim. An explicit `common.raw_path` is *copied* (both phases
/// read the raw-store location), and perseus also copies
/// `common.input_path` (it renders straight from the staged tree).
fn split_render_params(val: &mut serde_yaml::Value, ty: &str) -> serde_yaml::Value {
    use serde_yaml::{Mapping, Value};
    let mut render = Mapping::new();
    let Some(m) = val.as_mapping_mut() else {
        return Value::Mapping(render);
    };
    let worth_moving = |v: &Value| !v.as_sequence().is_some_and(|s| s.is_empty());
    if let Some(sync) = m
        .get_mut(Value::from("sync"))
        .and_then(|s| s.as_mapping_mut())
    {
        for key in ["period", "alignment_pairs"] {
            if let Some(v) = sync.remove(Value::from(key)) {
                if worth_moving(&v) {
                    render.insert(key.into(), v);
                }
            }
        }
    }
    for key in ["outlink_format", "only_render_labels"] {
        if let Some(v) = m.remove(Value::from(key)) {
            if worth_moving(&v) {
                render.insert(key.into(), v);
            }
        }
    }
    let mut rcommon = Mapping::new();
    if let Some(common) = m.get(Value::from("common")).and_then(|c| c.as_mapping()) {
        if let Some(v) = common.get(Value::from("raw_path")) {
            rcommon.insert("raw_path".into(), v.clone());
        }
        if ty == "perseus" {
            if let Some(v) = common.get(Value::from("input_path")) {
                rcommon.insert("input_path".into(), v.clone());
            }
        }
    }
    if !rcommon.is_empty() {
        render.insert("common".into(), Value::Mapping(rcommon));
    }
    Value::Mapping(render)
}

/// Drop `key: null` entries and (bottom-up) mappings that emptied out —
/// serde emits both for unset optional/default fields, and they'd read
/// as clutter in the migrated draft. `sync:` is kept even when empty:
/// its *presence* is what makes a source managed.
fn strip_nulls(v: &mut serde_yaml::Value) {
    if let Some(m) = v.as_mapping_mut() {
        for (_, val) in m.iter_mut() {
            strip_nulls(val);
        }
        let keys: Vec<serde_yaml::Value> = m
            .iter()
            .filter(|(k, val)| {
                val.is_null()
                    || (val.as_mapping().is_some_and(|mm| mm.is_empty())
                        && k.as_str() != Some("sync"))
            })
            .map(|(k, _)| k.clone())
            .collect();
        for k in keys {
            m.remove(&k);
        }
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
    format!(
        "\
{}# Every source's rendered markdown feeds these.

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
{}",
        divider("shared fan-in steps"),
        divider("")
    )
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

    /// Whatever these generators emit has to survive the round trip
    /// the user is about to put it through: parse as TOML, then pass
    /// the runner's own validation.
    fn assert_valid_toml_config(text: &str) -> Vec<String> {
        validate_config_text(text, std::path::Path::new("config.toml"))
            .unwrap_or_else(|e| panic!("generated config rejected: {e:#}\n---\n{text}"))
    }

    #[test]
    fn scaffold_is_valid_toml() {
        let sources = assert_valid_toml_config(&scaffold_toml());
        // The two fan-in steps both declare inputs, so neither is a
        // source — a scaffolded root has nothing to sync yet.
        assert!(sources.is_empty(), "{sources:?}");
    }

    /// The retired `sources:` format converts to TOML step pairs, with
    /// the params subtree landing under each step as a sub-table.
    #[test]
    fn legacy_sources_config_migrates_to_toml() {
        let out = migrate_legacy_config(
            "data_root: /tmp/dl\nsources:\n\
             \n  - name: slack\n    source:\n      type: slack_api\n      \
             sync: {channels: [chat-qi]}\n\
             \n  - name: off\n    enabled: false\n    source:\n      type: slack_api\n      \
             sync: {}\n",
        )
        .unwrap();
        let sources = assert_valid_toml_config(&out);

        assert!(out.contains("data_root = \"/tmp/dl\""), "{out}");
        assert!(out.contains("[[steps]]"), "{out}");
        assert!(out.contains("[steps.params.sync]"), "{out}");
        assert!(out.contains("channels = [\"chat-qi\"]"), "{out}");
        // Only the download half is a source (the render step declares
        // inputs), and the disabled entry came over commented out, so
        // it contributes no step at all.
        assert_eq!(sources, ["slack.download"]);
        assert!(out.contains("# (was `enabled: false`"), "{out}");
        assert!(out.contains("# [[steps]]"), "{out}");
        let ids: Vec<String> =
            datalib_dag::config::parse(&out, std::path::Path::new("config.toml"))
                .unwrap()
                .steps
                .into_iter()
                .map(|s| s.id)
                .collect();
        assert_eq!(
            ids,
            ["grid_index", "qmd_index", "slack.download", "slack.render"]
        );
    }

    /// A pre-TOML config already in the steps format converts by
    /// reserialization — including expanding a YAML anchor into the two
    /// copies TOML needs.
    #[test]
    fn yaml_steps_config_converts_to_toml() {
        let out = yaml_steps_to_toml(
            "steps:\n  - id: slack.download\n    command: datalib-step download slack_api\n\
             \n    outputs: [slack/raw]\n    params: &p\n      sync: {channels: [chat-qi]}\n\
             \n  - id: slack.render\n    command: datalib-step render slack_api\n\
             \n    inputs: [slack/raw]\n    outputs: [slack/rendered_md]\n    params: *p\n",
            std::path::Path::new("config.yaml"),
        )
        .unwrap();
        let sources = assert_valid_toml_config(&out);
        assert_eq!(sources, ["slack.download"]);
        assert_eq!(out.matches("channels = [\"chat-qi\"]").count(), 2, "{out}");
    }

    /// The format field, not the file already on disk, decides which
    /// parser validates a save — otherwise accepting a conversion
    /// (TOML text, root still holding config.yaml) would be rejected.
    #[test]
    fn saves_are_validated_in_their_own_format() {
        let toml = "[[steps]]\nid = \"x\"\ncommand = \"s\"\noutputs = [\"x/raw\"]\n";
        let yaml = "steps:\n  - id: x\n    command: s\n    outputs: [x/raw]\n";
        assert_eq!(
            validate_config_text(toml, std::path::Path::new("config.toml")).unwrap(),
            ["x"]
        );
        assert_eq!(
            validate_config_text(yaml, std::path::Path::new("config.yaml")).unwrap(),
            ["x"]
        );
        // Each is nonsense to the other's parser.
        assert!(validate_config_text(yaml, std::path::Path::new("config.toml")).is_err());
        assert!(validate_config_text(toml, std::path::Path::new("config.yaml")).is_err());
    }
}
