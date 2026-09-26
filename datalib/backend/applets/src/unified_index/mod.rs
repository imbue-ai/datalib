//! `datalib-applet unified_index` — the grid index, the qmd index and
//! the embedding map, served over HTTP.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{header, Response, StatusCode},
    middleware::{self, Next},
    response::Json,
    routing::{get, post},
    Router,
};
mod columns;
mod map;
mod problems;
#[cfg(test)]
mod qmd_search_tests;
mod results;
#[cfg(test)]
mod serve_tests;

use datalib_columns::Identity;
use datalib_unified_index::db::datalib_source_id;
use datalib_unified_index::qmd::index_state::{resolve_markdown_states, DocReport, SummaryCache};
use datalib_unified_index::qmd::{
    display_snippet, CollectionScope, GridIndex, QmdDaemon, QmdDaemonConfig, QmdIndexReader,
    QmdIndexSummary, QueryMode,
};
use datalib_unified_index::query::{parse_query, Field, FreeTextMode, ParsedQuery};
use datalib_unified_index::repo::{DocRow, DynIndexRepo, EdgeRowOut};
use datalib_unified_index::search::SearchRow;
use datalib_unified_index::sort::Sort;
use serde::{Deserialize, Serialize};

/// The step protocol's data-root variable, which the gateway sets for every
/// applet it starts.
const DATA_ROOT_ENV: &str = "DATALIB_DAG_DATA_ROOT";

/// Everything the handlers need, cloned per request.
#[derive(Clone)]
struct Index {
    /// The data root, for resolving a document's on-disk neighbours.
    root: Arc<PathBuf>,
    /// The grid index. Read-only: the `grid_index` step is its only
    /// writer, so holding it open across a sync is safe.
    repo: DynIndexRepo,
    /// Long-lived `qmd mcp` child for sub-second searches. Resolves its
    /// index per query, so a root with no index yet answers free text with
    /// an error, and searches once the first sync builds one, with no
    /// restart.
    qmd: Arc<QmdDaemon>,
    qmd_summary: Arc<SummaryCache>,
    results: Arc<results::ResultCache>,
}

pub fn serve(port: u16) -> Result<()> {
    let root = std::env::var_os(DATA_ROOT_ENV)
        .map(PathBuf::from)
        .with_context(|| {
            format!(
                "{DATA_ROOT_ENV} is not set. The gateway sets it to the data root; \
                 to run this by hand, set it yourself"
            )
        })?;
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
            qmd_summary: Arc::new(SummaryCache::default()),
            results: Arc::new(results::ResultCache::default()),
            repo: Arc::new(repo),
            root,
        };
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        // `port` may be 0 ("any"), so the bound one is the listener's.
        let bound = listener.local_addr().context("read the bound address")?;
        // Outermost, so every route is behind it, `/health` included.
        let gate = Arc::new(crate::gate::Gate::from_env(bound.port())?);
        let app = Router::new()
            .route("/search", get(search_handler))
            .route("/qmd_state", post(qmd_state))
            .route("/docs", get(list_docs))
            .route("/embedding_map", get(map::handler))
            .route("/embedding_map/matches", get(map::matches_handler))
            .route("/problems", get(problems::handler))
            .route("/chat/{markdown_uuid}", get(chat))
            .route("/asset/{markdown_uuid}/{*rel}", get(asset))
            .route(
                "/health",
                get(|| async { Json(serde_json::json!({"ok": true})) }),
            )
            .with_state(state)
            .layer(middleware::from_fn_with_state(gate, require_gateway));
        eprintln!("datalib-applet unified_index: listening on {bound}");
        // There was nothing to write first, so binding is all this one
        // owes before the gateway may look.
        crate::announce_port(bound.port());
        axum::serve(listener, app).await.context("serve")
    })
}

async fn require_gateway(
    State(gate): State<Arc<crate::gate::Gate>>,
    req: axum::extract::Request,
    next: Next,
) -> Response<Body> {
    // Decided in its own block: a borrow of `req` alive across the
    // `.await` below would make this future `!Send`.
    let admitted = {
        let header = |name: &str| req.headers().get(name).and_then(|v| v.to_str().ok());
        gate.admits(header("host"), header(crate::gate::SECRET_HEADER))
    };
    if admitted {
        return next.run(req).await;
    }
    Response::builder()
        .status(StatusCode::UNAUTHORIZED)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"error":"this port answers only to the datalib gateway"}"#,
        ))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

