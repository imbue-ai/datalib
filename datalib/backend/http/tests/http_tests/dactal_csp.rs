//! CI-visible guard for the two Content-Security-Policies: the app
//! page's (the second layer behind DOMPurify) and the DACTAL page's
//! (issue #138, mitigation 4).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::{router, ApiToken, AppState};
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "dactal-csp-itest";

async fn fetch(path: &str) -> (StatusCode, String) {
    let (status, _, body) = fetch_with_headers(path).await;
    (status, body)
}

async fn fetch_with_headers(path: &str) -> (StatusCode, axum::http::HeaderMap, String) {
    let db_path = tempfile::TempDir::with_prefix("datalib-http-csp-itest-")
        .expect("create tempdir")
        .keep()
        .join("backend_index.doltlite_db");
    let root: Arc<PathBuf> = Arc::new(db_path.parent().unwrap().to_path_buf());
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    let state = AppState {
        root: root.clone(),
        app: Arc::new(app),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        root_tx: tokio::sync::broadcast::channel(16).0,
        // No sampler running here, so the monitor is empty and every
        // tree reports as absent — the state a root nobody has walked
        // is in.
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
    let headers = resp.headers().clone();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (
        status,
        headers,
        String::from_utf8_lossy(&bytes).into_owned(),
    )
}

fn header_directive(csp: &str, name: &str) -> String {
    csp.split(';')
        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|d| d.starts_with(name))
        .unwrap_or_default()
}

/// The app page's policy is a header, so it covers the SPA fallback too
/// (`/anything` is the same document). What it must keep: no inline
/// script and no foreign script, which is what makes a sanitizer bypass
/// inert; `'unsafe-eval'`, which the card system needs; Tauri's IPC
/// origins in `connect-src`, or the desktop app's file pickers go dead.
#[tokio::test]
async fn the_app_page_carries_its_csp() {
    for path in ["/", "/some/card/route"] {
        let (status, headers, html) = fetch_with_headers(path).await;
        assert_eq!(status, StatusCode::OK, "{path}");
        assert!(
            html.contains("<div id=\"app\">"),
            "{path} is not the app shell"
        );
        let csp = headers
            .get("content-security-policy")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert_eq!(
            header_directive(csp, "script-src"),
            "script-src 'self' 'unsafe-eval'",
            "{path}: {csp}"
        );
        assert!(
            !header_directive(csp, "script-src").contains("'unsafe-inline'"),
            "{path}: {csp}"
        );
        let connect = header_directive(csp, "connect-src");
        assert!(connect.contains("'self'"), "{path}: {csp}");
        assert!(
            connect.contains("ipc:") && connect.contains("http://ipc.localhost"),
            "{path}: Tauri's IPC origins must stay in connect-src: {csp}"
        );
        assert_eq!(
            header_directive(csp, "object-src"),
            "object-src 'none'",
            "{csp}"
        );
        // No remote host, ever: a rendered email's tracking pixel is an
        // `<img>`, and this is the layer that holds when the sanitizer
        // misses one (issue #648). A load the person asks for goes
        // through `/api/remote`, which is 'self'.
        assert_eq!(
            header_directive(csp, "img-src"),
            "img-src 'self' data: blob:",
            "{csp}"
        );
        assert_eq!(
            header_directive(csp, "media-src"),
            "media-src 'self' data: blob:",
            "{csp}"
        );
        assert_eq!(
            header_directive(csp, "frame-ancestors"),
            "frame-ancestors 'none'",
            "{csp}"
        );
        assert_eq!(
            headers.get("x-content-type-options").unwrap(),
            "nosniff",
            "{path}"
        );
    }
    // A bundle asset is not a document; the policy belongs to the page.
    let (status, headers, _) = fetch_with_headers("/dactal/main.js").await;
    assert_eq!(status, StatusCode::OK);
    assert!(headers.get("content-security-policy").is_none());
}

#[tokio::test]
async fn the_dactal_page_still_carries_its_csp() {
    let (status, html) = fetch("/dactal/index.html").await;
    assert_eq!(status, StatusCode::OK);

    assert!(
        html.contains("Content-Security-Policy"),
        "the DACTAL page must declare a CSP — without it the vendored \
         engine's dactal.org paths are live again"
    );

    // The two directives that do the work. `script-src 'self'` kills the
    // <script src="https://dactal.org/…"> injections in
    // dactal_utils.js:325 and :381; `connect-src 'none'` kills the
    // fetch() in :393 that feeds `new Function` — and every other
    // request, since the page's rows arrive by message from the host.
    assert_eq!(
        directive(&html, "script-src"),
        "script-src 'self' 'unsafe-eval'",
        "script-src must stay 'self' — plus 'unsafe-eval', which the \
         query language genuinely needs. Full policy:\n{}",
        csp_of(&html)
    );
    assert_eq!(
        directive(&html, "connect-src"),
        "connect-src 'none'",
        "connect-src must stay 'none'. Full policy:\n{}",
        csp_of(&html)
    );

    // The failure mode this is really here for: someone hits a blocked
    // inline script and "fixes" it by widening script-src, which undoes
    // the whole mitigation.
    let script_src = directive(&html, "script-src");
    assert!(
        !script_src.contains("'unsafe-inline'"),
        "script-src must never allow 'unsafe-inline' — it re-opens \
         exactly what the CSP closes. If an inline script is in the way, \
         move it into a file (that is why main.js exists). Found: \
         {script_src:?}"
    );
}

/// The `content="…"` of the page's CSP meta tag. Anchored on the whole
/// `http-equiv=` attribute, not on the bare policy name: the comment
/// above the tag explains the policy and names it too, so a looser
/// search lands in the prose instead of the markup.
fn csp_of(html: &str) -> String {
    const ANCHOR: &str = r#"http-equiv="Content-Security-Policy""#;
    let Some((_, rest)) = html.split_once(ANCHOR) else {
        return String::new();
    };
    let Some((_, rest)) = rest.split_once("content=\"") else {
        return String::new();
    };
    rest.split('"').next().unwrap_or_default().to_string()
}

fn directive(html: &str, name: &str) -> String {
    let csp = csp_of(html);
    csp.split(';')
        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|d| d.starts_with(name))
        .unwrap_or_default()
}

/// The other half of the same invariant, from the page's side: the CSP
/// forbids inline script, so every `<script>` on the page must load from
/// a file. Moving `main.js` back inline would break the page at runtime
/// — in a browser nobody runs in CI — so catch it here instead.
#[tokio::test]
async fn the_dactal_page_has_no_inline_script() {
    let (_, html) = fetch("/dactal/index.html").await;
    for tag in html.split("<script").skip(1) {
        let open = tag.split('>').next().unwrap_or_default();
        assert!(
            open.contains("src="),
            "found a <script{open}> with no src — the page's CSP forbids \
             inline script, so this cannot run. Put it in a file next to \
             main.js."
        );
    }
    assert!(
        html.contains("main.js"),
        "the page's own logic should still load from main.js"
    );
}
