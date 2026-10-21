//! Integration test for the API token gate (`datalib_http::auth`).

use axum::body::Body;
use axum::http::{header, Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::{router, ApiToken, AppState, TrustedProxy};
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
    app_behind(None).await
}

async fn app_behind(trusted_proxy: Option<TrustedProxy>) -> (axum::Router, ApiToken) {
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
        trusted_proxy,
        allowed_hosts: Default::default(),
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

const OWNER: &str = r#"{"owner":true,"user_id":"u-owner","email":"owner@example.test"}"#;
const VISITOR: &str = r#"{"owner":false,"user_id":"u-guest","email":"guest@example.test"}"#;

/// Behind the proxy, the proxy's word is the credential: the owner gets
/// the page and the API with no token, and no session cookie is minted,
/// since the proxy's page is another site and the browser would not
/// send one anyway.
#[tokio::test]
async fn behind_a_trusted_proxy_the_owner_needs_no_token() {
    let (app, _) = app_behind(Some(TrustedProxy::Imbue)).await;
    for path in ["/", "/some/card/route", "/api/health"] {
        let resp = app
            .clone()
            .oneshot(
                Request::get(path)
                    .header("x-imbue-identity", OWNER)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{path}");
        assert!(
            resp.headers().get(header::SET_COOKIE).is_none(),
            "{path}: the proxy's header authenticates every request; no cookie to mint"
        );
    }
}

/// A shared workspace's visitor reaches the same server through the same
/// proxy, marked `owner:false`. The mirror is the owner's alone.
#[tokio::test]
async fn behind_a_trusted_proxy_a_visitor_is_refused() {
    let (app, _) = app_behind(Some(TrustedProxy::Imbue)).await;
    for path in [
        "/",
        "/api/health",
        "/applet/unified_document_index/search?q=&limit=1",
    ] {
        let req = Request::get(path)
            .header("x-imbue-identity", VISITOR)
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(&app, req).await, StatusCode::FORBIDDEN, "{path}");
    }
}

/// The token still works behind the proxy (an agent's curl on the same
/// host sends no identity header), and a request with neither is
/// refused as it always was — a malformed header included.
#[tokio::test]
async fn behind_a_trusted_proxy_the_token_still_works_and_nothing_else_does() {
    let (app, _) = app_behind(Some(TrustedProxy::Imbue)).await;
    let with_token = Request::get("/api/health")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(status(&app, with_token).await, StatusCode::OK);

    let bare = Request::get("/api/health").body(Body::empty()).unwrap();
    assert_eq!(status(&app, bare).await, StatusCode::UNAUTHORIZED);
    for bad in ["", "owner", r#"{"owner":"yes"}"#, "{}"] {
        let req = Request::get("/api/health")
            .header("x-imbue-identity", bad)
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            status(&app, req).await,
            StatusCode::UNAUTHORIZED,
            "identity {bad:?} must not authenticate"
        );
    }
}

/// Without the switch, the header is just bytes a client sent: a page
/// that forges it on a laptop deployment gets nowhere.
#[tokio::test]
async fn without_a_trusted_proxy_the_identity_header_is_ignored() {
    let (app, _) = app().await;
    for path in ["/", "/api/health"] {
        let req = Request::get(path)
            .header("x-imbue-identity", OWNER)
            .body(Body::empty())
            .unwrap();
        assert_eq!(status(&app, req).await, StatusCode::UNAUTHORIZED, "{path}");
    }
}