fn ensure_models(root: &std::path::Path) {
    // No index yet means no sync has run, so there is nothing to point
    // at the cache and no search to warm. The first sync creates the
    // directory and the indexer links it; this call is the belt for a
    // root the indexer has not touched in this incarnation.
    if !datalib_unified_index::qmd::qmd_index_path(root).exists() {
        eprintln!(
            "datalib-applet unified_index: no qmd index yet — free-text \
             search answers with an error until the first sync builds one"
        );
        return;
    }
    let qmd_dir = datalib_unified_index::qmd::qmd_state_dir(root);
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
        return;
    }
    // Verify (and on a cold cache, fetch) the pinned models off the
    // request path: a 2 GB download must not hold up the SQL side of
    // search, and qmd would otherwise pull unpinned copies itself on
    // the first semantic query.
    let effective = datalib_qmd_models::effective_models_dir(&qmd_dir, &models_dir);
    std::thread::spawn(move || {
        let models = datalib_qmd_models::PINNED_MODELS;
        match datalib_qmd_models::ensure_models(
            &effective,
            models,
            datalib_qmd_models::Fetch::from_env(),
        ) {
            Ok(outcomes) => {
                let missing = datalib_qmd_models::missing(models, &outcomes);
                if !missing.is_empty() {
                    eprintln!(
                        "datalib-applet unified_index: not fetching {} into {} \
                         ({} is set); whatever needs them will fail",
                        missing.join(", "),
                        effective.display(),
                        datalib_qmd_models::NO_FETCH_ENV
                    );
                }
            }
            Err(e) => eprintln!(
                "datalib-applet unified_index: could not provision qmd's models in {} ({e:#}); \
                 semantic search will fail until `datalib-step pull-models` succeeds",
                effective.display()
            ),
        }
    });
}

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub limit: Option<usize>,
    /// Where this page starts in the search's rows: the `next_offset` a
    /// previous page answered with. None is the first page.
    pub offset: Option<usize>,
    /// A grid column and direction, `created_at:desc` (see
    /// `datalib_unified_index::sort`). None is newest first, or qmd's rank
    /// for free text.
    pub sort: Option<String>,
    /// A row's uuid the page must reach, however far past `offset` it is:
    /// see `results::reaching`.
    pub through: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query_echo: serde_json::Value,
    /// The columns the rows carry, typed — see `datalib_columns`.
    pub columns: Vec<datalib_columns::ColumnSpec>,
    pub rows: Vec<SearchRow>,
    /// Every row the search holds, not just this page's.
    pub total: u64,
    /// The offset of the next page, or None when this one reaches the end.
    pub next_offset: Option<usize>,
    /// The commit the search was read at. A later page answered at another
    /// one means the index moved in between, and what the client holds
    /// should be read again.
    pub at: Option<String>,
    /// Backend-side errors the user should know about even though we
    /// returned 200, where a swallowed error would otherwise leave the
    /// UI staring at an empty grid with no signal. The UI surfaces
    /// these as toasts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
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
    /// The configured source this document came from, as the grid's
    /// Source column shows it. `None` when no grid row points at it.
    pub source_ref: Option<Identity>,
    pub body: String,
    /// What render could not fully do to this document, errors first.
    /// Drawn above the body.
    pub problems: Vec<problems::DocProblem>,
    /// Anything the applet could not read while answering; the document
    /// still opens.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
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
    let q = p.q.unwrap_or_default();
    let parsed = parse_query(&q);
    let limit = p.limit.unwrap_or(200).min(results::MAX_PAGE);
    let mut errors: Vec<String> = Vec::new();
    let sort = p.sort.as_deref().and_then(|spelled| {
        let sort = Sort::parse(spelled);
        if sort.is_none() {
            errors.push(format!(
                "unknown sort {spelled:?}; showing the default order"
            ));
        }
        sort
    });
    // Structured terms alone are a SQL filter; free text is qmd's, with the
    // structured terms applied to its hits. A qmd failure is the answer —
    // `query_echo.qmd_error`, no rows — not a quieter search in its place.
    let mut qmd_error: Option<String> = None;
    let offset = p.offset.unwrap_or(0);
    let through = p.through.as_deref();
    let page = match search_page(&s, &q, &parsed, sort, offset, limit, through).await {
        Ok(page) => page,
        Err(SearchFailure::Qmd(e)) => {
            qmd_error = Some(e);
            Page::default()
        }
        Err(SearchFailure::Index(e)) => {
            let msg = format!("structured search: {e}");
            eprintln!("search: {msg}");
            errors.push(msg);
            Page::default()
        }
    };
    let mut rows = page.rows;
    // The names and marks the config gives each source, read per
    // request: a rename lands on the next search, with no re-index.
    let sources = columns::Sources::read(&s.root);
    for row in &mut rows {
        sources.resolve(row);
    }
    Json(SearchResponse {
        columns: columns::columns(),
        query_echo: serde_json::json!({
            "free_text": parsed.free_text,
            "free_text_mode": match parsed.free_text_mode {
                FreeTextMode::Hybrid => "hybrid",
                FreeTextMode::Vsearch => "vsearch",
            },
            "documents": parsed.documents,
            "filters": parsed.filters.iter()
                .map(|(k, v)| (format!("{:?}", k), v.clone()))
                .collect::<Vec<_>>(),
            "qmd_error": qmd_error,
        }),
        rows,
        total: page.total as u64,
        next_offset: page.next_offset,
        at: page.at,
        errors,
    })
}

