//! `datalib-applet unified_grid_and_search_index` — the grid index, the search index and
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
mod body_pieces;
mod children;
pub(crate) mod columns;
mod grouping;
mod local_embedder;
mod map;
mod models;
mod mutes;
mod problems;
mod results;
#[cfg(test)]
mod search_by_example_tests;
#[cfg(test)]
mod search_finds_itself_tests;
mod search_terms;
#[cfg(test)]
mod search_tests;
#[cfg(test)]
mod serve_tests;
mod sql;
mod stored_text;
pub(crate) mod warm;

use body_pieces::body_pieces;
use datalib_columns::Identity;
use datalib_etl_render::indexed_markdown::{DocumentPage, PageAsk};
use datalib_etl_render::section::Node;
use datalib_schema::grid_rows::{GridRow, GridRowColumn};
use datalib_unified_grid_and_search_index::grid_columns::GridColumn;
use datalib_unified_grid_and_search_index::group::Within;
use datalib_unified_grid_and_search_index::hits::index_state::{DocReport, IndexStamp};
use datalib_unified_grid_and_search_index::hits::{display_snippet, GridIndex, IndexSummary};
use datalib_unified_grid_and_search_index::query::{parse_query, ParsedQuery, Semantic};
use datalib_unified_grid_and_search_index::repo::{DocRow, DynIndexRepo, EdgeRowOut};
use datalib_unified_grid_and_search_index::search::SearchRow;
use datalib_unified_grid_and_search_index::sort::{newest_first_by_date, Sort};
use datalib_unified_grid_and_search_index::view;
use serde::{Deserialize, Serialize};

/// The step protocol's data-root variable, which the gateway sets for every
/// applet it starts.
const DATA_ROOT_ENV: &str = datalib_env::DATALIB_DAG_DATA_ROOT;

/// Everything the handlers need, cloned per request.
#[derive(Clone)]
struct Index {
    /// The data root, for resolving a document's on-disk neighbours.
    root: Arc<PathBuf>,
    /// The grid index. Read-only: the `unified_grid_and_search_index` step is its only
    /// writer, so holding it open across a sync is safe.
    repo: DynIndexRepo,
    /// Embeds a Meaning search's text in this process, with the model the
    /// search index's vectors were made with.
    local_embedder: Arc<local_embedder::LocalEmbedder>,
    /// Every chunk's vector in the search index, kept across searches.
    held_vectors: Arc<datalib_unified_grid_and_search_index::search_index::HeldVectors>,
    results: Arc<results::ResultCache>,
    keyed: Arc<tokio::sync::Mutex<results::KeyedCache>>,
    coverage: Arc<tokio::sync::Mutex<results::VectorCoverage>>,
    /// The sources the grid holds, and the commit they were read at.
    grid_sources: Arc<tokio::sync::Mutex<Option<GridSources>>>,
    /// Turns for `/sql`'s queries, [`sql::AT_ONCE`] at a time.
    sql_slots: Arc<tokio::sync::Semaphore>,
    /// The problem rows a person muted: this applet is the store's one
    /// writer (`problem_mutes/README.md`).
    mutes: mutes::Mutes,
}

type GridSources = (Option<String>, Arc<Vec<String>>);

impl Index {
    /// `deadline` is how long one search may wait on the embedder.
    async fn open(
        root: PathBuf,
        models: models::ModelGate,
        deadline: std::time::Duration,
    ) -> Result<Index> {
        let root = Arc::new(root);
        let repo = datalib_unified_grid_and_search_index::dolt_repo::DoltRepo::open(root.clone())
            .await
            .with_context(|| format!("open the grid index under {}", root.display()))?;
        let models: Arc<dyn models::VerifiedModels> = Arc::new(models);
        Ok(Index {
            local_embedder: Arc::new(local_embedder::LocalEmbedder::new(models, deadline)),
            held_vectors: Arc::default(),
            results: Arc::new(results::ResultCache::default()),
            keyed: Arc::default(),
            coverage: Arc::default(),
            grid_sources: Arc::default(),
            sql_slots: Arc::new(tokio::sync::Semaphore::new(sql::AT_ONCE)),
            mutes: mutes::Mutes::new(&root),
            repo: Arc::new(repo),
            root,
        })
    }
}

/// One search, answered exactly as `/search` answers it, with no server:
/// `datalib-applet search`.
pub fn search_once(root: PathBuf, p: SearchParams) -> Result<SearchResponse> {
    // A mistyped root would otherwise answer an empty index: no rows, as
    // if nothing matched.
    let grid = datalib_runtime::layout::grid_index_db(&root);
    anyhow::ensure!(
        grid.exists(),
        "{} has no grid index (no {}): is it a data root, and has a sync built its index?",
        root.display(),
        grid.display()
    );
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(async move {
        let s = if embeds(&parse_query(p.q.as_deref().unwrap_or_default())) {
            open_with_models_verified(root).await?
        } else {
            let models = models::ModelGate::new(&root);
            Index::open(root, models, ONE_SEARCH_DEADLINE).await?
        };
        Ok(answer_search(&s, p).await)
    })
}

/// An [`Index`] for a caller with no server warming its models: the
/// embedding model verified first, on a thread of its own, and the search
/// given a cold start's budget rather than a served request's.
///
/// Nothing warms the model for a one-off search, so one that embeds its
/// text waits for it here, however long a first download takes, rather
/// than at the embedder's deadline. The tests open this way too, so they
/// cannot drift from what the applet does: before this they built the
/// gate themselves and left it `Idle`, which made their *first search*
/// the thing that verified a 300 MB model inside `EMBED_DEADLINE`.
async fn open_with_models_verified(root: PathBuf) -> Result<Index> {
    let models = models::ModelGate::new(&root);
    let gate = models.clone();
    tokio::task::spawn_blocking(move || gate.provision_now())
        .await
        .context("provision the embedding model")?;
    Index::open(root, models, ONE_SEARCH_DEADLINE).await
}

/// How long a served search waits for the embedding model to be verified:
/// under the gateway's 30 s (`APPLET_READ_TIMEOUT` in datalib-http), so a
/// model still on its way costs this search an error naming it rather than
/// a bare proxy timeout.
const EMBED_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// How long `search_once` waits for the embedding model. No gateway's proxy
/// timeout to stay under, and every one starts cold.
const ONE_SEARCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// Whether answering free text runs the embedding model. Fields and
/// words read tables, so `semantic:off` needs none.
fn embeds(parsed: &ParsedQuery) -> bool {
    !parsed.free_text.is_empty() && parsed.semantic != Semantic::Off
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
        // Warm the shared model cache before the first search rather
        // than during it.
        let models = models::ModelGate::new(&root);
        models.warm();
        let state = Index::open(root, models.clone(), EMBED_DEADLINE).await?;
        tokio::spawn(warm::keep_warm(state.clone(), models));
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
            .route("/search/groups", get(groups_handler))
            .route("/search/keys", get(search_keys))
            .route("/search/values", get(search_values))
            .route("/index_state", post(index_state))
            .route("/docs", get(list_docs))
            .route("/people", post(people))
            .route("/embedding_map", get(map::handler))
            .route("/embedding_map/matches", get(map::matches_handler))
            .route("/problems", get(problems::handler))
            .route("/problems/groups", get(problems::groups_handler))
            .route("/problems/keys", get(problems::keys_handler))
            .route("/problems/values", get(problems::values_handler))
            .route("/problems/mute", post(mutes::handler))
            .route("/chat/{markdown_uuid}", get(chat))
            .route("/children/{uuid}", get(children::handler))
            .route("/sql", post(sql::handler))
            .route("/sql/stores", get(sql::stores_handler))
            .route(
                "/health",
                get(|| async { Json(serde_json::json!({"ok": true})) }),
            )
            .with_state(state)
            .layer(middleware::from_fn_with_state(gate, require_gateway));
        tracing::info!(address = %bound, "listening");
        // There was nothing to write first, so binding is all this one
        // owes before the gateway may look.
        crate::announce_port(bound.port());
        axum::serve(listener, app).await.context("serve")
    })
}

pub(crate) async fn require_gateway(
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
            r#"{"error":"this port answers only to the Datalib gateway"}"#,
        ))
        .unwrap_or_else(|_| Response::new(Body::empty()))
}

#[derive(Debug, Deserialize)]
pub struct SearchParams {
    pub q: Option<String>,
    pub limit: Option<usize>,
    /// Where this page starts in the search's rows: the `next_offset` a
    /// previous page answered with. None is the first page.
    pub offset: Option<usize>,
    /// A grid column and direction, `created_at:desc` (see
    /// `datalib_unified_grid_and_search_index::sort`). None is newest first, or the search's rank
    /// for free text.
    pub sort: Option<String>,
    /// A row's uuid the page must reach, however far past `offset` it is:
    /// see `results::reaching`.
    pub through: Option<String>,
    /// Record ids as a JSON array: the answer says which of them are among
    /// the search's rows, wherever they fall (`SearchResponse::among`).
    pub among: Option<String>,
    /// The group whose rows to list, as `[[column, value], …]` (see
    /// `grouping`); none for the whole search.
    pub within: Option<String>,
    /// A search once had an answer per tab. It has one now, so a script
    /// still naming a tab is refused, not given a different list quietly.
    pub tab: Option<String>,
}

const TAB_IS_GONE: &str = "`tab` is gone: a search has one answer, every way of matching merged. \
                           Write `semantic:off` in the query to leave out search by meaning";

/// Which part of a search one request asks for.
struct PageSpec<'a> {
    sort: &'a [Sort],
    within: &'a [Within],
    offset: usize,
    limit: usize,
    through: Option<&'a str>,
    among: &'a [String],
}

#[derive(Debug, Serialize)]
pub struct SearchResponse {
    pub query_echo: serde_json::Value,
    /// The columns the rows carry, typed — see `datalib_columns`.
    pub columns: Vec<datalib_columns::ColumnSpec>,
    #[serde(flatten)]
    pub rows_spec: datalib_columns::RowsSpec,
    pub rows: Vec<SearchRow>,
    /// Every row the search holds, not just this page's.
    pub total: u64,
    /// The offset of the next page, or None when this one reaches the end.
    pub next_offset: Option<usize>,
    /// Where now falls in an order newest first by a date: how many rows
    /// are still to come, a calendar's meetings ahead. The grid opens
    /// below them. On the first page only; None for any other order or
    /// for free text.
    pub now_at: Option<usize>,
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
    /// Why the query cannot be read as typed — an unknown key, a word a
    /// closed key does not take. Often a filter not finished yet, so the
    /// UI keeps what it showed and says why beside the bar.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<String>,
    /// Which of the ids the request named in `among` are among the
    /// search's rows; absent when it named none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub among: Option<Vec<String>>,
}

impl SearchResponse {
    /// A request the search could not read: no rows, and why.
    fn unread(errors: Vec<String>) -> Self {
        SearchResponse {
            query_echo: serde_json::json!({}),
            columns: columns::columns(),
            rows_spec: columns::rows_spec(),
            rows: Vec::new(),
            total: 0,
            next_offset: None,
            now_at: None,
            at: None,
            errors,
            refused: Vec::new(),
            among: None,
        }
    }
}

/// Response shape for `/applet/unified_grid_and_search_index/chat/{markdown_uuid}`:
/// the rendered markdown minus the YAML frontmatter, as one `body` or, asked
/// with `?pieces=true`, as the renderer's `pieces`, each message its own.
/// Per-message scrolling uses the `<div id="m-{uuid}" data-section-uuid="…">`
/// wrappers the renderer draws.
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
    /// The picture the document is, where it is one (`datalib:photo/…`):
    /// such a document has no page, so it has no markdown and the UI draws
    /// it from the applet that serves the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The markdown, unless `pieces` was asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// The markdown as the renderer's pieces in order, each fold a node
    /// around the pieces it holds, so the UI renders each message as a
    /// fragment of its own; only when asked for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pieces: Option<Vec<Node>>,
    /// Asked for a page: the cursor to ask `before=` for the page above
    /// it, or null when it starts the document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    /// Asked for a page: the cursor to ask `after=` for the page below it,
    /// or null when it ends the document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<i64>,
    /// The commit of the render store the document was read at. A cursor
    /// is good for this version; a page read at another may not line up.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
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
    Json(answer_search(&s, p).await)
}

/// `/search`'s answer, which `datalib-applet search` prints as it is.
async fn answer_search(s: &Index, p: SearchParams) -> SearchResponse {
    let q = p.q.unwrap_or_default();
    let parsed = parse_query(&q);
    let refused = |why| SearchResponse {
        refused: vec![why],
        ..SearchResponse::unread(Vec::new())
    };
    if p.tab.is_some() {
        return refused(TAB_IS_GONE.to_string());
    }
    if let Some(why) = parsed.refusal() {
        return refused(why);
    }
    let (parsed, q) = match with_people(&s.root, &q, parsed).await {
        Ok(read) => read,
        Err(why) => return refused(why),
    };
    let limit = p.limit.unwrap_or(200).min(results::MAX_PAGE);
    let mut errors: Vec<String> = Vec::new();
    let sort = match p.sort.as_deref().map(view::order::<GridColumn>).transpose() {
        Ok(sort) => sort.unwrap_or_default(),
        Err(e) => {
            errors.push(format!("{e}; showing the default order"));
            Vec::new()
        }
    };
    // Structured terms alone are a SQL filter; free text is ranked within
    // the rows they leave (`ranked`). A ranking failure is the answer —
    // `query_echo.search_error`, no rows — not a quieter search in its place.
    // No index yet is not a failure: `search_index_missing`, and no rows.
    let mut search_error: Option<String> = None;
    let mut search_index_missing = false;
    let offset = p.offset.unwrap_or(0);
    let through = p.through.as_deref();
    let within = match p
        .within
        .as_deref()
        .map(grouping::parse_within::<GridColumn>)
        .transpose()
    {
        Ok(within) => within.unwrap_or_default(),
        Err(e) => {
            errors.push(e);
            return SearchResponse::unread(errors);
        }
    };
    let among = match p.among.as_deref().map(serde_json::from_str::<Vec<String>>) {
        None => Vec::new(),
        Some(Ok(ids)) => ids,
        Some(Err(e)) => {
            errors.push(format!("`among` is not a JSON array of ids: {e}"));
            return SearchResponse::unread(errors);
        }
    };
    let spec = PageSpec {
        sort: &sort,
        within: &within,
        offset,
        limit,
        through,
        among: &among,
    };
    let answered = search_page(s, &q, &parsed, spec).await;
    let read = answered.is_ok();
    let page = match answered {
        Ok(page) => page,
        Err(SearchFailure::NoIndex) => {
            search_index_missing = true;
            Page::default()
        }
        Err(SearchFailure::Ranking(e)) => {
            tracing::error!(query = %q, error = %e, "a free-text search failed to rank");
            search_error = Some(e);
            Page::default()
        }
        Err(SearchFailure::Index(e)) => {
            let msg = format!("structured search: {e}");
            tracing::error!(query = %q, error = %e, "a structured search failed");
            errors.push(msg);
            Page::default()
        }
    };
    let now_at = match newest_first_by_date(&sort) {
        Some(column) if read && offset == 0 && parsed.free_text.is_empty() => {
            match s
                .repo
                .count_after(&parsed, &within, column, &now_utc())
                .await
            {
                Ok(n) => Some(n),
                Err(e) => {
                    errors.push(format!("find where now falls: {e}"));
                    None
                }
            }
        }
        _ => None,
    };
    // On the first page, for the line above the results: a later page and a
    // group's rows have nothing to add to it.
    let without_vectors = if read && offset == 0 && within.is_empty() {
        match without_vectors(s, &parsed).await {
            Ok(counted) => counted,
            Err(e) => {
                errors.push(format!("count the documents without vectors: {e}"));
                None
            }
        }
    } else {
        None
    };
    let mut rows = page.rows;
    // The names and marks the config gives each source, read per
    // request: a rename lands on the next search, with no re-index.
    let sources = columns::Sources::read(&s.root);
    for row in &mut rows {
        sources.resolve(row);
        row.link = link_of(row);
    }
    SearchResponse {
        columns: columns::columns(),
        rows_spec: columns::rows_spec(),
        query_echo: serde_json::json!({
            "free_text": parsed.free_text,
            "semantic_cutoff": parsed.semantic.cutoff(),
            "search_error": search_error,
            "search_index_missing": search_index_missing,
            "partial": page.partial,
            "without_vectors": without_vectors,
        }),
        rows,
        total: page.total as u64,
        next_offset: page.next_offset,
        now_at,
        at: page.at,
        errors,
        refused: Vec::new(),
        among: page.among,
    }
}

