//! Integration test for the API token gate (`datalib_http::auth`).
//!
//! This is the regression guard for issue #138 — an unauthenticated
//! local API that any web page could drive into arbitrary code
//! execution via `PUT /api/config` + `POST /api/sync/jobs`. It asserts
//! the properties that keep that closed, against the real router:
//!
//!   - no credential → 401, on reads *and* on the write endpoints;
//!   - a wrong token is not a near-miss — it's the same 401;
//!   - each of the four accepted carriers (Bearer, `X-Datalib-Token`,
//!     `?token=`, cookie) works;
//!   - a document load that presents the token gets a session cookie
//!     back, `HttpOnly` + `SameSite=Lax`, and `?token=` is redirected
//!     away so it can't linger in history;
//!   - `CorsLayer::permissive()` is gone, so a cross-origin page can't
//!     read a response even if it somehow had a token;
//!   - the agent guides stay readable without one (they're what tells
//!     an agent how to authenticate).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use datalib_core::dolt_repo::{AppStore, DoltRepo};
use datalib_core::qmd::{QmdDaemon, QmdDaemonConfig};
use datalib_http::{router, ApiToken, AppState};
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "correct-horse-battery-staple";

fn unique_db_path() -> PathBuf {
    tempfile::TempDir::with_prefix("datalib-http-auth-itest-")
        .expect("create tempdir")
        .keep()
        .join("backend_index.doltlite_db")
}

async fn app() -> (axum::Router, ApiToken) {
    let db_path = unique_db_path();
    let root = Arc::new(db_path.parent().unwrap().to_path_buf());
    let dolt = DoltRepo::open(root.clone())
        .await
        .unwrap_or_else(|e| panic!("open doltlite at {}: {e}", db_path.display()));
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    let api_token = ApiToken::from_value(TOKEN, root.as_path());
    let state = AppState {
        root: root.clone(),
        repo: Arc::new(dolt),
        app: Arc::new(app),
        qmd_daemon: Arc::new(QmdDaemon::new(QmdDaemonConfig::new((*root).clone()))),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        applets: Arc::new(datalib_http::applets::AppletRegistry::build(
            Vec::new(),
            (*root).clone(),
            None,
        )),
        api_token: api_token.clone(),
    };
    (router(state), api_token)
}

async fn status(app: &axum::Router, req: Request<Body>) -> StatusCode {
    app.clone().oneshot(req).await.unwrap().status()
}

/// The cookie the gate mints, as a `name=value` pair ready to send back.
fn session_cookie(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .expect("Set-Cookie always has a name=value pair")
        .to_string()
}

#[tokio::test]
async fn unauthenticated_requests_are_refused() {
    let (app, _) = app().await;

    // Reads.
    assert_eq!(
        status(
            &app,
            Request::get("/api/health").body(Body::empty()).unwrap()
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(
            &app,
            Request::get("/api/config").body(Body::empty()).unwrap()
        )
        .await,
        StatusCode::UNAUTHORIZED
    );

    // The write endpoints from the issue's exploit chain: rewrite the
    // config to a step with an arbitrary `command:`, then run it.
    let put_config = Request::put("/api/config")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({
                "text": "[[steps]]\nid = \"pwn\"\ncommand = \"/bin/sh -c 'touch /tmp/pwned'\"\n"
            }))
            .unwrap(),
        ))
        .unwrap();
    assert_eq!(status(&app, put_config).await, StatusCode::UNAUTHORIZED);

    let enqueue = Request::post("/api/sync/jobs")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({"kind": "all"})).unwrap(),
        ))
        .unwrap();
    assert_eq!(status(&app, enqueue).await, StatusCode::UNAUTHORIZED);

    // Persistent-JS writes (the issue's "secondary, same root cause").
    let put_lib = Request::put("/api/lib/pwn")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({"source": "() => () => {}"})).unwrap(),
        ))
        .unwrap();
    assert_eq!(status(&app, put_lib).await, StatusCode::UNAUTHORIZED);

    // The SPA itself, and the DACTAL page that shares its origin.
    assert_eq!(
        status(&app, Request::get("/").body(Body::empty()).unwrap()).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        status(
            &app,
            Request::get("/dactal/index.html")
                .body(Body::empty())
                .unwrap()
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn a_wrong_token_is_just_as_refused() {
    let (app, _) = app().await;
    for bad in [
        "",
        "nope",
        // A prefix of the real token — the comparison is exact, not
        // "starts with".
        &TOKEN[..TOKEN.len() - 1],
        &format!("{TOKEN}x"),
    ] {
        let req = Request::get("/api/health")
            .header("x-datalib-token", bad)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            status(&app, req).await,
            StatusCode::UNAUTHORIZED,
            "token {bad:?} must not authenticate"
        );
        let req = Request::get(format!("/api/health?token={bad}"))
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            status(&app, req).await,
            StatusCode::UNAUTHORIZED,
            "?token={bad:?} must not authenticate"
        );
    }
}