enum SearchFailure {
    Qmd(String),
    Index(String),
}

fn index(e: impl std::fmt::Display) -> SearchFailure {
    SearchFailure::Index(e.to_string())
}

#[derive(Default)]
struct Page {
    rows: Vec<SearchRow>,
    total: usize,
    next_offset: Option<usize>,
    at: Option<String>,
}

/// One page of the search, with qmd's score and matched words on each
/// row it ranked.
async fn search_page(
    s: &Index,
    q: &str,
    parsed: &ParsedQuery,
    sort: Option<Sort>,
    offset: usize,
    limit: usize,
    through: Option<&str>,
) -> Result<Page, SearchFailure> {
    let (list, at) = search_results(s, q, parsed, sort).await?;
    let limit = results::reaching(&list, offset, limit, through);
    let (entries, next_offset) = results::page(&list, offset, limit);
    let uuids: Vec<String> = entries.iter().map(|e| e.uuid.clone()).collect();
    let mut rows = s.repo.rows_by_uuids(&uuids).await.map_err(index)?;
    let hits: std::collections::HashMap<&str, &(f64, String)> = entries
        .iter()
        .filter_map(|e| Some((e.uuid.as_str(), e.hit.as_ref()?)))
        .collect();
    for row in &mut rows {
        if let Some(hit) = hits.get(row.uuid.as_str()) {
            show_hit(row, hit);
        }
    }
    Ok(Page {
        rows,
        total: list.len(),
        next_offset,
        at,
    })
}

/// qmd's score, and the words the hit matched in place of the row's
/// opening words. A hit that showed nothing readable keeps the preview.
fn show_hit(row: &mut SearchRow, (score, context): &(f64, String)) {
    row.score = Some(*score);
    if !context.is_empty() {
        row.snippet = context.clone();
    }
}

/// The search's rows in order, from the cache when this query and sort
/// were listed at the index's current commit, and that commit.
async fn search_results(
    s: &Index,
    q: &str,
    parsed: &ParsedQuery,
    sort: Option<Sort>,
) -> Result<(Arc<Vec<results::Entry>>, Option<String>), SearchFailure> {
    let key = results::Key {
        q: q.to_string(),
        sort,
        at: s.repo.head().await.map_err(index)?,
    };
    if let Some(list) = s.results.get(&key) {
        return Ok((list, key.at));
    }
    let (list, at) = if parsed.free_text.is_empty() {
        let listing = s.repo.ordered_uuids(parsed, sort).await.map_err(index)?;
        let list: Vec<results::Entry> = listing
            .uuids
            .into_iter()
            .map(|uuid| results::Entry { uuid, hit: None })
            .collect();
        (list, listing.at)
    } else {
        let ranking = qmd_ranking(&s.root, &s.repo, &s.qmd, parsed, QMD_DEPTH)
            .await
            .map_err(|e| SearchFailure::Qmd(format!("{e:#}")))?;
        let uuids: Vec<String> = ranking.iter().map(|(uuid, _)| uuid.clone()).collect();
        let listing = s
            .repo
            .filter_uuids(parsed, &uuids, sort)
            .await
            .map_err(index)?;
        let mut hit_of: std::collections::HashMap<String, (f64, String)> =
            ranking.into_iter().collect();
        let list: Vec<results::Entry> = listing
            .uuids
            .into_iter()
            .map(|uuid| results::Entry {
                hit: hit_of.remove(&uuid),
                uuid,
            })
            .collect();
        (list, listing.at)
    };
    let list = Arc::new(list);
    // Filed under the commit it was actually read at, which is the key's
    // unless a seal landed between the two reads.
    s.results.put(
        results::Key {
            at: at.clone(),
            ..key
        },
        list.clone(),
    );
    Ok((list, at))
}

