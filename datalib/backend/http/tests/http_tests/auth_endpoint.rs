//! Integration test for the API token gate (`datalib_http::auth`).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use datalib_core::app_store::AppStore;
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
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    let api_token = ApiToken::from_value(TOKEN, root.as_path());
    let state = AppState {
        root: root.clone(),
        sync: datalib_http::supervisor::SyncControl::new(root.clone()),
        app: Arc::new(app),
        root_tx: tokio::sync::broadcast::channel(16).0,
        // No sampler running here, so the monitor is empty and every
        // tree reports as absent — the state a root nobody has walked
        // is in.
        usage: Default::default(),
        newer_root: Vec::new(),
        shutting_down: Default::default(),
        applets: Arc::new(datalib_http::applets::AppletRegistry::build(
            Vec::new(),
            (*root).clone(),
            None,
        )),
        api_token: api_token.clone(),
        frame_ancestors: Default::default(),
    };
    (router(state), api_token)
}

async fn status(app: &axum::Router, req: Request<Body>) -> StatusCode {
    app.clone().oneshot(req).await.unwrap().status()
}

fn session_cookie(set_cookie: &str) -> String {
    set_cookie
        .split(';')
        .next()
        .expect("Set-Cookie always has a name=value pair")
        .to_string()
}

fn set_cookies(resp: &axum::response::Response) -> Vec<String> {
    resp.headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .map(|v| v.to_str().unwrap().to_string())
        .collect()
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

    let enqueue = Request::post("/api/requests")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({"roots": []})).unwrap(),
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

    // The applet proxy. The grid's data left this binary for the
    // `unified_document_index` applet, and the token gate is an outermost layer
    // — so it still covers a route this process knows nothing about.
    // Worth pinning: an applet reached without the gate would be an
    // unauthenticated read of the whole mirror.
    assert_eq!(
        status(
            &app,
            Request::get("/applet/unified_document_index/search?q=&limit=1")
                .body(Body::empty())
                .unwrap()
        )
        .await,
        StatusCode::UNAUTHORIZED
    );

    // The SPA itself.
    assert_eq!(
        status(&app, Request::get("/").body(Body::empty()).unwrap()).await,
        StatusCode::UNAUTHORIZED
    );
}