#[tokio::test]
async fn every_accepted_carrier_works() {
    let (app, _) = app().await;

    for req in [
        Request::get("/api/health")
            .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap(),
        Request::get("/api/health")
            .header("x-datalib-token", TOKEN)
            .body(Body::empty())
            .unwrap(),
        // `?token=` is left alone on API routes: the `EventSource` on
        // /api/sync/stream and any `<img src>` that needs it can't set
        // headers, and a redirect would break them.
        Request::get(format!("/api/health?token={TOKEN}"))
            .body(Body::empty())
            .unwrap(),
        Request::get("/api/health")
            .header(header::COOKIE, "other=1; datalib_token_x=y")
            .header("x-datalib-token", TOKEN)
            .body(Body::empty())
            .unwrap(),
    ] {
        assert_eq!(status(&app, req).await, StatusCode::OK);
    }
}

#[tokio::test]
async fn a_document_load_mints_a_session_cookie_and_it_authenticates() {
    let (app, _) = app().await;

    let req = Request::get("/")
        .header("x-datalib-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let set_cookie = resp
        .headers()
        .get(header::SET_COOKIE)
        .expect("document load must mint a session")
        .to_str()
        .unwrap()
        .to_string();

    // The two attributes doing the security work: unreadable from JS,
    // and never attached to a cross-site subresource request — which is
    // the whole of the #138 attack.
    assert!(set_cookie.contains("HttpOnly"), "{set_cookie}");
    assert!(set_cookie.contains("SameSite=Lax"), "{set_cookie}");
    assert!(set_cookie.contains("Path=/"), "{set_cookie}");
    // No `Secure`: we serve plain http on loopback, and the browser
    // would drop a `Secure` cookie outright.
    assert!(!set_cookie.contains("Secure"), "{set_cookie}");

    // And the cookie alone gets the browser through the gate for the
    // subresources the page will ask for next.
    let req = Request::get("/api/health")
        .header(header::COOKIE, session_cookie(&set_cookie))
        .body(Body::empty())
        .unwrap();
    assert_eq!(status(&app, req).await, StatusCode::OK);
}

#[tokio::test]
async fn a_token_in_a_document_url_is_redirected_away() {
    let (app, _) = app().await;

    // Bare launch URL.
    let req = Request::get(format!("/?token={TOKEN}"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert!(resp.status().is_redirection(), "{:?}", resp.status());
    assert_eq!(resp.headers().get(header::LOCATION).unwrap(), "/");
    assert!(
        resp.headers().get(header::SET_COOKIE).is_some(),
        "the redirect is what hands the browser its session"
    );

    // A card URL: the token is stripped, everything else survives —
    // otherwise the redirect would drop the user's card state.
    let req = Request::get(format!("/?token={TOKEN}&cols=abc&q=hi"))
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.headers().get(header::LOCATION).unwrap(),
        "/?cols=abc&q=hi"
    );
}

#[tokio::test]
async fn no_permissive_cors_header_is_advertised() {
    let (app, _) = app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::get("/api/health")
                .header("x-datalib-token", TOKEN)
                .header(header::ORIGIN, "https://evil.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers()
            .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
            .is_none(),
        "a cross-origin page must not be able to read API responses"
    );
}

#[tokio::test]
async fn the_agent_guides_stay_public() {
    let (app, _) = app().await;
    for path in ["/agent/cards.md", "/agent/config.md"] {
        assert_eq!(
            status(&app, Request::get(path).body(Body::empty()).unwrap()).await,
            StatusCode::OK,
            "{path} is how an agent learns to authenticate"
        );
    }
}