/// How many hits qmd ranks for one free-text search: every page of it is
/// cut from these.
const QMD_DEPTH: usize = 1_000;

/// qmd's ranked answer to `parsed`'s free text, as grid rows: one row per
/// document, at its best-ranked hit, with the hit's score and the words it
/// matched. qmd runs on the long-lived daemon, on a blocking thread; a
/// failed search is the answer, and the daemon starts afresh for the next.
async fn qmd_ranking(
    root: &std::sync::Arc<PathBuf>,
    repo: &DynIndexRepo,
    daemon: &Arc<QmdDaemon>,
    parsed: &ParsedQuery,
    depth: usize,
) -> anyhow::Result<Vec<(String, (f64, String))>> {
    let parsed_for_qmd = parsed.clone();
    let daemon = daemon.clone();
    let scope = collection_scope(parsed);
    let hits = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let mode = match parsed_for_qmd.free_text_mode {
            FreeTextMode::Hybrid => QueryMode::Hybrid,
            FreeTextMode::Vsearch => QueryMode::Vsearch,
        };
        daemon.search(mode, &parsed_for_qmd.free_text, depth, &scope)
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
    // An `is:document` search wants the document a hit is in, not the
    // message it landed on — which the SQL filter would then drop.
    let ranked = idx.ranked_rows_one_per_doc(&hits, parsed.documents == Some(true), |h| {
        eprintln!(
            "ERROR search: qmd hit resolved to no grid rows: path={:?} score={}",
            h.path, h.score
        );
    });
    Ok(ranked
        .into_iter()
        .map(|(row, hit)| (row.uuid.clone(), (hit.score, display_snippet(&hit.snippet))))
        .collect())
}

/// The rows qmd ranks for `parsed` that its structured terms also match,
/// in rank order: what the embedding map lights up for free text.
async fn qmd_rows(
    root: &std::sync::Arc<PathBuf>,
    repo: &DynIndexRepo,
    daemon: &Arc<QmdDaemon>,
    parsed: &ParsedQuery,
    depth: usize,
) -> anyhow::Result<Vec<SearchRow>> {
    let ranking = qmd_ranking(root, repo, daemon, parsed, depth).await?;
    let uuids: Vec<String> = ranking.into_iter().map(|(uuid, _)| uuid).collect();
    let listing = repo
        .filter_uuids(parsed, &uuids, None)
        .await
        .map_err(|e| anyhow::anyhow!("filter the qmd hits: {e}"))?;
    repo.rows_by_uuids(&listing.uuids)
        .await
        .map_err(|e| anyhow::anyhow!("read the qmd hits: {e}"))
}

/// The qmd collections a parsed query may draw from.
///
/// There is one collection per group, named after it, and
/// `source_id:` names a group — so a scoped query becomes a scoped
/// retrieval instead of a filter over whatever the global top-N happened
/// to contain. That difference is the whole point: a source whose hits
/// never enter the global list cannot be recovered by filtering, so
/// `source_id:x <text>` used to come back empty while `x` matched
/// strongly on its own.
///
/// Only positive terms scope. A negated `-source_id:x` is "every
/// collection but this one", which needs a list this function has no way
/// to obtain; it stays unscoped and is left to the SQL filter, which is
/// no worse than before.
///
/// `source_id:datalib` names no collection: the storage rows are filed
/// under datalib but their markdown still sits in the measured source's
/// tree, so there is nothing called `datalib` to retrieve from. It is
/// dropped here and left to the SQL filter — scoping to a collection
/// that does not exist would come back empty.
fn collection_scope(parsed: &ParsedQuery) -> CollectionScope {
    let names: Vec<String> = parsed
        .terms
        .iter()
        .filter(|t| t.field == Field::SourceId && !t.negate)
        .filter(|t| t.value != datalib_source_id())
        .map(|t| t.value.clone())
        .collect();
    if names.is_empty() {
        CollectionScope::All
    } else {
        CollectionScope::Only(names)
    }
}