/// Pins what the search counts as now. The e2e suite sets it past its
/// fixture, which is dated in the 2360s; nothing else does.
const SEARCH_NOW_ENV: &str = "DATALIB_SEARCH_NOW";

/// Now, in the stamps' own form: UTC, `…Z`, to the microsecond.
fn now_utc() -> String {
    let utc =
        |t: chrono::DateTime<chrono::Utc>| t.to_rfc3339_opts(chrono::SecondsFormat::Micros, true);
    let Ok(pinned) = std::env::var(SEARCH_NOW_ENV) else {
        return utc(chrono::Utc::now());
    };
    match chrono::DateTime::parse_from_rfc3339(&pinned) {
        Ok(t) => utc(t.with_timezone(&chrono::Utc)),
        Err(e) => {
            tracing::error!(%pinned, error = %e, "{SEARCH_NOW_ENV} is not an RFC 3339 instant; using the clock");
            utc(chrono::Utc::now())
        }
    }
}

/// Where opening `row` goes: its page, at the row when it is a section of
/// the page.
fn link_of(row: &SearchRow) -> Option<String> {
    let page = row.markdown_uuid.as_deref()?;
    Some(datalib_uri::Name::doc(page, Some(&row.uuid)).write())
}

/// Why `answer` is not the search that was asked for, one reason a line:
/// what a script reading it must not take for "nothing matched".
pub fn unanswered(answer: &SearchResponse) -> Vec<String> {
    let echo = &answer.query_echo;
    let mut why: Vec<String> = answer
        .refused
        .iter()
        .map(|r| format!("refused: {r}"))
        .collect();
    why.extend(answer.errors.iter().cloned());
    if let Some(e) = echo["search_error"].as_str() {
        why.push(format!("the search could not rank the free text: {e}"));
    }
    if let Some(e) = echo["partial"].as_str() {
        why.push(e.to_string());
    }
    if echo["search_index_missing"].as_bool() == Some(true) {
        why.push(
            "this root has no search index yet, so free text finds nothing: a source's \
             words are searchable once `unified_grid_and_search_index` has run after its render"
                .to_string(),
        );
    }
    why
}

#[derive(Debug, Deserialize)]
pub struct GroupParams {
    pub q: Option<String>,
    /// The grid columns to group by, outermost first: `source_ref,kind`.
    pub by: String,
}

#[derive(Debug, Default, Serialize)]
pub struct GroupsResponse {
    pub groups: Vec<GroupOut>,
    /// More groups than one answer carries: the rest are left out.
    pub truncated: bool,
    pub at: Option<String>,
    /// Free text the search could not rank: no groups, and why.
    pub search_error: Option<String>,
    /// Free text before the first sync has built a search index: no groups.
    pub search_index_missing: bool,
    pub errors: Vec<String>,
    /// As on `/search`: why the query cannot be read as typed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub refused: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct GroupOut {
    /// The group's value in each grouped column, as `within=` spells it
    /// back.
    pub values: Vec<Option<String>>,
    pub count: u64,
    /// The group's newest row, which its labels are read from.
    pub sample: SearchRow,
}

/// `GET /search/groups?q=…&by=…` — the groups a search falls into, every
/// one with its count, in no order: the grid orders them. A group's rows
/// are `/search?…&within=…`.
async fn groups_handler(
    State(s): State<Index>,
    Query(p): Query<GroupParams>,
) -> Json<GroupsResponse> {
    let q = p.q.unwrap_or_default();
    let parsed = parse_query(&q);
    let mut out = GroupsResponse::default();
    if let Some(why) = parsed.refusal() {
        out.refused.push(why);
        return Json(out);
    }
    let (parsed, q) = match with_people(&s.root, &q, parsed).await {
        Ok(read) => read,
        Err(why) => {
            out.refused.push(why);
            return Json(out);
        }
    };
    let by = match grouping::parse_by::<GridColumn>(&p.by) {
        Ok(by) => by,
        Err(e) => {
            out.errors.push(e);
            return Json(out);
        }
    };
    match group_list(&s, &q, &parsed, &by).await {
        Ok(grouping) => {
            let sources = columns::Sources::read(&s.root);
            out.truncated = grouping.truncated;
            out.at = grouping.at;
            out.groups = grouping
                .groups
                .into_iter()
                .map(|g| {
                    let mut sample = g.sample;
                    sources.resolve(&mut sample);
                    sample.link = link_of(&sample);
                    GroupOut {
                        values: g.values,
                        count: g.count,
                        sample,
                    }
                })
                .collect();
        }
        Err(SearchFailure::NoIndex) => out.search_index_missing = true,
        Err(SearchFailure::Ranking(e)) => out.search_error = Some(e),
        Err(SearchFailure::Index(e)) => out.errors.push(format!("group the search: {e}")),
    }
    Json(out)
}

/// The groups of a search: over the search's ranking for free text, else over
/// every row its structured terms match.
async fn group_list(
    s: &Index,
    q: &str,
    parsed: &ParsedQuery,
    by: &[GridRowColumn],
) -> Result<datalib_unified_grid_and_search_index::group::Grouping, SearchFailure> {
    let among = if parsed.free_text.is_empty() {
        None
    } else {
        let head = s.repo.head().await.map_err(index)?;
        let answer = ranked(s, q, parsed, head).await?;
        Some(
            answer
                .list
                .iter()
                .map(|e| e.uuid.clone())
                .collect::<Vec<_>>(),
        )
    };
    s.repo
        .group_counts(parsed, by, among.as_deref())
        .await
        .map_err(index)
}

#[derive(Debug)]
enum SearchFailure {
    /// No sync has built the search index yet, so free text has nothing to
    /// search. A state to show, not an error.
    NoIndex,
    Ranking(String),
    Index(String),
}

impl std::fmt::Display for SearchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchFailure::NoIndex => f.write_str("no search index yet"),
            SearchFailure::Ranking(e) | SearchFailure::Index(e) => f.write_str(e),
        }
    }
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
    /// What part of the search could not answer, when the rest did.
    partial: Option<String>,
    among: Option<Vec<String>>,
}

/// One page of the search, with the search's score and matched words on each
/// row it ranked.
async fn search_page(
    s: &Index,
    q: &str,
    parsed: &ParsedQuery,
    spec: PageSpec<'_>,
) -> Result<Page, SearchFailure> {
    let (list, at, partial) = search_results(s, q, parsed, spec.sort, spec.within).await?;
    let limit = results::reaching(&list, spec.offset, spec.limit, spec.through);
    let (rows, next_offset) = page_rows(s, parsed, &list, spec.offset, limit).await?;
    let among = (!spec.among.is_empty()).then(|| results::among(&list, spec.among));
    Ok(Page {
        rows,
        total: list.len(),
        next_offset,
        at,
        partial,
        among,
    })
}

/// The `limit` rows of `list` from `offset`, each landed and showing what
/// matched, and where the next page starts.
async fn page_rows(
    s: &Index,
    parsed: &ParsedQuery,
    list: &[results::Entry],
    offset: usize,
    limit: usize,
) -> Result<(Vec<SearchRow>, Option<usize>), SearchFailure> {
    let (entries, next_offset) = results::page(list, offset, limit);
    land(s, parsed, entries).await?;
    let uuids: Vec<String> = entries.iter().map(|e| e.row().to_string()).collect();
    let mut rows = s.repo.rows_by_uuids(&uuids).await.map_err(index)?;
    let hits: std::collections::HashMap<&str, (f64, &str)> = entries
        .iter()
        .filter_map(|e| {
            let hit = e.hit.as_ref()?;
            let quoted = e.landed.get().map_or(hit.snippet.as_str(), |(_, q)| q);
            Some((e.row(), (hit.score, quoted)))
        })
        .collect();
    for row in &mut rows {
        if let Some(hit) = hits.get(row.uuid.as_str()) {
            show_hit(row, *hit);
        }
    }
    Ok((rows, next_offset))
}

/// How many of the documents this search could find search by meaning
/// cannot reach; `None` where it was not asked to look, or no index has
/// been built.
async fn without_vectors(
    s: &Index,
    parsed: &ParsedQuery,
) -> Result<Option<results::WithoutVectors>, SearchFailure> {
    if !embeds(parsed) {
        return Ok(None);
    }
    if !datalib_runtime::layout::grid_index_db(&s.root).exists() {
        return Ok(None);
    }
    let (index_stamp, cache_stamp) =
        datalib_unified_grid_and_search_index::search_index::stamps(&s.root);
    let stamp: Vec<IndexStamp> = vec![index_stamp, cache_stamp];
    let mut coverage = s.coverage.lock().await;
    let with_vectors = match coverage
        .with_vectors
        .as_ref()
        .filter(|(at, _)| *at == stamp)
    {
        Some((_, read)) => read.clone(),
        None => {
            let Some(read) = s
                .held_vectors
                .documents(&s.root)
                .await
                .map_err(embedding_failed)?
            else {
                return Ok(None);
            };
            let read = Arc::new(read);
            coverage.with_vectors = Some((stamp, read.clone()));
            read
        }
    };
    drop(coverage);
    let candidates = s.repo.documents_with_pages(parsed).await.map_err(index)?;
    Ok(Some(results::WithoutVectors::among(
        &candidates,
        &with_vectors,
    )))
}

/// the search's score, and the words the hit matched in place of the row's
/// opening words. A hit that showed nothing readable keeps the preview.
fn show_hit(row: &mut SearchRow, (score, context): (f64, &str)) {
    row.score = Some(score);
    if !context.is_empty() {
        row.snippet = context.to_string();
    }
}

/// Lands each of `entries` a ranking left on its whole document on the
/// message its words or chunk fall in, of the rows the search's other
/// terms keep, and quotes the lines it matched. One file read each, for
/// the rows a page shows: a ranking reads no document, since a long agent
/// session is megabytes and a search ranks a thousand.
async fn land(
    s: &Index,
    parsed: &ParsedQuery,
    entries: &[results::Entry],
) -> Result<(), SearchFailure> {
    let todo: Vec<(
        &results::Entry,
        &datalib_unified_grid_and_search_index::hits::SearchHit,
    )> = entries
        .iter()
        .filter_map(|e| Some((e, e.unlanded()?)))
        .collect();
    if todo.is_empty() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let documents: Vec<String> = todo.iter().map(|(e, _)| e.uuid.clone()).collect();
    let refs = s
        .repo
        .grid_row_refs_for_documents(&documents)
        .await
        .map_err(index)?;
    let where_kept = s.repo.document_stores(&documents).await.map_err(index)?;
    let fetched = started.elapsed();
    let kept: Option<std::collections::HashSet<String>> = keyed(s, parsed).await?.map(|keyed| {
        refs.iter()
            .filter(|r| keyed.rows.contains(&r.uuid))
            .map(|r| r.uuid.clone())
            .collect()
    });
    let filtered = started.elapsed();
    let n_refs = refs.len();
    let idx = GridIndex::new(refs).with_terms(&parsed.free_text);
    let idx = match kept {
        Some(kept) => idx.within(kept),
        None => idx,
    };
    let documents_only = parsed.documents() == Some(true);
    let work: Vec<_> = todo
        .iter()
        .map(|(e, hit)| (e.uuid.clone(), (*hit).clone(), e.landed.clone()))
        .collect();
    let n = work.len();
    let root = (*s.root).clone();
    tokio::task::spawn_blocking(move || {
        // Each hit lands by the item the search index named, and reads
        // only what the item it quotes says.
        let landed: Vec<_> = work
            .into_iter()
            .map(|(document, hit, cell)| {
                let at = idx.land(&document, &hit, documents_only);
                (document, hit, cell, at)
            })
            .collect();
        let wanted: Vec<(String, String)> = landed
            .iter()
            .filter_map(|(document, _, _, at)| {
                Some((document.clone(), at.as_ref()?.quote.clone()?))
            })
            .collect();
        // A store that will not read leaves the hits unquoted, not lost:
        // each still lands on the row it names.
        let texts = stored_text::item_texts(&root, &where_kept, &wanted).unwrap_or_else(|e| {
            tracing::warn!(error = %format!("{e:#}"), "could not read a page's items; quoting none");
            Default::default()
        });
        for (document, hit, cell, at) in landed {
            let found = at.map_or((document, String::new()), |at| {
                let said = at.quote.as_ref().and_then(|uuid| texts.get(uuid));
                let quoted = said.map_or_else(|| hit.snippet.clone(), |t| idx.quote(t));
                (at.row.uuid, display_snippet(&quoted))
            });
            // Another page may have landed it first, on the same row.
            let _ = cell.set(found);
        }
    })
    .await
    .map_err(|e| SearchFailure::Ranking(format!("landing a page's hits: {e}")))?;
    tracing::info!(
        landed = n,
        rows = n_refs,
        fetch_ms = fetched.as_millis() as u64,
        filter_ms = (filtered - fetched).as_millis() as u64,
        read_ms = (started.elapsed() - filtered).as_millis() as u64,
        "landed a page's hits"
    );
    Ok(())
}

