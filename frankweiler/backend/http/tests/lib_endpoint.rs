//! Integration test for the component library endpoints:
//! `PUT/GET /api/lib/{name}`, `GET /api/lib`, and
//! `POST /api/lib/{name}/rename`. Drives requests through
//! `tower::ServiceExt::oneshot` against a temp data root. Verifies:
//!   - PUT stores source + title/description sidecar; keep/clear
//!     semantics on re-PUT.
//!   - the manifest lists title/description and rename tombstones.
//!   - rename moves source + metadata, leaves a tombstone, 409s on a
//!     taken name, 400s on builtin names, and a later PUT to the old
//!     name retires the tombstone.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use frankweiler_core::dolt_repo::DoltRepo;
use frankweiler_core::qmd::{QmdDaemon, QmdDaemonConfig};
use frankweiler_http::{router, AppState};
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

fn unique_db_path() -> PathBuf {
    tempfile::TempDir::with_prefix("fw-http-lib-itest-")
        .expect("create tempdir")
        .keep()
        .join("backend_index.doltlite_db")
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
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, value)
}

async fn manifest(app: &axum::Router) -> Vec<serde_json::Value> {
    let req = Request::get("/api/lib").body(Body::empty()).unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024)
        .await
        .unwrap();
    serde_json::from_slice::<Vec<serde_json::Value>>(&bytes).unwrap()
}

fn entry<'a>(list: &'a [serde_json::Value], name: &str) -> Option<&'a serde_json::Value> {
    list.iter().find(|e| e["name"] == name)
}

#[tokio::test]
async fn lib_metadata_and_rename() {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let dolt = DoltRepo::open(&db_path, root.clone())
        .await
        .unwrap_or_else(|e| panic!("open doltlite at {}: {e}", db_path.display()));
    let app_state = AppState {
        root: root.clone(),
        config_path: Arc::new(root.join("config.yaml")),
        repo: Arc::new(dolt),
        qmd_daemon: Arc::new(QmdDaemon::new(QmdDaemonConfig::new((*root).clone()))),
        progress_tx: tokio::sync::broadcast::channel(16).0,
    };
    let app = router(app_state.clone());

    // PUT with title + description; both come back in the manifest.
    let (status, put1) = json_req(
        &app,
        "PUT",
        "/api/lib/card_abc",
        serde_json::json!({"source": "() => 1", "title": "Nice", "description": "Shows one."}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(put1["title"], "Nice");
    let list = manifest(&app).await;
    let e = entry(&list, "card_abc").expect("card_abc listed");
    assert_eq!(e["title"], "Nice");
    assert_eq!(e["description"], "Shows one.");

    // Re-PUT without metadata keeps it; "" clears just that field.
    let (_, put2) = json_req(
        &app,
        "PUT",
        "/api/lib/card_abc",
        serde_json::json!({"source": "() => 2"}),
    )
    .await;
    assert_eq!(put2["title"], "Nice");
    assert_eq!(put2["description"], "Shows one.");
    let (_, put3) = json_req(
        &app,
        "PUT",
        "/api/lib/card_abc",
        serde_json::json!({"source": "() => 2", "description": ""}),
    )
    .await;
    assert_eq!(put3["title"], "Nice");
    assert!(put3.get("description").is_none());

    // Builtin names are rejected outright.
    let (status, _) = json_req(
        &app,
        "PUT",
        "/api/lib/gridView",
        serde_json::json!({"source": "() => 1"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Rename: new entry carries the metadata, old name becomes a
    // tombstone with renamed_to and no hash-able source.
    let (status, renamed) = json_req(
        &app,
        "POST",
        "/api/lib/card_abc/rename",
        serde_json::json!({"new_name": "niceName"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(renamed["name"], "niceName");
    assert_eq!(renamed["title"], "Nice");
    let list = manifest(&app).await;
    assert_eq!(entry(&list, "niceName").unwrap()["title"], "Nice");
    let tomb = entry(&list, "card_abc").expect("tombstone listed");
    assert_eq!(tomb["renamed_to"], "niceName");
    assert_eq!(tomb["hash"], "");

    // The moved source is served under the new name; old name is gone.
    let req = Request::get("/api/lib/niceName")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let req = Request::get("/api/lib/card_abc")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Renaming onto a taken or builtin name fails; missing source 404s.
    let (_, _) = json_req(
        &app,
        "PUT",
        "/api/lib/card_other",
        serde_json::json!({"source": "() => 3"}),
    )
    .await;
    let (status, _) = json_req(
        &app,
        "POST",
        "/api/lib/card_other/rename",
        serde_json::json!({"new_name": "niceName"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = json_req(
        &app,
        "POST",
        "/api/lib/card_other/rename",
        serde_json::json!({"new_name": "documentView"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = json_req(
        &app,
        "POST",
        "/api/lib/card_missing/rename",
        serde_json::json!({"new_name": "card_new"}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Re-creating the old name retires its tombstone.
    let (status, _) = json_req(
        &app,
        "PUT",
        "/api/lib/card_abc",
        serde_json::json!({"source": "() => 4"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let list = manifest(&app).await;
    let revived = entry(&list, "card_abc").expect("card_abc listed again");
    assert!(revived.get("renamed_to").is_none());
    assert_ne!(revived["hash"], "");

    drop(app_state);
    let _ = std::fs::remove_file(&db_path);
}