/// Cap on how many documents one `/qmd_state` call may ask about.
/// Each one costs a file read + a SHA-256, so this bounds the work a
/// single request can trigger. The grid's default result limit is 200;
/// a client wanting more chunks its requests.
const QMD_STATE_MAX_DOCS: usize = 2_000;

#[derive(Debug, Deserialize)]
pub struct QmdStateRequest {
    /// The markdowns to report on. Duplicates are fine — they collapse.
    pub markdown_uuids: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct QmdStateResponse {
    /// False when this data root has no `index.sqlite` yet — nothing
    /// has synced, so every document is legitimately un-indexed rather
    /// than unknown.
    pub index_present: bool,
    pub summary: QmdIndexSummary,
    /// markdown_uuid → state. Every requested uuid appears.
    pub docs: std::collections::HashMap<String, DocReport>,
    /// Same contract as `/search`: errors the user should see even
    /// though we answered 200.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

async fn qmd_state(
    State(s): State<Index>,
    Json(req): Json<QmdStateRequest>,
) -> Json<QmdStateResponse> {
    let mut errors: Vec<String> = Vec::new();

    // Dedupe first: many grid rows share one markdown, and the caller
    // is not required to have collapsed them.
    let mut uuids: Vec<String> = req.markdown_uuids;
    uuids.sort();
    uuids.dedup();
    if uuids.len() > QMD_STATE_MAX_DOCS {
        errors.push(format!(
            "qmd_state: asked about {} documents, answering the first {QMD_STATE_MAX_DOCS}",
            uuids.len()
        ));
        uuids.truncate(QMD_STATE_MAX_DOCS);
    }

    let reader = match QmdIndexReader::open(&s.root).await {
        Ok(r) => r,
        Err(e) => {
            errors.push(format!("qmd index: {e}"));
            None
        }
    };
    let Some(reader) = reader else {
        // No index (or it would not open). With no index at all, every
        // document is un-indexed — a fact, not a failure, and one a
        // data root reports until its first sync. If instead the open
        // *errored*, we know nothing, so say so.
        let known = errors.is_empty();
        let report = if known {
            DocReport {
                indexed: Some(false),
                embedded: Some(false),
                note: Some("no qmd index yet".to_string()),
            }
        } else {
            DocReport {
                indexed: None,
                embedded: None,
                note: Some("the qmd index could not be opened".to_string()),
            }
        };
        return Json(QmdStateResponse {
            index_present: false,
            summary: QmdIndexSummary::default(),
            docs: uuids.into_iter().map(|u| (u, report.clone())).collect(),
            errors,
        });
    };

    let summary = match s.qmd_summary.summary(&s.root, &reader).await {
        Ok(v) => v,
        Err(e) => {
            errors.push(format!("qmd summary: {e}"));
            QmdIndexSummary::default()
        }
    };

    let docs = match resolve_markdown_states(s.repo.as_ref(), &reader, &uuids).await {
        Ok(d) => d,
        Err(e) => {
            errors.push(e);
            // Answer every uuid anyway, as unknown — an empty `docs`
            // would read to the client as "asked about nothing".
            uuids
                .into_iter()
                .map(|u| {
                    (
                        u,
                        DocReport {
                            indexed: None,
                            embedded: None,
                            note: Some("index state could not be resolved".to_string()),
                        },
                    )
                })
                .collect()
        }
    };

    Json(QmdStateResponse {
        index_present: true,
        summary,
        docs,
        errors,
    })
}

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
    let source_ref = meta
        .source_id
        .as_deref()
        .map(|id| columns::Sources::read(&s.root).identity(id));
    let outgoing_edges = s
        .repo
        .outgoing_edges(&markdown_uuid)
        .await
        .unwrap_or_default();
    // What render could not fully do to this document, for the banner
    // above the body. A read that fails is said, not swallowed: the
    // document still opens, and the banner says the problems could not
    // be read rather than showing none.
    let (problems, errors) = match s.repo.document_problems(&markdown_uuid).await {
        Ok(mut rows) => {
            problems::sort_for_banner(&mut rows);
            (
                rows.into_iter().map(problems::DocProblem::of).collect(),
                Vec::new(),
            )
        }
        Err(e) => (
            Vec::new(),
            vec![format!("could not read this document's problems: {e}")],
        ),
    };
    Ok(Json(ChatResponse {
        markdown_uuid,
        name: meta.name,
        account: meta.account,
        project: meta.project,
        channel: meta.channel,
        created_at: meta.created_at,
        source_label: meta.source_label,
        source_url,
        source_ref,
        body,
        outgoing_edges,
        problems,
        errors,
    }))
}

/// Serve a file living next to (or under) a rendered markdown. Relative
/// `![](blobs/foo.png)` references in the markdown body become
/// `/applet/unified_index/asset/{markdown_uuid}/blobs/foo.png` once the UI rewrites them;
/// this handler resolves them by looking up the markdown's on-disk path
/// and joining `rel` against its parent directory.
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

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Commits one chat per `(uuid, created_at)` to the root's grid index,
    /// the way the `grid_index` step writes and seals it.
    async fn index_chats(root: &std::path::Path, chats: &[(&str, &str)]) {
        use datalib_etl_render::grid_index::{apply_one, open_index, RenderedMarkdown, WriteLock};
        use datalib_schema::grid_rows::GridRow;
        use datalib_schema::providers::Provider;

        let pool = open_index(&datalib_runtime::layout::grid_index_db(root))
            .await
            .unwrap();
        let lock = WriteLock::new(pool.clone());
        for (uuid, created_at) in chats {
            let row = GridRow::builder()
                .uuid(*uuid)
                .provider(Provider::Claude)
                .kind("Chat")
                .source_label("Claude")
                .is_document(true)
                .created_at(Some(created_at.to_string()))
                .conversation_uuid(*uuid)
                .entire_chat(format!("/chat/{uuid}"))
                .body("")
                .markdown_uuid(Some(uuid.to_string()))
                .build()
                .unwrap();
            let md = RenderedMarkdown {
                markdown_uuid: uuid.to_string(),
                source_id: "enterprise".into(),
                upstream_cursor: None,
                bucket_key: None,
                md_path: root.join(format!("enterprise/{uuid}.md")),
                render_version: 1,
                rows: vec![row],
                sections: Vec::new(),
                edges: Vec::new(),
                problems: Vec::new(),
            };
            apply_one(&lock, root, &md).await.unwrap();
        }
        datalib_etl::doltlite_raw::commit_run(&pool, "chats")
            .await
            .unwrap();
        pool.close().await;
    }

