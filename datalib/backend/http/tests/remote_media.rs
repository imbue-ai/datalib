//! `GET /api/remote`: the one way a remote image reaches the app page
//! (issue #648). A stand-in remote host on loopback plays the sender's
//! server, so the fetch is exercised with the private-address guard
//! off; the guard itself is asserted through the router, where it is
//! on.

use axum::body::Body;
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Redirect};
use axum::routing::get;
use axum::Router;
use datalib_core::app_store::AppStore;
use datalib_http::remote_media::{fetch, Refusal, RemotePolicy};
use datalib_http::{router, ApiToken, AppState};
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "remote-media-itest";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot really a png";

/// The sender's server: an image, a page, a redirect to the image, a
/// redirect chain that never ends, and a body too large to be media.
async fn stand_in_remote() -> String {
    let app = Router::new()
        .route(
            "/pic.png",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], PNG) }),
        )
        .route(
            "/page.html",
            get(|| async { ([(header::CONTENT_TYPE, "text/html")], "<p>hi</p>") }),
        )
        .route(
            "/logo.svg",
            get(|| async { ([(header::CONTENT_TYPE, "image/svg+xml")], "<svg/>") }),
        )
        .route("/moved", get(|| async { Redirect::temporary("/pic.png") }))
        .route("/loop", get(|| async { Redirect::temporary("/loop") }))
        // A body with no end: the cap has to hold without a
        // Content-Length to judge by.
        .route(
            "/huge.png",
            get(|| async {
                let chunk = axum::body::Bytes::from(vec![0u8; 1 << 20]);
                let stream = futures::stream::repeat(chunk).map(Ok::<_, std::convert::Infallible>);
                (
                    [(header::CONTENT_TYPE, HeaderValue::from_static("image/png"))],
                    Body::from_stream(stream),
                )
                    .into_response()
            }),
        )
        .route("/missing.png", get(|| async { StatusCode::NOT_FOUND }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

const LOCAL: RemotePolicy = RemotePolicy {
    allow_private: true,
};

#[tokio::test]
async fn an_image_comes_back_as_its_bytes_and_bare_media_type() {
    let base = stand_in_remote().await;
    let got = fetch(&format!("{base}/pic.png"), &LOCAL).await.unwrap();
    assert_eq!(got.content_type, "image/png");
    assert_eq!(got.body, PNG);
}

#[tokio::test]
async fn a_redirect_is_followed_and_a_loop_is_not() {
    let base = stand_in_remote().await;
    let got = fetch(&format!("{base}/moved"), &LOCAL).await.unwrap();
    assert_eq!(got.content_type, "image/png");
    assert_eq!(
        fetch(&format!("{base}/loop"), &LOCAL).await.err().unwrap(),
        Refusal::TooManyRedirects
    );
}

#[tokio::test]
async fn anything_but_media_is_refused() {
    let base = stand_in_remote().await;
    assert_eq!(
        fetch(&format!("{base}/page.html"), &LOCAL)
            .await
            .err()
            .unwrap(),
        Refusal::NotMedia("text/html".into())
    );
    assert_eq!(
        fetch(&format!("{base}/missing.png"), &LOCAL)
            .await
            .err()
            .unwrap(),
        Refusal::UpstreamStatus(404)
    );
    assert_eq!(
        fetch(&format!("{base}/huge.png"), &LOCAL)
            .await
            .err()
            .unwrap(),
        Refusal::TooLarge
    );
}

/// An SVG is an image in an `<img>` and a document with scripts when
/// navigated to, so its response carries the sandbox policy every data
/// document gets.
#[tokio::test]
async fn an_svg_is_served_sandboxed() {
    let base = stand_in_remote().await;
    let got = fetch(&format!("{base}/logo.svg"), &LOCAL).await.unwrap();
    let resp = datalib_http::remote_media::response(got);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "image/svg+xml");
    assert!(resp.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .starts_with("sandbox"));
    assert_eq!(resp.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
}

/// Through the router the policy is the process's, and the process
/// has not been told to reach private addresses: loopback is refused
/// before anything connects, with the status that says why.
#[tokio::test]
async fn the_route_refuses_a_private_target_and_a_non_http_one() {
    let base = stand_in_remote().await;
    let (status, body) = call(&format!("/api/remote?url={base}/pic.png")).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("private address"), "{body}");

    let (status, body) = call("/api/remote?url=file:///etc/passwd").await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    let (status, _) = call("/api/remote").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

async fn call(path: &str) -> (StatusCode, String) {
    let root: Arc<PathBuf> = Arc::new(
        tempfile::TempDir::with_prefix("datalib-http-remote-itest-")
            .expect("create tempdir")
            .keep(),
    );
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    let state = AppState {
        root: root.clone(),
        app: Arc::new(app),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        root_tx: tokio::sync::broadcast::channel(16).0,
        usage: Default::default(),
        newer_root: Vec::new(),
        applets: Arc::new(datalib_http::applets::AppletRegistry::build(
            Vec::new(),
            (*root).clone(),
            None,
        )),
        api_token: ApiToken::from_value(TOKEN, root.as_path()),
    };
    let req = Request::get(path)
        .header("x-datalib-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = router(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}
