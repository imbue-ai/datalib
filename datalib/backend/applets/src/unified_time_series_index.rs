//! `datalib-applet unified_time_series_index` — serves the time-series
//! index: the list of time series, one time series, and points over a
//! range, downsampled to a budget. Each request reads the index's newest
//! commit through a detached read-only open, so the applet never holds
//! the file between requests. `docs/dev/plans/time_series.md` is the
//! design.

use std::collections::HashMap;
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
use datalib_etl_time_series::read::{self, Points};
use datalib_etl_time_series::store;
use datalib_time_series_schema::TimeSeriesRow;
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
/// that is only the index's own.
#[derive(Debug, Serialize, PartialEq)]
struct Listed {
    time_series_id: String,
    source_id: String,
    entity: String,
    metric: String,
    label: String,
    unit: String,
    time_kind: &'static str,
    point_count: i64,
    first_t: Option<i64>,
    last_t: Option<i64>,
}

impl From<&TimeSeriesRow> for Listed {
    fn from(r: &TimeSeriesRow) -> Self {
        Listed {
            time_series_id: r.time_series_id.clone(),
            source_id: r.source_id.clone(),
            entity: r.entity.clone(),
            metric: r.metric.clone(),
            label: r.label.clone(),
            unit: r.unit.clone(),
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
    /// Words every one of which the label, metric or entity holds,
    /// ignoring case.
    q: Option<String>,
}

fn matches(r: &TimeSeriesRow, query: &ListQuery) -> bool {
    if query.source_id.as_ref().is_some_and(|s| *s != r.source_id) {
        return false;
    }
    let hay = format!("{} {} {}", r.label, r.metric, r.entity).to_lowercase();
    query
        .q
        .as_deref()
        .unwrap_or("")
        .split_whitespace()
        .all(|w| hay.contains(&w.to_lowercase()))
}

async fn rows(root: &std::path::Path) -> Result<(Option<Reader>, Vec<TimeSeriesRow>)> {
    let Some(reader) = open(root).await? else {
        return Ok((None, Vec::new()));
    };
    match store::rows(reader.pool()).await {
        Ok(rows) => Ok((Some(reader), rows)),
        Err(e) => {
            reader.close().await;
            Err(e)
        }
    }
}

async fn list(
    State(root): AppState,
    Query(query): Query<ListQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let (reader, all) = rows(&root).await.map_err(ours)?;
    if let Some(r) = reader {
        r.close().await;
    }
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
    let (reader, all) = rows(&root).await.map_err(ours)?;
    if let Some(r) = reader {
        r.close().await;
    }
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
    points: Points,
}

async fn points(
    State(root): AppState,
    Json(req): Json<PointsRequest>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let budget = req.budget.unwrap_or(DEFAULT_BUDGET).clamp(1, MAX_BUDGET);
    let (reader, all) = rows(&root).await.map_err(ours)?;
    let Some(reader) = reader else {
        return Ok(Json(json!({ "time_series": [], "missing": req.ids })));
    };
    let by_id: HashMap<&str, &TimeSeriesRow> =
        all.iter().map(|r| (r.time_series_id.as_str(), r)).collect();
    let mut answered = Vec::new();
    let mut missing = Vec::new();
    let mut failed = None;
    for id in &req.ids {
        let Some(row) = by_id.get(id.as_str()) else {
            missing.push(id.clone());
            continue;
        };
        let from = req.from.unwrap_or(i64::MIN / 2);
        let to = req.to.unwrap_or(i64::MAX / 2);
        let (from, to) = (
            from.max(row.first_t.unwrap_or(from)),
            to.min(row.last_t.unwrap_or(to)),
        );
        match read::points(reader.pool(), row.ts_key, from, to, budget).await {
            Ok(points) => answered.push(Answered {
                listed: Listed::from(*row),
                points,
            }),
            Err(e) => {
                failed = Some(e);
                break;
            }
        }
    }
    reader.close().await;
    if let Some(e) = failed {
        return Err(ours(e));
    }
    Ok(Json(json!({ "time_series": answered, "missing": missing })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Request};
    use datalib_time_series_schema::{Point, TimeKind, TimeSeries};
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
    /// one time series, a range whole, a range bucketed, and an id the
    /// index does not hold.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_routes_read_the_sealed_index() {
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
            entity: "bridge".into(),
            metric: "temperature".into(),
            label: "Bridge temperature".into(),
            unit: "°C".into(),
            time_kind: TimeKind::Instant,
            points: (0..100)
                .map(|i| Point::value(i, 20.0 + i as f64 / 10.0))
                .collect(),
        };
        let pool = store::open(
            &datalib_runtime::layout::time_series_index_db(&root),
            datalib_etl::doltlite_raw::StoreKind::TimeSeriesIndex,
        )
        .await
        .unwrap();
        datalib_etl_time_series::apply(
            &pool,
            &[temp],
            &std::collections::BTreeSet::from(["yolink"]),
        )
        .await
        .unwrap();
        doltlite_raw::commit_run(&pool, "test").await.unwrap();
        pool.close().await;

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
        assert_eq!(whole["time_series"][0]["unit"], "°C");

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
    }

    fn row(source: &str, label: &str) -> TimeSeriesRow {
        TimeSeriesRow {
            ts_key: 1,
            time_series_id: "id".into(),
            source_id: source.into(),
            entity: "account".into(),
            metric: "resting_hr".into(),
            label: label.into(),
            unit: "bpm".into(),
            time_kind: TimeKind::Day,
            points_hash: String::new(),
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
        assert!(matches(&r, &q(Some("riker"), Some("resting ACCOUNT"))));
        assert!(!matches(&r, &q(Some("troi"), None)));
        assert!(!matches(&r, &q(None, Some("resting weight"))));
    }
}