    pub(super) async fn index_over(root: &std::path::Path) -> Index {
        let root = Arc::new(root.to_path_buf());
        Index {
            repo: Arc::new(
                datalib_unified_index::dolt_repo::DoltRepo::open(root.clone())
                    .await
                    .unwrap(),
            ),
            qmd: Arc::new(QmdDaemon::new(QmdDaemonConfig::new((*root).clone()))),
            qmd_summary: Arc::new(SummaryCache::default()),
            results: Arc::new(results::ResultCache::default()),
            root,
        }
    }

    pub(super) async fn search(
        s: &Index,
        q: &str,
        offset: Option<usize>,
        limit: usize,
        sort: Option<&str>,
    ) -> SearchResponse {
        let params = SearchParams {
            q: Some(q.to_string()),
            limit: Some(limit),
            offset,
            sort: sort.map(String::from),
            through: None,
        };
        search_handler(State(s.clone()), Query(params)).await.0
    }

    pub(super) fn uuids(r: &SearchResponse) -> Vec<&str> {
        r.rows.iter().map(|row| row.uuid.as_str()).collect()
    }

    /// Following `next_offset` from the first page reads every row once,
    /// newest first, all at one commit; a seal in between is a new search,
    /// answered at the new commit, so the client can tell what it holds
    /// is stale.
    #[tokio::test]
    async fn pages_read_the_search_once_and_a_seal_starts_a_new_one() {
        let tmp = tempfile::tempdir().unwrap();
        index_chats(
            tmp.path(),
            &[
                ("c-1", "2026-01-01T09:00:00+00:00"),
                ("c-2", "2026-01-02T09:00:00+00:00"),
                ("c-3", "2026-01-03T09:00:00+00:00"),
                ("c-4", "2026-01-04T09:00:00+00:00"),
                ("c-5", "2026-01-05T09:00:00+00:00"),
            ],
        )
        .await;
        let s = index_over(tmp.path()).await;

        let first = search(&s, "", None, 2, None).await;
        assert_eq!(uuids(&first), ["c-5", "c-4"]);
        assert_eq!((first.total, first.next_offset), (5, Some(2)));
        let at = first
            .at
            .clone()
            .expect("a committed index answers with its commit");
        let key = results::Key {
            q: String::new(),
            sort: None,
            at: Some(at.clone()),
        };
        assert!(
            s.results.get(&key).is_some(),
            "the list is kept for the next page"
        );

        let second = search(&s, "", first.next_offset, 2, None).await;
        assert_eq!(uuids(&second), ["c-3", "c-2"]);
        let last = search(&s, "", second.next_offset, 2, None).await;
        assert_eq!(uuids(&last), ["c-1"]);
        assert_eq!(last.next_offset, None);
        assert_eq!([&second.at, &last.at], [&first.at, &first.at]);

        index_chats(tmp.path(), &[("c-6", "2026-01-06T09:00:00+00:00")]).await;
        let after = search(&s, "", Some(2), 2, None).await;
        assert_ne!(after.at.as_deref(), Some(at.as_str()));
        assert_eq!(after.total, 6);
        assert_eq!(uuids(&after), ["c-4", "c-3"]);
    }