/// The proxy stamps the owner on everything it forwards, so a sibling
/// app's page could make the browser send an authenticated `POST`. The
/// browser's `Sec-Fetch-Site` says where a request came from; anything
/// from another site but a navigation is refused.
#[tokio::test]
async fn behind_a_trusted_proxy_a_write_from_another_origin_is_refused() {
    let (app, _) = app_behind(Some(TrustedProxy::Imbue)).await;
    let check = |site: Option<&str>| {
        let mut req = Request::post("/api/config/check")
            .header("x-imbue-identity", OWNER)
            .header("content-type", "application/json");
        if let Some(site) = site {
            req = req.header("sec-fetch-site", site);
        }
        req.body(Body::from(
            serde_json::to_vec(&serde_json::json!({"text": ""})).unwrap(),
        ))
        .unwrap()
    };
    for site in ["same-site", "cross-site"] {
        assert_eq!(
            status(&app, check(Some(site))).await,
            StatusCode::FORBIDDEN,
            "a write with Sec-Fetch-Site: {site}"
        );
    }
    // The app's own request, and a non-browser client the proxy let
    // through: past the gate, whatever the endpoint then says.
    for site in [Some("same-origin"), Some("none"), None] {
        let got = status(&app, check(site)).await;
        assert!(
            got != StatusCode::FORBIDDEN && got != StatusCode::UNAUTHORIZED,
            "Sec-Fetch-Site: {site:?} must pass the gate, got {got}"
        );
    }
    // The proxy's own page framing the app is a navigation from another
    // site; a script there reading the API is not.
    let read = |mode: &str| {
        Request::get("/")
            .header("x-imbue-identity", OWNER)
            .header("sec-fetch-site", "cross-site")
            .header("sec-fetch-mode", mode)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(status(&app, read("navigate")).await, StatusCode::OK);
    assert_eq!(status(&app, read("cors")).await, StatusCode::FORBIDDEN);
}

async fn cookie_for(app: &axum::Router) -> String {
    let resp = app
        .clone()
        .oneshot(
            Request::get(format!("/?token={TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    session_cookie(
        resp.headers()
            .get(header::SET_COOKIE)
            .expect("the launch URL mints a session")
            .to_str()
            .unwrap(),
    )
}

/// A link from anywhere still opens the app: a top-level navigation
/// carries the `SameSite=Lax` cookie, and it is served whatever site it
/// came from. A non-navigation read from another site is not.
#[tokio::test]
async fn a_navigation_from_another_site_is_served_and_a_fetch_is_not() {
    let (app, _) = app().await;
    let cookie = cookie_for(&app).await;
    let from_elsewhere = |path: &str, mode: &str| {
        Request::get(path)
            .header(header::HOST, "127.0.0.1:8731")
            .header(header::COOKIE, &cookie)
            .header("sec-fetch-site", "cross-site")
            .header("sec-fetch-mode", mode)
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        status(&app, from_elsewhere("/some/card/route", "navigate")).await,
        StatusCode::OK
    );
    for mode in ["cors", "no-cors"] {
        assert_eq!(
            status(&app, from_elsewhere("/api/health", mode)).await,
            StatusCode::FORBIDDEN,
            "a {mode} read from another site"
        );
    }
}

/// A `Host` the server does not answer to is a page that pointed its own
/// name at this address (DNS rebinding). Behind the proxy it could forge
/// the owner's identity header, so the name is checked before the header
/// is believed.
#[tokio::test]
async fn a_rebound_host_is_refused_even_with_the_owners_identity() {
    let (app, _) = app_behind(Some(TrustedProxy::Imbue)).await;
    let as_owner = |host: &str| {
        Request::get("/api/health")
            .header(header::HOST, host)
            .header("x-imbue-identity", OWNER)
            .header("sec-fetch-site", "same-origin")
            .header("sec-fetch-mode", "cors")
            .body(Body::empty())
            .unwrap()
    };
    assert_eq!(
        status(&app, as_owner("rebind.evil.example:8731")).await,
        StatusCode::FORBIDDEN
    );
    // What the Imbue forwarder sends: the registered `localhost:<port>`.
    assert_eq!(
        status(&app, as_owner("localhost:8731")).await,
        StatusCode::OK
    );
}

/// Outside proxy mode a rebound page has no credential to send, so `Host`
/// is not checked: a server behind a gateway that passes the browser's own
/// `Host` through, as Imbue Studio's share gateway does, needs no list.
#[tokio::test]
async fn outside_proxy_mode_any_host_is_served() {
    let (app, _) = app_behind(None).await;
    let req = Request::get("/api/health")
        .header(header::HOST, "datalib-x7k9.share.example")
        .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
        .header("sec-fetch-site", "same-origin")
        .header("sec-fetch-mode", "cors")
        .body(Body::empty())
        .unwrap();
    assert_eq!(status(&app, req).await, StatusCode::OK);
}

/// A browser too old to send `Sec-Fetch-Site` still sends `Origin` on a
/// write; with only the cookie to go on, that has to name this server.
/// A caller that presents the token proved more than a cookie can.
#[tokio::test]
async fn a_cookie_write_without_fetch_metadata_needs_this_servers_origin() {
    let (app, _) = app().await;
    let cookie = cookie_for(&app).await;
    let check = |origin: Option<&str>, cookie: Option<&str>| {
        let mut req = Request::post("/api/config/check")
            .header(header::HOST, "127.0.0.1:8731")
            .header("content-type", "application/json");
        if let Some(origin) = origin {
            req = req.header(header::ORIGIN, origin);
        }
        req = match cookie {
            Some(cookie) => req.header(header::COOKIE, cookie),
            None => req.header("x-datalib-token", TOKEN),
        };
        req.body(Body::from(r#"{"text":""}"#)).unwrap()
    };
    for origin in [
        None,
        Some("http://evil.example"),
        Some("http://127.0.0.1:9999"),
    ] {
        assert_eq!(
            status(&app, check(origin, Some(&cookie))).await,
            StatusCode::FORBIDDEN,
            "a cookie write from {origin:?}"
        );
    }
    for (origin, cookie) in [
        (Some("http://127.0.0.1:8731"), Some(cookie.as_str())),
        (None, None),
        (Some("http://evil.example"), None),
    ] {
        let got = status(&app, check(origin, cookie)).await;
        assert!(
            got != StatusCode::FORBIDDEN && got != StatusCode::UNAUTHORIZED,
            "origin {origin:?}, cookie {}: must pass the gate, got {got}",
            cookie.is_some()
        );
    }
}
