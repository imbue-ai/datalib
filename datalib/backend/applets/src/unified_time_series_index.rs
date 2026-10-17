//! `datalib-applet unified_time_series_index` — serves the time-series
//! index: the list of time series, one time series, and points over a
//! range, downsampled to a budget. Each request reads the index's newest
//! commit, and the points from each source's store at the commit the
//! index recorded for it, all through detached read-only opens, so the
//! applet holds no file between requests. `docs/dev/time_series.md` is
//! the design.

use std::collections::{BTreeMap, HashMap};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    middleware,
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use datalib_etl::doltlite_raw::{self, Reader};
use datalib_etl_time_series::read::{self, Answer};
use datalib_etl_time_series::store;
use datalib_time_series_schema::{TimeSeriesRow, TimeSeriesSourceRow, Unit};
use serde::{Deserialize, Serialize};
use serde_json::json;

const DATA_ROOT_ENV: &str = "DATALIB_DAG_DATA_ROOT";
/// Points per time series when a request names no budget: about one per
/// pixel of a wide chart.
const DEFAULT_BUDGET: i64 = 1_000;
const MAX_BUDGET: i64 = 20_000;

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
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        let bound = listener.local_addr().context("read the bound address")?;
        let gate = Arc::new(crate::gate::Gate::from_env(bound.port())?);
        let app = routes(Arc::new(root)).layer(middleware::from_fn_with_state(
            gate,
            crate::unified_document_index::require_gateway,
        ));
        tracing::info!(address = %bound, "listening");
        crate::announce_port(bound.port());
        axum::serve(listener, app).await.context("serve")
    })
}

fn routes(root: Arc<PathBuf>) -> Router {
    Router::new()
        .route("/time_series", get(list))
        .route("/time_series/{id}", get(one))
        .route("/points", post(points))
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .with_state(root)
}

type AppState = State<Arc<PathBuf>>;

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

fn ours(e: anyhow::Error) -> ApiError {
    let msg = format!("{e:#}");
    tracing::error!(error = %msg, "time-series index");
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, msg)
}

/// The index at its newest commit, or `None` before its first seal.
async fn open(root: &std::path::Path) -> Result<Option<Reader>> {
    let db = datalib_runtime::layout::time_series_index_db(root);
    if !db.exists() {
        return Ok(None);
    }
    doltlite_raw::open_reader(&db, None).await
}

/// One time series as the routes answer it: its row, without the key
/// that is only its store's own, and its unit read for a chart.
#[derive(Debug, Serialize, PartialEq)]
struct Listed {
    time_series_id: String,
    source_id: String,
    about_uuid: Option<String>,
    about_name: Option<String>,
    name: String,
    human_name: String,
    /// The UCUM code.
    unit: String,
    unit_label: String,
    /// Two time series can share an axis when this is the same
    /// ([`Unit::quantity`]); a value times `unit_factor` plus
    /// `unit_offset` is in the coherent SI unit.
    quantity: String,
    unit_factor: f64,
    unit_offset: f64,
    value_type: &'static str,
    time_kind: &'static str,
    point_count: i64,
    first_t: Option<i64>,
    last_t: Option<i64>,
}

impl From<&TimeSeriesRow> for Listed {
    fn from(r: &TimeSeriesRow) -> Self {
        // A unit only a newer build knows is shown as written, and keeps
        // an axis to itself.
        let (unit_label, quantity, unit_factor, unit_offset) = match Unit::parse(&r.unit) {
            Ok(u) => (u.label().to_string(), u.quantity(), u.factor(), u.offset()),
            Err(_) => (r.unit.clone(), format!("unknown:{}", r.unit), 1.0, 0.0),
        };
        Listed {
            time_series_id: r.time_series_id.clone(),
            source_id: r.source_id.clone(),
            about_uuid: r.about_uuid.clone(),
            about_name: r.about_name.clone(),
            name: r.name.clone(),
            human_name: r.human_name.clone(),
            unit: r.unit.clone(),
            unit_label,
            quantity,
            unit_factor,
            unit_offset,
            value_type: r.value_type.as_str(),
            time_kind: r.time_kind.as_str(),
            point_count: r.point_count,
            first_t: r.first_t,
            last_t: r.last_t,
        }
    }
}

#[derive(Deserialize, Default)]
struct ListQuery {
    source_id: Option<String>,
    /// Words every one of which the human name, the name or the name of
    /// what it is about holds, ignoring case.
    q: Option<String>,
}

fn matches(r: &TimeSeriesRow, query: &ListQuery) -> bool {
    if query.source_id.as_ref().is_some_and(|s| *s != r.source_id) {
        return false;
    }
    let about = r.about_name.as_deref().unwrap_or("");
    let hay = format!("{} {} {about}", r.human_name, r.name).to_lowercase();
    query
        .q
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .all(|w| hay.contains(&w.to_lowercase()))
}

