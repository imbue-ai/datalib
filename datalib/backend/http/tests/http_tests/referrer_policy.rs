//! A link followed out of the app tells the site it leads to nothing
//! about where it was clicked: every response says `no-referrer`.

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use datalib_http::router;
use tower::ServiceExt;

use crate::data_blobs::{blob_store, CONFIG};
use crate::support::{state, TEST_TOKEN};

async fn referrer_policy(
    app: &axum::Router,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, Option<String>) {
    let mut request = Request::builder().uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let policy = response
        .headers()
        .get(header::REFERRER_POLICY)
        .map(|v| v.to_str().unwrap().to_string());
    (response.status(), policy)
}

/// Clicking a link in a rendered email sent the sender's site
/// `Referer: http://127.0.0.1:<port>/`. The app page, a message served as
/// a document, and a refused request all ask the browser to send none.
#[tokio::test]
async fn every_response_asks_for_no_referrer() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("config.toml"), CONFIG).unwrap();
    let hashes = blob_store(
        &td.path().join("pdfs/ingest"),
        &[(
            Some("text/html"),
            b"<a href=\"https://sender.example/\">hi</a>",
        )],
    )
    .await;
    let app = router(state(td.path()).await);
    let document = datalib_blob_store::address("pdfs/ingest", &hashes[0]);

    for (uri, token, status) in [
        ("/", Some(TEST_TOKEN), StatusCode::OK),
        (document.as_str(), Some(TEST_TOKEN), StatusCode::OK),
        ("/api/health", Some(TEST_TOKEN), StatusCode::OK),
        ("/api/health", None, StatusCode::UNAUTHORIZED),
    ] {
        let (got_status, policy) = referrer_policy(&app, uri, token).await;
        assert_eq!(got_status, status, "{uri}");
        assert_eq!(policy.as_deref(), Some("no-referrer"), "{uri}");
    }
}