    /// An index the search cannot read is said, as an error the grid
    /// shows, not answered as an empty result.
    #[tokio::test]
    async fn a_search_the_index_cannot_answer_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let pool = datalib_etl_render::grid_index::open_index(
            &datalib_runtime::layout::grid_index_db(tmp.path()),
        )
        .await
        .unwrap();
        for ddl in [
            "DROP TABLE grid_rows",
            "CREATE TABLE grid_rows (only_column TEXT)",
        ] {
            sqlx::query(ddl).execute(&pool).await.unwrap();
        }
        datalib_etl::doltlite_raw::commit_run(&pool, "a table the search cannot read")
            .await
            .unwrap();
        pool.close().await;
        let s = index_over(tmp.path()).await;

        let r = search(&s, "", None, 10, None).await;
        assert!(r.rows.is_empty());
        assert_eq!(r.total, 0);
        assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
        assert!(r.errors[0].contains("no such column"), "{:?}", r.errors);

        let params = map::MatchParams {
            q: Some("source_id:enterprise".to_string()),
        };
        let matched = map::matches_handler(State(s), Query(params)).await.0;
        assert!(matched.markdown_uuids.is_empty());
        assert_eq!(matched.errors.len(), 1, "{:?}", matched.errors);
    }

    fn row(snippet: &str) -> SearchRow {
        SearchRow {
            snippet: snippet.into(),
            ..SearchRow::default()
        }
    }

    /// A hit shows the words it matched; one that matched nothing readable
    /// (front matter, markup) keeps the row's own opening words rather
    /// than blanking the Contents cell.
    #[test]
    fn a_hit_shows_its_words_unless_it_has_none() {
        let mut matched = row("opening words");
        show_hit(&mut matched, &(0.9, "the words it matched".into()));
        assert_eq!(matched.score, Some(0.9));
        assert_eq!(matched.snippet, "the words it matched");

        let mut unreadable = row("opening words");
        show_hit(&mut unreadable, &(0.5, String::new()));
        assert_eq!(unreadable.score, Some(0.5));
        assert_eq!(unreadable.snippet, "opening words");
    }

    /// A sort the grid names reorders the whole search, not the page; one
    /// this build does not know says so and answers in the default order.
    #[tokio::test]
    async fn a_sort_orders_the_whole_search_and_an_unknown_one_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        index_chats(
            tmp.path(),
            &[
                ("c-1", "2026-01-01T09:00:00+00:00"),
                ("c-2", "2026-01-02T09:00:00+00:00"),
                ("c-3", "2026-01-03T09:00:00+00:00"),
            ],
        )
        .await;
        let s = index_over(tmp.path()).await;

        let oldest = search(&s, "", None, 2, Some("created_at:asc")).await;
        assert_eq!(uuids(&oldest), ["c-1", "c-2"]);
        assert!(oldest.errors.is_empty(), "{:?}", oldest.errors);

        let unknown = search(&s, "", None, 2, Some("warp_factor")).await;
        assert_eq!(uuids(&unknown), ["c-3", "c-2"]);
        assert_eq!(unknown.errors.len(), 1, "{:?}", unknown.errors);
    }
}
