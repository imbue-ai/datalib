//! A protected library's responses stay out of the web view's disk cache,
//! which is outside the folder that guards them.

use axum::body::Body;
use axum::http::{header, Request, Response};
use datalib_http::{keep_off_disk, router};
use tower::ServiceExt;

use crate::support::{state, TEST_TOKEN};

async fn get(app: &axum::Router, uri: &str, token: Option<&str>) -> Response<Body> {
    let mut request = Request::builder().uri(uri);
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    app.clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

fn cache_control(response: &Response<Body>) -> Option<&str> {
    response
        .headers()
        .get(header::CACHE_CONTROL)
        .map(|value| value.to_str().unwrap())
}

/// The API, a page that sets its own cache rule, and a refused request all
/// say `no-store`, so nothing a route forgets can reach the disk.
#[tokio::test]
async fn nothing_a_protected_library_answers_may_be_stored() {
    let dir = tempfile::tempdir().unwrap();
    let app = keep_off_disk(router(state(dir.path()).await));

    for (uri, token) in [
        ("/api/health", Some(TEST_TOKEN)),
        ("/", Some(TEST_TOKEN)),
        ("/api/health", None),
    ] {
        let response = get(&app, uri, token).await;
        assert_eq!(cache_control(&response), Some("no-store"), "{uri}");
    }
}

/// `/assets/` is the app's own code, so it keeps its own rule and a reload
/// stays quick. Only that exact prefix: a path merely containing the word
/// is still refused the cache.
#[tokio::test]
async fn only_the_apps_own_code_may_be_stored() {
    let dir = tempfile::tempdir().unwrap();
    let app = keep_off_disk(router(state(dir.path()).await));

    let code = get(&app, "/assets/not-built-in-a-test.js", Some(TEST_TOKEN)).await;
    assert_ne!(cache_control(&code), Some("no-store"));
    let other = get(&app, "/data/assets/x", Some(TEST_TOKEN)).await;
    assert_eq!(cache_control(&other), Some("no-store"));
}

/// An ordinary library keeps its own rules: the page is revalidated, not
/// refused the cache. Also what shows the test above changed something.
#[tokio::test]
async fn an_ordinary_library_keeps_its_cache_rules() {
    let dir = tempfile::tempdir().unwrap();
    let app = router(state(dir.path()).await);

    let page = get(&app, "/", Some(TEST_TOKEN)).await;
    assert_eq!(cache_control(&page), Some("no-cache"));
}