/// The index's rows and its record of the source stores, at its newest
/// commit; empty before its first seal.
async fn index(root: &std::path::Path) -> Result<(Vec<TimeSeriesRow>, Vec<TimeSeriesSourceRow>)> {
    let Some(reader) = open(root).await? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let read = async {
        Ok::<_, anyhow::Error>((
            store::rows(reader.pool()).await?,
            store::sources(reader.pool()).await?,
        ))
    }
    .await;
    reader.close().await;
    read
}

async fn list(
    State(root): AppState,
    Query(query): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (all, _) = index(&root).await.map_err(ours)?;
    let listed: Vec<Listed> = all
        .iter()
        .filter(|r| matches(r, &query))
        .map(Listed::from)
        .collect();
    Ok(Json(json!({ "time_series": listed })))
}

async fn one(
    State(root): AppState,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (all, _) = index(&root).await.map_err(ours)?;
    match all.iter().find(|r| r.time_series_id == id) {
        Some(r) => Ok(Json(json!(Listed::from(r)))),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no time series {id}"),
        )),
    }
}

#[derive(Deserialize)]
struct PointsRequest {
    ids: Vec<String>,
    from: Option<i64>,
    to: Option<i64>,
    budget: Option<i64>,
}

#[derive(Serialize)]
struct Answered {
    #[serde(flatten)]
    listed: Listed,
    #[serde(flatten)]
    points: Answer,
}

/// A time series the index names whose points could not be read, and
/// why: its source's store is gone, or no longer has the commit the
/// index recorded.
#[derive(Serialize)]
struct Unreadable {
    time_series_id: String,
    error: String,
}

async fn points(
    State(root): AppState,
    Json(req): Json<PointsRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let budget = req.budget.unwrap_or(DEFAULT_BUDGET).clamp(1, MAX_BUDGET);
    let (all, sources) = index(&root).await.map_err(ours)?;
    let by_id: HashMap<&str, &TimeSeriesRow> =
        all.iter().map(|r| (r.time_series_id.as_str(), r)).collect();
    let mut missing = Vec::new();
    let mut by_source: BTreeMap<&str, Vec<&TimeSeriesRow>> = BTreeMap::new();
    for id in &req.ids {
        match by_id.get(id.as_str()) {
            Some(row) => by_source.entry(&row.source_id).or_default().push(row),
            None => missing.push(id.clone()),
        }
    }
    let mut answered: HashMap<&str, Answered> = HashMap::new();
    let mut unreadable = Vec::new();
    for (source_id, rows) in by_source {
        let record = sources.iter().find(|s| s.source_id == source_id);
        match read_source(&root, record, &rows, req.from, req.to, budget).await {
            Ok(points) => {
                for (row, points) in rows.iter().zip(points) {
                    answered.insert(
                        &row.time_series_id,
                        Answered {
                            listed: Listed::from(*row),
                            points,
                        },
                    );
                }
            }
            Err(e) => {
                let error = format!("{e:#}");
                tracing::error!(source_id, error = %error, "read a source's time series");
                unreadable.extend(rows.iter().map(|r| Unreadable {
                    time_series_id: r.time_series_id.clone(),
                    error: error.clone(),
                }));
            }
        }
    }
    let answered: Vec<Answered> = req
        .ids
        .iter()
        .filter_map(|id| answered.remove(id.as_str()))
        .collect();
    Ok(Json(json!({
        "time_series": answered,
        "missing": missing,
        "unreadable": unreadable,
    })))
}