/// The DACTAL page and its scripts are the one static exception: they
/// load without a session because the page runs in an opaque origin
/// (`sandbox` in the CSP header), where a session would be unusable —
/// and the fetch a sandboxed frame makes for its module carries none.
/// What must hold instead: the sandbox is on the wire, and an unknown
/// path under the prefix is a 404, never the app shell.
#[tokio::test]
async fn the_dactal_page_is_public_and_sandboxed() {
    let (app, _) = app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::get("/dactal/index.html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let csp = resp
        .headers()
        .get("content-security-policy")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(
        csp.split(';')
            .any(|d| d.trim().starts_with("sandbox") && d.contains("allow-scripts")),
        "the DACTAL document must be sandboxed by header, got {csp:?}"
    );
    assert!(
        !csp.contains("allow-same-origin"),
        "allow-same-origin would undo the sandbox: {csp:?}"
    );
    assert_eq!(
        status(
            &app,
            Request::get("/dactal/vendor/dactal.js")
                .body(Body::empty())
                .unwrap()
        )
        .await,
        StatusCode::OK
    );
    assert_eq!(
        status(
            &app,
            Request::get("/dactal/not-a-file")
                .body(Body::empty())
                .unwrap()
        )
        .await,
        StatusCode::NOT_FOUND
    );
}

/// An `/api` path that names no endpoint is a 404. The app shell's
/// fallback used to answer it with a 200 page, so a mistyped call — or
/// a step id with its slash left unencoded — read as success.
#[tokio::test]
async fn an_unknown_api_path_is_not_found_rather_than_the_app_shell() {
    let (app, _) = app().await;
    let authed = |req: axum::http::request::Builder| {
        req.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
            .body(Body::empty())
            .unwrap()
    };
    for req in [
        Request::get("/api/no/such/thing"),
        Request::post("/api/steps/slack/ingest/turn_off"),
    ] {
        let req = authed(req);
        let what = format!("{} {}", req.method(), req.uri());
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{what}");
    }
    let page = app
        .clone()
        .oneshot(authed(Request::get("/data_sources")))
        .await
        .unwrap();
    assert_eq!(
        page.status(),
        StatusCode::OK,
        "a UI route still gets the app"
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

/// A document load mints the session twice: a `Lax` cookie for the app
/// opened as the page, and a partitioned one for the app inside a frame
/// on another site (Studio), where a `Lax` cookie is not even stored.
/// Either one alone gets the browser in.
#[tokio::test]
async fn a_document_load_mints_two_session_cookies_and_either_authenticates() {
    let (app, _) = app().await;

    let req = Request::get("/")
        .header("x-datalib-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let cookies = set_cookies(&resp);
    let [lax, framed] = cookies.as_slice() else {
        panic!("document load must mint two cookies, got {cookies:?}");
    };

    // Unreadable from JS, and sent to this host alone: no `Domain`.
    for cookie in [lax, framed] {
        assert!(cookie.contains("HttpOnly"), "{cookie}");
        assert!(cookie.contains("Path=/"), "{cookie}");
        assert!(!cookie.contains("Domain"), "{cookie}");
    }
    // No `Secure` on the `Lax` copy: we serve plain http on loopback,
    // and the browser would drop a `Secure` cookie outright.
    assert!(lax.contains("SameSite=Lax"), "{lax}");
    assert!(!lax.contains("Secure"), "{lax}");
    // The framed copy lives in the framing site's jar and nowhere else.
    for attr in ["SameSite=None", "Secure", "Partitioned"] {
        assert!(framed.contains(attr), "{framed} lacks {attr}");
    }
    assert_ne!(session_cookie(lax), session_cookie(framed));

    for cookie in [lax, framed] {
        let req = Request::get("/api/health")
            .header(header::COOKIE, session_cookie(cookie))
            .header("sec-fetch-site", "same-origin")
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(&app, req).await, StatusCode::OK, "{cookie}");
    }
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

/// The cookie is attached by the browser whoever made the request, and a
/// page on another port of 127.0.0.1, or another app of a Studio
/// workspace, is the same *site*. `Sec-Fetch-Site` tells those from the
/// app's own requests, whatever credential they carry; a navigation from
/// anywhere still opens the app.
#[tokio::test]
async fn a_request_another_page_made_is_refused_whatever_it_carries() {
    let (app, _) = app().await;
    let launch = app
        .clone()
        .oneshot(
            Request::get(format!("/?token={TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let cookie = session_cookie(&set_cookies(&launch)[0]);
    let req = |method: &str, path: &str, site: &str, mode: &str| {
        Request::builder()
            .method(method)
            .uri(path)
            .header(header::COOKIE, &cookie)
            .header("sec-fetch-site", site)
            .header("sec-fetch-mode", mode)
            .body(Body::empty())
            .unwrap()
    };
    for (method, path, site, mode) in [
        ("POST", "/api/requests", "same-site", "navigate"),
        ("POST", "/api/requests", "cross-site", "no-cors"),
        ("GET", "/api/config", "same-site", "cors"),
        (
            "GET",
            "/applet/unified_document_index/search?q=",
            "cross-site",
            "no-cors",
        ),
    ] {
        assert_eq!(
            status(&app, req(method, path, site, mode)).await,
            StatusCode::FORBIDDEN,
            "{method} {path} from {site} ({mode})"
        );
    }
    // The token proves no more here: a page that had it is still another page.
    let with_token = Request::get("/api/config")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header("sec-fetch-site", "same-site")
        .header("sec-fetch-mode", "cors")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status(&app, with_token).await, StatusCode::FORBIDDEN);

    for (method, path, site, mode) in [
        ("GET", "/api/health", "same-origin", "cors"),
        ("GET", "/", "cross-site", "navigate"),
        ("GET", "/some/card", "same-site", "navigate"),
        ("GET", "/api/health", "none", "navigate"),
    ] {
        assert_eq!(
            status(&app, req(method, path, site, mode)).await,
            StatusCode::OK,
            "{method} {path} from {site} ({mode})"
        );
    }
}

/// Proxy mode is gone: a header claiming the owner sent the request
/// stands in for nothing, behind a proxy or not.
#[tokio::test]
async fn an_identity_header_is_no_credential() {
    let (app, _) = app().await;
    for path in ["/", "/api/health"] {
        let req = Request::get(path)
            .header("x-imbue-identity", r#"{"owner":true}"#)
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(&app, req).await, StatusCode::UNAUTHORIZED, "{path}");
    }
}
