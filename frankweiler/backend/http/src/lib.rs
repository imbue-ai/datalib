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
    body::Body,
    extract::{Path, Query, State},
    http::{header, Response, StatusCode},
    response::sse::{Event, KeepAlive, Sse},
    response::Json,
    routing::{get, post},
    Router,
};
use frankweiler_core::qmd::{GridIndex, QmdDaemon, QmdRunner, QmdRunnerConfig, QueryMode};
use frankweiler_core::query::{parse_query, FreeTextMode, ParsedQuery};
use frankweiler_core::repo::{DynRepo, EdgeRowOut, RepoError};
use frankweiler_core::search::SearchRow;
use frankweiler_core::version::git_hash;
use frankweiler_schema::feedback::FeedbackRow;
use frankweiler_schema::sync_jobs::SyncJobRow;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

mod embed;
pub mod worker;

#[derive(Clone)]
pub struct AppState {
    /// Data root on disk — drives the static `/api/media/*` mount and
    /// the `accounts.json` lookup. The SQL store is reached through
    /// [`AppState::repo`].
    pub root: Arc<PathBuf>,
    /// Self-contained config path for this data root (`<root>/config.yaml`).
    /// The config + setup endpoints read and write it, and the sync
    /// worker drives `frankweiler-sync --config <this>`. Keeping the
    /// config inside the root is what lets the app bootstrap from an
    /// empty directory with no external `~/.config` file.
    pub config_path: Arc<PathBuf>,
    /// All SQL flows through this seam.
    /// [`frankweiler_core::dolt_repo::DoltRepo`] against a single
    /// doltlite file is the only impl today.
    pub repo: DynRepo,
    /// Long-lived `qmd mcp` child for sub-second searches. `None` when
    /// no qmd index is materialized at startup (or its spawn check
    /// failed) — `run_qmd_search` then falls back to the per-call
    /// `npx … query` shell-out path so search still works.
    pub qmd_daemon: Option<Arc<QmdDaemon>>,
    /// Fan-out channel for live sync-job progress. The worker (and the
    /// enqueue/cancel handlers) publish [`worker::ProgressEvent`]s here;
    /// `GET /api/sync/stream` subscribes and pushes them to the UI over
    /// SSE, so progress is realtime push, not poll.
    pub progress_tx: worker::ProgressTx,
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
    // Slack image attachments are symlinked into `<root>/media/slack/<file_id>/`
    // by ingest; serve them verbatim so QMD-embedded `![](...)` URLs resolve.
    let media_dir = state.root.join("media");
    Router::new()
        .route("/api/health", get(health))
        .route("/api/search", get(search_handler))
        .route("/api/columns", get(columns))
        .route("/api/accounts", get(accounts))
        .route("/api/chat/{markdown_uuid}", get(chat))
        .route("/api/asset/{markdown_uuid}/{*rel}", get(asset))
        .route("/api/feedback", post(submit_feedback))
        .route("/api/card", post(create_card))
        .route("/api/card/{hash}", get(get_card))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/config/scaffold", get(config_scaffold))
        .route("/api/lib", get(list_lib))
        .route("/api/lib/{name}", get(get_lib).put(put_lib))
        .route("/agent.md", get(agent_guide))
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
        match run_qmd_search(&s.root, &s.repo, s.qmd_daemon.as_ref(), &parsed, limit).await {
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
    let created_at = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
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

/// Content-addressed JS store under `<root>/.frankweiler/cards/<hash>.js`.
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
    let dir = s.root.join(".frankweiler/cards");
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
    let path = s.root.join(".frankweiler/cards").join(format!("{hash}.js"));
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
// Stored one-file-per-name under `<root>/.frankweiler/lib/<name>.js`.
// The name doubles as a JS identifier injected into card scope, so it
// is constrained to a valid bare identifier (see `valid_lib_name`),
// which also makes it path-safe (no `/`, `.`, traversal).

#[derive(Debug, Deserialize)]
pub struct PutLibRequest {
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct LibEntry {
    pub name: String,
    /// sha256 of the source — the UI watches this to decide when a card
    /// that depends on this alias needs re-rendering.
    pub hash: String,
}

/// A lib name is injected into card scope as a bare identifier and
/// invoked as `name()`, so it must be a valid ASCII JS identifier. That
/// also makes it path-safe: no `/`, `.`, or `..`, so it can't traverse
/// out of the lib directory.
fn valid_lib_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first_ok = matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_' || c == '$');
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

/// List every named component with its content hash.
async fn list_lib(State(s): State<AppState>) -> Result<Json<Vec<LibEntry>>, StatusCode> {
    let dir = s.root.join(".frankweiler/lib");
    let mut out = Vec::new();
    match std::fs::read_dir(&dir) {
        Ok(rd) => {
            for ent in rd.flatten() {
                let path = ent.path();
                if path.extension().and_then(|e| e.to_str()) != Some("js") {
                    continue;
                }
                let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                    continue;
                };
                if !valid_lib_name(stem) {
                    continue;
                }
                if let Ok(src) = std::fs::read_to_string(&path) {
                    out.push(LibEntry {
                        name: stem.to_string(),
                        hash: sha256_hex(src.as_bytes()),
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
    let path = s.root.join(".frankweiler/lib").join(format!("{name}.js"));
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
    if !valid_lib_name(&name) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let dir = s.root.join(".frankweiler/lib");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("put_lib: mkdir {}: {e}", dir.display());
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    let path = dir.join(format!("{name}.js"));
    if let Err(e) = std::fs::write(&path, req.source.as_bytes()) {
        eprintln!("put_lib: write {}: {e}", path.display());
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    Ok(Json(LibEntry {
        name,
        hash: sha256_hex(req.source.as_bytes()),
    }))
}

/// Onboarding doc for a coding agent pointed at this instance. Served as
/// markdown at a stable, app-relative URL so a wayfinder snippet can
/// reference `<origin>/agent.md` without baking the content into the
/// wayfinder itself.
async fn agent_guide() -> (StatusCode, [(axum::http::HeaderName, &'static str); 1], &'static str) {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            "text/markdown; charset=utf-8",
        )],
        include_str!("agent_guide.md"),
    )
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

// --- Config / setup --------------------------------------------------------
//
// These three endpoints make the data root self-contained: the app reads
// and writes its own `<root>/config.yaml` instead of relying on a
// separate `~/.config/frankweiler/config.yaml`. An empty data root opens
// with no config; the UI's Setup tab scaffolds one, lets the user edit
// it, and saves it back here, after which `/api/sync/*` lights up.

/// Load the effective config for read purposes: prefer the data root's
/// own `config.yaml`; fall back to the legacy global path only when the
/// root has none yet (eases migration for existing installs).
fn load_effective_config(
    s: &AppState,
) -> Result<frankweiler_core::config::Config, frankweiler_core::config::ConfigError> {
    use frankweiler_core::config::{default_config_path, load_config};
    if s.config_path.exists() {
        load_config(Some(s.config_path.as_path()))
    } else {
        load_config(Some(&default_config_path()))
    }
}

#[derive(Debug, Serialize)]
pub struct ConfigResponse {
    /// Absolute path of `<root>/config.yaml` — shown in the UI so the
    /// user knows exactly which file they're editing.
    pub path: String,
    /// Whether that file exists yet. `false` on a fresh data root.
    pub exists: bool,
    /// Raw YAML text (empty string when the file doesn't exist).
    pub yaml: String,
    /// Whether the current bytes parse + validate as a `Config`.
    pub parsed_ok: bool,
    /// Loader error message when `parsed_ok` is false.
    pub error: Option<String>,
    /// Number of configured sources (0 when invalid/missing).
    pub source_count: usize,
}

/// `GET /api/config` — current `<root>/config.yaml` plus a parse check.
async fn get_config(State(s): State<AppState>) -> Json<ConfigResponse> {
    let path = s.config_path.as_ref().clone();
    let exists = path.exists();
    let yaml = std::fs::read_to_string(&path).unwrap_or_default();
    let (parsed_ok, error, source_count) = match frankweiler_core::config::load_config(Some(&path))
    {
        Ok(c) => (true, None, c.sources.len()),
        Err(e) => (false, Some(format!("{e}")), 0),
    };
    Json(ConfigResponse {
        path: path.display().to_string(),
        exists,
        yaml,
        parsed_ok,
        error,
        source_count,
    })
}

#[derive(Debug, Deserialize)]
pub struct PutConfigRequest {
    pub yaml: String,
}

#[derive(Debug, Serialize)]
pub struct PutConfigResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub source_count: usize,
}

/// `PUT /api/config` — validate then atomically write `<root>/config.yaml`.
///
/// We validate by writing to a sibling `.tmp` file and running the real
/// `load_config` (so date-format / Notion / Yolink invariants are caught,
/// not just YAML syntax), then `rename` into place only on success. A
/// rejected config never clobbers the existing one. Validation failures
/// return `200 {ok:false, error}` (the UI shows it inline); only genuine
/// I/O failures are 5xx.
async fn put_config(
    State(s): State<AppState>,
    Json(req): Json<PutConfigRequest>,
) -> Result<Json<PutConfigResponse>, StatusCode> {
    let path = s.config_path.as_ref().clone();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            eprintln!("put_config: mkdir {}: {e}", parent.display());
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    }
    let tmp = path.with_extension("yaml.tmp");
    if let Err(e) = std::fs::write(&tmp, req.yaml.as_bytes()) {
        eprintln!("put_config: write {}: {e}", tmp.display());
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    match frankweiler_core::config::load_config(Some(&tmp)) {
        Ok(cfg) => {
            let n = cfg.sources.len();
            if let Err(e) = std::fs::rename(&tmp, &path) {
                let _ = std::fs::remove_file(&tmp);
                eprintln!("put_config: rename {}: {e}", path.display());
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
            Ok(Json(PutConfigResponse {
                ok: true,
                error: None,
                source_count: n,
            }))
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Ok(Json(PutConfigResponse {
                ok: false,
                error: Some(format!("{e}")),
                source_count: 0,
            }))
        }
    }
}

/// `GET /api/config/scaffold` — a starter `config.yaml` for this data
/// root: `data_root` is pre-filled with the real path and a couple of
/// commented example sources show the shape. The UI drops this into the
/// editor when the root has no config yet.
async fn config_scaffold(State(s): State<AppState>) -> Json<ConfigResponse> {
    let root = s.root.display().to_string();
    let yaml = scaffold_yaml(&root);
    Json(ConfigResponse {
        path: s.config_path.display().to_string(),
        exists: s.config_path.exists(),
        yaml,
        parsed_ok: true,
        error: None,
        source_count: 0,
    })
}

/// Minimal-but-valid starter config. `sources: []` is accepted by the
/// loader, so the scaffold parses as-is; the commented blocks are ready
/// to uncomment. Credentials never live here — downloaders pull them
/// from `latchkey` at runtime.
fn scaffold_yaml(data_root: &str) -> String {
    format!(
        r#"# Frankweiler config for this data root. Edit here, Save, then use
# the Sync tab to pull your data in. Credentials are NOT stored in this
# file — each downloader reads them from `latchkey` at runtime, so run
# `latchkey auth set <provider>` first for any managed source.

data_root: {data_root}

sources: []

# Uncomment and adapt the sources you want. Each needs a unique `name`.
#
#  - name: claude
#    type: claude_api
#    sync: {{}}            # incremental pull of your Claude conversations
#
#  - name: chatgpt
#    type: chatgpt_api
#    sync: {{}}
#
#  - name: slack
#    type: slack_api
#    sync:
#      media: true
#      channels: ["general"]   # omit / use all_channels: true for everything
#
#  - name: github
#    type: github_api
#    sync: {{}}
#
#  - name: fastmail
#    type: email
#    sync:
#      hostname: api.fastmail.com
#
#  - name: contacts          # translate-only: no `sync:` block
#    type: carddav
#    input_path: ~/Downloads/contacts.vcf
"#
    )
}

/// Surface the typed `Config.sources` list to the UI as
/// `{name, type, managed}` entries. We re-load the config on every call
/// so a user editing the YAML doesn't have to restart the backend.
/// Returns an empty list (rather than 500) when the file is missing or
/// fails to parse, mirroring the previous behavior.
async fn sync_sources(State(s): State<AppState>) -> Json<Vec<SourceInfo>> {
    // Read the data root's own `config.yaml` (self-contained), falling
    // back to the legacy `~/.config/frankweiler/config.yaml` only when
    // the root has no config yet. Re-loaded per call so a config edit in
    // the Setup tab shows up without a backend restart.
    let cfg = match load_effective_config(&s) {
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
    });
    Ok(StatusCode::NO_CONTENT)
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
        assert_eq!(default_columns().len(), 11);
    }
}
