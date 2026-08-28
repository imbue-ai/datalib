//! `datalib-applet unified_index` — the grid index and the qmd index,
//! served over HTTP.
//!
//! This is the one part of the old `datalib-http` that had a reason to
//! leave: no core feature reads it, one program (`datalib-step`) already
//! produces it, and it is the only surface whose data lives in a tree
//! nothing else touches (`<root>/unified_index/`).
//!
//! ## Why grid and qmd are one applet
//!
//! A free-text search needs both in one request: qmd returns hits, the
//! grid resolves them to rows (`grid_row_refs`, then `search_by_uuids`
//! preserving rank order). Splitting them into two applets would put a
//! proxy hop in the middle of every query for no gain — they are
//! produced together, read together, and versioned together.
//!
//! ## Endpoints
//!
//! Reached through the gateway at `/applet/unified_index/…`, which is
//! also what the UI calls. There is no `/api/*` alias: `datalib-http`
//! does not know these routes exist, which is the whole point of the
//! move.
//!
//! ```text
//! /search?q=&limit=     the grid
//! /columns              its column set
//! /docs                 rendered documents, for the picker card
//! /chat/{uuid}          one document: header from the index, body from disk
//! /asset/{uuid}/{rel}   a file sitting next to that document
//! ```
//!
//! ## Why it contributes no components
//!
//! The gallery's grid and document views are builtins in the app
//! bundle, not components from the frontend store, so this applet has
//! nothing to write into `--frontend-dir` and never receives one. It is
//! the case the applet contract already allowed for and had no instance
//! of: a server that contributes endpoints and no UI.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, Response, StatusCode},
    response::Json,
    routing::get,
    Router,
};
use datalib_unified_index::qmd::{
    GridIndex, QmdDaemon, QmdDaemonConfig, QmdRunner, QmdRunnerConfig, QueryMode,
};
use datalib_unified_index::query::{parse_query, FreeTextMode, ParsedQuery};
use datalib_unified_index::repo::{DocRow, DynIndexRepo, EdgeRowOut};
use datalib_unified_index::search::SearchRow;
use serde::{Deserialize, Serialize};

/// Everything the handlers need, cloned per request.
#[derive(Clone)]
struct Index {
    /// The data root, for resolving a document's on-disk neighbours.
    root: Arc<PathBuf>,
    /// The grid index. Read-only: the `grid_index` step is its only
    /// writer, so holding it open across a sync is safe.
    repo: DynIndexRepo,
    /// Long-lived `qmd mcp` child for sub-second searches. Resolves its
    /// index lazily per query, so a root with no index yet (or one being
    /// rebuilt mid-sync) degrades to the SQL fallback and upgrades again
    /// with no restart.
    qmd: Arc<QmdDaemon>,
}

