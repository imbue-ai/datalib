//! Integration test for authoring the `user` namespace:
//! `PUT`/`GET /api/lib/{name}` and `POST /api/lib/{name}/rename`.
//!
//! The endpoints are a *writer*. What they produce is two ordinary
//! files in `system/frontend/user/`, indistinguishable from what an
//! applet writes into its own namespace — so these tests check the
//! files as much as the responses. Everything read back comes from
//! `/api/frontend`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::applets::AppletRegistry;
use datalib_http::frontend::frontend_dir;
use datalib_http::{router, ApiToken, AppState};
use datalib_unified_index::dolt_repo::DoltRepo;
use datalib_unified_index::qmd::{QmdDaemon, QmdDaemonConfig};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tower::ServiceExt;

/// Every route is behind the token gate (see `datalib_http::auth`), so
/// each request carries it. Stamping it here keeps the call sites
/// about what they actually assert.
const TEST_TOKEN: &str = "itest-token";

async fn send(app: &axum::Router, mut req: Request<Body>) -> axum::http::Response<Body> {
    req.headers_mut()
        .insert("x-datalib-token", TEST_TOKEN.parse().unwrap());
    app.clone().oneshot(req).await.unwrap()
}

async fn json_req(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap();
    let resp = send(app, req).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

async fn get_text(app: &axum::Router, uri: &str) -> (StatusCode, String) {
    let resp = send(app, Request::get(uri).body(Body::empty()).unwrap()).await;
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).to_string())
}

/// The `user` namespace as `/api/frontend` reports it.
async fn user_entries(app: &axum::Router) -> serde_json::Value {
    let resp = send(
        app,
        Request::get("/api/frontend").body(Body::empty()).unwrap(),
    )
    .await;
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    v["namespaces"]["user"]["entries"].clone()
}

fn user_dir(root: &Path) -> PathBuf {
    frontend_dir(root).join("user")
}

async fn app_for(root: &Path) -> axum::Router {
    let root = Arc::new(root.to_path_buf());
    let dolt = DoltRepo::open(root.clone()).await.unwrap();
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    router(AppState {
        root: root.clone(),
        repo: Arc::new(dolt),
        app: Arc::new(app),
        qmd_daemon: Arc::new(QmdDaemon::new(QmdDaemonConfig::new((*root).clone()))),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        api_token: ApiToken::from_value(TEST_TOKEN, root.as_path()),
        applets: Arc::new(AppletRegistry::build(Vec::new(), (*root).clone(), None)),
    })
}

/// A PUT is two files: the source under its own digest, and a metadata
/// document naming it. That is the entire storage format, and it is the
/// same one an applet writes.
#[tokio::test]
async fn put_writes_the_two_files_that_define_a_component() {
    let tmp = tempfile::tempdir().unwrap();
    let app = app_for(tmp.path()).await;
    let source = "export default () => (root) => {};";

    let (status, entry) = json_req(
        &app,
        "PUT",
        "/api/lib/widget",
        serde_json::json!({ "source": source, "title": "Widget", "description": "Does a thing." }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let hash = entry["hash"].as_str().unwrap().to_string();

    // On disk: <hash>.js and widget.json, nothing else.
    let dir = user_dir(tmp.path());
    assert_eq!(
        std::fs::read_to_string(dir.join(format!("{hash}.js"))).unwrap(),
        source
    );
    let meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("widget.json")).unwrap()).unwrap();
    assert_eq!(meta["component_hash"], hash);
    assert_eq!(meta["title"], "Widget");
    assert_eq!(meta["component_args"], serde_json::json!([]));

    // …and the store reads it back like any other namespace.
    let entries = user_entries(&app).await;
    assert_eq!(entries["widget"]["title"], "Widget");
    assert_eq!(entries["widget"]["component_hash"], hash);

    let (status, body) = get_text(&app, "/api/lib/widget").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, source);
}