/// The search's rows in order, from the cache when this query, sort and
/// group were listed at the index's current commit, and that commit.
async fn search_results(
    s: &Index,
    q: &str,
    parsed: &ParsedQuery,
    sort: &[Sort],
    within: &[Within],
) -> Result<(Arc<Vec<results::Entry>>, Option<String>, Option<String>), SearchFailure> {
    let key = results::Key {
        q: q.to_string(),
        sort: sort.to_vec(),
        within: within.to_vec(),
        at: s.repo.head().await.map_err(index)?,
    };
    if let Some(list) = s.results.get(&key) {
        return Ok((list, key.at, None));
    }
    let mut partial = None;
    let (list, at, keep) = if parsed.free_text.is_empty() {
        let listing = s
            .repo
            .ordered_uuids(parsed, sort, within)
            .await
            .map_err(index)?;
        let list: Vec<results::Entry> = listing
            .uuids
            .into_iter()
            .map(|uuid| results::Entry::new(uuid, None))
            .collect();
        (list, listing.at, true)
    } else {
        let answer = ranked(s, q, parsed, key.at.clone()).await?;
        partial = answer.partial;
        let keep = answer.keep;
        let ranked = answer.list;
        if sort.is_empty() && within.is_empty() {
            return Ok((ranked, answer.at, partial));
        }
        // Sorting or grouping by a column reads every row's column, so
        // every hit is landed first: the slow way, and the rare one. The
        // rows it lands on are the filters' already: they are only ordered.
        land(s, parsed, &ranked).await?;
        let uuids: Vec<String> = ranked.iter().map(|e| e.row().to_string()).collect();
        let listing = s
            .repo
            .filter_uuids(&parse_query(""), &uuids, sort, within)
            .await
            .map_err(index)?;
        let hit_of: std::collections::HashMap<&str, &results::Entry> =
            ranked.iter().map(|e| (e.row(), e)).collect();
        let list: Vec<results::Entry> = listing
            .uuids
            .into_iter()
            .map(|uuid| match hit_of.get(uuid.as_str()) {
                Some(e) => results::Entry {
                    uuid,
                    hit: e.hit.clone(),
                    landed: e.landed.clone(),
                },
                None => results::Entry::new(uuid, None),
            })
            .collect();
        (list, listing.at, keep)
    };
    let list = Arc::new(list);
    // Filed under the commit it was actually read at, which is the key's
    // unless a seal landed between the two reads.
    if keep {
        s.results.put(
            results::Key {
                at: at.clone(),
                ..key
            },
            list.clone(),
        );
    }
    Ok((list, at, partial))
}

/// A free-text search's ranked list, with the commit it was read at, whether
/// it may be kept, and what part of the search could not answer.
struct Ranked {
    list: Arc<Vec<results::Entry>>,
    at: Option<String>,
    keep: bool,
    partial: Option<String>,
}

/// The ranking of a free-text search, narrowed to its structured terms:
/// the list every sort and group of it is cut from, so it is ranked once
/// per search and commit rather than once per group.
/// With the commit it was read at, and whether it may be kept: one read
/// from terms written for another commit may be a pass out.
async fn ranked(
    s: &Index,
    q: &str,
    parsed: &ParsedQuery,
    at: Option<String>,
) -> Result<Ranked, SearchFailure> {
    let key = results::Key {
        q: q.to_string(),
        sort: Vec::new(),
        within: Vec::new(),
        at,
    };
    let cached = |list, at| Ranked {
        list,
        at,
        keep: true,
        partial: None,
    };
    if let Some(list) = s.results.get(&key) {
        return Ok(cached(list, key.at));
    }
    let _turn = s.results.turn(&key).await;
    if let Some(list) = s.results.get(&key) {
        return Ok(cached(list, key.at));
    }
    let answer = fused_ranking(s, parsed).await?;
    let at = key.at.clone();
    // A part that failed may answer next time: answer without it, and ask
    // again.
    let keep = answer.partial.is_none();
    let list: Arc<Vec<results::Entry>> = Arc::new(
        answer
            .ranking
            .into_iter()
            .map(|(uuid, hit)| results::Entry::new(uuid, Some(hit)))
            .collect(),
    );
    if keep {
        s.results.put(
            results::Key {
                at: at.clone(),
                ..key
            },
            list.clone(),
        );
    }
    Ok(Ranked {
        list,
        at,
        keep,
        partial: answer.partial,
    })
}

/// One ranking of a search's documents, best first, each with its score
/// and the hit still to land on a message.
type Ranking = Vec<(String, results::Hit)>;

/// A search's one ranking: its words and, unless `semantic:off`, its
/// meaning, each within the rows the search's filters keep, fused by
/// reciprocal rank, one entry a document, cut once.
///
/// A part that fails costs that part and says so, since the rest are still
/// true matches and search by meaning has a model to wait for: the first
/// launch downloads it. With nothing else to show, the failure is the answer.
async fn fused_ranking(s: &Index, parsed: &ParsedQuery) -> Result<Answer, SearchFailure> {
    let (words, meaning) = tokio::join!(words_ranking(s, parsed), meaning_ranking(s, parsed));
    let mut failed: Option<SearchFailure> = None;
    let mut part = |name: &str, answer: Result<Ranking, SearchFailure>| match answer {
        Ok(ranking) => ranking,
        Err(e) => {
            tracing::error!(part = name, error = %e, "part of a search could not answer");
            failed.get_or_insert(e);
            Vec::new()
        }
    };
    let words = part("words", words);
    let meaning = part("meaning", meaning);
    let rankings: [Ranking; 2] = [words, meaning];
    let partial = match failed {
        Some(failure) if rankings.iter().all(Vec::is_empty) => return Err(failure),
        Some(SearchFailure::Index(e)) => return Err(SearchFailure::Index(e)),
        Some(failure) => Some(format!(
            "Part of this search could not answer, so results may be missing: {failure}"
        )),
        None => None,
    };
    Ok(Answer {
        ranking: fuse(&rankings),
        partial,
    })
}

/// A ranking, and what part of the search could not answer.
struct Answer {
    ranking: Ranking,
    partial: Option<String>,
}

/// Reciprocal rank fusion, by document: a document scores the sum, over
/// the rankings it is in, of `1 / (K + rank)`, so one near the top of two
/// beats one at the top of one. It is shown with the hit of the first
/// ranking that found it, so put the most precise first. Cut once, here:
/// every filter was applied before any ranking was.
fn fuse(rankings: &[Ranking]) -> Ranking {
    const K: f64 = 60.0;
    let mut fused: Ranking = Vec::new();
    let mut at: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for ranking in rankings {
        for (rank, (doc, hit)) in ranking.iter().enumerate() {
            let score = 1.0 / (K + (rank + 1) as f64);
            match at.get(doc.as_str()) {
                Some(&i) => fused[i].1.score += score,
                None => {
                    at.insert(doc, fused.len());
                    let hit = results::Hit {
                        score,
                        ..hit.clone()
                    };
                    fused.push((doc.clone(), hit));
                }
            }
        }
    }
    fused.sort_by(|a, b| b.1.score.total_cmp(&a.1.score));
    fused.truncate(RANK_DEPTH);
    fused
}

/// The words of every document, ranked by BM25 over the search index's
/// messages.
async fn words_ranking(s: &Index, parsed: &ParsedQuery) -> Result<Ranking, SearchFailure> {
    let hits = word_hits(s, parsed).await?;
    documents_of_hits(s, parsed, hits).await
}

/// A ranked document, by its id, and its hit.
type DocumentHit = datalib_unified_grid_and_search_index::search_index::DocumentHit;

async fn word_hits(s: &Index, parsed: &ParsedQuery) -> Result<Vec<DocumentHit>, SearchFailure> {
    // Narrowed before the cut, as Meaning is: a common word's best
    // thousand documents rarely include the one the other terms ask for.
    let keyed = keyed(s, parsed).await?;
    let kept = keyed.as_deref().map(results::Keyed::kept_rows);
    let scope = search_scope(s).await?;
    datalib_unified_grid_and_search_index::search_index::word_hits(
        &s.root,
        &parsed.free_text,
        Some(scope.as_slice()),
        kept.as_ref(),
        RANK_DEPTH,
    )
    .await
    .map_err(|e| SearchFailure::Ranking(format!("the search index's words: {e:#}")))?
    .ok_or(SearchFailure::NoIndex)
}

/// The documents nearest the free text by cosine, scored here over the
/// stored vectors, down to the search's cutoff, and none with
/// `semantic:off`. Structured terms narrow the documents before any is
/// scored, so a filter never empties a ranking cut too shallow to hold its
/// rows. The search index's vectors, with the query embedded here.
async fn meaning_ranking(s: &Index, parsed: &ParsedQuery) -> Result<Ranking, SearchFailure> {
    let Some(cutoff) = parsed.semantic.cutoff() else {
        return Ok(Vec::new());
    };
    let text = parsed.free_text.clone();
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut hits = search_index_nearest(s, parsed, text).await?;
    let found = hits.len();
    hits.retain(|(_, h)| h.score >= cutoff);
    tracing::info!(
        found,
        hits = hits.len(),
        cutoff,
        "ranked a search by meaning"
    );
    documents_of_hits(s, parsed, hits).await
}

fn embedding_failed(e: anyhow::Error) -> SearchFailure {
    SearchFailure::Ranking(format!("{e:#}"))
}

async fn search_index_nearest(
    s: &Index,
    parsed: &ParsedQuery,
    text: String,
) -> Result<Vec<DocumentHit>, SearchFailure> {
    let started = std::time::Instant::now();
    let embedder = s.local_embedder.clone();
    let query = tokio::task::spawn_blocking(move || embedder.embed(&text))
        .await
        .map_err(|e| SearchFailure::Ranking(format!("query embedder task: {e}")))?
        .map_err(embedding_failed)?;
    let embedded = started.elapsed();
    let keyed = keyed(s, parsed).await?;
    let among = keyed.as_deref().map(|k| &k.kept);
    let narrowed = started.elapsed();
    let scope = search_scope(s).await?;
    let hits = s
        .held_vectors
        .nearest(&s.root, &query, Some(scope.as_slice()), among, RANK_DEPTH)
        .await
        .map_err(embedding_failed)?
        .ok_or(SearchFailure::NoIndex)?;
    tracing::info!(
        embed_ms = embedded.as_millis() as u64,
        narrow_ms = (narrowed - embedded).as_millis() as u64,
        score_ms = (started.elapsed() - narrowed).as_millis() as u64,
        among = among.map(|a| a.len()),
        "scored the search index's vectors"
    );
    Ok(hits)
}

/// What the structured terms keep of each document they reach: the
/// messages they keep, or all of it when they keep only the document's own
/// row. A long chat whose last message falls in a date window keeps its
/// row too, and that must not bring back its other days; its own row
/// keeps only the text that is no message's, such as a page's body.
fn kept_of(
    rows: Vec<(String, String, bool)>,
) -> std::collections::HashMap<String, datalib_unified_grid_and_search_index::hits::vectors::Kept> {
    use datalib_unified_grid_and_search_index::hits::vectors::Kept;
    let mut sections: std::collections::HashMap<String, (std::collections::HashSet<String>, bool)> =
        std::collections::HashMap::new();
    for (uuid, document, is_document) in rows {
        let (messages, own_row) = sections.entry(document).or_default();
        if is_document {
            *own_row = true;
        } else {
            messages.insert(uuid);
        }
    }
    sections
        .into_iter()
        .map(|(document, (messages, own_text))| {
            let kept = if messages.is_empty() {
                Kept::Whole
            } else {
                Kept::Sections { messages, own_text }
            };
            (document, kept)
        })
        .collect()
}

/// What the search's filters keep, worked out once for this query and the
/// index's commit and shared by every answer and page of it. `None` when
/// there is no filter.
async fn keyed(
    s: &Index,
    parsed: &ParsedQuery,
) -> Result<Option<Arc<results::Keyed>>, SearchFailure> {
    if parsed.terms.is_empty() {
        return Ok(None);
    }
    let key = (
        format!("{:?}", parsed.terms),
        s.repo.head().await.map_err(index)?,
    );
    let mut cache = s.keyed.lock().await;
    if let Some(keyed) = cache.get(&key) {
        return Ok(Some(keyed));
    }
    let rows = s.repo.matching_rows(parsed).await.map_err(index)?;
    let keyed = Arc::new(results::Keyed {
        rows: rows.iter().map(|(uuid, _, _)| uuid.clone()).collect(),
        documents: rows
            .iter()
            .map(|(_, document, _)| document.clone())
            .collect(),
        kept: kept_of(rows),
    });
    cache.put(key, keyed.clone());
    Ok(Some(keyed))
}

/// How many hits the search ranks for one free-text search: every page of it is
/// cut from these.
const RANK_DEPTH: usize = 1_000;

/// A ranking's hits as one entry per document, best first, each landed on a
/// message only when a page shows it, of the documents holding a row the
/// search's terms keep.
async fn documents_of_hits(
    s: &Index,
    parsed: &ParsedQuery,
    hits: Vec<DocumentHit>,
) -> Result<Ranking, SearchFailure> {
    let documents: Vec<String> = hits.iter().map(|(d, _)| d.clone()).collect();
    let present = match keyed(s, parsed).await? {
        Some(keyed) => documents
            .into_iter()
            .filter(|d| keyed.documents.contains(d))
            .collect(),
        None => s
            .repo
            .documents_kept(parsed, &documents)
            .await
            .map_err(index)?,
    };
    let mut ranking: Ranking = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (document, hit) in hits {
        if !present.contains(&document) {
            if parsed.terms.is_empty() {
                tracing::error!(document = %document, score = hit.score, "a hit resolved to no grid rows");
            }
            continue;
        }
        if seen.insert(document.clone()) {
            let hit = results::Hit {
                score: hit.score,
                snippet: String::new(),
                document: Some(Arc::new(hit)),
            };
            ranking.push((document, hit));
        }
    }
    Ok(ranking)
}

/// The rows a free-text search ranks, best first, at most `depth`: what
/// the embedding map lights up, the same list the grid shows.
async fn ranked_rows(
    s: &Index,
    q: &str,
    parsed: &ParsedQuery,
    depth: usize,
) -> Result<Vec<SearchRow>, SearchFailure> {
    let head = s.repo.head().await.map_err(index)?;
    let list = ranked(s, q, parsed, head).await?.list;
    let uuids: Vec<String> = list.iter().take(depth).map(|e| e.uuid.clone()).collect();
    s.repo.rows_by_uuids(&uuids).await.map_err(index)
}

/// The sources the grid holds at its current commit.
async fn grid_sources(s: &Index) -> Result<Arc<Vec<String>>, SearchFailure> {
    let head = s.repo.head().await.map_err(index)?;
    let mut held = s.grid_sources.lock().await;
    if let Some((at, sources)) = held.as_ref() {
        if *at == head {
            return Ok(sources.clone());
        }
    }
    let sources = Arc::new(s.repo.source_ids().await.map_err(index)?);
    *held = Some((head, sources.clone()));
    Ok(sources)
}

/// What a ranking reads: the sources the grid holds.
///
/// The search index can hold a source the grid does not: one turned off keeps
/// its collection, so its embeddings survive, while the grid leaves out a
/// render store it can no longer read. A hit there has no row to show, yet
/// it would take a place in the ranking's cut and its vectors would be held
/// and scored.
async fn search_scope(s: &Index) -> Result<Arc<Vec<String>>, SearchFailure> {
    grid_sources(s).await
}