/// `rows`' points from their source's store, opened at the commit the
/// index recorded for it.
async fn read_source(
    root: &std::path::Path,
    record: Option<&TimeSeriesSourceRow>,
    rows: &[&TimeSeriesRow],
    from: Option<i64>,
    to: Option<i64>,
    budget: i64,
) -> Result<Vec<Answer>> {
    let record = record.context("the index has no record of where this source's store is")?;
    let db = root.join(&record.store);
    let reader = doltlite_raw::open_reader(&db, Some(&record.commit_hash))
        .await
        .with_context(|| format!("open {} at {}", record.store, record.commit_hash))?
        .with_context(|| format!("{} holds nothing at {}", record.store, record.commit_hash))?;
    let mut out = Vec::with_capacity(rows.len());
    let mut failed = None;
    for row in rows {
        let from = from
            .unwrap_or(i64::MIN)
            .max(row.first_t.unwrap_or(i64::MIN));
        let to = to.unwrap_or(i64::MAX).min(row.last_t.unwrap_or(i64::MAX));
        match read::points(reader.pool(), row.ts_key, from, to, budget).await {
            Ok(points) => out.push(points),
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    reader.close().await;
    match failed {
        Some(e) => Err(e),
        None => Ok(out),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request};
    use datalib_etl::doltlite_raw::StoreKind;
    use datalib_etl_time_series::index;
    use datalib_time_series_schema::{Points, TimeKind, TimeSeries, ValueType};
    use tower::ServiceExt;

    async fn call(app: &Router, req: Request<Body>) -> (StatusCode, serde_json::Value) {
        let resp = app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or_default())
    }

    fn post(body: serde_json::Value) -> Request<Body> {
        Request::post("/points")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    /// An index sealed by its writer, read through the routes: the list,
    /// one time series, a range whole and bucketed from the source's store
    /// at the commit the index recorded rather than its newest, an id the
    /// index does not hold, and a store that is gone.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_routes_read_the_sources_at_the_commits_the_index_recorded() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().to_path_buf();
        let app = routes(Arc::new(root.clone()));
        let (status, empty) = call(
            &app,
            Request::get("/time_series").body(Body::empty()).unwrap(),
        )
        .await;
        assert_eq!(
            (status, empty["time_series"].as_array().map(Vec::len)),
            (StatusCode::OK, Some(0))
        );

        let temp = TimeSeries {
            time_series_id: "bridge-temp".into(),
            source_id: "yolink".into(),
            about_uuid: None,
            about_name: None,
            name: "temperature".into(),
            human_name: "Bridge temperature".into(),
            unit: "Cel".into(),
            time_kind: TimeKind::Instant,
            points: Points::floats((0..100).map(|i| (i, 20.0 + i as f64 / 10.0))),
        };
        let rel = "yolink/render_time_series/time_series.doltlite_db";
        let source = store::open(&root.join(rel), StoreKind::TimeSeries)
            .await
            .unwrap();
        datalib_etl_time_series::apply(&source, std::slice::from_ref(&temp), "yolink")
            .await
            .unwrap();
        doltlite_raw::commit_run(&source, "test").await.unwrap();
        let first = doltlite_raw::head_commit(&source).await.unwrap().unwrap();
        let ix = store::open(
            &datalib_runtime::layout::time_series_index_db(&root),
            StoreKind::TimeSeriesIndex,
        )
        .await
        .unwrap();
        index::index(
            &ix,
            &[index::Source {
                source_id: "yolink",
                store: rel,
                read: Some(index::Read {
                    pool: &source,
                    commit_hash: &first,
                }),
            }],
        )
        .await
        .unwrap();
        doltlite_raw::commit_run(&ix, "test").await.unwrap();
        ix.close().await;

        // The source moves on; the index still names the first commit, so
        // a reader sees the points as they were.
        let colder = TimeSeries {
            points: Points::floats((0..100).map(|i| (i, -5.0))),
            ..temp
        };
        datalib_etl_time_series::apply(&source, &[colder], "yolink")
            .await
            .unwrap();
        doltlite_raw::commit_run(&source, "colder").await.unwrap();
        source.close().await;

        let (_, listed) = call(
            &app,
            Request::get("/time_series?q=bridge")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(listed["time_series"][0]["time_series_id"], "bridge-temp");
        assert_eq!(listed["time_series"][0]["point_count"], 100);
        let (status, _) = call(
            &app,
            Request::get("/time_series/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (_, whole) = call(
            &app,
            post(json!({"ids": ["bridge-temp"], "from": 10, "to": 14})),
        )
        .await;
        assert_eq!(whole["time_series"][0]["kind"], "raw");
        assert_eq!(
            whole["time_series"][0]["points"].as_array().unwrap().len(),
            5
        );
        assert_eq!(whole["time_series"][0]["unit"], "Cel");
        assert_eq!(whole["time_series"][0]["unit_label"], "°C");
        assert_eq!(whole["time_series"][0]["quantity"], "Θ1");
        assert_eq!(whole["time_series"][0]["points"][0]["value"], 21.0);

        let (_, bucketed) = call(
            &app,
            post(json!({"ids": ["bridge-temp", "gone"], "budget": 10})),
        )
        .await;
        assert_eq!(bucketed["time_series"][0]["kind"], "buckets");
        assert_eq!(
            bucketed["time_series"][0]["buckets"]
                .as_array()
                .unwrap()
                .len(),
            10
        );
        assert_eq!(bucketed["missing"], json!(["gone"]));

        std::fs::remove_file(root.join(rel)).unwrap();
        let (_, lost) = call(&app, post(json!({"ids": ["bridge-temp"]}))).await;
        assert_eq!(lost["time_series"], json!([]));
        assert_eq!(lost["unreadable"][0]["time_series_id"], "bridge-temp");
    }

    fn row(source: &str, human_name: &str) -> TimeSeriesRow {
        TimeSeriesRow {
            source_id: source.into(),
            ts_key: 1,
            time_series_id: "id".into(),
            about_uuid: None,
            about_name: None,
            name: "resting_hr".into(),
            human_name: human_name.into(),
            unit: "{beats}/min".into(),
            value_type: ValueType::Float,
            time_kind: TimeKind::Day,
            point_count: 0,
            first_t: None,
            last_t: None,
        }
    }

    #[test]
    fn a_list_filters_by_source_and_by_every_word() {
        let r = row("riker", "Resting HR");
        let q = |source: Option<&str>, q: Option<&str>| ListQuery {
            source_id: source.map(str::to_string),
            q: q.map(str::to_string),
        };
        assert!(matches(&r, &q(None, None)));
        assert!(matches(&r, &q(Some("riker"), Some("resting_hr HR"))));
        assert!(!matches(&r, &q(Some("troi"), None)));
        assert!(!matches(&r, &q(None, Some("resting weight"))));
    }
}
