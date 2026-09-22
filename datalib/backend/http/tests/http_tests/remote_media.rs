//! `GET /api/remote_media` and the allow-list (issue #648). A stand-in
//! remote host on loopback plays the sender's server, so the target is
//! `DATALIB_REMOTE_MEDIA_ALLOW_PRIVATE=1` (BUILD.bazel); the guard
//! itself is asserted in the module's own tests, where the policy is
//! the default.

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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "remote-media-itest";
const PNG: &[u8] = b"\x89PNG\r\n\x1a\nnot really a png";

/// The sender's server: an image (counting how often it is asked
/// for), a page, a redirect to the image, a redirect chain that never
/// ends, and a body too large to be media.
async fn stand_in_remote() -> (String, Arc<AtomicUsize>) {
    let hits = Arc::new(AtomicUsize::new(0));
    let counted = hits.clone();
    let app = Router::new()
        .route(
            "/pic.png",
            get(move || {
                let counted = counted.clone();
                async move {
                    counted.fetch_add(1, Ordering::SeqCst);
                    ([(header::CONTENT_TYPE, "image/png")], PNG)
                }
            }),
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
    (format!("http://{addr}"), hits)
}

const LOCAL: RemotePolicy = RemotePolicy {
    allow_private: true,
};

#[tokio::test]
async fn an_image_comes_back_as_its_bytes_and_bare_media_type() {
    let (base, _) = stand_in_remote().await;
    let got = fetch(&format!("{base}/pic.png"), &LOCAL).await.unwrap();
    assert_eq!(got.content_type, "image/png");
    assert_eq!(got.body, PNG);
}

#[tokio::test]
async fn a_redirect_is_followed_and_a_loop_is_not() {
    let (base, _) = stand_in_remote().await;
    let got = fetch(&format!("{base}/moved"), &LOCAL).await.unwrap();
    assert_eq!(got.content_type, "image/png");
    assert_eq!(
        fetch(&format!("{base}/loop"), &LOCAL).await.err().unwrap(),
        Refusal::TooManyRedirects
    );
}

#[tokio::test]
async fn anything_but_media_is_refused() {
    let (base, _) = stand_in_remote().await;
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
    let (base, _) = stand_in_remote().await;
    let got = fetch(&format!("{base}/logo.svg"), &LOCAL).await.unwrap();
    let resp = datalib_http::remote_media::response(&got.content_type, got.body);
    assert_eq!(resp.headers()[header::CONTENT_TYPE], "image/svg+xml");
    assert!(resp.headers()[header::CONTENT_SECURITY_POLICY]
        .to_str()
        .unwrap()
        .starts_with("sandbox"));
    assert_eq!(resp.headers()[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
}

/// Through the route: nothing without a row; with one, the first
/// request fetches and keeps the bytes under
/// `system/remote_media/<sha256>` with a row saying so, the second is
/// answered from there, and the host is not asked again.
#[tokio::test]
async fn the_route_fetches_once_and_serves_from_the_cas_after() {
    let (base, hits) = stand_in_remote().await;
    let (root, app) = app().await;
    let url = format!("{base}/pic.png");
    let path = format!("/api/remote_media?url={url}");

    let (status, _, body) = call(&app, get_req(&path)).await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "{}",
        String::from_utf8_lossy(&body)
    );
    assert!(String::from_utf8_lossy(&body).contains("no rule"));
    assert_eq!(hits.load(Ordering::SeqCst), 0, "refused before any fetch");

    let host = url::Url::parse(&url).unwrap();
    let (status, _, _) = call(
        &app,
        post_json(
            "/api/remote_media/allow",
            &format!(
                r#"{{"scope":"host","key":"{}:{}"}}"#,
                host.host_str().unwrap(),
                host.port().unwrap()
            ),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, headers, body) = call(&app, get_req(&path)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    assert_eq!(headers[header::CONTENT_TYPE], "image/png");
    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(body, PNG);
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let (status, _, again) = call(&app, get_req(&path)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(again, PNG);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "served from the CAS");

    let (status, _, body) = call(&app, get_req("/api/remote_media/fetched")).await;
    assert_eq!(status, StatusCode::OK);
    let table: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(table["row_key"], "url");
    let rows = table["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["url"], url);
    assert_eq!(rows[0]["content_type"], "image/png");
    assert_eq!(rows[0]["byte_size"], PNG.len());
    let sha = rows[0]["sha256"].as_str().unwrap();
    let kept = datalib_core::layout::remote_media_dir(&root).join(sha);
    assert_eq!(std::fs::read(&kept).unwrap(), PNG, "{}", kept.display());
    assert!(!table["columns"].as_array().unwrap().is_empty());
}

/// A `document` row covers only a request made for that document, and
/// the check answers per URL what the route would do.
#[tokio::test]
async fn a_document_row_covers_requests_made_for_that_document() {
    let (base, hits) = stand_in_remote().await;
    let (_root, app) = app().await;
    let url = format!("{base}/pic.png");
    let (status, _, _) = call(
        &app,
        post_json(
            "/api/remote_media/allow",
            r#"{"scope":"document","key":"doc-1"}"#,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _, _) = call(&app, get_req(&format!("/api/remote_media?url={url}"))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _, _) = call(
        &app,
        get_req(&format!("/api/remote_media?url={url}&document=doc-2")),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    let (status, _, _) = call(
        &app,
        get_req(&format!("/api/remote_media?url={url}&document=doc-1")),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    let (status, _, body) = call(
        &app,
        post_json(
            "/api/remote_media/check",
            &format!(
                r#"{{"document":"doc-1","source":"mail","urls":["{url}","//other.example/x.png","not a url"]}}"#
            ),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let answer: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let allowed = answer["allowed"].as_array().unwrap();
    // Everything asked for under doc-1 is covered by its row; what is
    // not an absolute http(s) URL (the page makes `//host` absolute
    // before asking) is not.
    assert_eq!(allowed.len(), 1, "{answer}");
    assert_eq!(allowed[0]["url"], url);
    assert_eq!(allowed[0]["rule"]["scope"], "document");
    let (_, _, body) = call(
        &app,
        post_json(
            "/api/remote_media/check",
            &format!(r#"{{"document":"doc-2","urls":["{url}"]}}"#),
        ),
    )
    .await;
    let answer: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(answer["allowed"].as_array().unwrap().len(), 0);
}

/// A refusal comes back with its status and reason, and nothing is
/// recorded for it.
#[tokio::test]
async fn the_route_refuses_what_the_fetch_refuses() {
    let (base, _) = stand_in_remote().await;
    let (_root, app) = app().await;
    let (status, _, _) = call(
        &app,
        post_json(
            "/api/remote_media/allow",
            &format!(
                r#"{{"scope":"host","key":"{}"}}"#,
                base.trim_start_matches("http://")
            ),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, body) = call(
        &app,
        get_req(&format!("/api/remote_media?url={base}/page.html")),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(String::from_utf8_lossy(&body).contains("not an image"));
    let (status, _, body) = call(&app, get_req("/api/remote_media?url=file:///etc/passwd")).await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let (_, _, body) = call(&app, get_req("/api/remote_media/fetched")).await;
    let table: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(table["rows"].as_array().unwrap().len(), 0);
}

/// The allow-list round trip: a POST is one row however often it is
/// repeated, the list is a typed table, and a DELETE takes it out.
#[tokio::test]
async fn allows_are_posted_listed_and_deleted() {
    let (_root, app) = app().await;
    let post = |body: &'static str| {
        Request::post("/api/remote_media/allow")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    let (status, _, body) = call(&app, post(r#"{"scope":"host","key":"cdn.example"}"#)).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&body)
    );
    let first: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let (status, _, body) = call(&app, post(r#"{"scope":"host","key":"cdn.example"}"#)).await;
    assert_eq!(status, StatusCode::CREATED);
    let again: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(first["allow_uuid"], again["allow_uuid"]);
    let (status, _, _) = call(&app, post(r#"{"scope":"source","key":"mail"}"#)).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _, _) = call(&app, post(r#"{"scope":"planet","key":"risa"}"#)).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "an unknown scope is refused"
    );
    let (status, _, _) = call(&app, post(r#"{"scope":"host","key":"  "}"#)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, _, body) = call(&app, get_req("/api/remote_media/allow")).await;
    assert_eq!(status, StatusCode::OK);
    let table: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(table["row_key"], "allow_uuid");
    assert_eq!(table["rows"].as_array().unwrap().len(), 2);

    let id = first["allow_uuid"].as_str().unwrap();
    let del = |id: &str| {
        Request::delete(format!("/api/remote_media/allow/{id}"))
            .body(Body::empty())
            .unwrap()
    };
    let (status, _, _) = call(&app, del(id)).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _, _) = call(&app, del(id)).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, _, body) = call(&app, get_req("/api/remote_media/allow")).await;
    let table: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let rows = table["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["scope"], "source");
}

fn get_req(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

fn post_json(path: &str, body: &str) -> Request<Body> {
    Request::post(path)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn app() -> (PathBuf, axum::Router) {
    let root: Arc<PathBuf> = Arc::new(
        tempfile::TempDir::with_prefix("datalib-http-remote-itest-")
            .expect("create tempdir")
            .keep(),
    );
    let store = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    let state = AppState {
        root: root.clone(),
        app: Arc::new(store),
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
    ((*root).clone(), router(state))
}

async fn call(
    app: &axum::Router,
    mut req: Request<Body>,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    req.headers_mut()
        .insert("x-datalib-token", HeaderValue::from_static(TOKEN));
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 64 << 20)
        .await
        .unwrap();
    (status, headers, bytes.to_vec())
}