/// Serve until killed. The gateway supervises the process; there is
/// nothing to write before binding, so this binds immediately and
/// announces straight away.
pub fn serve(port: u16, params: &serde_json::Value) -> Result<()> {
    let root = match params.get("data_root").and_then(|v| v.as_str()) {
        Some(p) => PathBuf::from(p),
        // The gateway sets the step protocol's data-root variable and
        // runs us with the data root as cwd; `params.data_root` is the
        // override for running this by hand.
        None => std::env::var_os("DATALIB_DAG_DATA_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(".")),
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(async move {
        let root = Arc::new(root);
        // Warm the shared model cache before the first search rather
        // than during it. This used to run in `datalib-http`'s main,
        // which is the last place that still knew what qmd was.
        ensure_models(&root);
        let repo = datalib_unified_index::dolt_repo::DoltRepo::open(root.clone())
            .await
            .with_context(|| format!("open the grid index under {}", root.display()))?;
        let state = Index {
            qmd: Arc::new(QmdDaemon::new(QmdDaemonConfig::new((*root).clone()))),
            repo: Arc::new(repo),
            root,
        };
        let app = Router::new()
            .route("/search", get(search_handler))
            .route("/columns", get(columns))
            .route("/docs", get(list_docs))
            .route("/chat/{markdown_uuid}", get(chat))
            .route("/asset/{markdown_uuid}/{*rel}", get(asset))
            .route(
                "/health",
                get(|| async { Json(serde_json::json!({"ok": true})) }),
            )
            .with_state(state);
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        // `port` may be 0 ("any"), so the bound one is the listener's.
        let bound = listener.local_addr().context("read the bound address")?;
        eprintln!("datalib-applet unified_index: listening on {bound}");
        // There was nothing to write first, so binding is all this one
        // owes before the gateway may look.
        crate::announce_port(bound.port());
        axum::serve(listener, app).await.context("serve")
    })
}

/// Point this data root's qmd dir at the shared model cache.
///
/// Models live once in `~/.cache/qmd/models` (~2 GB) and every data root
/// reaches them through a symlink, so qmd — run with
/// `XDG_CACHE_HOME=<root>/unified_index` — resolves out to that one copy
/// instead of downloading its own. Best-effort: a pre-existing real
/// directory is left alone, and a cold cache pays a one-time download on
/// the first semantic search rather than blocking startup.
fn ensure_models(root: &std::path::Path) {
    // No index yet means no sync has run, so there is nothing to point
    // at the cache and no search to warm. The first sync creates the
    // directory and the indexer links it; this call is the belt for a
    // root the indexer has not touched in this incarnation.
    if !datalib_unified_index::qmd::qmd_index_path(root).exists() {
        eprintln!(
            "datalib-applet unified_index: no qmd index yet — free-text \
             search falls back to SQL until the first sync builds one"
        );
        return;
    }
    let qmd_dir = datalib_core::layout::qmd_dir(root);
    let models_dir = datalib_qmd_indexer::default_models_dir();
    if let Err(e) = std::fs::create_dir_all(&models_dir)
        .map_err(anyhow::Error::from)
        .and_then(|()| datalib_qmd_indexer::ensure_models_symlink(&qmd_dir, &models_dir))
    {
        eprintln!(
            "datalib-applet unified_index: could not ensure the models symlink ({e:#}); \
             continuing with {}/models as-is",
            qmd_dir.display()
        );
    } else if !datalib_qmd_indexer::models_present(&qmd_dir.join("models")) {
        eprintln!(
            "datalib-applet unified_index: model cache cold — the first \
             semantic search will download models (one-time, shared \
             across data roots)"
        );
    }
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

/// Response shape for `/applet/unified_index/chat/{markdown_uuid}`. The body is the raw
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

async fn search_handler(
    State(s): State<Index>,
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
        match run_qmd_search(&s.root, &s.repo, &s.qmd, &parsed, limit).await {
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
    repo: &DynIndexRepo,
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
async fn list_docs(State(s): State<Index>) -> Result<Json<Vec<DocRow>>, StatusCode> {
    match s.repo.list_docs(500).await {
        Ok(rows) => Ok(Json(rows)),
        Err(e) => {
            eprintln!("list_docs: {e}");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

async fn chat(
    State(s): State<Index>,
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
/// `/applet/unified_index/asset/{markdown_uuid}/blobs/foo.png` once the UI rewrites them;
/// this handler resolves them by looking up the markdown's on-disk path
/// and joining `rel` against its parent directory.
///
/// Path-traversal guard: canonicalize both the parent dir and the target,
/// reject the request if the target escapes the parent.
async fn asset(
    State(s): State<Index>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The grid's column set is part of this applet's wire contract:
    /// the UI renders whatever `/columns` lists, so a column silently
    /// disappearing is a blank column in the app rather than an error.
    #[test]
    fn default_columns_listed() {
        assert_eq!(default_columns().len(), 11);
    }

    /// Frontmatter trimming is text handling, not parsing — a body
    /// without it is returned unchanged rather than treated as broken.
    #[test]
    fn strips_only_a_leading_frontmatter_block() {
        assert_eq!(strip_frontmatter("---\ntitle: x\n---\nbody\n"), "body\n");
        assert_eq!(strip_frontmatter("body only\n"), "body only\n");
        assert_eq!(
            strip_frontmatter("---\nunterminated\n"),
            "---\nunterminated\n"
        );
    }
}
