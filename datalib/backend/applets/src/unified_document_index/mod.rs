//! `datalib-applet unified_document_index` — the grid index, the qmd index and
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
mod body_sections;
pub(crate) mod columns;
mod grouping;
mod local_embedder;
mod map;
mod models;
mod mutes;
mod problems;
#[cfg(test)]
mod qmd_search_tests;
mod results;
#[cfg(test)]
mod search_by_example_tests;
#[cfg(test)]
mod search_finds_itself_tests;
mod search_terms;
#[cfg(test)]
mod serve_tests;
mod sql;
pub(crate) mod warm;

use body_sections::{body_sections, BodySection};
use datalib_columns::Identity;
use datalib_schema::grid_rows::{markdown_uuid_of_path, GridRow, GridRowColumn};
use datalib_unified_document_index::db::datalib_source_id;
use datalib_unified_document_index::grid_columns::GridColumn;
use datalib_unified_document_index::group::Within;
use datalib_unified_document_index::qmd::index_state::{
    resolve_markdown_states, DocReport, IndexStamp, SummaryCache,
};
use datalib_unified_document_index::qmd::vectors::Freshness;
use datalib_unified_document_index::qmd::{
    display_snippet, CollectionScope, GridIndex, QmdIndexReader, QmdIndexSummary,
};
use datalib_unified_document_index::query::{parse_query, Field, ParsedQuery, Semantic};
use datalib_unified_document_index::repo::{DocRow, DynIndexRepo, EdgeRowOut};
use datalib_unified_document_index::search::SearchRow;
use datalib_unified_document_index::sort::{newest_first_by_date, Sort};
use datalib_unified_document_index::view;
use serde::{Deserialize, Serialize};

/// The step protocol's data-root variable, which the gateway sets for every
/// applet it starts.
const DATA_ROOT_ENV: &str = datalib_env::DATALIB_DAG_DATA_ROOT;

/// Everything the handlers need, cloned per request.
#[derive(Clone)]
struct Index {
    /// The data root, for resolving a document's on-disk neighbours.
    root: Arc<PathBuf>,
    /// The grid index. Read-only: the `grid_index` step is its only
    /// writer, so holding it open across a sync is safe.
    repo: DynIndexRepo,
    /// Embeds a Meaning search's text, so the model loads once; the
    /// vectors themselves are scored here, never by qmd.
    embedder: Arc<datalib_qmd_indexer::QueryEmbedder>,
    /// One read connection to qmd's index, and what Meaning has read
    /// through it, kept across searches.
    qmd_index: Arc<datalib_unified_document_index::qmd::HeldIndex>,
    /// Which index Words and Meaning read.
    reads: SearchReads,
    /// Embeds a Meaning search's text in this process, for the search index.
    local_embedder: Arc<local_embedder::LocalEmbedder>,
    /// Every chunk's vector in the search index, kept across searches.
    held_vectors: Arc<datalib_unified_document_index::search_index::HeldVectors>,
    qmd_summary: Arc<SummaryCache>,
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

/// Which index a search reads its words and meaning from: qmd's, or our
/// own (`docs/dev/plans/search_index_from_ram.md`), named by
/// `DATALIB_SEARCH_READS` while the two are compared.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub(crate) enum SearchReads {
    Qmd,
    SearchIndex,
}

impl SearchReads {
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }

    /// qmd's when the variable is unset; a spelling this build does not
    /// know stops the applet rather than reading the other index.
    fn from_env() -> Result<Self> {
        let name = datalib_env::DATALIB_SEARCH_READS;
        match std::env::var(name) {
            Err(std::env::VarError::NotPresent) => Ok(SearchReads::Qmd),
            Ok(value) => value.parse().map_err(|_| {
                let known: Vec<&str> = <SearchReads as strum::VariantArray>::VARIANTS
                    .iter()
                    .map(|r| r.as_str())
                    .collect();
                anyhow::anyhow!("{name}={value:?} names no index; it is one of {known:?}")
            }),
            Err(e) => Err(anyhow::anyhow!("{name}: {e}")),
        }
    }
}