/// Cap on how many documents one `/index_state` call may ask about, which
/// bounds the work a single request can trigger. The grid's default
/// result limit is 200; a client wanting more chunks its requests.
const INDEX_STATE_MAX_DOCS: usize = 2_000;

#[derive(Debug, Deserialize)]
pub struct IndexStateRequest {
    /// The markdowns to report on. Duplicates are fine — they collapse.
    pub markdown_uuids: Vec<String>,
}

/// Which documents the search index holds and has embedded. The route and
/// its fields keep the names they had when the search index answered them.
#[derive(Debug, Serialize)]
pub struct IndexStateResponse {
    /// False when no `unified_grid_and_search_index` keyword pass has written the search index
    /// yet, so every document is legitimately un-indexed rather than
    /// unknown.
    pub index_present: bool,
    pub summary: IndexSummary,
    /// markdown_uuid → state. Every requested uuid appears.
    pub docs: std::collections::HashMap<String, DocReport>,
    /// Same contract as `/search`: errors the user should see even
    /// though we answered 200.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

async fn index_state(
    State(s): State<Index>,
    Json(req): Json<IndexStateRequest>,
) -> Json<IndexStateResponse> {
    let mut errors: Vec<String> = Vec::new();
    // Dedupe first: many grid rows share one markdown, and the caller
    // is not required to have collapsed them.
    let mut uuids: Vec<String> = req.markdown_uuids;
    uuids.sort();
    uuids.dedup();
    if uuids.len() > INDEX_STATE_MAX_DOCS {
        errors.push(format!(
            "index_state: asked about {} documents, answering the first {INDEX_STATE_MAX_DOCS}",
            uuids.len()
        ));
        uuids.truncate(INDEX_STATE_MAX_DOCS);
    }
    let every = |report: DocReport| -> std::collections::HashMap<String, DocReport> {
        uuids.iter().map(|u| (u.clone(), report.clone())).collect()
    };
    let opened = datalib_search_index::Reader::open(
        &datalib_runtime::layout::grid_index_db(&s.root),
        &datalib_runtime::layout::inference_cache_db(&s.root),
    )
    .await;
    let reader = match opened {
        Ok(Some(reader)) => reader,
        Ok(None) => {
            return Json(IndexStateResponse {
                index_present: false,
                summary: IndexSummary::default(),
                docs: every(DocReport {
                    indexed: Some(false),
                    embedded: Some(false),
                    note: Some("no search index yet".to_string()),
                }),
                errors,
            });
        }
        Err(e) => {
            errors.push(format!("search index: {e:#}"));
            return Json(IndexStateResponse {
                index_present: false,
                summary: IndexSummary::default(),
                docs: every(DocReport {
                    indexed: None,
                    embedded: None,
                    note: Some("the search index could not be opened".to_string()),
                }),
                errors,
            });
        }
    };
    let model = datalib_search_index::model_key();
    let read = async {
        let states = reader.states(&model, &uuids).await?;
        let coverage = reader.coverage(&model).await?;
        anyhow::Ok((states, coverage))
    }
    .await;
    reader.close().await;
    let (docs, summary) = match read {
        Ok((states, (documents, embedded))) => (
            uuids
                .iter()
                .map(|u| {
                    let report = match states.get(u) {
                        Some(&embedded) => DocReport {
                            indexed: Some(true),
                            embedded: Some(embedded),
                            note: None,
                        },
                        None => DocReport {
                            indexed: Some(false),
                            embedded: Some(false),
                            note: None,
                        },
                    };
                    (u.clone(), report)
                })
                .collect(),
            IndexSummary {
                documents,
                embedded,
            },
        ),
        Err(e) => {
            errors.push(format!("search index: {e:#}"));
            (
                every(DocReport {
                    indexed: None,
                    embedded: None,
                    note: Some("index state could not be read".to_string()),
                }),
                IndexSummary::default(),
            )
        }
    };
    Json(IndexStateResponse {
        index_present: true,
        summary,
        docs,
        errors,
    })
}

#[derive(Deserialize)]
struct PeopleBody {
    handles: Vec<String>,
}

/// Every source's account of whoever holds each asked-for handle, ranked
/// (`datalib_unified_grid_and_search_index::people`). A handle no source mentions is
/// simply absent.
async fn people(
    State(s): State<Index>,
    Json(body): Json<PeopleBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match s.repo.people_for_handles(&body.handles).await {
        Ok(rows) => Ok(Json(serde_json::json!({
            "people": datalib_unified_grid_and_search_index::people::merge_and_rank(rows)
        }))),
        Err(e) => {
            tracing::error!(error = %e, "could not read people by handle");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

/// `parsed` with each `me` and `contact:` value given the handles it
/// reaches, and the key its results are cached under: the query and
/// those handles, so a search after they change is a new search.
pub async fn with_people(
    root: &std::path::Path,
    q: &str,
    parsed: ParsedQuery,
) -> Result<(ParsedQuery, String), String> {
    let (parsed, key) = with_me(root, q, parsed).await?;
    with_contacts(root, &key, parsed).await
}

/// Each `me` value given the owner's handles: `me:self`, the stand-in
/// sources write for them, every handle a source says is its account's
/// own, and every handle of the contacts the person marked as themself,
/// read from the contacts store now. Refused when the index predates the
/// accounts' handles.
async fn with_me(
    root: &std::path::Path,
    q: &str,
    parsed: ParsedQuery,
) -> Result<(ParsedQuery, String), String> {
    if !parsed.names_me() {
        return Ok((parsed, q.to_string()));
    }
    let accounts = search_terms::account_handles(root)
        .await
        .map_err(|e| format!("read the account's handles: {e:#}"))?
        .ok_or_else(|| {
            "`me` reads the account's handles, which the next sync of the index writes".to_string()
        })?;
    let marked = marked_as_me(root)
        .await
        .map_err(|e| format!("read the contacts marked as you: {e:#}"))?;
    let mut mine: Vec<String> = std::iter::once(datalib_handle::Handle::me().as_str().to_string())
        .chain(accounts)
        .collect();
    let new: Vec<String> = marked.into_iter().filter(|h| !mine.contains(h)).collect();
    mine.extend(new);
    let key = format!("{q}\u{1f}me={}", mine.join(","));
    Ok((parsed.with_me_handles(&mine), key))
}

/// None on a root without a contacts app.
async fn marked_as_me(root: &std::path::Path) -> anyhow::Result<Vec<String>> {
    let Some(reader) = datalib_contacts::read::ContactsReader::open(root).await? else {
        return Ok(Vec::new());
    };
    let handles = reader.me_handles().await;
    reader.close().await;
    Ok(handles?.into_iter().collect())
}

/// `parsed` with each `contact:` value given the handles its contact
/// reaches, read from the contacts store now (a detached read: the
/// `datalib_contacts` applet stays its one writer). Refused, saying why,
/// when the query names a contact and the root has no contacts store, or
/// the store has no such contact.
async fn with_contacts(
    root: &std::path::Path,
    q: &str,
    parsed: ParsedQuery,
) -> Result<(ParsedQuery, String), String> {
    let ids = parsed.contact_ids();
    if ids.is_empty() {
        return Ok((parsed, q.to_string()));
    }
    let reader = datalib_contacts::read::ContactsReader::open(root)
        .await
        .map_err(|e| format!("read the contacts app's store: {e:#}"))?
        .ok_or_else(|| {
            "`contact:` reads the contacts app, which this library does not have".to_string()
        })?;
    let mut reached: std::collections::HashMap<String, Vec<String>> = Default::default();
    let mut failed: Option<String> = None;
    for id in &ids {
        match reader.handles(id).await {
            Ok(Some(handles)) => {
                reached.insert(id.clone(), handles.into_iter().collect());
            }
            Ok(None) => {
                failed = Some(format!("the contacts app has no contact `{id}`"));
                break;
            }
            Err(e) => {
                failed = Some(format!("read contact `{id}`: {e:#}"));
                break;
            }
        }
    }
    reader.close().await;
    if let Some(why) = failed {
        return Err(why);
    }
    let key = ids.iter().fold(q.to_string(), |key, id| {
        format!("{key}\u{1f}{id}={}", reached[id].join(","))
    });
    Ok((parsed.with_contact_handles(&reached), key))
}

/// The contacts whose name holds `typed`, as `contact:<id>` values
/// labelled with the name; none on a root without a contacts app.
async fn contacts_named(
    root: &std::path::Path,
    typed: &str,
) -> Result<Vec<datalib_columns::ValueSuggestion>, String> {
    let Some(reader) = datalib_contacts::read::ContactsReader::open(root)
        .await
        .map_err(|e| format!("{e:#}"))?
    else {
        return Ok(Vec::new());
    };
    let found = reader.search(typed, CONTACTS_OFFERED).await;
    reader.close().await;
    Ok(found
        .map_err(|e| format!("{e:#}"))?
        .into_iter()
        .map(|c| datalib_columns::ValueSuggestion {
            value: format!(
                "{}{}",
                datalib_unified_grid_and_search_index::terms_keys::CONTACT,
                c.contact_id
            ),
            count: None,
            label: Some(c.name),
        })
        .collect())
}

/// The most contacts one suggestion answer leads with.
const CONTACTS_OFFERED: u32 = 8;

/// The keys the search bar offers as a person types.
async fn search_keys() -> Json<Vec<datalib_columns::SearchKeySpec>> {
    Json(columns::grid_keys())
}

/// `GET /search/values?key=…&typed=…&q=…`: what the search bar suggests
/// for one key's value. The rest of the query narrows it by its
/// structured terms; its free text does not, which would be a ranked search
/// per keystroke.
async fn search_values(
    State(s): State<Index>,
    Query(p): Query<columns::ValuesParams>,
) -> Result<Json<Vec<datalib_columns::ValueSuggestion>>, (StatusCode, String)> {
    if let Some(key) = datalib_unified_grid_and_search_index::terms_keys::key(&p.key) {
        let (narrowed, _) = with_people(&s.root, &p.q, parse_query(&p.q))
            .await
            .map_err(|why| (StatusCode::BAD_REQUEST, why))?;
        let failed = |e: String| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("suggest values: {e}"),
            )
        };
        // Your contacts first: one picked reaches every handle linked to it.
        let mut out = if key.person {
            contacts_named(&s.root, &p.typed).await.map_err(failed)?
        } else {
            Vec::new()
        };
        let kinds: Vec<u8> = key.kinds().iter().map(|k| k.code()).collect();
        let values = s
            .repo
            .term_value_counts(&narrowed, &kinds, &p.typed, key.person)
            .await
            .map_err(|e| failed(e.to_string()))?;
        out.extend(columns::counted(values));
        return Ok(Json(out));
    }
    let suggested = match columns::value_source::<GridRow>(&p.key) {
        columns::ValueSource::Words(words) => Ok(columns::words_holding(&words, &p.typed)),
        columns::ValueSource::Column(column) => s
            .repo
            .value_counts(&parse_query(&p.q), column, &p.typed)
            .await
            .map(columns::counted),
        columns::ValueSource::Nothing => Ok(Vec::new()),
    };
    suggested.map(Json).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("suggest values: {e}"),
        )
    })
}