/// The capability the old name-only format could not express: a
/// component that appears in the gallery *with* arguments.
#[tokio::test]
async fn component_args_round_trip() {
    let tmp = tempfile::tempdir().unwrap();
    let app = app_for(tmp.path()).await;
    let (status, _) = json_req(
        &app,
        "PUT",
        "/api/lib/chart",
        serde_json::json!({
            "source": "export default (a, b) => (root) => {};",
            "title": "Chart",
            "component_args": ["weekly", 7],
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = user_entries(&app).await;
    assert_eq!(
        entries["chart"]["component_args"],
        serde_json::json!(["weekly", 7])
    );
}

/// Absent title/description keep what is stored, so a plain source
/// re-PUT doesn't wipe them; an empty string clears.
#[tokio::test]
async fn metadata_keeps_or_clears() {
    let tmp = tempfile::tempdir().unwrap();
    let app = app_for(tmp.path()).await;
    let put = |body: serde_json::Value| json_req(&app, "PUT", "/api/lib/w", body);

    put(serde_json::json!({ "source": "export default 1;", "title": "T", "description": "D" }))
        .await;

    // Source-only re-PUT keeps both.
    put(serde_json::json!({ "source": "export default 2;" })).await;
    let entries = user_entries(&app).await;
    assert_eq!(entries["w"]["title"], "T");
    assert_eq!(entries["w"]["description"], "D");

    // Empty string clears.
    put(serde_json::json!({ "source": "export default 3;", "description": "" })).await;
    let entries = user_entries(&app).await;
    assert_eq!(entries["w"]["title"], "T");
    assert_eq!(entries["w"]["description"], "");
}

/// Re-PUTting repoints the name at new bytes. The old component stays
/// addressable — it is content-addressed, so it is still a correct
/// answer for anything mid-render.
#[tokio::test]
async fn re_put_repoints_the_name_and_keeps_the_old_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let app = app_for(tmp.path()).await;
    let (_, first) = json_req(
        &app,
        "PUT",
        "/api/lib/w",
        serde_json::json!({ "source": "export default 1;" }),
    )
    .await;
    let (_, second) = json_req(
        &app,
        "PUT",
        "/api/lib/w",
        serde_json::json!({ "source": "export default 2;" }),
    )
    .await;
    let (a, b) = (
        first["hash"].as_str().unwrap(),
        second["hash"].as_str().unwrap(),
    );
    assert_ne!(a, b);

    let entries = user_entries(&app).await;
    assert_eq!(entries["w"]["component_hash"], b);
    // Both files are still there.
    let dir = user_dir(tmp.path());
    assert!(dir.join(format!("{a}.js")).is_file());
    assert!(dir.join(format!("{b}.js")).is_file());
}

/// A rename moves the metadata and leaves a redirect, so cards still
/// saying `comp.user.old(…)` can follow.
#[tokio::test]
async fn rename_moves_the_component_and_leaves_a_tombstone() {
    let tmp = tempfile::tempdir().unwrap();
    let app = app_for(tmp.path()).await;
    json_req(
        &app,
        "PUT",
        "/api/lib/old",
        serde_json::json!({ "source": "export default 1;", "title": "T" }),
    )
    .await;

    let (status, entry) = json_req(
        &app,
        "POST",
        "/api/lib/old/rename",
        serde_json::json!({ "new_name": "new" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(entry["name"], "new");

    let entries = user_entries(&app).await;
    assert_eq!(entries["new"]["title"], "T");
    assert_eq!(entries["old"]["renamed_to"], "new");
    // The tombstone is a redirect, not a component.
    assert!(entries["old"]["component_hash"].is_null());

    // Writing the old name again retires the tombstone.
    json_req(
        &app,
        "PUT",
        "/api/lib/old",
        serde_json::json!({ "source": "export default 9;" }),
    )
    .await;
    let entries = user_entries(&app).await;
    assert!(entries["old"]["renamed_to"].is_null());
    assert!(entries["old"]["component_hash"].is_string());
}

#[tokio::test]
async fn rename_rejects_bad_and_taken_names() {
    let tmp = tempfile::tempdir().unwrap();
    let app = app_for(tmp.path()).await;
    for n in ["a", "b"] {
        json_req(
            &app,
            "PUT",
            &format!("/api/lib/{n}"),
            serde_json::json!({ "source": "export default 1;" }),
        )
        .await;
    }

    let (status, _) = json_req(
        &app,
        "POST",
        "/api/lib/a/rename",
        serde_json::json!({ "new_name": "b" }),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "a taken name must 409");

    let (status, _) = json_req(
        &app,
        "POST",
        "/api/lib/a/rename",
        serde_json::json!({ "new_name": "not an identifier" }),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _) = json_req(
        &app,
        "POST",
        "/api/lib/missing/rename",
        serde_json::json!({ "new_name": "c" }),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

/// A name reaches card source as `comp.user.<name>`, so it has to be a
/// JavaScript identifier — which also keeps it safe as a filename.
#[tokio::test]
async fn put_rejects_names_that_are_not_identifiers() {
    let tmp = tempfile::tempdir().unwrap();
    let app = app_for(tmp.path()).await;
    for bad in ["2fa", "has-dash", "with%20space"] {
        let (status, _) = json_req(
            &app,
            "PUT",
            &format!("/api/lib/{bad}"),
            serde_json::json!({ "source": "export default 1;" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad}");
    }
}

/// Namespacing removed the need to reserve builtin view names: a
/// component called `gridView` is `comp.user.gridView`, which cannot
/// shadow the builtin `gridView()`.
#[tokio::test]
async fn a_component_may_take_a_builtin_view_name() {
    let tmp = tempfile::tempdir().unwrap();
    let app = app_for(tmp.path()).await;
    let (status, _) = json_req(
        &app,
        "PUT",
        "/api/lib/gridView",
        serde_json::json!({ "source": "export default 1;", "title": "Mine" }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let entries = user_entries(&app).await;
    assert_eq!(entries["gridView"]["title"], "Mine");
}

#[tokio::test]
async fn get_missing_is_not_found() {
    let tmp = tempfile::tempdir().unwrap();
    let app = app_for(tmp.path()).await;
    let (status, _) = get_text(&app, "/api/lib/nope").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