impl Index {
    /// `deadline` is how long one search may wait on qmd's embedder.
    async fn open(
        root: PathBuf,
        models: models::ModelGate,
        deadline: std::time::Duration,
    ) -> Result<Index> {
        let root = Arc::new(root);
        let reads = SearchReads::from_env()?;
        let repo = datalib_unified_document_index::dolt_repo::DoltRepo::open(root.clone())
            .await
            .with_context(|| format!("open the grid index under {}", root.display()))?;
        let models: Arc<dyn datalib_qmd_indexer::VerifiedModels> = Arc::new(models);
        Ok(Index {
            embedder: Arc::new(datalib_qmd_indexer::QueryEmbedder::new(
                &root,
                None,
                models.clone(),
                deadline,
            )),
            local_embedder: Arc::new(local_embedder::LocalEmbedder::new(models, deadline)),
            reads,
            held_vectors: Arc::default(),
            qmd_index: Arc::default(),
            qmd_summary: Arc::new(SummaryCache::default()),
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
/// embedding model verified first, on a thread of its own, and qmd given
/// a cold start's budget rather than a served request's.
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
        .context("provision qmd's embedding model")?;
    Index::open(root, models, ONE_SEARCH_DEADLINE).await
}

/// How long a served search waits on qmd to embed its text: under the
/// gateway's 30 s (`APPLET_READ_TIMEOUT` in datalib-http), so a qmd that
/// never answers costs this search an error naming it, and the next a
/// fresh qmd, rather than a bare proxy timeout.
const EMBED_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// How long `search_once` waits on qmd's embedder. No gateway's proxy
/// timeout to stay under, and every one starts it cold, loading its model.
const ONE_SEARCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);

/// Whether answering free text runs qmd's embedding model. Fields and
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
            .route("/qmd_state", post(qmd_state))
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
            r#"{"error":"this port answers only to the datalib gateway"}"#,
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
    /// `datalib_unified_document_index::sort`). None is newest first, or qmd's rank
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

/// Response shape for `/applet/unified_document_index/chat/{markdown_uuid}`. The body is the raw
/// QMD content minus the YAML frontmatter — the UI runs markdown-it on
/// it, one section at a time where `sections` says where they lie. We do
/// **not** ship a structured `messages[]` array; per-message scrolling
/// uses the `<div id="m-{uuid}" data-section-uuid="…">` wrappers the
/// renderer emits in the body.
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
    /// such a document has no page, so `body` is empty and the UI draws
    /// it from the applet that serves the name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    pub body: String,
    /// Where each section of `body` lies, so the UI renders each message
    /// as a fragment of its own. Absent when the renderer declares no
    /// sections, or the file changed since the index was read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sections: Option<Vec<BodySection>>,
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
    // the rows they leave (`ranked`). A qmd failure is the answer —
    // `query_echo.qmd_error`, no rows — not a quieter search in its place.
    // No index yet is not a failure: `qmd_index_missing`, and no rows.
    let mut qmd_error: Option<String> = None;
    let mut qmd_index_missing = false;
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
            qmd_index_missing = true;
            Page::default()
        }
        Err(SearchFailure::Qmd(e)) => {
            tracing::error!(query = %q, error = %e, "a free-text search failed in qmd");
            qmd_error = Some(e);
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
        match without_vectors(s, &q, &parsed).await {
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
            "qmd_error": qmd_error,
            "qmd_index_missing": qmd_index_missing,
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
    if let Some(e) = echo["qmd_error"].as_str() {
        why.push(format!("qmd could not answer the free text: {e}"));
    }
    if let Some(e) = echo["partial"].as_str() {
        why.push(e.to_string());
    }
    if echo["qmd_index_missing"].as_bool() == Some(true) {
        why.push(
            "this root has no search index yet, so free text finds nothing: a source's \
             words are searchable once its `keyword_index` step has run"
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
    /// Free text qmd could not rank: no groups, and why.
    pub qmd_error: Option<String>,
    /// Free text before the first sync has built a qmd index: no groups.
    pub qmd_index_missing: bool,
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
        Err(SearchFailure::NoIndex) => out.qmd_index_missing = true,
        Err(SearchFailure::Qmd(e)) => out.qmd_error = Some(e),
        Err(SearchFailure::Index(e)) => out.errors.push(format!("group the search: {e}")),
    }
    Json(out)
}

/// The groups of a search: over qmd's ranking for free text, else over
/// every row its structured terms match.
async fn group_list(
    s: &Index,
    q: &str,
    parsed: &ParsedQuery,
    by: &[GridRowColumn],
) -> Result<datalib_unified_document_index::group::Grouping, SearchFailure> {
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
    /// No sync has built the qmd index yet, so free text has nothing to
    /// search. A state to show, not an error.
    NoIndex,
    Qmd(String),
    Index(String),
}

impl std::fmt::Display for SearchFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SearchFailure::NoIndex => f.write_str("no search index yet"),
            SearchFailure::Qmd(e) | SearchFailure::Index(e) => f.write_str(e),
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

/// One page of the search, with qmd's score and matched words on each
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
    q: &str,
    parsed: &ParsedQuery,
) -> Result<Option<results::WithoutVectors>, SearchFailure> {
    if !embeds(parsed) || search_terms::identifiers(&parsed.free_text).is_some() {
        return Ok(None);
    }
    let stamp: Vec<IndexStamp> = match s.reads {
        SearchReads::Qmd => {
            let index = datalib_unified_document_index::qmd::qmd_index_path(&s.root);
            if !index.exists() {
                return Ok(None);
            }
            vec![IndexStamp::of(&index)]
        }
        SearchReads::SearchIndex => {
            if !datalib_runtime::layout::search_index_db(&s.root).exists() {
                return Ok(None);
            }
            let (index, cache) = datalib_unified_document_index::search_index::stamps(&s.root);
            vec![index, cache]
        }
    };
    let key = (
        q.to_string(),
        s.repo.head().await.map_err(index)?,
        stamp.clone(),
    );
    let mut coverage = s.coverage.lock().await;
    if let Some((_, counted)) = coverage.last.as_ref().filter(|(k, _)| *k == key) {
        return Ok(Some(counted.clone()));
    }
    let moving = index_moving(&stamp, std::time::SystemTime::now());
    let with_vectors =
        match coverage
            .with_vectors
            .as_ref()
            .filter(|(at, _)| moving || *at == stamp)
        {
            Some((_, read)) => read.clone(),
            None => {
                let read =
                    match s.reads {
                        SearchReads::Qmd => {
                            let Some(open) = s.qmd_index.open(&s.root).await.map_err(|e| {
                                SearchFailure::Qmd(format!("open the qmd index: {e}"))
                            })?
                            else {
                                return Ok(None);
                            };
                            open.reader.documents_with_vectors().await.map_err(|e| {
                                SearchFailure::Qmd(format!("read qmd's vectors: {e}"))
                            })?
                        }
                        SearchReads::SearchIndex => {
                            let fresh = if moving {
                                Freshness::AsHeld
                            } else {
                                Freshness::Current
                            };
                            let Some(read) = s
                                .held_vectors
                                .documents(&s.root, fresh)
                                .await
                                .map_err(embedding_failed)?
                            else {
                                return Ok(None);
                            };
                            read
                        }
                    };
                let read = Arc::new(read);
                coverage.with_vectors = Some((stamp, read.clone()));
                read
            }
        };
    let candidates = s.repo.documents_with_pages(parsed).await.map_err(index)?;
    let counted = results::WithoutVectors::among(&candidates, &with_vectors);
    coverage.last = Some((key, counted.clone()));
    Ok(Some(counted))
}

/// qmd's score, and the words the hit matched in place of the row's
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
        &datalib_unified_document_index::qmd::QmdHit,
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
    // A hit from the search index names its document, not its file.
    let unfiled: Vec<String> = todo
        .iter()
        .filter(|(_, hit)| hit.path.is_empty())
        .map(|(e, _)| e.uuid.clone())
        .collect();
    let files = if unfiled.is_empty() {
        Default::default()
    } else {
        s.repo.md_paths_for(&unfiled).await.map_err(index)?
    };
    let fetched = started.elapsed();
    // Every term, `source:` too: a source's file can hold rows filed
    // under another, which its collection's ranking does not drop.
    let kept: Option<std::collections::HashSet<String>> = if parsed.terms.is_empty() {
        None
    } else if let Some(keyed) = keyed(s, parsed).await? {
        Some(
            refs.iter()
                .filter(|r| keyed.rows.contains(&r.uuid))
                .map(|r| r.uuid.clone())
                .collect(),
        )
    } else {
        let uuids: Vec<String> = refs.iter().map(|r| r.uuid.clone()).collect();
        let listing = s
            .repo
            .filter_uuids(parsed, &uuids, &[], &[])
            .await
            .map_err(index)?;
        Some(listing.uuids.into_iter().collect())
    };
    let filtered = started.elapsed();
    let n_refs = refs.len();
    let words = datalib_unified_document_index::qmd::lex::strip_lex_syntax(&parsed.free_text);
    let idx = GridIndex::new((*s.root).clone(), refs).with_terms(&words);
    let idx = match kept {
        Some(kept) => idx.within(kept),
        None => idx,
    };
    let documents_only = parsed.documents() == Some(true);
    let work: Vec<_> = todo
        .iter()
        .map(|(e, hit)| {
            let mut hit = (*hit).clone();
            if let Some(file) = files.get(&e.uuid).filter(|_| hit.path.is_empty()) {
                hit.path = file.display().to_string();
            }
            (e.uuid.clone(), hit, e.landed.clone())
        })
        .collect();
    let n = work.len();
    tokio::task::spawn_blocking(move || {
        for (document, hit, landed) in work {
            let found = idx
                .land(&hit, documents_only)
                .map_or((document, String::new()), |(row, quoted)| {
                    (row.uuid, display_snippet(&quoted))
                });
            // Another page may have landed it first, on the same row.
            let _ = landed.set(found);
        }
    })
    .await
    .map_err(|e| SearchFailure::Qmd(format!("landing a page's hits: {e}")))?;
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
        // every hit is landed first: the slow way, and the rare one.
        land(s, parsed, &ranked).await?;
        let uuids: Vec<String> = ranked.iter().map(|e| e.row().to_string()).collect();
        let listing = s
            .repo
            .filter_uuids(parsed, &uuids, sort, within)
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

/// qmd's ranking of a free-text search, narrowed to its structured terms:
/// the list every sort and group of it is cut from, so qmd runs once per
/// search and commit rather than once per group.
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
    let (list, at) = entries_kept(s, parsed, answer.ranking, key.at.clone()).await?;
    // Terms written for another commit than the rows were read at may be
    // a pass out, and a part that failed may answer next time: answer with
    // them, and ask again.
    let keep = answer.terms_at.is_none_or(|t| t == at) && answer.partial.is_none();
    let list: Arc<Vec<results::Entry>> = Arc::new(list);
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

/// A ranking's entries the search's terms keep, and the commit they were
/// read at. A document's hit was narrowed by them before it was ranked, and
/// lands on a row they keep.
async fn entries_kept(
    s: &Index,
    parsed: &ParsedQuery,
    ranking: Ranking,
    at: Option<String>,
) -> Result<(Vec<results::Entry>, Option<String>), SearchFailure> {
    let rows: Vec<String> = ranking
        .iter()
        .filter(|(_, hit)| hit.document.is_none())
        .map(|(uuid, _)| uuid.clone())
        .collect();
    // The keys, once worked out, answer from memory; a list of documents
    // alone asks nothing.
    let (kept, at): (std::collections::HashSet<String>, Option<String>) = if rows.is_empty() {
        (Default::default(), at)
    } else if let Some(keyed) = keyed(s, parsed).await? {
        let kept = rows
            .into_iter()
            .filter(|r| keyed.rows.contains(r))
            .collect();
        (kept, at)
    } else {
        let listing = s
            .repo
            .filter_uuids(parsed, &rows, &[], &[])
            .await
            .map_err(index)?;
        (listing.uuids.into_iter().collect(), listing.at)
    };
    let list = ranking
        .into_iter()
        .filter(|(uuid, hit)| hit.document.is_some() || kept.contains(uuid))
        .map(|(uuid, hit)| results::Entry::new(uuid, Some(hit)))
        .collect();
    Ok((list, at))
}

/// One ranking of a search's rows, best first, each with its score and
/// the words it matched.
type Ranking = Vec<(String, results::Hit)>;

/// The search terms' ranking, and the grid generation it was read at.
type TermsRanking = (Ranking, Option<Option<String>>);

/// The ids, people, titles and names a row answers to, from the grid
/// index's search terms.
/// Every match when other terms narrow the search, since they are applied
/// after this list.
async fn fields_ranking(s: &Index, parsed: &ParsedQuery) -> Result<TermsRanking, SearchFailure> {
    let Some(m) = search_terms::fields(&parsed.free_text) else {
        return Ok((Vec::new(), None));
    };
    let depth = if narrows_documents(parsed) {
        usize::MAX
    } else {
        QMD_DEPTH
    };
    let found = search_terms::lookup(&s.root, &m, depth)
        .await
        .map_err(index)?;
    let found = found.ok_or(SearchFailure::NoIndex)?;
    let ranking = search_terms::rank(&found)
        .into_iter()
        .map(|(uuid, (score, snippet))| {
            let hit = results::Hit {
                score,
                snippet,
                document: None,
            };
            (uuid, hit)
        })
        .collect();
    Ok((ranking, Some(found.grid_generation)))
}

/// A search's one ranking: its words, its fields and, unless `semantic:off`,
/// its meaning, each narrowed by the search's other terms, fused by
/// reciprocal rank, one entry a document. A search of identifiers alone is
/// the fields', whose match is exact.
///
/// A part that fails costs that part and says so, since the rest are still
/// true matches and search by meaning has a model to wait for: the first
/// launch downloads it. With nothing else to show, the failure is the answer.
async fn fused_ranking(s: &Index, parsed: &ParsedQuery) -> Result<Answer, SearchFailure> {
    if search_terms::identifiers(&parsed.free_text).is_some() {
        let (ranking, terms_at) = fields_ranking(s, parsed).await?;
        return Ok(Answer {
            ranking,
            terms_at,
            partial: None,
        });
    }
    let (fields, words, meaning) = tokio::join!(
        fields_ranking(s, parsed),
        words_ranking(s, parsed),
        meaning_ranking(s, parsed),
    );
    let (fields, terms_at) = match fields {
        Ok(fields) => (fields.0, fields.1),
        Err(SearchFailure::Index(e)) => return Err(SearchFailure::Index(e)),
        Err(e) => {
            // The terms are the index's own: they fail with it.
            tracing::warn!(error = %e, "the search terms could not answer");
            (Vec::new(), None)
        }
    };
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
    let mut rankings: [Ranking; 3] = [words, meaning, fields];
    for r in &mut rankings {
        // Each cut to the depth fused: a rank past it adds next to nothing.
        r.truncate(QMD_DEPTH);
    }
    let partial = match failed {
        Some(failure) if rankings.iter().all(Vec::is_empty) => return Err(failure),
        Some(SearchFailure::Index(e)) => return Err(SearchFailure::Index(e)),
        Some(failure) => Some(match failure {
            SearchFailure::NoIndex => {
                "No sync has built the search index yet, so these results match names and ids only"
                    .to_string()
            }
            other => {
                format!("Part of this search could not answer, so results may be missing: {other}")
            }
        }),
        None => None,
    };
    let uuids: Vec<String> = rankings.iter().flatten().map(|(u, _)| u.clone()).collect();
    let doc_of = s.repo.markdowns_of(&uuids).await.map_err(index)?;
    Ok(Answer {
        ranking: fuse(&rankings, &doc_of),
        terms_at,
        partial,
    })
}

/// A ranking, the grid generation the terms in it were read at, and what
/// part of the search could not answer.
struct Answer {
    ranking: Ranking,
    terms_at: Option<Option<String>>,
    partial: Option<String>,
}

/// Reciprocal rank fusion, by document: a document scores the sum, over
/// the rankings it is in, of `1 / (K + rank)`, its rank in each being its
/// place among that ranking's documents, so one near the top of two beats
/// one at the top of one, and a chat that matched on several messages
/// counts once. It is shown on the row, and with the words, of the first
/// ranking that found it, so put the most precise first. `doc_of` names
/// each row's document; a row it does not name is its own.
fn fuse(rankings: &[Ranking], doc_of: &std::collections::HashMap<String, String>) -> Ranking {
    const K: f64 = 60.0;
    let mut fused: Ranking = Vec::new();
    let mut at: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
    for ranking in rankings {
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for (uuid, hit) in ranking {
            let doc = doc_of.get(uuid).map_or(uuid.as_str(), String::as_str);
            if !seen.insert(doc) {
                continue;
            }
            let score = 1.0 / (K + seen.len() as f64);
            match at.get(doc) {
                Some(&i) => fused[i].1.score += score,
                None => {
                    at.insert(doc, fused.len());
                    let hit = results::Hit {
                        score,
                        ..hit.clone()
                    };
                    fused.push((uuid.clone(), hit));
                }
            }
        }
    }
    fused.sort_by(|a, b| b.1.score.total_cmp(&a.1.score));
    fused.truncate(QMD_DEPTH);
    fused
}

/// The words of every document, ranked by BM25: from qmd's keyword index,
/// read from its own table so a source can answer with more than the 20
/// documents `qmd mcp` takes from each, or from the search index's
/// messages ([`SearchReads`]).
async fn words_ranking(s: &Index, parsed: &ParsedQuery) -> Result<Ranking, SearchFailure> {
    let hits = word_hits(s, parsed).await?;
    documents_of_hits(s, parsed, hits).await
}

/// A ranked document, by its id, and its hit.
type DocumentHit = datalib_unified_document_index::search_index::DocumentHit;

async fn word_hits(s: &Index, parsed: &ParsedQuery) -> Result<Vec<DocumentHit>, SearchFailure> {
    use datalib_unified_document_index::qmd::lex;
    let Some(query) = lex::fts5_query(&parsed.free_text) else {
        return Ok(Vec::new());
    };
    // Narrowed before the cut, as Meaning is: a common word's best
    // thousand documents rarely include the one the other terms ask for.
    let keyed = keyed(s, parsed).await?;
    let among = keyed.as_deref().map(|k| &k.documents);
    let scope = search_scope(s, parsed).await?;
    // The words as typed come first: BM25 alone ranks a place that says
    // the sentence below short ones that merely hold each word.
    let phrase = lex::fts5_phrase_query(&parsed.free_text);
    let mut hits = Vec::new();
    for query in phrase.iter().chain([&query]) {
        let found = match s.reads {
            SearchReads::Qmd => qmd_word_hits(s, query, scope.names(), among).await?,
            SearchReads::SearchIndex => datalib_unified_document_index::search_index::word_hits(
                &s.root,
                query,
                scope.names(),
                among,
                QMD_DEPTH,
            )
            .await
            .map_err(|e| SearchFailure::Qmd(format!("the search index's words: {e:#}")))?
            .ok_or(SearchFailure::NoIndex)?,
        };
        let seen: std::collections::HashSet<String> =
            hits.iter().map(|(d, _): &DocumentHit| d.clone()).collect();
        hits.extend(found.into_iter().filter(|(d, _)| !seen.contains(d)));
    }
    hits.truncate(QMD_DEPTH);
    Ok(hits)
}

async fn qmd_word_hits(
    s: &Index,
    query: &datalib_unified_document_index::qmd::lex::Fts5Query,
    collections: Option<&[String]>,
    among: Option<&std::collections::HashSet<String>>,
) -> Result<Vec<DocumentHit>, SearchFailure> {
    let Some(open) = s
        .qmd_index
        .open(&s.root)
        .await
        .map_err(|e| SearchFailure::Qmd(format!("open the qmd index: {e}")))?
    else {
        return Err(SearchFailure::NoIndex);
    };
    let hits = open
        .reader
        .keyword_hits(query, collections, among, QMD_DEPTH)
        .await
        .map_err(|e| SearchFailure::Qmd(format!("qmd's keyword index: {e}")))?;
    Ok(qmd_documents(hits))
}

/// qmd's hits by their document's id, which their path names. A hit whose
/// path names none is dropped, loudly, so its lost score is seen.
fn qmd_documents(hits: Vec<datalib_unified_document_index::qmd::QmdHit>) -> Vec<DocumentHit> {
    hits.into_iter()
        .filter_map(|hit| match markdown_uuid_of_path(&hit.path) {
            Some(document) => Some((document.to_string(), hit)),
            None => {
                tracing::error!(path = %hit.path, score = hit.score, "a qmd hit names no document");
                None
            }
        })
        .collect()
}

/// The documents nearest the free text by cosine, scored here over the
/// stored vectors, down to the search's cutoff, and none with
/// `semantic:off`. Structured terms narrow the documents before any is
/// scored, so a filter never empties a ranking cut too shallow to hold its
/// rows. qmd's vectors with the query embedded by qmd, or the search
/// index's with the query embedded here ([`SearchReads`]).
async fn meaning_ranking(s: &Index, parsed: &ParsedQuery) -> Result<Ranking, SearchFailure> {
    use datalib_unified_document_index::qmd::lex::{has_lex_syntax, strip_lex_syntax};
    let Some(cutoff) = parsed.semantic.cutoff() else {
        return Ok(Vec::new());
    };
    let text = if has_lex_syntax(&parsed.free_text) {
        strip_lex_syntax(&parsed.free_text)
    } else {
        parsed.free_text.clone()
    };
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let mut hits = match s.reads {
        SearchReads::Qmd => qmd_nearest(s, parsed, text).await?,
        SearchReads::SearchIndex => search_index_nearest(s, parsed, text).await?,
    };
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
    SearchFailure::Qmd(format!("{e:#}"))
}

async fn qmd_nearest(
    s: &Index,
    parsed: &ParsedQuery,
    text: String,
) -> Result<Vec<DocumentHit>, SearchFailure> {
    let Some(open) = s
        .qmd_index
        .open(&s.root)
        .await
        .map_err(|e| SearchFailure::Qmd(format!("open the qmd index: {e}")))?
    else {
        return Err(SearchFailure::NoIndex);
    };
    let reader = &open.reader;
    let fresh = if index_moving(
        &[IndexStamp::of(
            &datalib_unified_document_index::qmd::qmd_index_path(&s.root),
        )],
        std::time::SystemTime::now(),
    ) {
        Freshness::AsHeld
    } else {
        Freshness::Current
    };
    let started = std::time::Instant::now();
    let embedder = s.embedder.clone();
    let query = tokio::task::spawn_blocking(move || embedder.embed(&text))
        .await
        .map_err(|e| SearchFailure::Qmd(format!("query embedder task: {e}")))?
        .map_err(embedding_failed)?;
    let embedded = started.elapsed();
    let keyed = keyed(s, parsed).await?;
    let among = keyed.as_deref().map(|k| &k.kept);
    let narrowed = started.elapsed();
    let scope = search_scope(s, parsed).await?;
    let hits = reader
        .nearest_documents(
            &open.vectors,
            &datalib_unified_document_index::qmd::vectors::QueryVector {
                model: &query.model,
                vector: &query.vector,
            },
            scope.names(),
            among,
            QMD_DEPTH,
            fresh,
        )
        .await
        .map_err(embedding_failed)?;
    tracing::info!(
        embed_ms = embedded.as_millis() as u64,
        narrow_ms = (narrowed - embedded).as_millis() as u64,
        score_ms = (started.elapsed() - narrowed).as_millis() as u64,
        among = among.map(|a| a.len()),
        as_held = fresh == Freshness::AsHeld,
        "scored qmd's vectors"
    );
    Ok(qmd_documents(hits))
}

async fn search_index_nearest(
    s: &Index,
    parsed: &ParsedQuery,
    text: String,
) -> Result<Vec<DocumentHit>, SearchFailure> {
    let (index, cache) = datalib_unified_document_index::search_index::stamps(&s.root);
    let fresh = if index_moving(&[index, cache], std::time::SystemTime::now()) {
        Freshness::AsHeld
    } else {
        Freshness::Current
    };
    let started = std::time::Instant::now();
    let embedder = s.local_embedder.clone();
    let query = tokio::task::spawn_blocking(move || embedder.embed(&text))
        .await
        .map_err(|e| SearchFailure::Qmd(format!("query embedder task: {e}")))?
        .map_err(embedding_failed)?;
    let embedded = started.elapsed();
    let keyed = keyed(s, parsed).await?;
    let among = keyed.as_deref().map(|k| &k.kept);
    let narrowed = started.elapsed();
    let scope = search_scope(s, parsed).await?;
    let hits = s
        .held_vectors
        .nearest(&s.root, &query, scope.names(), among, QMD_DEPTH, fresh)
        .await
        .map_err(embedding_failed)?
        .ok_or(SearchFailure::NoIndex)?;
    tracing::info!(
        embed_ms = embedded.as_millis() as u64,
        narrow_ms = (narrowed - embedded).as_millis() as u64,
        score_ms = (started.elapsed() - narrowed).as_millis() as u64,
        among = among.map(|a| a.len()),
        as_held = fresh == Freshness::AsHeld,
        "scored the search index's vectors"
    );
    Ok(hits)
}

/// What the structured terms keep of each document they reach: the
/// messages they keep, or all of it when they keep only the document's own
/// row. A long chat whose last message falls in a date window keeps its
/// row too, and that must not bring back its other days.
fn kept_of(
    rows: Vec<(String, String, bool)>,
) -> std::collections::HashMap<String, datalib_unified_document_index::qmd::vectors::Kept> {
    use datalib_unified_document_index::qmd::vectors::Kept;
    let mut sections: std::collections::HashMap<String, std::collections::HashSet<String>> =
        std::collections::HashMap::new();
    for (uuid, document, is_document) in rows {
        let kept = sections.entry(document).or_default();
        if !is_document {
            kept.insert(uuid);
        }
    }
    sections
        .into_iter()
        .map(|(document, kept)| {
            let kept = if kept.is_empty() {
                Kept::Whole
            } else {
                Kept::Sections(kept)
            };
            (document, kept)
        })
        .collect()
}

/// What the search's structured terms keep, worked out once for this query
/// and the index's commit and shared by every answer and page of it.
/// `None` when they keep whole sources, which `collection_scope` narrows.
async fn keyed(
    s: &Index,
    parsed: &ParsedQuery,
) -> Result<Option<Arc<results::Keyed>>, SearchFailure> {
    if !narrows_documents(parsed) {
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

/// Whether a search's structured terms narrow it below whole sources,
/// which `collection_scope` already does for a positive `source:`.
fn narrows_documents(parsed: &ParsedQuery) -> bool {
    let scoped = collection_scope(parsed);
    parsed.terms.iter().any(|t| {
        let scopes = matches!(t.field, Field::Column(k) if k.column == GridRowColumn::SourceId)
            && !t.negate
            && scoped.names().is_some_and(|n| n.contains(&t.value));
        !scopes
    })
}

/// How many hits qmd ranks for one free-text search: every page of it is
/// cut from these.
const QMD_DEPTH: usize = 1_000;

/// How long the qmd index must sit still before a search reads what
/// changed in it. A sync's embed step commits every few seconds, and on a
/// large library reading the documents again at each commit took a search
/// past its deadline; until then a search answers from what it last read,
/// and the warm loop reads the rest once the sync is done.
const INDEX_SETTLE: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether the index changed within [`INDEX_SETTLE`] of `now`. A change
/// stamped after `now` is a clock that moved back, and counts as recent.
fn index_moving(stamps: &[IndexStamp], now: std::time::SystemTime) -> bool {
    stamps.iter().any(|stamp| {
        stamp
            .last_change()
            .is_some_and(|at| now.duration_since(at).map_or(true, |d| d < INDEX_SETTLE))
    })
}

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

/// What a ranking reads: what the query names, of the sources the grid holds.
///
/// The qmd index can hold a source the grid does not: one turned off keeps
/// its collection, so its embeddings survive, while the grid leaves out a
/// render store it can no longer read. A hit there has no row to show, yet
/// it would take a place in the ranking's cut and its vectors would be held
/// and scored.
async fn search_scope(s: &Index, parsed: &ParsedQuery) -> Result<CollectionScope, SearchFailure> {
    let held = grid_sources(s).await?;
    Ok(CollectionScope::Only(match collection_scope(parsed) {
        CollectionScope::All => held.to_vec(),
        CollectionScope::Only(named) => named.into_iter().filter(|n| held.contains(n)).collect(),
    }))
}

/// The qmd collections a parsed query asks for.
///
/// There is one collection per group, named after it, and
/// `source:` names a group — so a scoped query becomes a scoped
/// retrieval instead of a filter over whatever the global top-N happened
/// to contain. That difference is the whole point: a source whose hits
/// never enter the global list cannot be recovered by filtering, so
/// `source:x <text>` used to come back empty while `x` matched
/// strongly on its own.
///
/// Only positive terms scope. A negated `-source:x` is "every
/// collection but this one", which needs a list this function has no way
/// to obtain; it stays unscoped and is left to the SQL filter, which is
/// no worse than before.
///
/// `source:datalib` names no collection: the storage rows are filed
/// under datalib but their markdown still sits in the measured source's
/// tree, so there is nothing called `datalib` to retrieve from. It is
/// dropped here and left to the SQL filter — scoping to a collection
/// that does not exist would come back empty.
fn collection_scope(parsed: &ParsedQuery) -> CollectionScope {
    let names: Vec<String> = parsed
        .terms
        .iter()
        .filter(|t| {
            matches!(t.field, Field::Column(k) if k.column == GridRowColumn::SourceId) && !t.negate
        })
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

#[derive(Deserialize)]
struct PeopleBody {
    handles: Vec<String>,
}

/// Every source's account of whoever holds each asked-for handle, ranked
/// (`datalib_unified_document_index::people`). A handle no source mentions is
/// simply absent.
async fn people(
    State(s): State<Index>,
    Json(body): Json<PeopleBody>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match s.repo.people_for_handles(&body.handles).await {
        Ok(rows) => Ok(Json(serde_json::json!({
            "people": datalib_unified_document_index::people::merge_and_rank(rows)
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
                datalib_unified_document_index::terms_keys::CONTACT,
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
/// structured terms; its free text does not, which would be a qmd search
/// per keystroke.
async fn search_values(
    State(s): State<Index>,
    Query(p): Query<columns::ValuesParams>,
) -> Result<Json<Vec<datalib_columns::ValueSuggestion>>, (StatusCode, String)> {
    if let Some(key) = datalib_unified_document_index::terms_keys::key(&p.key) {
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

async fn chat(
    State(s): State<Index>,
    Path(markdown_uuid): Path<String>,
) -> Result<Json<ChatResponse>, StatusCode> {
    // QMDs are write-only output. We read the file just to ship its body
    // to the UI as-is; structured metadata comes from grid_rows. Per-section
    // anchors in the body (`<div id="m-{uuid}" data-section-uuid="…">`)
    // let the UI scroll-and-highlight without a structured chat schema.
    // One UUID → one file: no enumeration, no fallbacks.
    let file = s
        .repo
        .document_file(&markdown_uuid)
        .await
        .map_err(|e| {
            tracing::error!(document = %markdown_uuid, error = %e, "could not find the document's file");
            StatusCode::INTERNAL_SERVER_ERROR
        })?
        .ok_or(StatusCode::NOT_FOUND)?;
    let meta = s
        .repo
        .chat_meta(&markdown_uuid)
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    let (body, sections) = match std::fs::read_to_string(&file.path) {
        Ok(raw) => {
            let body = strip_frontmatter(&raw);
            let sections = file.section_layout.as_deref().and_then(|layout| {
                let found = body_sections(&raw, raw.len() - body.len(), layout);
                if found.is_none() {
                    tracing::warn!(
                        document = %markdown_uuid,
                        "the stored section layout does not spell the file; \
                         the body goes out without one",
                    );
                }
                found
            });
            (body.to_string(), sections)
        }
        // A picture's document is drawn by its applet, not from a page.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && meta.image.is_some() => {
            (String::new(), None)
        }
        Err(e) => {
            tracing::error!(
                document = %markdown_uuid,
                path = %file.path.display(),
                error = %e,
                "could not read the document's file",
            );
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
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
        sections,
        outgoing_edges,
        problems,
        errors,
    }))
}

/// Strip a leading YAML front-matter block: a `---` line, then everything
/// up to the next line that is `---` by itself. Text trimming, not
/// parsing; a line that only starts with `---` closes nothing.
fn strip_frontmatter(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("---\n") else {
        return text;
    };
    let mut at = 0;
    for line in rest.split_inclusive('\n') {
        at += line.len();
        if line.trim_end_matches(['\n', '\r']) == "---" {
            return &rest[at..];
        }
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A search reads what changed in the qmd index only once the index has
    /// sat still for `INDEX_SETTLE`: during a sync it answers from what it
    /// holds rather than re-reading the index at every embed commit.
    #[test]
    fn an_index_written_within_the_settle_time_is_moving() {
        let tmp = tempfile::tempdir().unwrap();
        let index = tmp.path().join("index.sqlite");
        assert!(
            !index_moving(&[IndexStamp::of(&index)], std::time::SystemTime::now()),
            "no index is not moving"
        );
        std::fs::write(&index, b"").unwrap();
        let stamp = IndexStamp::of(&index);
        let at = stamp.last_change().unwrap();
        let second = std::time::Duration::from_secs(1);
        assert!(index_moving(std::slice::from_ref(&stamp), at));
        assert!(index_moving(
            std::slice::from_ref(&stamp),
            at + INDEX_SETTLE - second
        ));
        assert!(!index_moving(
            std::slice::from_ref(&stamp),
            at + INDEX_SETTLE
        ));
        assert!(
            index_moving(std::slice::from_ref(&stamp), at - second),
            "a clock that moved back"
        );

        let mut wal = index.clone().into_os_string();
        wal.push("-wal");
        std::fs::write(&wal, b"").unwrap();
        let later = std::fs::File::options().write(true).open(&wal).unwrap();
        later.set_modified(at + INDEX_SETTLE * 2).unwrap();
        assert!(
            index_moving(&[IndexStamp::of(&index)], at + INDEX_SETTLE * 2 + second),
            "a commit lands in the write-ahead log, not the index file"
        );
    }

    /// What `source:` hands qmd: every positive id; nothing for a
    /// negated one or for `datalib`, which has no collection.
    #[test]
    fn source_ids_become_the_collections_qmd_searches() {
        let scope = |q: &str| collection_scope(&parse_query(q));
        let only =
            |names: &[&str]| CollectionScope::Only(names.iter().map(|n| n.to_string()).collect());
        assert_eq!(scope("tea"), CollectionScope::All);
        assert_eq!(scope("tea source:a"), only(&["a"]));
        assert_eq!(scope("tea source:a source:b"), only(&["a", "b"]));
        assert_eq!(scope("tea -source:a"), CollectionScope::All);
        assert_eq!(scope("tea -source:a source:b"), only(&["b"]));
        assert_eq!(
            scope(&format!("tea source:{}", datalib_source_id())),
            CollectionScope::All
        );
    }

    /// A chat whose last message falls in a date window keeps its own row
    /// as well as that day's messages; meaning must rank it by those
    /// messages, not by every day of it.
    #[test]
    fn a_kept_document_row_counts_whole_only_without_kept_messages() {
        use datalib_unified_document_index::qmd::vectors::Kept;
        let row = |uuid: &str, document: &str, is_document| {
            (uuid.to_string(), document.to_string(), is_document)
        };
        let kept = kept_of(vec![
            row("bridge-log", "bridge-log", true),
            row("stardate-2", "bridge-log", false),
            row("ten-forward", "ten-forward", true),
        ]);
        assert_eq!(
            kept["bridge-log"],
            Kept::Sections(["stardate-2".to_string()].into())
        );
        assert_eq!(kept["ten-forward"], Kept::Whole);
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
        assert_eq!(strip_frontmatter("---\n---\n\nbody"), "\nbody");
    }

    /// Only a `---` line by itself closes the block; one that starts a
    /// value used to, and the rest of the front matter became body
    /// (#992).
    #[test]
    fn a_dash_run_inside_the_front_matter_closes_nothing() {
        assert_eq!(
            strip_frontmatter("---\ntitle: a\n---b\n---x: y\n---\n\nbody\n"),
            "\nbody\n"
        );
    }

    /// Commits one chat per `(uuid, created_at)` to the root's grid index,
    /// the way the `grid_index` step writes and seals it.
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
    /// the `grid_index` step writes and seals it.
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
    }

    /// `row` as a document of its own, filed where its id files it under
    /// the source its `qmd_path` names, else `enterprise`.
    fn own_document(
        root: &std::path::Path,
        mut row: datalib_schema::grid_rows::GridRow,
    ) -> datalib_etl_render::grid_index::RenderedMarkdown {
        let source = row
            .qmd_path
            .as_deref()
            .and_then(|p| p.split_once('/'))
            .map_or("enterprise".to_string(), |(s, _)| s.to_string());
        let rel = format!(
            "{source}/render_markdown/{}",
            datalib_schema::grid_rows::doc_file_rel(&row.uuid)
        );
        row.qmd_path = Some(rel.clone());
        datalib_etl_render::grid_index::RenderedMarkdown {
            section_layout: None,
            content_hash: None,
            markdown_uuid: row.uuid.clone(),
            source_id: source,
            upstream_cursor: None,
            bucket_key: None,
            md_path: root.join(rel),
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            edges: Vec::new(),
            contacts: Vec::new(),
            photos: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// Commits each row as a document of its own.
    async fn index_rows(root: &std::path::Path, rows: Vec<datalib_schema::grid_rows::GridRow>) {
        use datalib_etl_render::grid_index::{apply_one, open_index, WriteLock};

        let pool = open_index(&datalib_runtime::layout::grid_index_db(root))
            .await
            .unwrap();
        let lock = WriteLock::for_index(pool.clone());
        for row in rows {
            apply_one(&lock, root, &own_document(root, row))
                .await
                .unwrap();
        }
        datalib_etl_render::grid_index::seal(&lock).await.unwrap();
        pool.close().await;
    }

    /// The route says where each message of the body begins, from the
    /// sections the renderer handed over, so the UI can render each as a
    /// fragment of its own; and says nothing once a render has rewritten
    /// the file since the index was read, rather than cut it wrongly.
    #[tokio::test]
    async fn the_document_route_sends_where_each_message_lies() {
        use datalib_etl_render::grid_index::{apply_one, open_index, WriteLock};
        use datalib_etl_render::section::{join, Section};

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let uuid = "00000000-0000-4000-8000-0000000000d1";
        let wrapper = |m: &str, text: &str| {
            format!(
                "<div id=\"m-{m}\" data-section-uuid=\"{m}\" class=\"msg msg--claude\">\n\n{text}\n\n</div>\n\n"
            )
        };
        let sections = vec![
            Section::unkeyed("---\ntitle: \"Ten Forward\"\n---\n\n# Ten Forward\n\n".into()),
            Section::keyed(
                "m1",
                wrapper("m1", "Tea, Earl Grey 🫖 <div style=\"display:none\">"),
            ),
            Section::keyed("m2", wrapper("m2", "Make it so.")),
        ];
        let mut doc = own_document(
            root,
            document_row(uuid, "2026-01-01T00:00:00Z", "Chat")
                .build()
                .unwrap(),
        );
        std::fs::create_dir_all(doc.md_path.parent().unwrap()).unwrap();
        std::fs::write(&doc.md_path, join(&sections)).unwrap();
        doc.sections = sections.clone();
        let pool = open_index(&datalib_runtime::layout::grid_index_db(root))
            .await
            .unwrap();
        let lock = WriteLock::for_index(pool.clone());
        apply_one(&lock, root, &doc).await.unwrap();
        datalib_etl_render::grid_index::seal(&lock).await.unwrap();
        pool.close().await;

        let s = index_over(root).await;
        let Json(got) = chat(State(s.clone()), Path(uuid.to_string()))
            .await
            .unwrap();
        let sent = got.sections.expect("the route sends the sections");
        assert_eq!(
            sent.iter().map(|b| b.uuid.as_deref()).collect::<Vec<_>>(),
            [None, Some("m1"), Some("m2")]
        );
        let units: Vec<u16> = got.body.encode_utf16().collect();
        let mut at = 0;
        let pieces: Vec<String> = sent
            .iter()
            .map(|b| {
                let piece = String::from_utf16(&units[at..at + b.len]).unwrap();
                at += b.len;
                piece
            })
            .collect();
        assert_eq!(at, units.len(), "the sections cover the body");
        assert_eq!(pieces[0], "\n# Ten Forward\n\n", "front matter dropped");
        assert_eq!(pieces[1], sections[1].md);
        assert_eq!(pieces[2], sections[2].md);

        std::fs::write(&doc.md_path, "---\n---\n\nrewritten since\n").unwrap();
        let Json(got) = chat(State(s), Path(uuid.to_string())).await.unwrap();
        assert_eq!(got.body, "\nrewritten since\n");
        assert!(got.sections.is_none(), "a stale layout is not sent");
    }

    /// Read from the search index, a hit is the message holding the words,
    /// and lands there, quoted from it. A word only the thread's title
    /// holds finds it too, and an excluded word drops a message, not the
    /// thread.
    #[tokio::test]
    async fn a_words_search_reads_the_search_index_and_lands_on_the_message() {
        use datalib_etl_render::grid_index::{apply_one, open_index, WriteLock};
        use datalib_etl_render::section::{join, Section};
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
            root,
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
            message.qmd_path = doc.rows[0].qmd_path.clone();
            doc.rows.push(message);
        }
        std::fs::create_dir_all(doc.md_path.parent().unwrap()).unwrap();
        std::fs::write(&doc.md_path, join(&sections)).unwrap();
        doc.sections = sections.clone();
        let pool = open_index(&datalib_runtime::layout::grid_index_db(root))
            .await
            .unwrap();
        let lock = WriteLock::for_index(pool.clone());
        apply_one(&lock, root, &doc).await.unwrap();
        datalib_etl_render::grid_index::seal(&lock).await.unwrap();
        pool.close().await;

        let index =
            datalib_search_index::Index::open(&datalib_runtime::layout::search_index_db(root))
                .await
                .unwrap();
        index
            .put(&[IndexDoc {
                source_id: doc.source_id.clone(),
                markdown_uuid: uuid.into(),
                content_hash: "h".into(),
                title: "Engineering".into(),
                sections: sections
                    .iter()
                    .map(|s| IndexSection {
                        uuid: s.uuid.clone(),
                        text: s.md.clone(),
                    })
                    .collect(),
            }])
            .await
            .unwrap();
        index.close().await;

        let mut s = index_over(root).await;
        s.reads = SearchReads::SearchIndex;
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

    /// The switch reads the two spellings it knows and refuses any other,
    /// rather than quietly reading qmd's index.
    #[test]
    fn the_search_reads_switch_names_one_index_or_stops() {
        assert_eq!("qmd".parse::<SearchReads>().ok(), Some(SearchReads::Qmd));
        assert_eq!(
            "search_index".parse::<SearchReads>().ok(),
            Some(SearchReads::SearchIndex)
        );
        assert!("qdrant".parse::<SearchReads>().is_err());
        for r in <SearchReads as strum::VariantArray>::VARIANTS {
            assert_eq!(r.as_str().parse::<SearchReads>().ok(), Some(*r));
        }
    }

    /// Whether this process has a file open whose inode is `ino` and,
    /// with `deleted`, that no name links to any more. A deleted file's
    /// number is free for the next file created, by any test in this
    /// process, so after a delete only the link count tells them apart.
    fn held_open(ino: u64, deleted: bool) -> bool {
        use std::os::unix::fs::MetadataExt;
        std::fs::read_dir("/dev/fd")
            .unwrap()
            .flatten()
            .filter_map(|fd| std::fs::metadata(fd.path()).ok())
            .any(|m| m.ino() == ino && (!deleted || m.nlink() == 0))
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
        let found = |rows: Vec<datalib_unified_document_index::search::SearchRow>| {
            rows.into_iter().map(|r| r.uuid).collect::<Vec<_>>()
        };
        let uuids = vec![a.to_string(), b.to_string(), c.to_string()];
        assert_eq!(found(s.repo.rows_by_uuids(&uuids).await.unwrap()), [a, b]);

        let db = datalib_runtime::layout::grid_index_db(root);
        let before = std::fs::metadata(&db).unwrap().ino();
        assert!(
            held_open(before, false),
            "the first read left no reader open"
        );
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", db.display()));
        }
        index_chats(root, &[(c, "2026-01-03T00:00:00Z")]).await;

        assert_eq!(found(s.repo.rows_by_uuids(&uuids).await.unwrap()), [c]);
        assert!(
            !held_open(before, true),
            "the reader still holds the grid index a reset deleted"
        );
    }

    /// A pasted uuid or address is answered from the search terms, never by
    /// qmd, which this root has no index for.
    #[tokio::test]
    async fn an_identifier_search_is_answered_from_the_terms_without_qmd() {
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
        let qmd_missing = |r: &SearchResponse| r.query_echo["qmd_index_missing"] == true;
        let s = index_over(tmp.path()).await;
        let by_id = search(&s, a, None, 10, None).await;
        assert!(!qmd_missing(&by_id), "{:?}", by_id.query_echo);
        assert_eq!(uuids(&by_id), [a]);
        assert_eq!(by_id.rows[0].snippet, format!("id: {a}"));

        let by_address = search(&s, "Bo@Example.com", None, 10, None).await;
        assert_eq!(uuids(&by_address), [b]);
        assert_eq!(by_address.rows[0].snippet, "from: email:bo@example.com");

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
            apply_one(&lock, root, &own_document(root, row))
                .await
                .unwrap();
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

    /// The index `datalib-applet search` answers from, opened by the
    /// applet's own [`open_with_models_verified`] rather than by
    /// repeating it here. A test is a caller with no server warming its
    /// models, which is exactly what that function is for.
    pub(super) async fn index_over(root: &std::path::Path) -> Index {
        open_with_models_verified(root.to_path_buf())
            .await
            .expect("open an index with qmd's embedding model verified")
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
        Fields,
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
            Part::Fields => fields_ranking(s, &parsed).await.map(|r| r.0),
            Part::Words => words_ranking(s, &parsed).await,
            Part::Meaning => meaning_ranking(s, &parsed).await,
        };
        let answered = async {
            let at = s.repo.head().await.map_err(index)?;
            let (list, _) = entries_kept(s, &parsed, ranking?, at).await?;
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
                query_echo: serde_json::json!({ "qmd_error": null }),
                ..SearchResponse::unread(Vec::new())
            },
            Err(e) => SearchResponse {
                query_echo: serde_json::json!({ "qmd_error": e.to_string() }),
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
    /// A chat each ranking found on a different message is one entry,
    /// placed on the first ranking's message with its words, scored by
    /// every ranking that found it; repeats within a ranking push no other
    /// document down (#1438).
    #[test]
    fn fusion_lists_each_document_once_on_the_first_rankings_row() {
        let ranking = |rows: &[(&str, &str)]| -> Ranking {
            rows.iter()
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
        let doc_of: std::collections::HashMap<String, String> = ["m12", "m40", "m3", "m5"]
            .into_iter()
            .map(|m| (m.to_string(), "away-team-chat".to_string()))
            .collect();
        let fused = fuse(
            &[
                ranking(&[("m12", "warp core breach"), ("email", "warp")]),
                ranking(&[("m40", "a chunk"), ("email", "")]),
                ranking(&[("m3", ""), ("m5", ""), ("email", "from: riker")]),
            ],
            &doc_of,
        );
        let order: Vec<&str> = fused.iter().map(|(uuid, _)| uuid.as_str()).collect();
        assert_eq!(order, ["m12", "email"]);
        assert_eq!(fused[0].1.snippet, "warp core breach");
        assert_eq!(fused[0].1.score, 3.0 / 61.0);
        assert_eq!(fused[1].1.score, 3.0 / 62.0, "second in each ranking");
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