async fn list_docs(State(s): State<Index>) -> Result<Json<Vec<DocRow>>, StatusCode> {
    match s.repo.list_docs(500).await {
        Ok(rows) => Ok(Json(rows)),
        Err(e) => {
            tracing::error!(error = %e, "could not list the documents");
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct ChatParams {
    /// Answer with `pieces` rather than `body`.
    #[serde(default)]
    pieces: bool,
    /// With `pieces`: answer a page of at most this many of the document's
    /// top-level nodes, rather than all of them.
    limit: Option<usize>,
    /// The page around this item's piece.
    at: Option<String>,
    /// The page ending just before this cursor.
    before: Option<i64>,
    /// The page from this cursor on.
    after: Option<i64>,
}

const MAX_PAGE: usize = 1000;

impl ChatParams {
    /// The page asked for, if one is: `Err` names a request that cannot be
    /// answered.
    fn page(&self) -> Result<Option<(PageAsk, usize)>, &'static str> {
        let Some(limit) = self.limit else {
            return match (&self.at, self.before, self.after) {
                (None, None, None) => Ok(None),
                _ => Err("at, before and after page a document: give limit too"),
            };
        };
        if !self.pieces {
            return Err("a page of a document comes as pieces: give pieces=true");
        }
        if !(1..=MAX_PAGE).contains(&limit) {
            return Err("limit takes 1 to 1000");
        }
        let ask = match (&self.at, self.before, self.after) {
            (None, None, None) => PageAsk::Start,
            (Some(uuid), None, None) => PageAsk::At(uuid.clone()),
            (None, Some(cursor), None) => PageAsk::Before(cursor),
            (None, None, Some(cursor)) => PageAsk::After(cursor),
            _ => return Err("give one of at, before and after"),
        };
        Ok(Some((ask, limit)))
    }
}

async fn chat(
    State(s): State<Index>,
    Path(markdown_uuid): Path<String>,
    Query(p): Query<ChatParams>,
) -> Result<Json<ChatResponse>, StatusCode> {
    let page = p.page().map_err(|why| {
        tracing::warn!(document = %markdown_uuid, why, "a document asked for in a way it cannot be answered");
        StatusCode::BAD_REQUEST
    })?;
    // The document's markdown as its render store spells it; structured
    // metadata comes from grid_rows.
    let where_kept = s
        .repo
        .document_stores(std::slice::from_ref(&markdown_uuid))
        .await
        .map_err(|e| {
            tracing::error!(document = %markdown_uuid, error = %e, "could not find where the document is kept");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    if where_kept.is_empty() {
        return Err(StatusCode::NOT_FOUND);
    }
    let meta = s
        .repo
        .chat_meta(&markdown_uuid)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let version = where_kept.get(&markdown_uuid).map(|k| k.commit.clone());
    let root = (*s.root).clone();
    let document = markdown_uuid.clone();
    let read = tokio::task::spawn_blocking(move || match page {
        Some((ask, limit)) => stored_text::page(&root, &where_kept, &document, ask, limit),
        None => Ok(stored_text::spell(&root, &where_kept)?
            .remove(&document)
            .map(|doc| DocumentPage {
                sections: doc.sections,
                from_top: true,
                ..Default::default()
            })),
    })
    .await
    .map_err(|e| {
        tracing::error!(document = %markdown_uuid, error = %e, "reading the document panicked");
        StatusCode::INTERNAL_SERVER_ERROR
    })?
    .map_err(|e| {
        tracing::error!(document = %markdown_uuid, error = %format!("{e:#}"), "could not read the document");
        StatusCode::INTERNAL_SERVER_ERROR
    })?;
    let (pieces, before, after) = match read {
        // A picture's document is drawn by its applet, not from a page.
        _ if meta.image.is_some() => (Vec::new(), None, None),
        Some(page) => (
            body_pieces(page.sections, page.from_top),
            page.before,
            page.after,
        ),
        // The store at the commit the index read no longer holds it.
        None => return Err(StatusCode::NOT_FOUND),
    };
    let (body, pieces) = if p.pieces {
        (None, Some(pieces))
    } else {
        (Some(body_pieces::joined(&pieces)), None)
    };
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
            // A muted line stays, marked; a store that will not read
            // leaves every line unmarked and says so.
            let (mutes, errors) = match s.mutes.all().await {
                Ok(mutes) => (mutes, Vec::new()),
                Err(e) => (
                    Default::default(),
                    vec![format!("could not read which problems are muted: {e:#}")],
                ),
            };
            (
                rows.into_iter()
                    .map(|row| {
                        let muted = mutes.contains_key(&row.problem_uuid);
                        problems::DocProblem::of(row, muted)
                    })
                    .collect(),
                errors,
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
        image: meta.image,
        body,
        pieces,
        before,
        after,
        version,
        outgoing_edges,
        problems,
        errors,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A chat whose last message falls in a date window keeps its own row
    /// as well as that day's messages; meaning must rank it by those
    /// messages, not by every day of it. Its own row still keeps the text
    /// that is no message's: a page's body was dropped from every search
    /// with a filter, because the filter kept some of its comments too.
    #[test]
    fn a_kept_document_row_counts_whole_only_without_kept_messages() {
        use datalib_unified_grid_and_search_index::hits::vectors::Kept;
        let row = |uuid: &str, document: &str, is_document| {
            (uuid.to_string(), document.to_string(), is_document)
        };
        let kept = kept_of(vec![
            row("bridge-log", "bridge-log", true),
            row("stardate-2", "bridge-log", false),
            row("stardate-3", "away-team", false),
            row("ten-forward", "ten-forward", true),
        ]);
        assert_eq!(
            kept["bridge-log"],
            Kept::Sections {
                messages: ["stardate-2".to_string()].into(),
                own_text: true,
            }
        );
        assert_eq!(
            kept["away-team"],
            Kept::Sections {
                messages: ["stardate-3".to_string()].into(),
                own_text: false,
            }
        );
        assert_eq!(kept["ten-forward"], Kept::Whole);
    }

    /// Commits one chat per `(uuid, created_at)` to the root's grid index,
    /// the way the `unified_grid_and_search_index` step writes and seals it.
    async fn index_chats(root: &std::path::Path, chats: &[(&str, &str)]) {
        let docs: Vec<Doc> = chats
            .iter()
            .map(|(uuid, created_at)| (*uuid, *created_at, "Chat", None))
            .collect();
        index_documents(root, &docs).await;
    }

    /// A document's uuid, when it was made, its kind and its author.
    type Doc<'a> = (&'a str, &'a str, &'a str, Option<&'a str>);

    /// Commits one document per entry to the root's grid index, the way
    /// the `unified_grid_and_search_index` step writes and seals it.
    async fn index_documents(root: &std::path::Path, docs: &[Doc<'_>]) {
        let rows = docs
            .iter()
            .map(|(uuid, created_at, kind, author)| {
                document_row(uuid, created_at, kind)
                    .author(author.map(String::from))
                    .build()
                    .unwrap()
            })
            .collect();
        index_rows(root, rows).await;
    }

    fn document_row(
        uuid: &str,
        created_at: &str,
        kind: &str,
    ) -> datalib_schema::grid_rows::GridRowBuilder {
        datalib_schema::grid_rows::GridRow::builder()
            .uuid(uuid)
            .provider(datalib_schema::providers::Provider::Claude)
            .kind(kind)
            .source_label("Claude")
            .is_document(true)
            .created_at(Some(created_at.to_string()))
            .conversation_uuid(uuid)
            .entire_chat(format!("/chat/{uuid}"))
            .body("")
            .markdown_uuid(Some(uuid.to_string()))
            .item_count(Some(1))
    }

    /// `row` as a document of its own, under the source it names, else
    /// `enterprise`.
    fn own_document(
        row: datalib_schema::grid_rows::GridRow,
    ) -> datalib_etl_render::grid_index::RenderedMarkdown {
        let source = row
            .source_id
            .clone()
            .unwrap_or_else(|| "enterprise".to_string());
        datalib_etl_render::grid_index::RenderedMarkdown {
            searchable: true,
            content_hash: None,
            markdown_uuid: row.uuid.clone(),
            source_id: source,
            upstream_cursor: None,
            render_unit_key: None,
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            front: Default::default(),
            edges: Vec::new(),
            contacts: Vec::new(),
            data_nodes: Vec::new(),
            filings: Vec::new(),
            photos: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// Writes `docs` into their sources' render stores and runs the grid
    /// index over them, as a sync does: the index reads each store at its
    /// head and records that commit, which is where the route reads a
    /// document's text.
    async fn render_and_index(
        root: &std::path::Path,
        docs: Vec<datalib_etl_render::grid_index::RenderedMarkdown>,
    ) {
        let sources: std::collections::BTreeSet<String> =
            docs.iter().map(|d| d.source_id.clone()).collect();
        let at = root.to_path_buf();
        tokio::task::spawn_blocking(move || {
            for source in sources_of(&docs) {
                let store = datalib_etl_render::indexed_markdown::IndexedMarkdownStore::open(
                    &datalib_etl::layout::render_markdown_root(&at, &source),
                )
                .unwrap();
                for d in docs.iter().filter(|d| d.source_id == source) {
                    store.put_document(d).unwrap();
                }
                store.commit("test render").unwrap();
                store.close();
            }
        })
        .await
        .unwrap();
        let pool = datalib_etl_render::grid_index::open_index(
            &datalib_runtime::layout::grid_index_db(root),
        )
        .await
        .unwrap();
        let sources: Vec<String> = sources.into_iter().collect();
        datalib_etl_render::grid_index::build_grid_index_for(
            &pool,
            root,
            &sources,
            |_| {},
            None,
            &datalib_etl::stop::StopFlag::new(),
        )
        .await
        .unwrap();
        datalib_etl_render::grid_index::close_index(pool)
            .await
            .unwrap();
    }

    fn sources_of(docs: &[datalib_etl_render::grid_index::RenderedMarkdown]) -> Vec<String> {
        let set: std::collections::BTreeSet<String> =
            docs.iter().map(|d| d.source_id.clone()).collect();
        set.into_iter().collect()
    }

    /// Commits each row as a document of its own.
    async fn index_rows(root: &std::path::Path, rows: Vec<datalib_schema::grid_rows::GridRow>) {
        use datalib_etl_render::grid_index::{apply_one, open_index, WriteLock};

        let pool = open_index(&datalib_runtime::layout::grid_index_db(root))
            .await
            .unwrap();
        let lock = WriteLock::for_index(pool.clone());
        for row in rows {
            apply_one(&lock, &own_document(row)).await.unwrap();
        }
        datalib_etl_render::grid_index::seal(&lock).await.unwrap();
        pool.close().await;
    }

    /// The route sends the document as its render store spells it, as the
    /// renderer's pieces when asked, so the UI can render each message as a
    /// fragment of its own, and joined as `body` otherwise. It reads the
    /// store at the commit the index read: a render the index has not read
    /// yet changes nothing until it has, so the text always matches the
    /// grid's rows.
    // The render store's reader blocks in place, which needs worker threads.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_document_route_sends_each_message_as_a_piece() {
        use datalib_etl_render::section::Section;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let uuid = "00000000-0000-4000-8000-0000000000d1";
        let class = || "msg msg--claude".to_string();
        let sections = vec![
            Section::unkeyed("---\ntitle: \"Ten Forward\"\n---\n\n# Ten Forward\n\n".into()),
            Section::fold_open("tool-group", "🛠 1 tool step"),
            Section::boxed(
                "m1",
                class(),
                "Tea, Earl Grey 🫖 <div style=\"display:none\">\n\n",
            ),
            Section::fold_close(),
            Section::boxed("m2", class(), "Make it so.\n\n"),
        ];
        let body = datalib_etl_render::section::join(&sections)
            .strip_prefix("---\ntitle: \"Ten Forward\"\n---\n")
            .map(str::to_string);
        let mut doc = own_document(
            document_row(uuid, "2026-01-01T00:00:00Z", "Chat")
                .build()
                .unwrap(),
        );
        doc.sections = sections.clone();
        render_and_index(root, vec![doc.clone()]).await;

        let s = index_over(root).await;
        let got = pieces_of(&s, uuid).await;
        let boxed = |uuid: &str, md: &str| Node::Piece {
            uuid: Some(uuid.into()),
            class: Some(class()),
            md: md.into(),
        };
        assert_eq!(
            got,
            vec![
                Node::Piece {
                    uuid: None,
                    class: None,
                    md: "\n# Ten Forward\n\n".into()
                },
                Node::Fold {
                    fold: "tool-group".into(),
                    summary: "🛠 1 tool step".into(),
                    pieces: vec![boxed(
                        "m1",
                        "Tea, Earl Grey 🫖 <div style=\"display:none\">\n\n"
                    )],
                },
                boxed("m2", "Make it so.\n\n"),
            ],
            "front matter dropped, a message what is inside its box, a fold around its pieces"
        );
        let Json(whole) = chat(
            State(s.clone()),
            Path(uuid.to_string()),
            Query(ChatParams::default()),
        )
        .await
        .unwrap();
        assert_eq!(
            whole.body, body,
            "the body is the document, front matter dropped"
        );
        assert_eq!(Some(body_pieces::joined(&got)), body);
        assert_eq!(whole.pieces, None, "pieces only when asked");

        let mut since = doc.clone();
        since.sections = vec![Section::unkeyed("---\n---\n\nrewritten since\n".into())];
        let store = datalib_etl_render::indexed_markdown::IndexedMarkdownStore::open(
            &datalib_etl::layout::render_markdown_root(root, &doc.source_id),
        )
        .unwrap();
        let rerendered = since.clone();
        tokio::task::spawn_blocking(move || {
            store.put_document(&rerendered).unwrap();
            store.commit("a render the index has not read").unwrap();
            store.close();
        })
        .await
        .unwrap();
        assert_eq!(
            Some(body_pieces::joined(&pieces_of(&s, uuid).await)),
            body,
            "what the index read"
        );

        render_and_index(root, vec![since]).await;
        let got = pieces_of(&s, uuid).await;
        assert_eq!(body_pieces::joined(&got), "\nrewritten since\n");
    }

    /// Asked a page at a time, the route walks a document in its own
    /// order, by the cursors each page gives, and the pages joined are the
    /// body; a fold comes whole, a page around an item holds it, and a
    /// request that names two places is refused.
    // The render store's reader blocks in place, which needs worker threads.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_document_route_answers_a_page_at_a_time() {
        use datalib_etl_render::section::Section;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let uuid = "00000000-0000-4000-8000-0000000000d2";
        let boxed = |u: &str| Section::boxed(u, "msg".into(), &format!("{u} said\n\n"));
        let mut doc = own_document(
            document_row(uuid, "2026-01-01T00:00:00Z", "Chat")
                .build()
                .unwrap(),
        );
        doc.sections = vec![
            Section::unkeyed("---\ntitle: \"Ten Forward\"\n---\n\n# Ten Forward\n\n".into()),
            boxed("m1"),
            Section::fold_open("tool-group", "🛠 2 tool steps"),
            boxed("m2"),
            boxed("m3"),
            Section::fold_close(),
            boxed("m4"),
        ];
        render_and_index(root, vec![doc]).await;
        let s = index_over(root).await;
        let ask = |params: ChatParams| {
            let s = s.clone();
            async move { chat(State(s), Path(uuid.to_string()), Query(params)).await }
        };
        let page = |at: Option<&str>, after: Option<i64>| ChatParams {
            pieces: true,
            limit: Some(1),
            at: at.map(str::to_string),
            after,
            ..Default::default()
        };

        let Json(whole) = ask(ChatParams::default()).await.unwrap();
        let mut pages = vec![ask(page(None, None)).await.unwrap().0];
        while let Some(after) = pages.last().unwrap().after {
            pages.push(ask(page(None, Some(after))).await.unwrap().0);
        }
        assert_eq!(pages.len(), 4, "the title, m1, the fold whole, m4");
        assert_eq!(pages[0].before, None);
        assert!(pages[1].before.is_some());
        assert!(
            matches!(&pages[2].pieces.as_ref().unwrap()[0], Node::Fold { pieces, .. } if pieces.len() == 2)
        );
        let joined: String = pages
            .iter()
            .map(|p| body_pieces::joined(p.pieces.as_ref().unwrap()))
            .collect();
        assert_eq!(Some(joined), whole.body);
        assert!(whole.version.is_some() && pages.iter().all(|p| p.version == whole.version));

        let around = ask(page(Some("m3"), None)).await.unwrap().0;
        assert!(matches!(&around.pieces.unwrap()[..], [Node::Fold { .. }]));

        let both = ChatParams {
            before: Some(1),
            ..page(None, Some(2))
        };
        assert_eq!(ask(both).await.unwrap_err(), StatusCode::BAD_REQUEST);
        let no_limit = ChatParams {
            at: Some("m1".into()),
            pieces: true,
            ..Default::default()
        };
        assert_eq!(ask(no_limit).await.unwrap_err(), StatusCode::BAD_REQUEST);
    }

    /// The document's pieces, as the route answers them when asked.
    async fn pieces_of(s: &Index, uuid: &str) -> Vec<Node> {
        let Json(got) = chat(
            State(s.clone()),
            Path(uuid.to_string()),
            Query(ChatParams {
                pieces: true,
                ..Default::default()
            }),
        )
        .await
        .unwrap();
        assert_eq!(got.body, None, "no body beside the pieces");
        got.pieces.expect("the route sends the pieces")
    }

    /// Read from the search index, a hit is the message holding the words,
    /// and lands there, quoted from it. A word only the thread's title
    /// holds finds it too, and an excluded word drops a message, not the
    /// thread.
    // The render store's reader blocks in place, which needs worker threads.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_words_search_reads_the_search_index_and_lands_on_the_message() {
        use datalib_etl_render::section::Section;
        use datalib_search_index::{IndexDoc, IndexSection};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let uuid = "00000000-0000-4000-8000-0000000000e1";
        let wrapper = |m: &str, text: &str| {
            format!(
                "<div id=\"m-{m}\" data-section-uuid=\"{m}\" class=\"msg msg--claude\">\n\n{text}\n\n</div>\n\n"
            )
        };
        let (m1, m2, m3) = (
            "00000000-0000-4000-8000-0000000000f1",
            "00000000-0000-4000-8000-0000000000f2",
            "00000000-0000-4000-8000-0000000000f3",
        );
        let sections = vec![
            Section::unkeyed("---\ntitle: \"Engineering\"\n---\n\n# Engineering\n\n".into()),
            Section::keyed(m1, wrapper(m1, "The warp core is stable.")),
            Section::keyed(m2, wrapper(m2, "Deck twelve reports a coolant leak.")),
            Section::keyed(
                m3,
                wrapper(m3, "Seal the warp core, the coolant is venting."),
            ),
        ];
        let mut doc = own_document(
            document_row(uuid, "2026-01-01T00:00:00Z", "Chat")
                .build()
                .unwrap(),
        );
        for m in [m1, m2, m3] {
            let mut message = document_row(m, "2026-01-01T00:00:00Z", "Message")
                .markdown_uuid(Some(uuid.to_string()))
                .conversation_uuid(uuid)
                .build()
                .unwrap();
            message.is_document = false;
            doc.rows.push(message);
        }
        doc.sections = sections.clone();
        render_and_index(root, vec![doc.clone()]).await;

        let index =
            datalib_search_index::Index::open(&datalib_runtime::layout::grid_index_db(root))
                .await
                .unwrap();
        index
            .put(&[IndexDoc {
                source_id: doc.source_id.clone(),
                markdown_uuid: uuid.into(),
                content_hash: "h".into(),
                title: "Engineering".into(),
                head: None,
                sections: sections
                    .iter()
                    .map(|s| IndexSection {
                        uuid: s.uuid.clone(),
                        text: s.md.clone(),
                        lead: None,
                    })
                    .collect(),
            }])
            .await
            .unwrap();
        index.close().await;

        let s = index_over(root).await;
        let found = search(&s, "warp coolant semantic:off", None, 10, None).await;
        assert_eq!(found.rows.len(), 1, "{:?}", found.query_echo);
        assert_eq!(found.rows[0].uuid, m3, "the message holding both words");
        assert!(
            found.rows[0].snippet.contains("venting"),
            "{:?}",
            found.rows[0].snippet
        );

        let titled = search(&s, "engineering semantic:off", None, 10, None).await;
        assert_eq!(titled.rows.len(), 1, "{:?}", titled.query_echo);
        let without = search(&s, "warp -coolant semantic:off", None, 10, None).await;
        assert_eq!(without.rows.len(), 1, "{:?}", without.query_echo);
        assert_eq!(without.rows[0].uuid, m1, "the message without the word");
    }

    /// A row titled `Holodeck safety review`, of `source`.
    fn holodeck_row(
        uuid: &str,
        at: &str,
        kind: &str,
        source: &str,
    ) -> datalib_schema::grid_rows::GridRow {
        let mut row = document_row(uuid, at, kind)
            .conversation_name(Some("Holodeck safety review".to_string()))
            .build()
            .unwrap();
        row.source_id = Some(source.to_string());
        row
    }

    /// `docs` in the grid index, and in the search index with `title`, each
    /// row one piece saying `text`: a message's piece its own, a document
    /// with no messages one page.
    async fn index_with_text(
        root: &std::path::Path,
        docs: &[datalib_etl_render::grid_index::RenderedMarkdown],
        title: &str,
        text: &str,
    ) {
        use datalib_etl_render::grid_index::{apply_one, open_index, seal, WriteLock};
        use datalib_search_index::{IndexDoc, IndexSection};
        let pool = open_index(&datalib_runtime::layout::grid_index_db(root))
            .await
            .unwrap();
        let lock = WriteLock::for_index(pool.clone());
        for doc in docs {
            apply_one(&lock, doc).await.unwrap();
        }
        seal(&lock).await.unwrap();
        pool.close().await;
        let index =
            datalib_search_index::Index::open(&datalib_runtime::layout::grid_index_db(root))
                .await
                .unwrap();
        let indexed: Vec<IndexDoc> = docs
            .iter()
            .map(|doc| {
                let messages: Vec<IndexSection> = doc
                    .rows
                    .iter()
                    .filter(|r| !r.is_document)
                    .map(|r| IndexSection {
                        uuid: Some(r.uuid.clone()),
                        text: text.to_string(),
                        lead: Some("Holodeck log".to_string()),
                    })
                    .collect();
                let sections = if messages.is_empty() {
                    vec![IndexSection {
                        uuid: None,
                        text: text.to_string(),
                        lead: None,
                    }]
                } else {
                    messages
                };
                IndexDoc {
                    source_id: doc.source_id.clone(),
                    markdown_uuid: doc.markdown_uuid.clone(),
                    content_hash: "h".into(),
                    title: title.into(),
                    head: None,
                    sections,
                }
            })
            .collect();
        index.put(&indexed).await.unwrap();
        index.close().await;
    }

    /// Repro (#1887): a filter was applied after a ranking's cut. A thousand
    /// newer chats say "holodeck" as often as an older email does, so the
    /// email ranks past the depth, and `source:` must keep it before the
    /// cut. The names ranker, which lost it, is gone; this guards the words.
    #[tokio::test]
    async fn a_source_keeps_a_match_ranked_past_the_depth() {
        let tmp = tempfile::tempdir().unwrap();
        let email = "00000000-0000-4000-8000-00000000e0e0";
        let mut docs = vec![own_document(holodeck_row(
            email,
            "2026-01-01T09:00:00+00:00",
            "Email",
            "tng_email",
        ))];
        docs.extend((0..RANK_DEPTH).map(|i| {
            own_document(holodeck_row(
                &format!("00000000-0000-4000-8000-{i:012}"),
                "2026-02-01T09:00:00+00:00",
                "Chat",
                "holodeck_logs",
            ))
        }));
        index_with_text(tmp.path(), &docs, "Safety review", "holodeck drill").await;
        let s = index_over(tmp.path()).await;
        let every = search(&s, "holodeck semantic:off", None, 10, None).await;
        assert_eq!(every.total, RANK_DEPTH as u64, "{:?}", every.query_echo);
        let kept = search(&s, "holodeck source:tng_email semantic:off", None, 10, None).await;
        assert_eq!(uuids(&kept), [email], "{:?}", kept.query_echo);
    }

    /// A word matches as written, `+` and `-` ones too, case and accents
    /// folded; another form of it is meaning's to find. With porter stems
    /// `drill` found "drills" and `late -drill` dropped the log.
    #[tokio::test]
    async fn a_word_matches_as_written() {
        let tmp = tempfile::tempdir().unwrap();
        let log = "00000000-0000-4000-8000-00000000e0e0";
        let docs = [own_document(holodeck_row(
            log,
            "2026-01-01T09:00:00+00:00",
            "Email",
            "tng_email",
        ))];
        index_with_text(
            tmp.path(),
            &docs,
            "Safety review",
            "the drills were running late",
        )
        .await;
        let s = index_over(tmp.path()).await;
        for (q, found) in [
            ("drills", true),
            ("DRILLS", true),
            ("drill", false),
            ("run", false),
            ("+drills", true),
            ("late -drills", false),
            ("late -drill", true),
            ("\"drills were\"", true),
            ("\"drill\"", false),
        ] {
            let r = search(&s, &format!("{q} semantic:off"), None, 10, None).await;
            let want: &[&str] = if found { &[log] } else { &[] };
            assert_eq!(uuids(&r), want, "{q}: {:?}", r.query_echo);
        }
    }

    /// Repro (#1887): a ranking cut its matches, not its documents, before
    /// keeping one per document: one long chat whose every message says the
    /// word filled the cut, and an older document saying it was never
    /// found. The names ranker did this with a chat's title on each
    /// message, and the words with the best eight thousand messages.
    #[tokio::test]
    async fn one_long_conversation_does_not_crowd_out_another_document() {
        let tmp = tempfile::tempdir().unwrap();
        let older = "00000000-0000-4000-8000-00000000e0e0";
        let long = "00000000-0000-4000-8000-00000000c0c0";
        let mut chat = own_document(holodeck_row(
            long,
            "2026-02-01T09:00:00+00:00",
            "Chat",
            "holodeck_logs",
        ));
        for i in 0..RANK_DEPTH * 8 + 1 {
            let mut message = holodeck_row(
                &format!("00000000-0000-4000-8000-{i:012}"),
                "2026-02-01T09:00:00+00:00",
                "Message",
                "holodeck_logs",
            );
            message.is_document = false;
            message.markdown_uuid = Some(long.to_string());
            message.conversation_uuid = long.to_string();
            chat.rows.push(message);
        }
        let email = own_document(holodeck_row(
            older,
            "2026-01-01T09:00:00+00:00",
            "Email",
            "tng_email",
        ));
        index_with_text(
            tmp.path(),
            &[chat, email],
            "Safety review",
            "holodeck drill",
        )
        .await;

        let s = index_over(tmp.path()).await;
        let found = search(&s, "holodeck semantic:off", None, 10, None).await;
        let documents: std::collections::HashSet<Option<&str>> = found
            .rows
            .iter()
            .map(|r| r.markdown_uuid.as_deref())
            .collect();
        assert!(
            documents.contains(&Some(older)) && documents.contains(&Some(long)),
            "found only {documents:?} ({:?})",
            found.query_echo
        );
    }

    /// Whether any test in this process has the file `(dev, ino)` open.
    /// The caller keeps a name linked to that file, so its number cannot
    /// be handed to another test's file while it looks. Each descriptor
    /// is asked with `fstat` on a copy of it: on macOS, `stat` of
    /// `/dev/fd/N` answers with devfs's device number, not the file's.
    fn held_open((dev, ino): (u64, u64)) -> bool {
        use std::os::fd::{BorrowedFd, RawFd};
        use std::os::unix::fs::MetadataExt;
        std::fs::read_dir("/dev/fd")
            .unwrap()
            .flatten()
            .filter_map(|entry| entry.file_name().to_str()?.parse::<RawFd>().ok())
            .filter_map(|fd| {
                // SAFETY: only `dup`ed. A descriptor another test closed
                // meanwhile fails with EBADF or copies whatever took its
                // number; neither touches the original.
                unsafe { BorrowedFd::borrow_raw(fd) }
                    .try_clone_to_owned()
                    .ok()
            })
            .filter_map(|fd| std::fs::File::from(fd).metadata().ok())
            .any(|m| (m.dev(), m.ino()) == (dev, ino))
    }

    /// A reset deletes the grid index, and the next pass makes a new file
    /// under the same name. The applet's held reader lets go of the old
    /// one at its next read, or it would answer from rows that are gone,
    /// and hold their disk, until it restarted.
    #[tokio::test]
    async fn a_rebuilt_grid_index_is_reopened() {
        use std::os::unix::fs::MetadataExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let (a, b, c) = (
            "00000000-0000-4000-8000-0000000000a1",
            "00000000-0000-4000-8000-0000000000b2",
            "00000000-0000-4000-8000-0000000000c3",
        );
        index_chats(root, &[(a, "2026-01-01T00:00:00Z")]).await;
        index_chats(root, &[(b, "2026-01-02T00:00:00Z")]).await;
        let s = index_over(root).await;
        let found = |rows: Vec<datalib_unified_grid_and_search_index::search::SearchRow>| {
            rows.into_iter().map(|r| r.uuid).collect::<Vec<_>>()
        };
        let uuids = vec![a.to_string(), b.to_string(), c.to_string()];
        assert_eq!(found(s.repo.rows_by_uuids(&uuids).await.unwrap()), [a, b]);

        let db = datalib_runtime::layout::grid_index_db(root);
        let before = std::fs::metadata(&db).map(|m| (m.dev(), m.ino())).unwrap();
        assert!(held_open(before), "the first read left no reader open");
        // Freed, the inode's number goes to the next file made on this
        // disk, by any test; a second name keeps it ours.
        std::fs::hard_link(&db, root.join("the deleted grid index")).unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
        index_chats(root, &[(c, "2026-01-03T00:00:00Z")]).await;

        assert_eq!(found(s.repo.rows_by_uuids(&uuids).await.unwrap()), [c]);
        assert!(
            !held_open(before),
            "the reader still holds the grid index a reset deleted"
        );
    }

    /// A pasted uuid or address is a filter on the search terms, answered
    /// without the search index, which this root does not have yet.
    #[tokio::test]
    async fn a_pasted_id_or_address_is_a_filter_without_the_search_index() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b) = (
            "00000000-0000-4000-8000-0000000000a1",
            "00000000-0000-4000-8000-0000000000b2",
        );
        index_rows(
            tmp.path(),
            vec![
                document_row(a, "2026-01-01T09:00:00+00:00", "Chat")
                    .author_handle(Some("email:ann@example.com".to_string()))
                    .build()
                    .unwrap(),
                document_row(b, "2026-01-02T09:00:00+00:00", "Chat")
                    .author_handle(Some("email:bo@example.com".to_string()))
                    .build()
                    .unwrap(),
            ],
        )
        .await;
        let index_missing = |r: &SearchResponse| r.query_echo["search_index_missing"] == true;
        let s = index_over(tmp.path()).await;
        let by_id = search(&s, a, None, 10, None).await;
        assert!(!index_missing(&by_id), "{:?}", by_id.query_echo);
        assert_eq!(uuids(&by_id), [a]);

        let by_address = search(&s, "Bo@Example.com", None, 10, None).await;
        assert_eq!(uuids(&by_address), [b]);
        let without = search(&s, "-bo@example.com", None, 10, None).await;
        assert_eq!(uuids(&without), [a]);

        let kept = search(&s, &format!("{a} kind:Chat"), None, 10, None).await;
        assert_eq!(uuids(&kept), [a]);
        let narrowed = search(&s, &format!("{a} kind:Email"), None, 10, None).await;
        assert_eq!(uuids(&narrowed), Vec::<&str>::new());
    }

    /// A grid that holds one page asks whether its selected rows are still
    /// in a new search, wherever they fall in it: `among` answers for every
    /// row the search holds, not just the page.
    #[tokio::test]
    async fn among_says_which_named_rows_are_in_the_search_past_the_page() {
        let tmp = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            "00000000-0000-4000-8000-0000000000a1",
            "00000000-0000-4000-8000-0000000000b2",
            "00000000-0000-4000-8000-0000000000c3",
        );
        index_rows(
            tmp.path(),
            vec![
                document_row(a, "2026-01-01T09:00:00+00:00", "Chat")
                    .build()
                    .unwrap(),
                document_row(b, "2026-01-02T09:00:00+00:00", "Chat")
                    .build()
                    .unwrap(),
                document_row(c, "2026-01-03T09:00:00+00:00", "Email")
                    .build()
                    .unwrap(),
            ],
        )
        .await;
        let s = index_over(tmp.path()).await;
        let params = SearchParams {
            q: Some("kind:Chat".to_string()),
            limit: Some(1),
            offset: None,
            sort: None,
            through: None,
            among: Some(serde_json::json!([c, a, b]).to_string()),
            within: None,
            tab: None,
        };
        let r = search_handler(State(s.clone()), Query(params)).await.0;
        assert_eq!(r.rows.len(), 1, "one page of one row");
        assert_eq!(r.among, Some(vec![a.to_string(), b.to_string()]));
        assert_eq!(search(&s, "kind:Chat", None, 1, None).await.among, None);
    }

    /// Commits each row as a document of its own, with the search terms a
    /// render supplies for it (an email's To and Cc).
    async fn index_rows_with_terms(
        root: &std::path::Path,
        rows: Vec<(
            datalib_schema::grid_rows::GridRow,
            Vec<(datalib_schema::search_terms::SearchTermKind, &str)>,
        )>,
    ) {
        use datalib_etl_render::grid_index::{apply_one, open_index, WriteLock};
        use datalib_schema::search_terms::SearchTerm;

        let pool = open_index(&datalib_runtime::layout::grid_index_db(root))
            .await
            .unwrap();
        let lock = WriteLock::for_index(pool.clone());
        for (mut row, supplied) in rows {
            row.set_supplied_terms(
                supplied
                    .into_iter()
                    .map(|(kind, value)| SearchTerm::new(kind, value)),
            );
            apply_one(&lock, &own_document(row)).await.unwrap();
        }
        datalib_etl_render::grid_index::seal(&lock).await.unwrap();
        pool.close().await;
    }

    /// Three messages: Ann writes to Bo, copying Cy; Bo answers Ann; a
    /// model, known by name alone, writes a note.
    async fn mail(root: &std::path::Path) {
        use datalib_schema::search_terms::SearchTermKind::{Cc, To};
        let from = |uuid: &str, at: &str, name: &str, handle: Option<&str>| {
            document_row(uuid, at, "Email")
                .author(Some(name.to_string()))
                .author_handle(handle.map(String::from))
                .build()
                .unwrap()
        };
        index_rows_with_terms(
            root,
            vec![
                (
                    from(
                        "m-1",
                        "2026-01-01T09:00:00+00:00",
                        "Ann",
                        Some("email:ann@example.com"),
                    ),
                    vec![(To, "email:bo@example.com"), (Cc, "email:cy@example.com")],
                ),
                (
                    from(
                        "m-2",
                        "2026-01-02T09:00:00+00:00",
                        "Bo",
                        Some("email:bo@example.com"),
                    ),
                    vec![(To, "email:ann@example.com")],
                ),
                (
                    from("m-3", "2026-01-03T09:00:00+00:00", "claude-opus", None),
                    Vec::new(),
                ),
            ],
        )
        .await;
    }

    /// No source here says who the owner is, so `me` finds nothing until
    /// the person marks their contact; then it reaches every handle the
    /// contact holds, at once and with no re-index, and unmarking takes
    /// it back.
    #[tokio::test]
    async fn me_reaches_the_contacts_marked_as_me() {
        use datalib_contacts::{store_path, ContactKind, Store};
        let tmp = tempfile::tempdir().unwrap();
        mail(tmp.path()).await;
        let s = index_over(tmp.path()).await;
        let found = |q: &'static str| {
            let s = s.clone();
            async move {
                let r = search(&s, q, None, 10, None).await;
                assert!(r.refused.is_empty() && r.errors.is_empty(), "{q}: {r:?}");
                uuids(&r).iter().map(|u| u.to_string()).collect::<Vec<_>>()
            }
        };
        assert!(found("from:me").await.is_empty());

        let store = Store::open(&store_path(tmp.path())).await.unwrap();
        let ann = store
            .create(
                "Ann",
                ContactKind::Person,
                &[datalib_handle::Handle::email("ann@example.com").unwrap()],
            )
            .await
            .unwrap();
        store.set_me(&ann, true).await.unwrap();
        store.close().await;
        assert_eq!(found("from:me").await, ["m-1"]);
        assert_eq!(found("to:me").await, ["m-2"]);

        let store = Store::open(&store_path(tmp.path())).await.unwrap();
        store.set_me(&ann, false).await.unwrap();
        store.close().await;
        assert!(found("from:me").await.is_empty());
    }

    /// An index written before it held the account's handles refuses
    /// `me` by name, where it would otherwise fail on a missing table or
    /// quietly find only the stand-in "Me".
    #[tokio::test]
    async fn me_on_an_index_without_the_accounts_handles_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        mail(tmp.path()).await;
        let path = datalib_runtime::layout::grid_index_db(tmp.path());
        use sqlx::{ConnectOptions, Connection};
        let mut conn = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(datalib_runtime::plain_sqlite::uri(&path))
            .connect()
            .await
            .unwrap();
        sqlx::query("DROP TABLE account_handles")
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let s = index_over(tmp.path()).await;
        let r = search(&s, "from:me", None, 10, None).await;
        assert!(r.rows.is_empty());
        assert!(
            r.refused.iter().any(|why| why.contains("`me`")),
            "{:?}",
            r.refused
        );
        assert!(search(&s, "from:ann", None, 10, None)
            .await
            .refused
            .is_empty());
    }

    /// Each person key reads the search terms in its role: a handle by its
    /// one value, a quoted name whole, a bare name or part of an address
    /// in part, `with:` in any role, `-` the rows without. `author:` and `author_handle:` are
    /// `from:`, which also finds an author known by name alone.
    #[tokio::test]
    async fn a_person_is_found_in_the_role_the_key_names() {
        let tmp = tempfile::tempdir().unwrap();
        mail(tmp.path()).await;
        let s = index_over(tmp.path()).await;
        for (q, want) in [
            ("from:Ann@Example.com", vec!["m-1"]),
            ("author:ann", vec!["m-1"]),
            ("author_handle:email:bo@example.com", vec!["m-2"]),
            ("from:claude", vec!["m-3"]),
            // Quoted, a name is matched whole, case-blind.
            (r#"author:"ann""#, vec!["m-1"]),
            (r#"author:"An""#, vec![]),
            (r#"from:"claude-opus""#, vec!["m-3"]),
            (r#"-author:"claude-opus""#, vec!["m-2", "m-1"]),
            ("to:ann@example.com", vec!["m-2"]),
            ("cc:cy@example.com", vec!["m-1"]),
            ("to:cy@example.com", vec![]),
            ("recipient:cy@example.com", vec!["m-1"]),
            ("with:ann@example.com", vec!["m-2", "m-1"]),
            ("involves:cy", vec!["m-1"]),
            ("-with:ann@example.com", vec!["m-3"]),
            ("with:ann@example.com kind:Email -from:bo", vec!["m-1"]),
        ] {
            let r = search(&s, q, None, 10, None).await;
            assert!(r.refused.is_empty() && r.errors.is_empty(), "{q}: {r:?}");
            assert_eq!(uuids(&r), want, "{q}");
        }
    }

    /// A contact's card is about its person: `with:` finds it by any of
    /// its handles or names, and `from:` does not, since nobody wrote it.
    #[tokio::test]
    async fn with_finds_the_card_about_a_person_and_from_does_not() {
        use datalib_schema::search_terms::SearchTermKind::About;
        let tmp = tempfile::tempdir().unwrap();
        let card = document_row("card-ann", "2026-01-01T09:00:00+00:00", "Contact")
            .contact(Some("Ann Example".to_string()))
            .build()
            .unwrap();
        let message = document_row("m-ann", "2026-01-02T09:00:00+00:00", "Email")
            .author(Some("Ann".to_string()))
            .author_handle(Some("email:ann@example.com".to_string()))
            .build()
            .unwrap();
        index_rows_with_terms(
            tmp.path(),
            vec![
                (
                    card,
                    vec![(About, "email:ann@example.com"), (About, "Ann Example")],
                ),
                (message, Vec::new()),
            ],
        )
        .await;
        let s = index_over(tmp.path()).await;
        for (q, want) in [
            ("with:ann@example.com", vec!["m-ann", "card-ann"]),
            (r#"with:"Ann Example""#, vec!["card-ann"]),
            ("from:ann@example.com", vec!["m-ann"]),
        ] {
            let r = search(&s, q, None, 10, None).await;
            assert!(r.refused.is_empty() && r.errors.is_empty(), "{q}: {r:?}");
            assert_eq!(uuids(&r), want, "{q}");
        }
    }

    /// A person key's values come from the search terms of its kinds,
    /// most rows first, among the rows the rest of the query keeps.
    #[tokio::test]
    async fn a_person_key_suggests_handles_and_names() {
        let tmp = tempfile::tempdir().unwrap();
        mail(tmp.path()).await;
        let s = index_over(tmp.path()).await;
        let values = |key: &str, typed: &str, q: &str| {
            let params = columns::ValuesParams {
                key: key.into(),
                typed: typed.into(),
                q: q.into(),
            };
            let s = s.clone();
            async move {
                let got = search_values(State(s), Query(params)).await.unwrap().0;
                got.into_iter().map(|v| v.value).collect::<Vec<_>>()
            }
        };
        assert_eq!(
            values("from", "ann", "").await,
            ["Ann", "email:ann@example.com"]
        );
        assert_eq!(
            values("with", "example.com", "").await,
            [
                "email:ann@example.com",
                "email:bo@example.com",
                "email:cy@example.com"
            ]
        );
        assert_eq!(
            values("to", "", "from:bo@example.com").await,
            ["email:ann@example.com"]
        );
        // Any part of a value, case-blind, by trigram or, shorter, by
        // reading every value.
        assert_eq!(
            values("from", "XAMPLE.c", "").await,
            ["email:ann@example.com", "email:bo@example.com"]
        );
        assert_eq!(
            values("from", "nn", "").await,
            ["Ann", "email:ann@example.com"]
        );
    }

    /// Picard writes as "Jean-Luc" by email and Slack; Riker, as "Will",
    /// writes to Picard's address; Troi writes to Riker.
    async fn bridge(root: &std::path::Path) {
        use datalib_schema::search_terms::SearchTermKind::To;
        let from = |uuid: &str, at: &str, name: &str, handle: &str| {
            document_row(uuid, at, "Email")
                .author(Some(name.to_string()))
                .author_handle(Some(handle.to_string()))
                .build()
                .unwrap()
        };
        index_rows_with_terms(
            root,
            vec![
                (
                    from(
                        "b-1",
                        "2026-01-01T09:00:00+00:00",
                        "Jean-Luc",
                        "email:jlp@e.org",
                    ),
                    Vec::new(),
                ),
                (
                    from(
                        "b-2",
                        "2026-01-02T09:00:00+00:00",
                        "Jean-Luc",
                        "slack:T1/U_JLP",
                    ),
                    Vec::new(),
                ),
                (
                    from(
                        "b-3",
                        "2026-01-03T09:00:00+00:00",
                        "Will",
                        "email:wtr@e.org",
                    ),
                    vec![(To, "email:jlp@e.org")],
                ),
                (
                    from(
                        "b-4",
                        "2026-01-04T09:00:00+00:00",
                        "Deanna",
                        "email:troi@e.org",
                    ),
                    vec![(To, "email:wtr@e.org")],
                ),
            ],
        )
        .await;
    }

    /// A name reaches the handles seen under it: Picard is "Jean-Luc" on
    /// his rows, so `to:jean` finds what was sent to his address, though
    /// the address itself holds no "jean".
    #[tokio::test]
    async fn a_name_finds_the_handles_seen_under_it() {
        let tmp = tempfile::tempdir().unwrap();
        bridge(tmp.path()).await;
        let s = index_over(tmp.path()).await;
        for (q, want) in [
            ("to:jean", vec!["b-3"]),
            (r#"to:"Jean-Luc""#, vec!["b-3"]),
            (r#"to:"Jean""#, vec![]),
            ("with:will", vec!["b-4", "b-3"]),
            ("from:jean", vec!["b-2", "b-1"]),
        ] {
            let r = search(&s, q, None, 10, None).await;
            assert!(r.refused.is_empty() && r.errors.is_empty(), "{q}: {r:?}");
            assert_eq!(uuids(&r), want, "{q}");
        }
    }

    /// A contact is every handle linked to it, a merged-away contact's and
    /// a group's members' too, read when the search runs: a link made after
    /// a search changes the next one. A contact the store lacks, or a root
    /// with no contacts app, is refused by name.
    #[tokio::test]
    async fn a_contact_is_every_handle_linked_to_it() {
        use datalib_contacts::{store_path, ContactKind, Store};
        let tmp = tempfile::tempdir().unwrap();
        bridge(tmp.path()).await;
        let s = index_over(tmp.path()).await;
        let refused = search(&s, "from:contact:nobody", None, 10, None).await;
        assert!(
            refused.refused.iter().any(|r| r.contains("contacts app")),
            "{:?}",
            refused.refused
        );

        let email = |s: &str| datalib_handle::Handle::email(s).unwrap();
        let store = Store::open(&store_path(tmp.path())).await.unwrap();
        let picard = store
            .create("Captain", ContactKind::Person, &[email("jlp@e.org")])
            .await
            .unwrap();
        let couple = store
            .create("Riker and Troi", ContactKind::Group, &[])
            .await
            .unwrap();
        let riker = store
            .create("Riker", ContactKind::Person, &[email("wtr@e.org")])
            .await
            .unwrap();
        store.add_member(&couple, &riker).await.unwrap();
        store.close().await;

        let found = |q: String| {
            let s = s.clone();
            async move {
                let r = search(&s, &q, None, 10, None).await;
                assert!(r.refused.is_empty() && r.errors.is_empty(), "{q}: {r:?}");
                uuids(&r).iter().map(|u| u.to_string()).collect::<Vec<_>>()
            }
        };
        assert_eq!(found(format!("from:contact:{picard}")).await, ["b-1"]);
        assert_eq!(
            found(format!("with:contact:{picard}")).await,
            ["b-3", "b-1"]
        );
        assert_eq!(
            found(format!("with:contact:{couple}")).await,
            ["b-4", "b-3"],
            "a group reaches its members"
        );

        // Linking his Slack handle widens the next search at once.
        let store = Store::open(&store_path(tmp.path())).await.unwrap();
        store
            .link(
                &datalib_handle::Handle::slack("T1", "U_JLP").unwrap(),
                &picard,
            )
            .await
            .unwrap();
        store.close().await;
        assert_eq!(
            found(format!("from:contact:{picard}")).await,
            ["b-2", "b-1"]
        );

        let unknown = search(&s, "from:contact:no-such-id", None, 10, None).await;
        assert!(
            unknown
                .refused
                .iter()
                .any(|r| r.contains("no contact `no-such-id`")),
            "{:?}",
            unknown.refused
        );

        let params = columns::ValuesParams {
            key: "from".into(),
            typed: "capt".into(),
            q: String::new(),
        };
        let offered = search_values(State(s.clone()), Query(params))
            .await
            .unwrap()
            .0;
        assert_eq!(offered[0].value, format!("contact:{picard}"));
        assert_eq!(offered[0].label.as_deref(), Some("Captain"));
    }

    /// The index the applet serves over `root`, opened by its own
    /// [`Index::open`], with the embedding model every test in this binary
    /// shares ([`shared_embedder`]).
    pub(super) async fn index_over(root: &std::path::Path) -> Index {
        let mut s = Index::open(
            root.to_path_buf(),
            models::ModelGate::new(root),
            ONE_SEARCH_DEADLINE,
        )
        .await
        .expect("open an index");
        s.local_embedder = shared_embedder();
        s
    }

    /// The embedding model, verified by the applet's own
    /// [`open_with_models_verified`] and loaded once for the whole test
    /// binary: each test's own would hash the model's 300 MB again (its
    /// verified stamp lives beside the model, in that test's root) and
    /// load it again. Verified on a thread of its own, since a test's
    /// runtime cannot block on another.
    fn shared_embedder() -> Arc<local_embedder::LocalEmbedder> {
        static SHARED: std::sync::OnceLock<(
            tempfile::TempDir,
            Arc<local_embedder::LocalEmbedder>,
        )> = std::sync::OnceLock::new();
        SHARED
            .get_or_init(|| {
                let root = super::search_tests::fixture_root();
                let opened = std::thread::scope(|scope| {
                    scope
                        .spawn(|| {
                            tokio::runtime::Runtime::new()
                                .expect("a runtime to verify the model on")
                                .block_on(open_with_models_verified(root.path().to_path_buf()))
                        })
                        .join()
                        .expect("verifying the model panicked")
                })
                .expect("open an index with the embedding model verified");
                (root, opened.local_embedder)
            })
            .1
            .clone()
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
            among: None,
            within: None,
            tab: None,
        };
        search_handler(State(s.clone()), Query(params)).await.0
    }

    /// One part of what a search merges, on its own, read the way a search
    /// reads its pages: what each part finds has rules of its own, and the
    /// merged answer would hide a part's misses behind another's hits.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Part {
        Words,
        Meaning,
    }

    pub(super) async fn search_part(
        s: &Index,
        q: &str,
        part: Part,
        offset: usize,
        limit: usize,
    ) -> SearchResponse {
        let parsed = parse_query(q);
        assert_eq!(parsed.refusal(), None, "{q}");
        let (parsed, _) = with_people(&s.root, q, parsed).await.unwrap();
        let ranking = match part {
            Part::Words => words_ranking(s, &parsed).await,
            Part::Meaning => meaning_ranking(s, &parsed).await,
        };
        let answered = async {
            let list: Vec<results::Entry> = ranking?
                .into_iter()
                .map(|(uuid, hit)| results::Entry::new(uuid, Some(hit)))
                .collect();
            let (mut rows, next_offset) = page_rows(s, &parsed, &list, offset, limit).await?;
            let sources = columns::Sources::read(&s.root);
            for row in &mut rows {
                sources.resolve(row);
                row.link = link_of(row);
            }
            Ok::<_, SearchFailure>((rows, list.len(), next_offset))
        };
        match answered.await {
            Ok((rows, total, next_offset)) => SearchResponse {
                rows,
                total: total as u64,
                next_offset,
                query_echo: serde_json::json!({ "search_error": null }),
                ..SearchResponse::unread(Vec::new())
            },
            Err(e) => SearchResponse {
                query_echo: serde_json::json!({ "search_error": e.to_string() }),
                ..SearchResponse::unread(Vec::new())
            },
        }
    }

    pub(super) fn uuids(r: &SearchResponse) -> Vec<&str> {
        r.rows.iter().map(|row| row.uuid.as_str()).collect()
    }

    /// The first page of an order newest first by a date says how many
    /// rows are still to come, so the grid opens below them; any other
    /// order says nothing. The stamps to come are centuries out so the
    /// calendar cannot reach them.
    #[tokio::test]
    async fn a_newest_first_date_order_says_where_now_falls() {
        let tmp = tempfile::tempdir().unwrap();
        index_chats(
            tmp.path(),
            &[
                ("past-1", "2020-01-01T09:00:00+00:00"),
                ("past-2", "2020-01-02T09:00:00+00:00"),
                ("past-3", "2020-01-03T09:00:00+00:00"),
                ("ahead-1", "2900-01-01T09:00:00+00:00"),
                ("ahead-2", "2900-01-02T09:00:00+00:00"),
            ],
        )
        .await;
        let s = index_over(tmp.path()).await;
        let now_at = |r: SearchResponse| r.now_at;

        assert_eq!(now_at(search(&s, "", None, 2, None).await), Some(2));
        assert_eq!(
            now_at(search(&s, "", None, 2, Some("created_at:desc")).await),
            Some(2)
        );
        assert_eq!(
            now_at(search(&s, "", None, 2, Some("created_at")).await),
            None
        );
        assert_eq!(now_at(search(&s, "", None, 2, Some("kind")).await), None);
        assert_eq!(now_at(search(&s, "", Some(2), 2, None).await), None);
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
            sort: Vec::new(),
            within: Vec::new(),
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
        sqlx::query("UPDATE store_generation SET generation = generation + 1")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
        let s = index_over(tmp.path()).await;

        let r = search(&s, "", None, 10, None).await;
        assert!(r.rows.is_empty());
        assert_eq!(r.total, 0);
        assert_eq!(r.errors.len(), 1, "{:?}", r.errors);
        assert!(r.errors[0].contains("no such column"), "{:?}", r.errors);

        let g = groups(&s, "", "kind").await;
        assert!(g.groups.is_empty());
        assert_eq!(g.errors.len(), 1, "{:?}", g.errors);

        let params = map::MatchParams {
            q: Some("source:enterprise".to_string()),
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
    /// A document both rankings found is one entry, shown with the first
    /// ranking's hit and scored by both, so one near the top of two beats
    /// one at the top of one.
    #[test]
    fn fusion_lists_each_document_once_with_the_first_rankings_hit() {
        let ranking = |documents: &[(&str, &str)]| -> Ranking {
            documents
                .iter()
                .map(|(uuid, words)| {
                    let hit = results::Hit {
                        score: 1.0,
                        snippet: words.to_string(),
                        document: None,
                    };
                    (uuid.to_string(), hit)
                })
                .collect()
        };
        let fused = fuse(&[
            ranking(&[("away-team-chat", "warp core breach"), ("email", "warp")]),
            ranking(&[
                ("lone-top", "a chunk"),
                ("away-team-chat", ""),
                ("email", ""),
            ]),
        ]);
        let order: Vec<&str> = fused.iter().map(|(uuid, _)| uuid.as_str()).collect();
        assert_eq!(order, ["away-team-chat", "email", "lone-top"]);
        assert_eq!(fused[0].1.snippet, "warp core breach");
        assert_eq!(fused[0].1.score, 1.0 / 61.0 + 1.0 / 62.0);
        assert_eq!(fused[1].1.score, 1.0 / 62.0 + 1.0 / 63.0);
    }

    #[test]
    fn a_hit_shows_its_words_unless_it_has_none() {
        let mut matched = row("opening words");
        show_hit(&mut matched, (0.9, "the words it matched"));
        assert_eq!(matched.score, Some(0.9));
        assert_eq!(matched.snippet, "the words it matched");

        let mut unreadable = row("opening words");
        show_hit(&mut unreadable, (0.5, ""));
        assert_eq!(unreadable.score, Some(0.5));
        assert_eq!(unreadable.snippet, "opening words");
    }

    pub(super) async fn groups(s: &Index, q: &str, by: &str) -> GroupsResponse {
        let params = GroupParams {
            q: Some(q.to_string()),
            by: by.to_string(),
        };
        groups_handler(State(s.clone()), Query(params)).await.0
    }

    pub(super) async fn search_within(
        s: &Index,
        q: &str,
        within: &str,
        limit: usize,
    ) -> SearchResponse {
        let params = SearchParams {
            q: Some(q.to_string()),
            limit: Some(limit),
            offset: None,
            sort: None,
            through: None,
            among: None,
            within: Some(within.to_string()),
            tab: None,
        };
        search_handler(State(s.clone()), Query(params)).await.0
    }

    async fn crew(root: &std::path::Path) {
        index_documents(
            root,
            &[
                ("log-1", "2026-01-01T09:00:00+00:00", "Log", Some("picard")),
                ("log-2", "2026-01-02T09:00:00+00:00", "Log", Some("riker")),
                ("log-3", "2026-01-03T09:00:00+00:00", "Log", Some("picard")),
                ("note-1", "2026-01-04T09:00:00+00:00", "Note", None),
                ("note-2", "2026-01-05T09:00:00+00:00", "Note", Some("data")),
            ],
        )
        .await;
    }

    /// Every group comes with its true count and its newest row, whose
    /// labels the grid shows the group by before any of its rows are read;
    /// a nested group names a value in each column, a missing one as null.
    #[tokio::test]
    async fn a_search_falls_into_groups_each_counted_with_its_newest_row() {
        let tmp = tempfile::tempdir().unwrap();
        crew(tmp.path()).await;
        let s = index_over(tmp.path()).await;

        let by_kind = groups(&s, "", "kind").await;
        assert!(by_kind.errors.is_empty(), "{:?}", by_kind.errors);
        let mut got: Vec<(Vec<Option<String>>, u64, String)> = by_kind
            .groups
            .iter()
            .map(|g| (g.values.clone(), g.count, g.sample.uuid.clone()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                (vec![Some("Log".into())], 3, "log-3".into()),
                (vec![Some("Note".into())], 2, "note-2".into()),
            ]
        );
        assert!(by_kind.at.is_some());

        let nested = groups(&s, "", "kind,author").await;
        let mut paths: Vec<(Vec<Option<String>>, u64)> = nested
            .groups
            .iter()
            .map(|g| (g.values.clone(), g.count))
            .collect();
        paths.sort();
        assert_eq!(
            paths,
            [
                (vec![Some("Log".into()), Some("picard".into())], 2),
                (vec![Some("Log".into()), Some("riker".into())], 1),
                (vec![Some("Note".into()), None], 1),
                (vec![Some("Note".into()), Some("data".into())], 1),
            ]
        );
    }

    /// A group's rows are a search of their own: paged, newest first, and
    /// a group with no value in a column is the rows with none.
    #[tokio::test]
    async fn a_group_lists_its_own_rows() {
        let tmp = tempfile::tempdir().unwrap();
        crew(tmp.path()).await;
        let s = index_over(tmp.path()).await;

        let logs = search_within(&s, "", r#"[["kind","Log"]]"#, 2).await;
        assert_eq!(uuids(&logs), ["log-3", "log-2"]);
        assert_eq!((logs.total, logs.next_offset), (3, Some(2)));

        let unsigned = search_within(&s, "", r#"[["kind","Note"],["author",null]]"#, 10).await;
        assert_eq!(uuids(&unsigned), ["note-1"]);
    }

    /// A column no row value can group, or a group that does not read,
    /// is said rather than answered as the whole search.
    #[tokio::test]
    async fn a_grouping_that_does_not_read_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        crew(tmp.path()).await;
        let s = index_over(tmp.path()).await;

        let by_score = groups(&s, "", "score").await;
        assert!(by_score.groups.is_empty());
        assert_eq!(by_score.errors.len(), 1, "{:?}", by_score.errors);

        let garbled = search_within(&s, "", "kind:Log", 10).await;
        assert!(garbled.rows.is_empty());
        assert_eq!(garbled.errors.len(), 1, "{:?}", garbled.errors);
    }

    /// Each column of a sort breaks the ties of the one before; a sort
    /// that puts score beside a column is refused, and the default order
    /// shown.
    #[tokio::test]
    async fn a_sort_of_several_columns_breaks_ties_in_turn() {
        let tmp = tempfile::tempdir().unwrap();
        crew(tmp.path()).await;
        let s = index_over(tmp.path()).await;

        let r = search(&s, "", None, 10, Some("kind:desc,created_at:asc")).await;
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(uuids(&r), ["note-1", "note-2", "log-1", "log-2", "log-3"]);

        let refused = search(&s, "", None, 10, Some("kind,score")).await;
        assert_eq!(refused.errors.len(), 1, "{:?}", refused.errors);
        assert_eq!(
            uuids(&refused),
            ["note-2", "note-1", "log-3", "log-2", "log-1"]
        );
    }

    /// A key the search does not have is refused by name, by the search
    /// and by the groups, rather than quietly matching every row. It is
    /// a refusal, not an error: mid-keystroke, `is:do` is a filter not
    /// yet finished, and the grid toasted every error it was handed.
    #[tokio::test]
    async fn an_unknown_key_is_refused_not_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        crew(tmp.path()).await;
        let s = index_over(tmp.path()).await;

        let r = search(&s, "rank:captain", None, 10, None).await;
        assert!(r.rows.is_empty());
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        assert_eq!(r.refused.len(), 1, "{:?}", r.refused);
        assert!(r.refused[0].contains("`rank:`"), "{:?}", r.refused);

        let g = groups(&s, "rank:captain", "kind").await;
        assert!(g.groups.is_empty());
        assert!(g.errors.is_empty(), "{:?}", g.errors);
        assert_eq!(g.refused.len(), 1, "{:?}", g.refused);

        let org = search(&s, "org_name:*", None, 10, None).await;
        assert!(
            org.refused.is_empty() && org.errors.is_empty(),
            "a key every column now has: {:?} {:?}",
            org.refused,
            org.errors
        );
    }

    /// `author:*` (`from:*`, through the search terms) is the rows with an
    /// author, `-author:*` the ones without.
    #[tokio::test]
    async fn a_star_keeps_the_rows_with_a_value() {
        let tmp = tempfile::tempdir().unwrap();
        crew(tmp.path()).await;
        let s = index_over(tmp.path()).await;

        let signed = search(&s, "author:*", None, 10, None).await;
        assert_eq!(uuids(&signed), ["note-2", "log-3", "log-2", "log-1"]);
        let unsigned = search(&s, "-author:*", None, 10, None).await;
        assert_eq!(uuids(&unsigned), ["note-1"]);
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

    /// "Oldest first" and "smallest first" once opened on every row with
    /// no date or no size, and a lowercase name sorted after every capital.
    #[tokio::test]
    async fn a_sort_puts_rows_with_no_value_last_either_way_and_ignores_case() {
        let tmp = tempfile::tempdir().unwrap();
        let row = |uuid: &str, created: &str, author: Option<&str>, size: Option<i64>| {
            document_row(uuid, created, "Chat")
                .author(author.map(String::from))
                .byte_size(size)
                .build()
                .unwrap()
        };
        let mut undated = row("no-date", "2026-01-02T09:00:00+00:00", None, None);
        undated.created_at = None;
        index_rows(
            tmp.path(),
            vec![
                row(
                    "old",
                    "2026-01-01T09:00:00+00:00",
                    Some("jlpicard"),
                    Some(10),
                ),
                row("new", "2026-01-03T09:00:00+00:00", Some("Worf"), Some(30)),
                row("blank", "2026-01-02T09:00:00+00:00", Some(""), Some(20)),
                undated,
            ],
        )
        .await;
        let s = index_over(tmp.path()).await;
        let order = |sort: &'static str| {
            let s = &s;
            async move {
                let r = search(s, "", None, 10, Some(sort)).await;
                assert!(r.errors.is_empty(), "{sort}: {:?}", r.errors);
                uuids(&r).iter().map(|u| u.to_string()).collect::<Vec<_>>()
            }
        };

        assert_eq!(
            order("created_at:asc").await,
            ["old", "blank", "new", "no-date"]
        );
        assert_eq!(
            order("created_at:desc").await,
            ["new", "blank", "old", "no-date"]
        );
        assert_eq!(
            order("byte_size:asc").await,
            ["old", "blank", "new", "no-date"]
        );
        assert_eq!(
            order("byte_size:desc").await,
            ["new", "blank", "old", "no-date"]
        );

        let authors = order("author_ref:asc").await;
        assert_eq!(authors[..2], ["old", "new"], "jlpicard sorts before Worf");
        let backwards = order("author_ref:desc").await;
        assert_eq!(backwards[..2], ["new", "old"]);
        for blanks in [&authors, &backwards] {
            assert!(
                blanks[2..].contains(&"blank".to_string())
                    && blanks[2..].contains(&"no-date".to_string()),
                "a blank name and no name come last: {blanks:?}"
            );
        }
    }
}
