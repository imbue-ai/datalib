//! `GET`/`PUT /api/ui/state/{name}`: what the UI keeps in the library.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_http::router;
use tower::ServiceExt;

use crate::support::{state, TEST_TOKEN};

async fn call(app: &axum::Router, method: &str, uri: &str, body: &str) -> (StatusCode, String) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(uri)
                .header("x-datalib-token", TEST_TOKEN)
                .header("content-type", "application/json")
                .body(Body::from(body.to_owned()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

/// A document put is the document got, in a file under `system/` that a
/// second server on the same root (the next launch) reads back.
#[tokio::test]
async fn a_put_document_comes_back_after_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(state(tmp.path()).await);

    let (status, _) = call(&app, "GET", "/api/ui/state/layout", "").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let doc = r#"{"kind":"box","children":[]}"#;
    let (status, _) = call(&app, "PUT", "/api/ui/state/layout", doc).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(tmp.path().join("system/ui-state/layout.json").is_file());

    let again = router(state(tmp.path()).await);
    let (status, body) = call(&again, "GET", "/api/ui/state/layout", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, doc);
}

/// Neither a name that could leave the directory nor a body that is not
/// JSON is written.
#[tokio::test]
async fn bad_names_and_bodies_are_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let app = router(state(tmp.path()).await);

    let (status, _) = call(&app, "PUT", "/api/ui/state/..%2Fconfig", "{}").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&app, "PUT", "/api/ui/state/layout", "not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(!tmp.path().join("system/ui-state/layout.json").exists());
}
