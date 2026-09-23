//! A page of the app reports what happened on it through
//! `POST /api/ui/events`, and it comes back as a `ui` process with its
//! lines: `GET /api/processes?process=ui`, `GET /api/log?process=<id>`.
//! One test, because the subscriber it installs is the process's only
//! one.

use axum::body::Body;
use axum::http::{header, Method, Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::{router, ApiToken, AppState};
use std::path::Path;
use std::sync::Arc;
use tower::ServiceExt;

const TEST_TOKEN: &str = "ui-events-test-token";
const PAGE: &str = "0192b9c1-7d2e-7a4b-9c3d-1e2f3a4b5c6d";

async fn state(root: &Path) -> AppState {
    let root = Arc::new(root.to_path_buf());
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    AppState {
        root: root.clone(),
        app: Arc::new(app),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        root_tx: tokio::sync::broadcast::channel(16).0,
        usage: Default::default(),
        newer_root: Vec::new(),
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(AppletRegistry::from_data_root(&root, None)),
    }
}

async fn send(
    root: &Path,
    method: Method,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, Vec<u8>) {
    let app = router(state(root).await);
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-datalib-token", TEST_TOKEN)
        .header("x-datalib-page", PAGE);
    let body = match body {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, bytes.to_vec())
}

async fn get_json(root: &Path, uri: &str) -> serde_json::Value {
    let (status, body) = send(root, Method::GET, uri, None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{uri}: {}",
        String::from_utf8_lossy(&body)
    );
    serde_json::from_slice(&body).unwrap()
}

fn batch(events: serde_json::Value, closing: bool) -> serde_json::Value {
    serde_json::json!({
        "page": { "process_id": PAGE, "started_at": "2026-09-21T10:00:00.000-07:00" },
        "events": events,
        "closing": closing,
    })
}

#[tokio::test]
async fn a_page_is_a_process_and_what_it_reports_are_its_lines() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let log = datalib_http::logging::init(root).expect("the store opens in a temp dir");

    let (status, body) = send(
        root,
        Method::POST,
        "/api/ui/events",
        Some(batch(
            serde_json::json!([
                { "at": "2026-09-21T10:00:00.100-07:00", "name": "page_load",
                  "fields": { "user_agent": "test" } },
                { "at": "2026-09-21T10:00:01.000-07:00", "name": "navigate", "msg": "/cards" },
                { "at": "2026-09-21T10:00:02.000-07:00", "name": "error", "level": "error",
                  "msg": "boom", "fields": { "stack": "at x" } },
            ]),
            false,
        )),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "{}",
        String::from_utf8_lossy(&body)
    );

    // Refusals, each before anything is written.
    let mut bad_name = batch(
        serde_json::json!([{ "at": "2026-09-21T10:00:00-07:00", "name": "Nav" }]),
        false,
    );
    let (status, _) = send(root, Method::POST, "/api/ui/events", Some(bad_name.clone())).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    bad_name["page"]["process_id"] = "not-a-uuid".into();
    bad_name["events"] = serde_json::json!([]);
    let (status, _) = send(root, Method::POST, "/api/ui/events", Some(bad_name)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // The page goes away.
    let (status, _) = send(
        root,
        Method::POST,
        "/api/ui/events",
        Some(batch(serde_json::json!([]), true)),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // The writer flushes on an interval; dropping it is the final flush.
    drop(log);

    let pages = get_json(root, "/api/processes?process=ui").await;
    let pages = pages.as_array().unwrap();
    assert_eq!(pages.len(), 1, "{pages:?}");
    let page = &pages[0];
    assert_eq!(page["process_id"], PAGE);
    assert_eq!(page["process"], "ui");
    assert_eq!(page["started_at_utc"], "2026-09-21T17:00:00.000000+00:00");
    assert_eq!(page["tz_offset"], "-07:00");
    assert!(page["run_id"].is_null());
    assert!(page["finished_at_utc"].is_string(), "closed: {page}");

    let lines = get_json(root, &format!("/api/log?process={PAGE}")).await;
    let lines = lines.as_array().unwrap();
    let targets: Vec<&str> = lines
        .iter()
        .map(|l| l["target"].as_str().unwrap())
        .collect();
    assert_eq!(
        targets,
        ["ui.page_load", "ui.navigate", "ui.error"],
        "{lines:#?}"
    );
    for l in lines {
        assert_eq!(l["process"], "ui");
        assert_eq!(l["process_id"], PAGE);
        assert!(l["run_id"].is_null());
    }
    assert_eq!(lines[0]["msg"], "page_load");
    assert_eq!(lines[0]["ts_utc"], "2026-09-21T17:00:00.100000+00:00");
    assert_eq!(lines[1]["msg"], "/cards");
    assert_eq!(lines[2]["level"], "error");
    let fields: serde_json::Value =
        serde_json::from_str(lines[2]["fields"].as_str().unwrap()).unwrap();
    assert_eq!(fields["stack"], "at x");

    // The request log names the page on every request it made.
    let all = get_json(root, "/api/log").await;
    let posts: Vec<&serde_json::Value> = all
        .as_array()
        .unwrap()
        .iter()
        .filter(|l| {
            l["target"] == datalib_http::request_log::TARGET
                && l["msg"]
                    .as_str()
                    .unwrap()
                    .starts_with("POST /api/ui/events")
        })
        .collect();
    assert_eq!(posts.len(), 4, "{posts:#?}");
    let fields: serde_json::Value =
        serde_json::from_str(posts[0]["fields"].as_str().unwrap()).unwrap();
    assert_eq!(fields["page"], PAGE);
}
